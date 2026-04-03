mod capture;
mod paths;
mod session_log;
mod transcriber;
mod ui;
mod updater;

use std::collections::HashMap;
use std::collections::VecDeque;
use std::fs;
use std::fs::OpenOptions;
use std::io;
use std::io::Write;
use std::sync::mpsc as std_mpsc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use clap::Parser;
use dialoguer::{MultiSelect, Password, theme::ColorfulTheme};
use serde::Deserialize;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tracing_subscriber::EnvFilter;

use crate::capture::{AudioChunk, SourceKind, available_sources, ensure_sources_supported};
use crate::paths::{home_config_path, home_logs_dir};
use crate::session_log::SessionLogger;
use crate::transcriber::{
    AudioSender, Config as TranscriberConfig, TranscriptEvent, TranscriptReceiver, UsageMetadata,
};
use crate::ui::{App, AppEvent, UsageSnapshot};

const DEFAULT_MODEL: &str = "gemini-3.1-flash-live-preview";
const DEFAULT_SYSTEM_INSTRUCTION: &str = "reply with less than 3 words.";
const DEFAULT_LOG_MAX_FILES: usize = 100;
const TRANSCRIBER_SETUP_TIMEOUT: Duration = Duration::from_secs(15);
const RECONNECT_BACKOFF_BASE: Duration = Duration::from_millis(500);
const RECONNECT_BACKOFF_MAX: Duration = Duration::from_secs(5);
const AUDIO_SEGMENT_SILENCE_RESET: Duration = Duration::from_millis(300);

#[derive(Debug, Parser)]
#[command(
    author,
    version,
    about = "Minimal terminal UI for Gemini Live transcription"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
    #[command(flatten)]
    run: RunArgs,
}

#[derive(Debug, clap::Subcommand)]
enum Command {
    /// Print the resolved config and log paths.
    Paths,
    /// Replace the current executable with the latest GitHub release build.
    Update,
}

#[derive(Debug, clap::Args)]
struct RunArgs {
    #[arg(long, env = "GEMINI_API_KEY")]
    token: Option<String>,
    #[arg(long)]
    model: Option<String>,
    #[arg(long)]
    instruction: Option<String>,
    #[arg(long = "source", value_enum)]
    sources: Vec<SourceKind>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct HomeConfig {
    api_key: Option<String>,
    model: Option<String>,
    instruction: Option<String>,
    sources: Option<Vec<SourceKind>>,
    logs: Option<LogsConfig>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct LogsConfig {
    max_files: Option<usize>,
}

#[derive(Debug, Clone)]
struct LaunchConfig {
    token: String,
    model: String,
    instruction: Option<String>,
    log_max_files: usize,
    preference_summary: String,
    sources: Vec<SourceKind>,
}

struct LiveConnection {
    sender: AudioSender,
    receiver: TranscriptReceiver,
}

struct TranscriberWorkerConfig {
    token: String,
    model: String,
    instruction: Option<String>,
    audio_rx: mpsc::UnboundedReceiver<AudioChunk>,
    ui_tx: std_mpsc::Sender<AppEvent>,
    shutdown_rx: watch::Receiver<bool>,
    log_max_files: usize,
}

struct ConnectLiveSessionContext<'a> {
    source: SourceKind,
    token: &'a str,
    model: &'a str,
    system_instruction: Option<&'a str>,
    resume_handle: Option<&'a str>,
    ui_tx: &'a std_mpsc::Sender<AppEvent>,
    logger: &'a SessionLogger,
    latest_resume_handle: &'a mut Option<String>,
}

#[derive(Default)]
struct TurnTimingTracker {
    latest_segment_start: Option<Instant>,
    audio_active: bool,
    accumulated_silence: Duration,
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();

    let cli = Cli::parse();
    if let Some(command) = cli.command {
        match command {
            Command::Paths => {
                print_paths()?;
                return Ok(());
            }
            Command::Update => {
                updater::run_self_update()?;
                return Ok(());
            }
        }
    }

    let config = resolve_launch_config(cli.run)?;

    let (ui_tx, ui_rx) = std_mpsc::channel();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    let mut worker_handles: Vec<JoinHandle<Result<()>>> = Vec::new();
    let mut audio_routes: HashMap<SourceKind, mpsc::UnboundedSender<AudioChunk>> = HashMap::new();

    for source in &config.sources {
        let (audio_tx, audio_rx) = mpsc::unbounded_channel();
        audio_routes.insert(*source, audio_tx);

        worker_handles.push(tokio::spawn(run_transcriber_worker(
            *source,
            TranscriberWorkerConfig {
                token: config.token.clone(),
                model: config.model.clone(),
                instruction: config.instruction.clone(),
                audio_rx,
                ui_tx: ui_tx.clone(),
                shutdown_rx: shutdown_rx.clone(),
                log_max_files: config.log_max_files,
            },
        )));
    }

    worker_handles.push(tokio::task::spawn_blocking({
        let sources = config.sources.clone();
        let audio_routes = audio_routes.clone();
        let ui_tx = ui_tx.clone();
        let shutdown_rx = shutdown_rx.clone();
        move || capture::run_capture(sources, audio_routes, ui_tx, shutdown_rx)
    }));

    let mut app = App::new(
        config.model.clone(),
        config.preference_summary.clone(),
        config.sources.clone(),
    );
    let ui_result = ui::run(&mut app, ui_rx, ui_tx.clone());

    let _ = shutdown_tx.send(true);

    for handle in worker_handles {
        match handle.await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => tracing::error!(?error, "background task failed"),
            Err(error) => tracing::error!(?error, "background task panicked"),
        }
    }

    ui_result?;
    Ok(())
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("gemini_live_transcribe=info,warn"));

    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(io::stderr)
        .try_init();
}

fn resolve_launch_config(args: RunArgs) -> Result<LaunchConfig> {
    let theme = ColorfulTheme::default();
    let file_config = load_home_config()?;
    let RunArgs {
        token: token_arg,
        model,
        instruction,
        sources: source_args,
    } = args;

    let token = match token_arg.or_else(|| sanitize_text(file_config.api_key.clone())) {
        Some(token) if !token.trim().is_empty() => token.trim().to_owned(),
        _ => {
            let token = Password::with_theme(&theme)
                .with_prompt("Gemini API key")
                .allow_empty_password(false)
                .interact()
                .context("read Gemini API key")?;
            token.trim().to_owned()
        }
    };

    if token.is_empty() {
        bail!("Gemini API key cannot be empty");
    }

    let sources = if !source_args.is_empty() {
        normalize_sources(source_args)
    } else if let Some(config_sources) = normalize_sources_opt(file_config.sources.clone()) {
        config_sources
    } else {
        let sources = available_sources();
        let items = sources
            .iter()
            .map(|source| source.prompt_label())
            .collect::<Vec<_>>();

        let selections = MultiSelect::with_theme(&theme)
            .with_prompt("Choose audio source(s)")
            .items(&items)
            .interact()
            .context("select audio sources")?;

        if selections.is_empty() {
            bail!("at least one audio source must be selected");
        }

        selections.into_iter().map(|index| sources[index]).collect()
    };
    ensure_sources_supported(&sources)?;

    let model = sanitize_text(model)
        .or_else(|| sanitize_text(file_config.model))
        .unwrap_or_else(|| DEFAULT_MODEL.to_owned());
    let instruction =
        sanitize_instruction(instruction).or_else(|| sanitize_instruction(file_config.instruction));
    let log_max_files = resolve_log_max_files(file_config.logs)?;

    Ok(LaunchConfig {
        token,
        model,
        log_max_files,
        preference_summary: preference_summary(instruction.as_deref()),
        instruction,
        sources,
    })
}

async fn run_transcriber_worker(source: SourceKind, worker: TranscriberWorkerConfig) -> Result<()> {
    let TranscriberWorkerConfig {
        token,
        model,
        instruction,
        mut audio_rx,
        ui_tx,
        mut shutdown_rx,
        log_max_files,
    } = worker;

    let logger = SessionLogger::create(source, &model, log_max_files)
        .with_context(|| format!("create session log for {}", source.title()))?;
    let mut live_connection: Option<LiveConnection> = None;
    let mut resume_handle: Option<String> = None;
    let mut pending_audio = VecDeque::new();
    let mut has_connected_once = false;
    let mut reconnect_attempt = 0u32;
    let system_instruction = compose_system_instruction(instruction.as_deref());
    let mut turn_timing = TurnTimingTracker::default();

    loop {
        if live_connection.is_none() {
            let status = connection_status_label(has_connected_once, resume_handle.as_deref());
            let notice = reconnect_notice(source, has_connected_once, resume_handle.as_deref());
            let connect_handle = resume_handle.clone();
            let _ = ui_tx.send(AppEvent::SourceStatus {
                source,
                status: status.into(),
            });
            if let Some(notice) = notice {
                let _ = ui_tx.send(AppEvent::Notice(notice));
            }
            logger.log_lifecycle(
                "state",
                serde_json::json!({
                    "status": status,
                    "reconnect_attempt": reconnect_attempt,
                    "resume_handle_present": resume_handle.is_some(),
                }),
            );

            let connect_context = ConnectLiveSessionContext {
                source,
                token: &token,
                model: &model,
                system_instruction: system_instruction.as_deref(),
                resume_handle: connect_handle.as_deref(),
                ui_tx: &ui_tx,
                logger: &logger,
                latest_resume_handle: &mut resume_handle,
            };

            match connect_live_session(connect_context).await {
                Ok(connection) => {
                    live_connection = Some(connection);
                    has_connected_once = true;
                    reconnect_attempt = 0;
                    let _ = ui_tx.send(AppEvent::SourceStatus {
                        source,
                        status: "listening".into(),
                    });
                    logger.log_lifecycle("state", serde_json::json!({ "status": "listening" }));
                }
                Err(error) if has_connected_once => {
                    reconnect_attempt = reconnect_attempt.saturating_add(1);
                    let backoff = reconnect_backoff(reconnect_attempt);
                    let _ = ui_tx.send(AppEvent::Notice(format!(
                        "{} reconnect failed: {:#}. retrying in {} ms",
                        source.title(),
                        error,
                        backoff.as_millis()
                    )));
                    logger.log_lifecycle(
                        "reconnect_failed",
                        serde_json::json!({
                            "error": format!("{error:#}"),
                            "reconnect_attempt": reconnect_attempt,
                            "backoff_ms": backoff.as_millis(),
                        }),
                    );

                    tokio::select! {
                        _ = shutdown_rx.changed() => break,
                        _ = tokio::time::sleep(backoff) => {}
                    }
                    continue;
                }
                Err(error) => {
                    let message = format!("{error:#}");
                    let _ = ui_tx.send(AppEvent::SourceError {
                        source,
                        error: message.clone(),
                    });
                    logger.log_lifecycle(
                        "fatal_error",
                        serde_json::json!({
                            "error": message,
                        }),
                    );
                    let _ = logger.flush();
                    return Err(error);
                }
            }
        }

        let mut should_reconnect = false;
        let mut reconnect_reason = None;

        {
            let connection = live_connection
                .as_mut()
                .expect("live connection should exist before streaming");
            let pending_chunk = pending_audio.front().cloned();

            tokio::select! {
                _ = shutdown_rx.changed() => {
                    break;
                }
                maybe_chunk = audio_rx.recv() => {
                    match maybe_chunk {
                        Some(chunk) if !chunk.samples.is_empty() => pending_audio.push_back(chunk),
                        Some(_) => {}
                        None => break,
                    }
                }
                send_result = async {
                    let Some(chunk) = pending_chunk.as_ref() else {
                        return Ok(());
                    };
                    send_audio_chunk(&mut connection.sender, chunk).await
                }, if pending_chunk.is_some() => {
                    if let Err(error) = send_result {
                        should_reconnect = true;
                        reconnect_reason = Some(format!("send audio failed: {error:#}"));
                    } else {
                        let chunk = pending_audio
                            .pop_front()
                            .expect("pending audio should exist after successful send");
                        turn_timing.observe_sent_audio(&chunk);
                    }
                }
                next_event = connection.receiver.next_event() => {
                    match next_event {
                        Ok(Some(event)) => match handle_transcript_event(
                            source,
                            event,
                            &ui_tx,
                            &logger,
                            &mut resume_handle,
                            &mut turn_timing,
                        ) {
                            RuntimeEventOutcome::Continue => {}
                            RuntimeEventOutcome::SetupComplete => {}
                            RuntimeEventOutcome::ConnectionClosed(reason) => {
                                should_reconnect = true;
                                reconnect_reason = Some(reason);
                            }
                        },
                        Ok(None) => {
                            should_reconnect = true;
                            reconnect_reason = Some("connection closed without close frame".into());
                        }
                        Err(error) => {
                            should_reconnect = true;
                            reconnect_reason = Some(format!("receive event failed: {error:#}"));
                        }
                    }
                }
            }
        }

        if should_reconnect {
            reconnect_attempt = reconnect_attempt.saturating_add(1);
            let reason = reconnect_reason.unwrap_or_else(|| "connection interrupted".into());
            let _ = ui_tx.send(AppEvent::SourceStatus {
                source,
                status: "reconnecting".into(),
            });
            let _ = ui_tx.send(AppEvent::Notice(format!(
                "{} reconnecting: {reason}",
                source.title()
            )));
            logger.log_lifecycle(
                "connection_lost",
                serde_json::json!({
                    "reason": reason,
                    "reconnect_attempt": reconnect_attempt,
                    "resume_handle_present": resume_handle.is_some(),
                }),
            );
            live_connection = None;
        }
    }

    if let Some(mut connection) = live_connection {
        let _ = flush_pending_audio(&mut connection.sender, &mut pending_audio).await;
        let _ = connection.sender.end_audio().await;
        let _ = connection.sender.close().await;
    }

    let _ = ui_tx.send(AppEvent::SourceStatus {
        source,
        status: "stopped".into(),
    });
    logger.log_lifecycle("state", serde_json::json!({ "status": "stopped" }));
    let _ = logger.flush();

    Ok(())
}

fn print_paths() -> Result<()> {
    let config_path = ensure_home_config_exists()?
        .ok_or_else(|| anyhow!("application directories are unavailable"))?;
    let logs_dir =
        home_logs_dir().ok_or_else(|| anyhow!("application directories are unavailable"))?;
    println!("config_path={}", config_path.display());
    println!("logs_dir={}", logs_dir.display());
    Ok(())
}

fn handle_transcript_event(
    source: SourceKind,
    event: TranscriptEvent,
    ui_tx: &std_mpsc::Sender<AppEvent>,
    logger: &SessionLogger,
    resume_handle: &mut Option<String>,
    turn_timing: &mut TurnTimingTracker,
) -> RuntimeEventOutcome {
    match event {
        TranscriptEvent::SetupComplete => RuntimeEventOutcome::SetupComplete,
        TranscriptEvent::InputTranscription(text) => {
            let _ = ui_tx.send(AppEvent::Transcript {
                source,
                at: turn_timing.transcript_started_at(),
                text,
            });
            RuntimeEventOutcome::Continue
        }
        TranscriptEvent::TurnComplete => {
            turn_timing.complete_turn();
            let _ = ui_tx.send(AppEvent::TurnComplete { source });
            RuntimeEventOutcome::Continue
        }
        TranscriptEvent::UsageMetadata(usage) => {
            let _ = ui_tx.send(AppEvent::Usage {
                source,
                usage: usage_snapshot_from_metadata(&usage),
            });
            RuntimeEventOutcome::Continue
        }
        TranscriptEvent::SessionResumptionUpdate {
            new_handle,
            resumable,
        } => {
            if resumable {
                if let Some(handle) = new_handle {
                    *resume_handle = Some(handle.clone());
                    logger.log_lifecycle(
                        "session_resumption_update",
                        serde_json::json!({
                            "resumable": true,
                            "handle_present": true,
                        }),
                    );
                } else {
                    logger.log_lifecycle(
                        "session_resumption_update",
                        serde_json::json!({
                            "resumable": true,
                            "handle_present": false,
                        }),
                    );
                }
            } else {
                logger.log_lifecycle(
                    "session_resumption_update",
                    serde_json::json!({
                        "resumable": false,
                        "handle_present": false,
                    }),
                );
            }
            RuntimeEventOutcome::Continue
        }
        TranscriptEvent::GoAway { time_left } => {
            let _ = ui_tx.send(AppEvent::Notice(format!(
                "{} connection will renew in {time_left}",
                source.title()
            )));
            logger.log_lifecycle(
                "go_away",
                serde_json::json!({
                    "time_left": time_left,
                }),
            );
            RuntimeEventOutcome::Continue
        }
        TranscriptEvent::ConnectionClosed(reason) => {
            let _ = ui_tx.send(AppEvent::Notice(format!(
                "{} connection closed: {reason}",
                source.title()
            )));
            RuntimeEventOutcome::ConnectionClosed(reason)
        }
        TranscriptEvent::ModelText => RuntimeEventOutcome::Continue,
    }
}

async fn connect_live_session(context: ConnectLiveSessionContext<'_>) -> Result<LiveConnection> {
    let ConnectLiveSessionContext {
        source,
        token,
        model,
        system_instruction,
        resume_handle,
        ui_tx,
        logger,
        latest_resume_handle,
    } = context;

    let config = TranscriberConfig {
        api_key: token.to_owned(),
        model: model.to_owned(),
        sample_rate: 16_000,
        system_instruction: system_instruction.map(str::to_owned),
        session_handle: resume_handle.map(str::to_owned),
    };

    let (sender, mut receiver) = transcriber::connect(config, logger.clone())
        .await
        .with_context(|| format!("connect transcriber for {}", source.title()))?;
    let mut setup_turn_timing = TurnTimingTracker::default();

    timeout(TRANSCRIBER_SETUP_TIMEOUT, async {
        loop {
            match receiver.next_event().await? {
                Some(event) => match handle_transcript_event(
                    source,
                    event,
                    ui_tx,
                    logger,
                    latest_resume_handle,
                    &mut setup_turn_timing,
                ) {
                    RuntimeEventOutcome::SetupComplete => break Ok::<(), anyhow::Error>(()),
                    RuntimeEventOutcome::ConnectionClosed(reason) => {
                        break Err(anyhow!(
                            "Gemini Live connection closed before setup completed: {reason}"
                        ));
                    }
                    RuntimeEventOutcome::Continue => {}
                },
                None => {
                    break Err(anyhow!(
                        "Gemini Live connection closed before setup completed"
                    ));
                }
            }
        }
    })
    .await
    .context("wait for Gemini Live setup")??;

    Ok(LiveConnection { sender, receiver })
}

async fn send_audio_chunk(sender: &mut AudioSender, chunk: &AudioChunk) -> Result<()> {
    sender.send_audio(&chunk.samples).await
}

async fn flush_pending_audio(
    sender: &mut AudioSender,
    pending_audio: &mut VecDeque<AudioChunk>,
) -> Result<()> {
    while let Some(chunk) = pending_audio.front() {
        sender.send_audio(&chunk.samples).await?;
        pending_audio.pop_front();
    }
    Ok(())
}

fn connection_status_label(has_connected_once: bool, resume_handle: Option<&str>) -> &'static str {
    if !has_connected_once {
        "connecting"
    } else if resume_handle.is_some() {
        "resuming"
    } else {
        "reconnecting"
    }
}

fn reconnect_notice(
    source: SourceKind,
    has_connected_once: bool,
    resume_handle: Option<&str>,
) -> Option<String> {
    if !has_connected_once {
        return None;
    }

    Some(match resume_handle {
        Some(_) => format!("{} resuming existing Live session", source.title()),
        None => format!(
            "{} reconnecting without a resumable handle; session context will restart",
            source.title()
        ),
    })
}

fn reconnect_backoff(attempt: u32) -> Duration {
    let multiplier = 2u32.saturating_pow(attempt.saturating_sub(1).min(4));
    let millis = RECONNECT_BACKOFF_BASE
        .as_millis()
        .saturating_mul(u128::from(multiplier))
        .min(RECONNECT_BACKOFF_MAX.as_millis());
    Duration::from_millis(millis as u64)
}

fn usage_snapshot_from_metadata(metadata: &UsageMetadata) -> UsageSnapshot {
    UsageSnapshot {
        prompt_token_count: metadata.prompt_token_count,
        cached_content_token_count: metadata.cached_content_token_count,
        response_token_count: metadata.response_token_count,
        tool_use_prompt_token_count: metadata.tool_use_prompt_token_count,
        thoughts_token_count: metadata.thoughts_token_count,
        total_token_count: metadata.total_token_count,
    }
}

enum RuntimeEventOutcome {
    Continue,
    SetupComplete,
    ConnectionClosed(String),
}

impl TurnTimingTracker {
    fn observe_sent_audio(&mut self, chunk: &AudioChunk) {
        if chunk.has_activity {
            if !self.audio_active {
                self.latest_segment_start = Some(chunk.started_at);
            }
            self.audio_active = true;
            self.accumulated_silence = Duration::ZERO;
            return;
        }

        if self.audio_active {
            self.accumulated_silence = self.accumulated_silence.saturating_add(chunk.duration);
            if self.accumulated_silence >= AUDIO_SEGMENT_SILENCE_RESET {
                self.audio_active = false;
                self.accumulated_silence = Duration::ZERO;
            }
        }
    }

    fn transcript_started_at(&self) -> Instant {
        self.latest_segment_start.unwrap_or_else(Instant::now)
    }

    fn complete_turn(&mut self) {}
}

fn load_home_config() -> Result<HomeConfig> {
    let Some(path) = ensure_home_config_exists()? else {
        return Ok(HomeConfig::default());
    };

    let raw = fs::read_to_string(&path)
        .with_context(|| format!("read config file {}", path.display()))?;

    toml::from_str(&raw).with_context(|| format!("parse config file {}", path.display()))
}

fn ensure_home_config_exists() -> Result<Option<std::path::PathBuf>> {
    let Some(path) = home_config_path() else {
        return Ok(None);
    };

    ensure_home_config_file(&path)?;
    Ok(Some(path))
}

fn ensure_home_config_file(path: &std::path::Path) -> Result<()> {
    if path.exists() {
        return Ok(());
    }

    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("config path has no parent: {}", path.display()))?;
    fs::create_dir_all(parent)
        .with_context(|| format!("create config directory {}", parent.display()))?;

    let mut file = match OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => return Ok(()),
        Err(error) => {
            return Err(error).with_context(|| format!("create config file {}", path.display()));
        }
    };

    file.write_all(default_home_config_toml().as_bytes())
        .with_context(|| format!("write default config file {}", path.display()))?;
    file.flush()
        .with_context(|| format!("flush default config file {}", path.display()))?;
    Ok(())
}

fn default_home_config_toml() -> String {
    format!(
        concat!(
            "# Auto-generated default config for gemini-live-transcribe.\n",
            "# Fill in api_key if you want to avoid the interactive prompt.\n",
            "\n",
            "# api_key = \"YOUR_GEMINI_API_KEY\"\n",
            "model = \"{model}\"\n",
            "# instruction = \"{instruction}\"\n",
            "# sources = [\"microphone\"]\n",
            "\n",
            "[logs]\n",
            "max_files = {max_files}\n"
        ),
        model = DEFAULT_MODEL,
        instruction = DEFAULT_SYSTEM_INSTRUCTION,
        max_files = DEFAULT_LOG_MAX_FILES
    )
}

fn resolve_log_max_files(logs: Option<LogsConfig>) -> Result<usize> {
    let max_files = logs
        .and_then(|logs| logs.max_files)
        .unwrap_or(DEFAULT_LOG_MAX_FILES);
    if max_files == 0 {
        bail!("config logs.max_files must be at least 1");
    }
    Ok(max_files)
}

fn sanitize_text(value: Option<String>) -> Option<String> {
    value.and_then(|value| {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_owned())
        }
    })
}

fn sanitize_instruction(instruction: Option<String>) -> Option<String> {
    sanitize_text(instruction)
}

fn normalize_sources(sources: Vec<SourceKind>) -> Vec<SourceKind> {
    let mut normalized = Vec::with_capacity(sources.len());
    for source in sources {
        if !normalized.contains(&source) {
            normalized.push(source);
        }
    }
    normalized
}

fn normalize_sources_opt(sources: Option<Vec<SourceKind>>) -> Option<Vec<SourceKind>> {
    let sources = sources?;
    let normalized = normalize_sources(sources);
    if normalized.is_empty() {
        None
    } else {
        Some(normalized)
    }
}

fn compose_system_instruction(custom: Option<&str>) -> Option<String> {
    Some(
        custom
            .map(str::trim)
            .filter(|custom| !custom.is_empty())
            .unwrap_or(DEFAULT_SYSTEM_INSTRUCTION)
            .to_owned(),
    )
}

fn preference_summary(custom: Option<&str>) -> String {
    if custom.is_some_and(|custom| !custom.trim().is_empty()) {
        "prompt: custom".to_owned()
    } else {
        "prompt: default".to_owned()
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, Instant};

    use super::{
        DEFAULT_SYSTEM_INSTRUCTION, HomeConfig, LogsConfig, TurnTimingTracker,
        compose_system_instruction, default_home_config_toml, ensure_home_config_file,
        normalize_sources_opt, preference_summary, resolve_log_max_files, sanitize_instruction,
        sanitize_text,
    };
    use crate::capture::{AudioChunk, SourceKind, available_sources, ensure_sources_supported};

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn trims_empty_custom_instruction() {
        assert_eq!(sanitize_instruction(Some("  ".into())), None);
        assert_eq!(
            sanitize_instruction(Some(" keep numerals ".into())),
            Some("keep numerals".into())
        );
    }

    #[test]
    fn composes_system_instruction_from_custom_instruction() {
        let instruction = compose_system_instruction(Some("Keep speaker hesitations."))
            .expect("instruction should be present");

        assert_eq!(instruction, "Keep speaker hesitations.");
    }

    #[test]
    fn composes_system_instruction_from_default_instruction() {
        let instruction =
            compose_system_instruction(None).expect("default instruction should be present");

        assert_eq!(instruction, DEFAULT_SYSTEM_INSTRUCTION);
    }

    #[test]
    fn summarizes_preferences_for_status_bar() {
        assert_eq!(preference_summary(None), "prompt: default");
        assert_eq!(
            preference_summary(Some("Use short sentences")),
            "prompt: custom"
        );
    }

    #[test]
    fn trims_empty_plain_text_fields() {
        assert_eq!(sanitize_text(Some("   ".into())), None);
        assert_eq!(sanitize_text(Some(" model ".into())), Some("model".into()));
    }

    #[test]
    fn normalizes_sources_from_config() {
        assert_eq!(
            normalize_sources_opt(Some(vec![
                SourceKind::Microphone,
                SourceKind::Microphone,
                SourceKind::SystemAudio,
            ])),
            Some(vec![SourceKind::Microphone, SourceKind::SystemAudio])
        );
        assert_eq!(normalize_sources_opt(Some(vec![])), None);
    }

    #[test]
    fn rejects_sources_that_are_not_supported_on_this_platform() {
        let supported = available_sources().to_vec();
        ensure_sources_supported(&supported).expect("supported sources should validate");

        #[cfg(not(target_os = "macos"))]
        {
            let error = ensure_sources_supported(&[SourceKind::SystemAudio])
                .expect_err("unsupported source should be rejected");
            assert!(error.to_string().contains("only supported on macOS"));
        }
    }

    #[test]
    fn turn_timing_uses_first_active_audio_chunk_as_segment_start() {
        let mut tracker = TurnTimingTracker::default();
        let started_at = Instant::now();

        tracker.observe_sent_audio(&AudioChunk {
            samples: vec![0.1; 1600],
            started_at,
            duration: Duration::from_millis(100),
            has_activity: true,
        });

        assert_eq!(tracker.transcript_started_at(), started_at);
        assert_eq!(tracker.transcript_started_at(), started_at);
    }

    #[test]
    fn turn_timing_advances_after_silence_and_new_activity() {
        let mut tracker = TurnTimingTracker::default();
        let first = Instant::now();
        let second = first + Duration::from_secs(2);

        tracker.observe_sent_audio(&AudioChunk {
            samples: vec![0.1; 1600],
            started_at: first,
            duration: Duration::from_millis(100),
            has_activity: true,
        });
        assert_eq!(tracker.transcript_started_at(), first);

        tracker.observe_sent_audio(&AudioChunk {
            samples: vec![0.0; 1600],
            started_at: first + Duration::from_millis(100),
            duration: Duration::from_millis(100),
            has_activity: false,
        });
        tracker.observe_sent_audio(&AudioChunk {
            samples: vec![0.0; 1600],
            started_at: first + Duration::from_millis(200),
            duration: Duration::from_millis(100),
            has_activity: false,
        });
        tracker.observe_sent_audio(&AudioChunk {
            samples: vec![0.0; 1600],
            started_at: first + Duration::from_millis(300),
            duration: Duration::from_millis(100),
            has_activity: false,
        });

        tracker.observe_sent_audio(&AudioChunk {
            samples: vec![0.1; 1600],
            started_at: second,
            duration: Duration::from_millis(100),
            has_activity: true,
        });

        assert_eq!(tracker.transcript_started_at(), second);
    }

    #[test]
    fn parses_home_config_toml() {
        let config: HomeConfig = toml::from_str(
            r#"
api_key = "test-key"
model = "gemini-3.1-flash-live-preview"
instruction = "Keep filler words."
sources = ["microphone", "system-audio"]

[logs]
max_files = 100
"#,
        )
        .expect("config should parse");

        assert_eq!(config.api_key.as_deref(), Some("test-key"));
        assert_eq!(
            config.model.as_deref(),
            Some("gemini-3.1-flash-live-preview")
        );
        assert_eq!(config.instruction.as_deref(), Some("Keep filler words."));
        assert_eq!(
            config.sources,
            Some(vec![SourceKind::Microphone, SourceKind::SystemAudio])
        );
        assert_eq!(config.logs.and_then(|logs| logs.max_files), Some(100));
    }

    #[test]
    fn defaults_log_retention_when_logs_config_is_absent() {
        assert_eq!(
            resolve_log_max_files(None).expect("default should resolve"),
            100
        );
        assert_eq!(
            resolve_log_max_files(Some(LogsConfig::default())).expect("default should resolve"),
            100
        );
    }

    #[test]
    fn rejects_zero_log_retention() {
        let error = resolve_log_max_files(Some(LogsConfig { max_files: Some(0) }))
            .expect_err("zero should be rejected");
        assert!(error.to_string().contains("logs.max_files"));
    }

    #[test]
    fn generated_default_config_is_parseable() {
        let config: HomeConfig =
            toml::from_str(&default_home_config_toml()).expect("default config should parse");

        assert_eq!(
            config.model.as_deref(),
            Some("gemini-3.1-flash-live-preview")
        );
        assert_eq!(config.instruction, None);
        assert_eq!(config.logs.and_then(|logs| logs.max_files), Some(100));
    }

    #[test]
    fn ensure_home_config_file_creates_default_config() {
        let dir = create_test_dir("create-default-config");
        let path = dir.join("config.toml");

        ensure_home_config_file(&path).expect("default config should be created");

        let raw = fs::read_to_string(&path).expect("config should be readable");
        assert!(raw.contains("model = \"gemini-3.1-flash-live-preview\""));
        assert!(raw.contains("# instruction = \"reply with less than 3 words.\""));
        assert!(raw.contains("[logs]"));
        assert!(raw.contains("max_files = 100"));

        cleanup_test_dir(&dir);
    }

    #[test]
    fn ensure_home_config_file_does_not_overwrite_existing_config() {
        let dir = create_test_dir("keep-existing-config");
        let path = dir.join("config.toml");
        fs::write(&path, "model = \"custom-model\"\n").expect("seed config should be written");

        ensure_home_config_file(&path).expect("existing config should be preserved");

        assert_eq!(
            fs::read_to_string(&path).expect("config should be readable"),
            "model = \"custom-model\"\n"
        );

        cleanup_test_dir(&dir);
    }

    fn create_test_dir(name: &str) -> PathBuf {
        let counter = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "gemini-live-transcribe-main-{name}-{}-{counter}",
            std::process::id()
        ));
        if dir.exists() {
            fs::remove_dir_all(&dir).expect("existing temp dir should be removable");
        }
        fs::create_dir_all(&dir).expect("temp dir should be created");
        dir
    }

    fn cleanup_test_dir(dir: &Path) {
        let _ = fs::remove_dir_all(dir);
    }
}
