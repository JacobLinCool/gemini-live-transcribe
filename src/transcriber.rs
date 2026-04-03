//! Gemini Live API transport.

use std::collections::VecDeque;

use anyhow::{Context, Result, anyhow};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use crate::session_log::SessionLogger;

const WS_URL: &str = "wss://generativelanguage.googleapis.com/ws/\
                       google.ai.generativelanguage.v1beta.GenerativeService.BidiGenerateContent";

type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

#[derive(Debug, Clone)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

#[derive(Debug, Clone)]
pub enum SessionMode {
    Draft,
    Finalizer { tool: ToolDefinition },
}

#[derive(Debug, Clone)]
pub struct Config {
    pub api_key: String,
    pub model: String,
    pub sample_rate: u32,
    pub system_instruction: Option<String>,
    pub session_handle: Option<String>,
    pub mode: SessionMode,
}

impl Config {
    #[allow(dead_code)]
    pub fn new(api_key: String) -> Self {
        Self {
            api_key,
            model: "models/gemini-3.1-flash-live-preview".into(),
            sample_rate: 16_000,
            system_instruction: None,
            session_handle: None,
            mode: SessionMode::Draft,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageMetadata {
    pub prompt_token_count: u32,
    pub cached_content_token_count: u32,
    pub response_token_count: u32,
    pub tool_use_prompt_token_count: u32,
    pub thoughts_token_count: u32,
    pub total_token_count: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FunctionCallRequest {
    pub id: String,
    pub name: String,
    pub args: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FunctionResponse {
    pub id: String,
    pub name: String,
    pub response: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TranscriptEvent {
    SetupComplete,
    InputTranscription(String),
    ModelText,
    GenerationComplete,
    TurnComplete,
    ToolCall(Vec<FunctionCallRequest>),
    ToolCallCancellation(Vec<String>),
    UsageMetadata(UsageMetadata),
    SessionResumptionUpdate {
        new_handle: Option<String>,
        resumable: bool,
    },
    GoAway {
        time_left: String,
    },
    ConnectionClosed(String),
}

pub struct AudioSender {
    sink: SplitSink<WsStream, Message>,
    sample_rate: u32,
    logger: SessionLogger,
}

impl AudioSender {
    pub async fn send_audio(&mut self, samples: &[f32]) -> Result<()> {
        if samples.is_empty() {
            return Ok(());
        }

        let pcm_bytes = f32_to_i16_le_bytes(samples);
        let encoded = BASE64.encode(&pcm_bytes);

        let msg = json!({
            "realtimeInput": {
                "audio": {
                    "data": encoded,
                    "mimeType": format!("audio/pcm;rate={}", self.sample_rate),
                }
            }
        });

        self.send_json("audio", msg).await
    }

    pub async fn send_realtime_text(&mut self, text: &str) -> Result<()> {
        let msg = json!({
            "realtimeInput": {
                "text": text,
            }
        });

        self.send_json("realtime_text", msg).await
    }

    pub async fn send_activity_start(&mut self) -> Result<()> {
        let msg = json!({
            "realtimeInput": {
                "activityStart": {}
            }
        });
        self.send_json("activity_start", msg).await
    }

    pub async fn send_activity_end(&mut self) -> Result<()> {
        let msg = json!({
            "realtimeInput": {
                "activityEnd": {}
            }
        });
        self.send_json("activity_end", msg).await
    }

    pub async fn send_tool_response(&mut self, responses: &[FunctionResponse]) -> Result<()> {
        if responses.is_empty() {
            return Ok(());
        }

        let msg = json!({
            "toolResponse": {
                "functionResponses": responses.iter().map(|response| {
                    json!({
                        "id": response.id,
                        "name": response.name,
                        "response": response.response,
                    })
                }).collect::<Vec<_>>()
            }
        });

        self.send_json("tool_response", msg).await
    }

    pub async fn end_audio(&mut self) -> Result<()> {
        let msg = json!({ "realtimeInput": { "audioStreamEnd": true } });
        self.send_json("audio_stream_end", msg).await
    }

    pub async fn close(mut self) -> Result<()> {
        self.logger
            .log_lifecycle("client_close", serde_json::json!({}));
        self.sink.close().await?;
        Ok(())
    }

    async fn send_json(&mut self, event: &str, msg: Value) -> Result<()> {
        self.logger.log_outbound_json(event, &msg);
        self.sink
            .send(Message::Text(msg.to_string().into()))
            .await
            .with_context(|| format!("send {event} message"))
    }
}

pub struct TranscriptReceiver {
    stream: SplitStream<WsStream>,
    logger: SessionLogger,
    pending_events: VecDeque<TranscriptEvent>,
}

impl TranscriptReceiver {
    pub async fn next_event(&mut self) -> Result<Option<TranscriptEvent>> {
        if let Some(event) = self.pending_events.pop_front() {
            return Ok(Some(event));
        }

        loop {
            let msg = match self.stream.next().await {
                Some(Ok(msg)) => msg,
                Some(Err(error)) => return Err(error.into()),
                None => return Ok(None),
            };

            let text = match &msg {
                Message::Text(text) => text.to_string(),
                Message::Binary(bytes) => String::from_utf8_lossy(bytes).into_owned(),
                Message::Close(frame) => {
                    tracing::debug!("ws close frame: {frame:?}");
                    let reason = format_close_frame(frame.as_ref());
                    self.logger.log_close(&reason);
                    return Ok(Some(TranscriptEvent::ConnectionClosed(reason)));
                }
                _ => continue,
            };

            tracing::trace!("ws recv: {text}");
            self.logger.log_inbound_message_text(&text);

            let value: Value = serde_json::from_str(&text).context("parse server message")?;
            let events = parse_server_message_events(&value)?;
            if events.is_empty() {
                tracing::trace!("unhandled: {text}");
                continue;
            }

            self.pending_events.extend(events);
            return Ok(self.pending_events.pop_front());
        }
    }
}

pub async fn connect(
    config: Config,
    logger: SessionLogger,
) -> Result<(AudioSender, TranscriptReceiver)> {
    let url = format!("{WS_URL}?key={}", config.api_key);

    let (ws, _) = tokio_tungstenite::connect_async(&url)
        .await
        .context("connect to Gemini Live API")?;

    let (mut sink, stream) = ws.split();
    let setup = build_setup_message(&config);

    logger.log_outbound_json("setup", &setup);

    sink.send(Message::Text(setup.to_string().into()))
        .await
        .context("send setup message")?;

    Ok((
        AudioSender {
            sink,
            sample_rate: config.sample_rate,
            logger: logger.clone(),
        },
        TranscriptReceiver {
            stream,
            logger,
            pending_events: VecDeque::new(),
        },
    ))
}

fn build_setup_message(config: &Config) -> Value {
    let mut setup = json!({
        "setup": {
            "model": normalize_model_name(&config.model),
            "generationConfig": {
                "responseModalities": ["AUDIO"],
            },
            "contextWindowCompression": {
                "slidingWindow": {
                    "targetTokens": 32768,
                },
            },
            "sessionResumption": {},
        }
    });

    match &config.mode {
        SessionMode::Draft => {
            setup["setup"]["inputAudioTranscription"] = json!({});
        }
        SessionMode::Finalizer { tool } => {
            if should_use_medium_thinking_level(&config.model) {
                setup["setup"]["generationConfig"]["thinkingConfig"] = json!({
                    "thinkingLevel": "medium",
                });
            }
            setup["setup"]["realtimeInputConfig"] = json!({
                "automaticActivityDetection": {
                    "disabled": true,
                }
            });
            setup["setup"]["tools"] = json!([{
                "functionDeclarations": [{
                    "name": tool.name,
                    "description": tool.description,
                    "parameters": tool.parameters,
                }]
            }]);
        }
    }

    if let Some(instruction) = config.system_instruction.as_deref() {
        setup["setup"]["systemInstruction"] = json!({
            "parts": [{ "text": instruction }]
        });
    }

    if let Some(handle) = sanitize_session_handle(config.session_handle.as_deref()) {
        setup["setup"]["sessionResumption"]["handle"] = Value::String(handle);
    }

    setup
}

fn normalize_model_name(model: &str) -> String {
    let trimmed = model.trim();
    if trimmed.starts_with("models/") {
        trimmed.to_owned()
    } else {
        format!("models/{trimmed}")
    }
}

fn should_use_medium_thinking_level(model: &str) -> bool {
    let normalized = normalize_model_name(model);
    let model = normalized.strip_prefix("models/").unwrap_or(&normalized);
    model.starts_with("gemini-3") && model.contains("flash")
}

fn format_close_frame(frame: Option<&CloseFrame>) -> String {
    match frame {
        Some(frame) if frame.reason.is_empty() => format!("code {}", frame.code),
        Some(frame) => format!("code {}: {}", frame.code, frame.reason),
        None => "connection closed without close frame".to_string(),
    }
}

fn f32_to_i16_le_bytes(samples: &[f32]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(samples.len() * 2);
    for &sample in samples {
        let clamped = sample.clamp(-1.0, 1.0);
        let value = (clamped * 32767.0) as i16;
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    bytes
}

fn parse_server_message_events(value: &Value) -> Result<Vec<TranscriptEvent>> {
    if let Some(error) = value.get("error") {
        let message = error
            .get("message")
            .and_then(|message| message.as_str())
            .unwrap_or_else(|| error.as_str().unwrap_or("unknown Gemini Live error"));
        return Err(anyhow!("Gemini Live error: {message}"));
    }

    let mut events = Vec::new();

    if let Some(usage_metadata) = value.get("usageMetadata").and_then(parse_usage_metadata) {
        events.push(TranscriptEvent::UsageMetadata(usage_metadata));
    }

    if value.get("setupComplete").is_some() {
        events.push(TranscriptEvent::SetupComplete);
        return Ok(events);
    }

    if let Some(go_away) = value.get("goAway") {
        let time_left = go_away
            .get("timeLeft")
            .and_then(|time| time.as_str())
            .unwrap_or("unknown")
            .to_string();
        events.push(TranscriptEvent::GoAway { time_left });
        return Ok(events);
    }

    if let Some(update) = value.get("sessionResumptionUpdate") {
        let resumable = update
            .get("resumable")
            .and_then(|value| value.as_bool())
            .unwrap_or(false);
        let new_handle =
            sanitize_session_handle(update.get("newHandle").and_then(|value| value.as_str()));
        events.push(TranscriptEvent::SessionResumptionUpdate {
            new_handle,
            resumable,
        });
        return Ok(events);
    }

    if let Some(tool_call) = value.get("toolCall") {
        let function_calls = tool_call
            .get("functionCalls")
            .and_then(Value::as_array)
            .map(|calls| {
                calls
                    .iter()
                    .map(parse_function_call)
                    .collect::<Result<Vec<_>>>()
            })
            .transpose()?
            .unwrap_or_default();
        if !function_calls.is_empty() {
            events.push(TranscriptEvent::ToolCall(function_calls));
        }
        return Ok(events);
    }

    if let Some(tool_call_cancellation) = value.get("toolCallCancellation") {
        let ids = tool_call_cancellation
            .get("ids")
            .and_then(Value::as_array)
            .map(|ids| {
                ids.iter()
                    .filter_map(|value| value.as_str().map(str::to_owned))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if !ids.is_empty() {
            events.push(TranscriptEvent::ToolCallCancellation(ids));
        }
        return Ok(events);
    }

    if let Some(content) = value.get("serverContent") {
        if let Some(text) = content
            .get("inputTranscription")
            .and_then(|input| input.get("text"))
            .and_then(|text| text.as_str())
            .filter(|text| !text.is_empty())
        {
            events.push(TranscriptEvent::InputTranscription(text.to_string()));
        }

        if let Some(parts) = content
            .get("modelTurn")
            .and_then(|turn| turn.get("parts"))
            .and_then(|parts| parts.as_array())
            && parts.iter().any(|part| {
                part.get("text")
                    .and_then(|text| text.as_str())
                    .is_some_and(|text| !text.is_empty())
            })
        {
            events.push(TranscriptEvent::ModelText);
        }

        if content
            .get("generationComplete")
            .and_then(|flag| flag.as_bool())
            == Some(true)
        {
            events.push(TranscriptEvent::GenerationComplete);
        }

        if content.get("turnComplete").and_then(|flag| flag.as_bool()) == Some(true) {
            events.push(TranscriptEvent::TurnComplete);
        }
    }

    Ok(events)
}

fn parse_function_call(value: &Value) -> Result<FunctionCallRequest> {
    let id = value
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| anyhow!("tool call missing id"))?
        .to_owned();
    let name = value
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
        .ok_or_else(|| anyhow!("tool call missing name"))?
        .to_owned();
    let args = value
        .get("args")
        .cloned()
        .unwrap_or_else(|| Value::Object(Default::default()));

    Ok(FunctionCallRequest { id, name, args })
}

fn parse_usage_metadata(value: &Value) -> Option<UsageMetadata> {
    let usage = UsageMetadata {
        prompt_token_count: read_u32_field(value, "promptTokenCount").unwrap_or(0),
        cached_content_token_count: read_u32_field(value, "cachedContentTokenCount").unwrap_or(0),
        response_token_count: read_u32_field(value, "responseTokenCount").unwrap_or(0),
        tool_use_prompt_token_count: read_u32_field(value, "toolUsePromptTokenCount").unwrap_or(0),
        thoughts_token_count: read_u32_field(value, "thoughtsTokenCount").unwrap_or(0),
        total_token_count: read_u32_field(value, "totalTokenCount").unwrap_or(0),
    };

    if usage.prompt_token_count == 0
        && usage.cached_content_token_count == 0
        && usage.response_token_count == 0
        && usage.tool_use_prompt_token_count == 0
        && usage.thoughts_token_count == 0
        && usage.total_token_count == 0
    {
        return None;
    }

    Some(usage)
}

fn read_u32_field(value: &Value, field: &str) -> Option<u32> {
    value
        .get(field)?
        .as_u64()
        .map(|value| value.min(u64::from(u32::MAX)) as u32)
}

fn sanitize_session_handle(handle: Option<&str>) -> Option<String> {
    let handle = handle?;
    let trimmed = handle.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{
        Config, FunctionResponse, SessionMode, ToolDefinition, TranscriptEvent, UsageMetadata,
        build_setup_message, f32_to_i16_le_bytes, normalize_model_name,
        parse_server_message_events, sanitize_session_handle,
    };

    #[test]
    fn converts_f32_audio_to_i16_pcm() {
        let bytes = f32_to_i16_le_bytes(&[-1.0, -0.5, 0.0, 0.5, 1.0]);
        let expected = vec![i16::MIN + 1, -16383, 0, 16383, i16::MAX];
        let decoded = bytes
            .chunks_exact(2)
            .map(|chunk| i16::from_le_bytes([chunk[0], chunk[1]]))
            .collect::<Vec<_>>();

        assert_eq!(decoded, expected);
    }

    #[test]
    fn build_setup_message_enables_compression_and_resumption_for_draft() {
        let setup = build_setup_message(&Config {
            api_key: "key".into(),
            model: "gemini-3.1-flash-live-preview".into(),
            sample_rate: 16_000,
            system_instruction: Some("Keep filler words.".into()),
            session_handle: Some("resume-123".into()),
            mode: SessionMode::Draft,
        });

        assert_eq!(
            setup["setup"]["contextWindowCompression"],
            json!({ "slidingWindow": { "targetTokens": 32768 } })
        );
        assert_eq!(setup["setup"]["sessionResumption"]["handle"], "resume-123");
        assert_eq!(setup["setup"]["inputAudioTranscription"], json!({}));
    }

    #[test]
    fn build_setup_message_disables_aad_for_finalizer() {
        let setup = build_setup_message(&Config {
            api_key: "key".into(),
            model: "gemini-3.1-flash-live-preview".into(),
            sample_rate: 16_000,
            system_instruction: Some("Finalize transcript.".into()),
            session_handle: None,
            mode: SessionMode::Finalizer {
                tool: ToolDefinition {
                    name: "finalize_transcript".into(),
                    description: "Finalize a transcript turn.".into(),
                    parameters: json!({
                        "type": "object",
                        "properties": {
                            "text": { "type": "string" }
                        }
                    }),
                },
            },
        });

        assert_eq!(
            setup["setup"]["realtimeInputConfig"]["automaticActivityDetection"]["disabled"],
            true
        );
        assert_eq!(
            setup["setup"]["generationConfig"]["thinkingConfig"]["thinkingLevel"],
            "medium"
        );
        assert_eq!(
            setup["setup"]["tools"][0]["functionDeclarations"][0]["name"],
            "finalize_transcript"
        );
    }

    #[test]
    fn build_setup_message_keeps_draft_without_thinking_level() {
        let setup = build_setup_message(&Config {
            api_key: "key".into(),
            model: "gemini-3.1-flash-live-preview".into(),
            sample_rate: 16_000,
            system_instruction: Some("Keep filler words.".into()),
            session_handle: None,
            mode: SessionMode::Draft,
        });

        assert!(setup["setup"]["generationConfig"]["thinkingConfig"].is_null());
    }

    #[test]
    fn build_setup_message_skips_medium_thinking_for_non_flash_models() {
        let setup = build_setup_message(&Config {
            api_key: "key".into(),
            model: "gemini-3.1-pro-preview".into(),
            sample_rate: 16_000,
            system_instruction: Some("Finalize transcript.".into()),
            session_handle: None,
            mode: SessionMode::Finalizer {
                tool: ToolDefinition {
                    name: "finalize_transcript".into(),
                    description: "Finalize a transcript turn.".into(),
                    parameters: json!({
                        "type": "object",
                        "properties": {
                            "text": { "type": "string" }
                        }
                    }),
                },
            },
        });

        assert!(setup["setup"]["generationConfig"]["thinkingConfig"].is_null());
    }

    #[test]
    fn normalizes_model_name_for_websocket_setup() {
        assert_eq!(
            normalize_model_name("gemini-3.1-flash-live-preview"),
            "models/gemini-3.1-flash-live-preview"
        );
        assert_eq!(
            normalize_model_name("models/custom"),
            "models/custom".to_string()
        );
    }

    #[test]
    fn parses_usage_metadata_and_turn_completion_from_same_message() {
        let events = parse_server_message_events(&json!({
            "usageMetadata": {
                "promptTokenCount": 12,
                "cachedContentTokenCount": 3,
                "responseTokenCount": 4,
                "toolUsePromptTokenCount": 0,
                "thoughtsTokenCount": 0,
                "totalTokenCount": 19
            },
            "serverContent": {
                "inputTranscription": {
                    "text": "hello world"
                },
                "generationComplete": true,
                "turnComplete": true
            }
        }))
        .expect("events should parse");

        assert_eq!(
            events[0],
            TranscriptEvent::UsageMetadata(UsageMetadata {
                prompt_token_count: 12,
                cached_content_token_count: 3,
                response_token_count: 4,
                tool_use_prompt_token_count: 0,
                thoughts_token_count: 0,
                total_token_count: 19,
            })
        );
        assert!(matches!(
            &events[1],
            TranscriptEvent::InputTranscription(text) if text == "hello world"
        ));
        assert!(matches!(events[2], TranscriptEvent::GenerationComplete));
        assert!(matches!(events[3], TranscriptEvent::TurnComplete));
    }

    #[test]
    fn parses_tool_call_requests() {
        let events = parse_server_message_events(&json!({
            "toolCall": {
                "functionCalls": [
                    {
                        "id": "call-1",
                        "name": "finalize_transcript",
                        "args": {
                            "text": "hello"
                        }
                    }
                ]
            }
        }))
        .expect("tool call should parse");

        assert!(matches!(
            &events[0],
            TranscriptEvent::ToolCall(calls)
            if calls.len() == 1
                && calls[0].id == "call-1"
                && calls[0].name == "finalize_transcript"
                && calls[0].args["text"] == "hello"
        ));
    }

    #[test]
    fn parses_tool_call_cancellation() {
        let events = parse_server_message_events(&json!({
            "toolCallCancellation": {
                "ids": ["call-1", "call-2"]
            }
        }))
        .expect("tool call cancellation should parse");

        assert!(matches!(
            &events[0],
            TranscriptEvent::ToolCallCancellation(ids)
                if ids == &vec!["call-1".to_string(), "call-2".to_string()]
        ));
    }

    #[test]
    fn trims_empty_session_handles() {
        assert_eq!(sanitize_session_handle(Some("  ")), None);
        assert_eq!(
            sanitize_session_handle(Some(" handle-123 ")),
            Some("handle-123".into())
        );
    }

    #[test]
    fn function_response_structure_is_serializable() {
        let response = FunctionResponse {
            id: "call-1".into(),
            name: "finalize_transcript".into(),
            response: json!({ "accepted": true }),
        };
        assert_eq!(response.name, "finalize_transcript");
    }
}
