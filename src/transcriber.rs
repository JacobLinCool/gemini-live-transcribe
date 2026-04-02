//! Gemini Live API transcriber.

use std::collections::VecDeque;

use anyhow::{Context, Result};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use crate::session_log::SessionLogger;

const WS_URL: &str = "wss://generativelanguage.googleapis.com/ws/\
                       google.ai.generativelanguage.v1beta.GenerativeService.BidiGenerateContent";

type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

pub struct Config {
    pub api_key: String,
    pub model: String,
    pub sample_rate: u32,
    pub system_instruction: Option<String>,
    pub session_handle: Option<String>,
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

#[derive(Debug, Clone)]
pub enum TranscriptEvent {
    SetupComplete,
    InputTranscription(String),
    ModelText,
    TurnComplete,
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

        let msg = serde_json::json!({
            "realtimeInput": {
                "audio": {
                    "data": encoded,
                    "mimeType": format!("audio/pcm;rate={}", self.sample_rate),
                }
            }
        });

        self.sink
            .send(Message::Text(msg.to_string().into()))
            .await
            .context("send audio chunk")?;
        Ok(())
    }

    pub async fn end_audio(&mut self) -> Result<()> {
        let msg = serde_json::json!({ "realtimeInput": { "audioStreamEnd": true } });
        self.logger.log_outbound_json("audio_stream_end", &msg);
        self.sink
            .send(Message::Text(msg.to_string().into()))
            .await
            .context("send audio-stream-end")?;
        Ok(())
    }

    pub async fn close(mut self) -> Result<()> {
        self.logger
            .log_lifecycle("client_close", serde_json::json!({}));
        self.sink.close().await?;
        Ok(())
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

            let value: serde_json::Value =
                serde_json::from_str(&text).context("parse server message")?;
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

fn build_setup_message(config: &Config) -> serde_json::Value {
    let mut setup = serde_json::json!({
        "setup": {
            "model": normalize_model_name(&config.model),
            "generationConfig": {
                "responseModalities": ["AUDIO"],
            },
            "contextWindowCompression": {
                "slidingWindow": {},
            },
            "sessionResumption": {},
            "inputAudioTranscription": {},
        }
    });

    if let Some(instruction) = config.system_instruction.as_deref() {
        setup["setup"]["systemInstruction"] = serde_json::json!({
            "parts": [{ "text": instruction }]
        });
    }

    if let Some(handle) = sanitize_session_handle(config.session_handle.as_deref()) {
        setup["setup"]["sessionResumption"]["handle"] = serde_json::Value::String(handle);
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

fn parse_server_message_events(value: &serde_json::Value) -> Result<Vec<TranscriptEvent>> {
    if let Some(error) = value.get("error") {
        let message = error
            .get("message")
            .and_then(|message| message.as_str())
            .unwrap_or_else(|| error.as_str().unwrap_or("unknown Gemini Live error"));
        return Err(anyhow::anyhow!("Gemini Live error: {message}"));
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

        if content.get("turnComplete").and_then(|flag| flag.as_bool()) == Some(true) {
            events.push(TranscriptEvent::TurnComplete);
        }
    }

    Ok(events)
}

fn parse_usage_metadata(value: &serde_json::Value) -> Option<UsageMetadata> {
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

fn read_u32_field(value: &serde_json::Value, field: &str) -> Option<u32> {
    value
        .get(field)?
        .as_u64()
        .map(|value| value.min(u64::from(u32::MAX)) as u32)
}

fn sanitize_session_handle(handle: Option<&str>) -> Option<String> {
    handle.and_then(|handle| {
        let trimmed = handle.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_owned())
        }
    })
}

#[cfg(test)]
mod tests {
    use super::{
        Config, TranscriptEvent, UsageMetadata, build_setup_message, f32_to_i16_le_bytes,
        format_close_frame, normalize_model_name, parse_server_message_events,
        sanitize_session_handle,
    };
    use serde_json::json;
    use tokio_tungstenite::tungstenite::protocol::CloseFrame;
    use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;

    #[test]
    fn converts_f32_audio_to_i16_pcm() {
        let bytes = f32_to_i16_le_bytes(&[-1.0, -0.5, 0.0, 0.5, 1.0]);
        let values = bytes
            .chunks_exact(2)
            .map(|chunk| i16::from_le_bytes([chunk[0], chunk[1]]))
            .collect::<Vec<_>>();

        assert_eq!(values, vec![-32767, -16383, 0, 16383, 32767]);
    }

    #[test]
    fn normalizes_model_name_for_websocket_setup() {
        assert_eq!(
            normalize_model_name("gemini-3.1-flash-live-preview"),
            "models/gemini-3.1-flash-live-preview"
        );
        assert_eq!(
            normalize_model_name("models/gemini-3.1-flash-live-preview"),
            "models/gemini-3.1-flash-live-preview"
        );
    }

    #[test]
    fn formats_close_frames_for_debugging() {
        let close = CloseFrame {
            code: CloseCode::Protocol,
            reason: "bad setup".into(),
        };

        assert_eq!(format_close_frame(Some(&close)), "code 1002: bad setup");
        assert_eq!(
            format_close_frame(None),
            "connection closed without close frame"
        );
    }

    #[test]
    fn build_setup_message_enables_compression_and_resumption() {
        let setup = build_setup_message(&Config {
            api_key: "token".into(),
            model: "gemini-3.1-flash-live-preview".into(),
            sample_rate: 16_000,
            system_instruction: Some("Keep filler words.".into()),
            session_handle: Some("handle-123".into()),
        });

        assert_eq!(
            setup["setup"]["contextWindowCompression"]["slidingWindow"],
            json!({})
        );
        assert_eq!(setup["setup"]["sessionResumption"]["handle"], "handle-123");
        assert_eq!(
            setup["setup"]["systemInstruction"]["parts"][0]["text"],
            "Keep filler words."
        );
    }

    #[test]
    fn parses_usage_metadata_and_turn_completion_from_same_message() {
        let events = parse_server_message_events(&json!({
            "usageMetadata": {
                "promptTokenCount": 10,
                "cachedContentTokenCount": 2,
                "responseTokenCount": 3,
                "toolUsePromptTokenCount": 0,
                "thoughtsTokenCount": 0,
                "totalTokenCount": 13
            },
            "serverContent": {
                "inputTranscription": {
                    "text": "hello world"
                },
                "turnComplete": true
            }
        }))
        .expect("message should parse");

        assert_eq!(events.len(), 3);
        assert!(matches!(
            &events[0],
            TranscriptEvent::UsageMetadata(UsageMetadata {
                total_token_count: 13,
                ..
            })
        ));
        assert!(matches!(
            &events[1],
            TranscriptEvent::InputTranscription(text) if text == "hello world"
        ));
        assert!(matches!(events[2], TranscriptEvent::TurnComplete));
    }

    #[test]
    fn parses_session_resumption_update() {
        let events = parse_server_message_events(&json!({
            "sessionResumptionUpdate": {
                "resumable": true,
                "newHandle": "resume-handle"
            }
        }))
        .expect("message should parse");

        assert_eq!(events.len(), 1);
        assert!(matches!(
            &events[0],
            TranscriptEvent::SessionResumptionUpdate {
                resumable: true,
                new_handle: Some(handle),
            } if handle == "resume-handle"
        ));
    }

    #[test]
    fn parses_usage_metadata_when_optional_counts_are_missing() {
        let events = parse_server_message_events(&json!({
            "usageMetadata": {
                "promptTokenCount": 21,
                "responseTokenCount": 5,
                "totalTokenCount": 26
            }
        }))
        .expect("message should parse");

        assert_eq!(events.len(), 1);
        assert!(matches!(
            &events[0],
            TranscriptEvent::UsageMetadata(UsageMetadata {
                prompt_token_count: 21,
                cached_content_token_count: 0,
                response_token_count: 5,
                thoughts_token_count: 0,
                total_token_count: 26,
                ..
            })
        ));
    }

    #[test]
    fn trims_empty_session_handles() {
        assert_eq!(sanitize_session_handle(Some("  ")), None);
        assert_eq!(
            sanitize_session_handle(Some(" handle-1 ")),
            Some("handle-1".into())
        );
    }
}
