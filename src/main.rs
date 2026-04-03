mod capture;
mod export;
mod paths;
mod session_log;
mod transcriber;
mod transcript;
mod ui;
mod updater;

use std::collections::{HashMap, VecDeque};
use std::fs;
use std::fs::OpenOptions;
use std::io;
use std::io::Write;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::{Arc, Mutex, mpsc as std_mpsc};
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
use crate::export::write_transcript_export;
use crate::paths::{home_config_path, home_debug_dir, home_logs_dir, home_transcripts_dir};
use crate::session_log::SessionLogger;
use crate::transcriber::{
    AudioSender, Config as TranscriberConfig, FunctionCallRequest, FunctionResponse, SessionMode,
    ToolDefinition, TranscriptEvent, TranscriptReceiver, UsageMetadata,
};
use crate::transcript::{
    DisplayAction, FinalizedTurnPayload, TranscriptStore, TranscriptionConfig,
    TranscriptionProfile, unix_timestamp_ms,
};
use crate::ui::{App, AppEvent, UsageSnapshot};

const DEFAULT_MODEL: &str = "gemini-3.1-flash-live-preview";
const DEFAULT_DRAFT_INSTRUCTION: &str = "reply with less than 3 words.";
const DEFAULT_LOG_MAX_FILES: usize = 100;
const TRANSCRIBER_SETUP_TIMEOUT: Duration = Duration::from_secs(15);
const RECONNECT_BACKOFF_BASE: Duration = Duration::from_millis(500);
const RECONNECT_BACKOFF_MAX: Duration = Duration::from_secs(5);
const DRAFT_SHUTDOWN_GRACE: Duration = Duration::from_secs(5);
const FINALIZER_DRAIN_GRACE: Duration = Duration::from_secs(15);
const FINALIZER_RESPONSE_TIMEOUT: Duration = Duration::from_secs(30);
const FINALIZER_RETRY_BACKOFFS: [Duration; 3] = [
    Duration::from_millis(500),
    Duration::from_secs(1),
    Duration::from_secs(2),
];
const FINALIZER_REPLAY_BATCH_SAMPLES: usize = 640;

type SharedTranscriptStore = Arc<Mutex<TranscriptStore>>;
type SharedQueueMetrics = Arc<Mutex<QueueMetrics>>;

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
    draft_instruction: Option<String>,
    #[arg(long)]
    finalizer_instruction: Option<String>,
    #[arg(long)]
    debug: bool,
    #[arg(long = "source", value_enum)]
    sources: Vec<SourceKind>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct HomeConfig {
    api_key: Option<String>,
    model: Option<String>,
    draft_instruction: Option<String>,
    finalizer_instruction: Option<String>,
    sources: Option<Vec<SourceKind>>,
    logs: Option<LogsConfig>,
    transcription: Option<TranscriptionConfig>,
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
    draft_instruction: Option<String>,
    finalizer_instruction: Option<String>,
    transcription_profile: TranscriptionProfile,
    profile_summary: String,
    log_max_files: usize,
    debug: bool,
    sources: Vec<SourceKind>,
}

struct LiveConnection {
    sender: AudioSender,
    receiver: TranscriptReceiver,
}

struct DraftWorkerConfig {
    token: String,
    model: String,
    draft_instruction: Option<String>,
    audio_rx: mpsc::UnboundedReceiver<AudioChunk>,
    pending_turn_tx: mpsc::UnboundedSender<PendingTurn>,
    ui_tx: std_mpsc::Sender<AppEvent>,
    transcript_store: SharedTranscriptStore,
    queue_metrics: SharedQueueMetrics,
    shutdown_rx: watch::Receiver<bool>,
    log_max_files: usize,
}

struct FinalizerWorkerConfig {
    token: String,
    model: String,
    finalizer_instruction: Option<String>,
    transcription_profile: TranscriptionProfile,
    pending_turn_rx: mpsc::UnboundedReceiver<PendingTurn>,
    ui_tx: std_mpsc::Sender<AppEvent>,
    transcript_store: SharedTranscriptStore,
    queue_metrics: SharedQueueMetrics,
    shutdown_rx: watch::Receiver<bool>,
    log_max_files: usize,
}

struct DraftEventContext<'a> {
    source: SourceKind,
    ui_tx: &'a std_mpsc::Sender<AppEvent>,
    logger: &'a SessionLogger,
    transcript_store: &'a SharedTranscriptStore,
    pending_turn_tx: &'a mpsc::UnboundedSender<PendingTurn>,
    queue_metrics: &'a SharedQueueMetrics,
    resume_handle: &'a mut Option<String>,
    current_turn: &'a mut Option<CurrentTurn>,
    last_completed_turn_id: &'a mut Option<String>,
    last_completed_draft_text: &'a mut String,
    next_turn_index: &'a mut u64,
}

struct LiveSessionRequest<'a> {
    source: SourceKind,
    token: &'a str,
    model: &'a str,
    system_instruction: Option<&'a str>,
    resume_handle: Option<&'a str>,
    mode: SessionMode,
}

struct FinalizerReplayContext<'a> {
    source: SourceKind,
    connection: &'a mut LiveConnection,
    logger: &'a SessionLogger,
    ui_tx: &'a std_mpsc::Sender<AppEvent>,
    resume_handle: &'a mut Option<String>,
    shutdown_deadline: Option<tokio::time::Instant>,
}

#[derive(Debug, Clone)]
struct FinalizerToolCall {
    call_id: String,
    payload: FinalizedTurnPayload,
}

#[derive(Debug, Clone)]
struct PendingTurn {
    turn_id: String,
    audio_samples: Vec<f32>,
    audio_duration: Duration,
    attempt_count: usize,
}

#[derive(Debug)]
struct CurrentTurn {
    turn_id: String,
    started_at: Instant,
    draft_text: String,
    audio_samples: Vec<f32>,
    audio_duration: Duration,
}

#[derive(Debug, Default)]
struct QueueMetrics {
    queue_depth: usize,
    pending_audio_secs: f32,
}

#[derive(Debug, Clone, Deserialize)]
struct FinalizeTranscriptArgs {
    text: String,
    output_language: String,
    #[serde(default)]
    detected_languages: Vec<String>,
    is_empty_or_noise: bool,
    display_action: DisplayAction,
}

#[tokio::main]
async fn main() -> Result<()> {
    init_rustls_crypto_provider()?;
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
    let session_id = unix_timestamp_ms().to_string();
    let transcript_store = Arc::new(Mutex::new(TranscriptStore::new(
        session_id.clone(),
        config.model.clone(),
        config.model.clone(),
        config.transcription_profile.clone(),
        config.sources.clone(),
    )));

    let (ui_tx, ui_rx) = std_mpsc::channel();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    let mut worker_handles: Vec<JoinHandle<Result<()>>> = Vec::new();
    let mut audio_routes: HashMap<SourceKind, mpsc::UnboundedSender<AudioChunk>> = HashMap::new();

    for source in &config.sources {
        let (audio_tx, audio_rx) = mpsc::unbounded_channel();
        let (pending_turn_tx, pending_turn_rx) = mpsc::unbounded_channel();
        let queue_metrics = Arc::new(Mutex::new(QueueMetrics::default()));

        audio_routes.insert(*source, audio_tx);

        worker_handles.push(tokio::spawn(run_draft_worker(
            *source,
            DraftWorkerConfig {
                token: config.token.clone(),
                model: config.model.clone(),
                draft_instruction: config.draft_instruction.clone(),
                audio_rx,
                pending_turn_tx,
                ui_tx: ui_tx.clone(),
                transcript_store: Arc::clone(&transcript_store),
                queue_metrics: Arc::clone(&queue_metrics),
                shutdown_rx: shutdown_rx.clone(),
                log_max_files: config.log_max_files,
            },
        )));

        worker_handles.push(tokio::spawn(run_finalizer_worker(
            *source,
            FinalizerWorkerConfig {
                token: config.token.clone(),
                model: config.model.clone(),
                finalizer_instruction: config.finalizer_instruction.clone(),
                transcription_profile: config.transcription_profile.clone(),
                pending_turn_rx,
                ui_tx: ui_tx.clone(),
                transcript_store: Arc::clone(&transcript_store),
                queue_metrics,
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
        let debug_capture = config
            .debug
            .then(|| capture::DebugCaptureConfig::new(session_id.clone()));
        move || capture::run_capture(sources, audio_routes, ui_tx, shutdown_rx, debug_capture)
    }));

    let mut app = App::new(
        config.model.clone(),
        config.profile_summary.clone(),
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

    let export_path = {
        let mut store = transcript_store
            .lock()
            .map_err(|_| anyhow!("transcript store mutex poisoned"))?;
        write_transcript_export(&mut store)?
    };
    tracing::info!(path = %export_path.display(), "wrote transcript export");

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

fn init_rustls_crypto_provider() -> Result<()> {
    static INIT_RESULT: OnceLock<Result<(), String>> = OnceLock::new();

    let init_result = INIT_RESULT.get_or_init(|| {
        if rustls::crypto::CryptoProvider::get_default().is_some() {
            return Ok(());
        }

        rustls::crypto::aws_lc_rs::default_provider()
            .install_default()
            .map_err(|_| "install rustls aws-lc-rs CryptoProvider".to_owned())
    });

    init_result
        .as_ref()
        .map(|_| ())
        .map_err(|message| anyhow!(message.clone()))
}

fn resolve_launch_config(args: RunArgs) -> Result<LaunchConfig> {
    let theme = ColorfulTheme::default();
    let file_config = load_home_config()?;
    let RunArgs {
        token: token_arg,
        model,
        draft_instruction,
        finalizer_instruction,
        debug,
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
    let draft_instruction = sanitize_instruction(draft_instruction)
        .or_else(|| sanitize_instruction(file_config.draft_instruction));
    let finalizer_instruction = sanitize_instruction(finalizer_instruction)
        .or_else(|| sanitize_instruction(file_config.finalizer_instruction));
    let log_max_files = resolve_log_max_files(file_config.logs)?;
    let transcription_profile = TranscriptionProfile::from_config(file_config.transcription)?;

    Ok(LaunchConfig {
        token,
        model,
        draft_instruction,
        finalizer_instruction,
        transcription_profile: transcription_profile.clone(),
        profile_summary: transcription_profile.summary(),
        log_max_files,
        debug,
        sources,
    })
}

async fn run_draft_worker(source: SourceKind, worker: DraftWorkerConfig) -> Result<()> {
    let DraftWorkerConfig {
        token,
        model,
        draft_instruction,
        mut audio_rx,
        pending_turn_tx,
        ui_tx,
        transcript_store,
        queue_metrics,
        mut shutdown_rx,
        log_max_files,
    } = worker;

    let logger = SessionLogger::create(source, "draft", &model, log_max_files)
        .with_context(|| format!("create draft session log for {}", source.title()))?;
    let mut live_connection: Option<LiveConnection> = None;
    let mut resume_handle: Option<String> = None;
    let mut pending_audio = VecDeque::new();
    let mut has_connected_once = false;
    let mut reconnect_attempt = 0u32;
    let system_instruction = compose_draft_system_instruction(draft_instruction.as_deref());
    let mut current_turn: Option<CurrentTurn> = None;
    let mut last_completed_turn_id: Option<String> = None;
    let mut last_completed_draft_text = String::new();
    let mut next_turn_index = 1u64;
    let mut shutdown_requested = false;
    let mut shutdown_deadline = None;
    let mut sent_audio_stream_end = false;

    loop {
        if live_connection.is_none() {
            let resume_handle_snapshot = resume_handle.clone();
            let resume_handle_ref = resume_handle_snapshot.as_deref();
            let status = connection_status_label(has_connected_once, resume_handle_ref);
            let notice = reconnect_notice(source, "draft", has_connected_once, resume_handle_ref);
            let _ = ui_tx.send(AppEvent::DraftStatus {
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

            match connect_live_session(
                LiveSessionRequest {
                    source,
                    token: &token,
                    model: &model,
                    system_instruction: system_instruction.as_deref(),
                    resume_handle: resume_handle_ref,
                    mode: SessionMode::Draft,
                },
                &logger,
                &ui_tx,
                &mut resume_handle,
            )
            .await
            {
                Ok(connection) => {
                    live_connection = Some(connection);
                    has_connected_once = true;
                    reconnect_attempt = 0;
                    let _ = ui_tx.send(AppEvent::DraftStatus {
                        source,
                        status: "listening".into(),
                    });
                    logger.log_lifecycle("state", serde_json::json!({ "status": "listening" }));
                }
                Err(error) if has_connected_once || shutdown_requested => {
                    reconnect_attempt = reconnect_attempt.saturating_add(1);
                    let backoff = reconnect_backoff(reconnect_attempt);
                    logger.log_lifecycle(
                        "reconnect_failed",
                        serde_json::json!({
                            "error": format!("{error:#}"),
                            "reconnect_attempt": reconnect_attempt,
                            "backoff_ms": backoff.as_millis(),
                        }),
                    );
                    if shutdown_requested {
                        break;
                    }
                    let _ = ui_tx.send(AppEvent::Notice(format!(
                        "{} draft reconnect failed: {:#}. retrying in {} ms",
                        source.title(),
                        error,
                        backoff.as_millis()
                    )));
                    tokio::select! {
                        _ = shutdown_rx.changed() => request_shutdown(&mut shutdown_requested, &mut shutdown_deadline, DRAFT_SHUTDOWN_GRACE),
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

        if shutdown_requested {
            if !sent_audio_stream_end {
                if let Some(connection) = live_connection.as_mut() {
                    let _ = flush_pending_audio(&mut connection.sender, &mut pending_audio).await;
                    let _ = connection.sender.end_audio().await;
                }
                sent_audio_stream_end = true;
            }

            if shutdown_deadline.is_some_and(|deadline| tokio::time::Instant::now() >= deadline) {
                break;
            }
        }

        let mut should_reconnect = false;
        let mut reconnect_reason = None;

        {
            let connection = live_connection
                .as_mut()
                .expect("draft connection should exist before streaming");
            let pending_chunk = pending_audio.front().cloned();

            tokio::select! {
                changed = shutdown_rx.changed(), if !shutdown_requested => {
                    let _ = changed;
                    request_shutdown(&mut shutdown_requested, &mut shutdown_deadline, DRAFT_SHUTDOWN_GRACE);
                }
                maybe_chunk = audio_rx.recv(), if !shutdown_requested => {
                    match maybe_chunk {
                        Some(chunk) if !chunk.samples.is_empty() => {
                            if current_turn.is_some() || chunk.has_activity {
                                let turn = ensure_current_turn(
                                    source,
                                    &transcript_store,
                                    &mut current_turn,
                                    &mut next_turn_index,
                                    chunk.started_at,
                                );
                                turn.audio_samples.extend_from_slice(&chunk.samples);
                                turn.audio_duration += chunk.duration;
                            }
                            pending_audio.push_back(chunk);
                        }
                        Some(_) => {}
                        None => {
                            request_shutdown(&mut shutdown_requested, &mut shutdown_deadline, DRAFT_SHUTDOWN_GRACE);
                        }
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
                        pending_audio.pop_front();
                    }
                }
                next_event = connection.receiver.next_event() => {
                    match next_event {
                        Ok(Some(event)) => {
                            match handle_draft_event(
                                event,
                                DraftEventContext {
                                    source,
                                    ui_tx: &ui_tx,
                                    logger: &logger,
                                    transcript_store: &transcript_store,
                                    pending_turn_tx: &pending_turn_tx,
                                    queue_metrics: &queue_metrics,
                                    resume_handle: &mut resume_handle,
                                    current_turn: &mut current_turn,
                                    last_completed_turn_id: &mut last_completed_turn_id,
                                    last_completed_draft_text: &mut last_completed_draft_text,
                                    next_turn_index: &mut next_turn_index,
                                },
                            ) {
                                DraftEventOutcome::Continue => {}
                                DraftEventOutcome::ConnectionClosed(reason) => {
                                    should_reconnect = true;
                                    reconnect_reason = Some(reason);
                                }
                            }
                        }
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
            let _ = ui_tx.send(AppEvent::DraftStatus {
                source,
                status: "reconnecting".into(),
            });
            let _ = ui_tx.send(AppEvent::Notice(format!(
                "{} draft reconnecting: {reason}",
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
            sent_audio_stream_end = false;
        }
    }

    if let Some(turn) = current_turn.take() {
        mark_unfinalized_turn(&transcript_store, source, &turn.turn_id);
    }

    if let Some(connection) = live_connection {
        let _ = connection.sender.close().await;
    }

    let _ = ui_tx.send(AppEvent::DraftStatus {
        source,
        status: "stopped".into(),
    });
    logger.log_lifecycle("state", serde_json::json!({ "status": "stopped" }));
    let _ = logger.flush();

    Ok(())
}

async fn run_finalizer_worker(source: SourceKind, worker: FinalizerWorkerConfig) -> Result<()> {
    let FinalizerWorkerConfig {
        token,
        model,
        finalizer_instruction,
        transcription_profile,
        mut pending_turn_rx,
        ui_tx,
        transcript_store,
        queue_metrics,
        mut shutdown_rx,
        log_max_files,
    } = worker;

    let logger = SessionLogger::create(source, "finalizer", &model, log_max_files)
        .with_context(|| format!("create finalizer session log for {}", source.title()))?;
    let system_instruction =
        compose_finalizer_instruction(&transcription_profile, finalizer_instruction.as_deref());
    let tool = finalize_transcript_tool_definition();
    let mut live_connection: Option<LiveConnection> = None;
    let mut resume_handle: Option<String> = None;
    let mut has_connected_once = false;
    let mut reconnect_attempt = 0u32;
    let mut shutdown_requested = false;
    let mut shutdown_deadline = None;

    loop {
        let next_turn = if shutdown_requested {
            let deadline = match shutdown_deadline {
                Some(deadline) => deadline,
                None => {
                    mark_remaining_unfinalized(&transcript_store, &mut pending_turn_rx, source)?;
                    update_queue_metrics_after_shutdown(source, &queue_metrics, &ui_tx)?;
                    break;
                }
            };
            match timeout_at(deadline, pending_turn_rx.recv()).await {
                Ok(turn) => turn,
                Err(_) => {
                    mark_remaining_unfinalized(&transcript_store, &mut pending_turn_rx, source)?;
                    update_queue_metrics_after_shutdown(source, &queue_metrics, &ui_tx)?;
                    break;
                }
            }
        } else {
            tokio::select! {
                changed = shutdown_rx.changed() => {
                    let _ = changed;
                    request_shutdown(&mut shutdown_requested, &mut shutdown_deadline, FINALIZER_DRAIN_GRACE);
                    continue;
                }
                turn = pending_turn_rx.recv() => turn,
            }
        };

        let Some(mut turn) = next_turn else {
            if shutdown_requested {
                break;
            }
            continue;
        };

        let mut terminal_result = None;
        while terminal_result.is_none() {
            if live_connection.is_none() {
                let resume_handle_snapshot = resume_handle.clone();
                let resume_handle_ref = resume_handle_snapshot.as_deref();
                let status = connection_status_label(has_connected_once, resume_handle_ref);
                let notice =
                    reconnect_notice(source, "finalizer", has_connected_once, resume_handle_ref);
                let _ = ui_tx.send(AppEvent::FinalizerStatus {
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

                match connect_live_session(
                    LiveSessionRequest {
                        source,
                        token: &token,
                        model: &model,
                        system_instruction: Some(&system_instruction),
                        resume_handle: resume_handle_ref,
                        mode: SessionMode::Finalizer { tool: tool.clone() },
                    },
                    &logger,
                    &ui_tx,
                    &mut resume_handle,
                )
                .await
                {
                    Ok(connection) => {
                        live_connection = Some(connection);
                        has_connected_once = true;
                        reconnect_attempt = 0;
                        let _ = ui_tx.send(AppEvent::FinalizerStatus {
                            source,
                            status: "ready".into(),
                        });
                        logger.log_lifecycle("state", serde_json::json!({ "status": "ready" }));
                    }
                    Err(error) => {
                        reconnect_attempt = reconnect_attempt.saturating_add(1);
                        let backoff = reconnect_backoff(reconnect_attempt);
                        logger.log_lifecycle(
                            "reconnect_failed",
                            serde_json::json!({
                                "error": format!("{error:#}"),
                                "reconnect_attempt": reconnect_attempt,
                                "backoff_ms": backoff.as_millis(),
                            }),
                        );
                        if shutdown_requested {
                            terminal_result = Some(FinalizerOutcome::Unfinalized(format!(
                                "finalizer shutdown before reconnect: {error:#}"
                            )));
                            continue;
                        }
                        let _ = ui_tx.send(AppEvent::Notice(format!(
                            "{} finalizer reconnect failed: {:#}. retrying in {} ms",
                            source.title(),
                            error,
                            backoff.as_millis()
                        )));
                        tokio::select! {
                            changed = shutdown_rx.changed(), if !shutdown_requested => { let _ = changed; request_shutdown(&mut shutdown_requested, &mut shutdown_deadline, FINALIZER_DRAIN_GRACE); }
                            _ = tokio::time::sleep(backoff) => {}
                        }
                        continue;
                    }
                }
            }

            let result = replay_turn_with_finalizer(
                &turn,
                FinalizerReplayContext {
                    source,
                    connection: live_connection
                        .as_mut()
                        .expect("finalizer connection should exist"),
                    logger: &logger,
                    ui_tx: &ui_tx,
                    resume_handle: &mut resume_handle,
                    shutdown_deadline,
                },
            )
            .await;

            match result {
                Ok(payload) => {
                    with_transcript_store(&transcript_store, |store| {
                        store.mark_final(
                            source,
                            &turn.turn_id,
                            payload.clone(),
                            unix_timestamp_ms(),
                        );
                    })?;
                    let _ = ui_tx.send(AppEvent::TurnFinalized {
                        source,
                        turn_id: turn.turn_id.clone(),
                        text: payload.text.clone(),
                        display_action: payload.display_action,
                    });
                    terminal_result = Some(FinalizerOutcome::Success);
                }
                Err(FinalizerTurnError::Reconnect(reason)) => {
                    reconnect_attempt = reconnect_attempt.saturating_add(1);
                    let _ = ui_tx.send(AppEvent::FinalizerStatus {
                        source,
                        status: "reconnecting".into(),
                    });
                    let _ = ui_tx.send(AppEvent::Notice(format!(
                        "{} finalizer reconnecting: {reason}",
                        source.title()
                    )));
                    logger.log_lifecycle(
                        "connection_lost",
                        serde_json::json!({
                            "reason": reason,
                            "reconnect_attempt": reconnect_attempt,
                            "resume_handle_present": resume_handle.is_some(),
                            "turn_id": turn.turn_id,
                        }),
                    );
                    live_connection = None;
                    if turn.attempt_count >= FINALIZER_RETRY_BACKOFFS.len() {
                        terminal_result = Some(FinalizerOutcome::Failed(format!(
                            "finalizer reconnect budget exhausted: {reason}"
                        )));
                    } else {
                        let backoff = FINALIZER_RETRY_BACKOFFS[turn.attempt_count];
                        turn.attempt_count += 1;
                        logger.log_lifecycle(
                            "turn_retry",
                            serde_json::json!({
                                "turn_id": turn.turn_id,
                                "attempt_count": turn.attempt_count,
                                "backoff_ms": backoff.as_millis(),
                                "error": reason,
                                "kind": "reconnect",
                            }),
                        );
                        tokio::select! {
                            changed = shutdown_rx.changed(), if !shutdown_requested => { let _ = changed; request_shutdown(&mut shutdown_requested, &mut shutdown_deadline, FINALIZER_DRAIN_GRACE); }
                            _ = tokio::time::sleep(backoff) => {}
                        }
                    }
                }
                Err(FinalizerTurnError::Retryable(error)) => {
                    if turn.attempt_count >= FINALIZER_RETRY_BACKOFFS.len() {
                        terminal_result = Some(FinalizerOutcome::Failed(error));
                    } else {
                        let backoff = FINALIZER_RETRY_BACKOFFS[turn.attempt_count];
                        turn.attempt_count += 1;
                        logger.log_lifecycle(
                            "turn_retry",
                            serde_json::json!({
                                "turn_id": turn.turn_id,
                                "attempt_count": turn.attempt_count,
                                "backoff_ms": backoff.as_millis(),
                                "error": error,
                            }),
                        );
                        tokio::select! {
                            changed = shutdown_rx.changed(), if !shutdown_requested => { let _ = changed; request_shutdown(&mut shutdown_requested, &mut shutdown_deadline, FINALIZER_DRAIN_GRACE); }
                            _ = tokio::time::sleep(backoff) => {}
                        }
                    }
                }
                Err(FinalizerTurnError::Shutdown(reason)) => {
                    terminal_result = Some(FinalizerOutcome::Unfinalized(reason));
                }
                Err(FinalizerTurnError::Permanent(error)) => {
                    terminal_result = Some(FinalizerOutcome::Failed(error));
                }
            }
        }

        match terminal_result.expect("terminal result should be set") {
            FinalizerOutcome::Success => {
                decrement_queue_metrics(
                    source,
                    &queue_metrics,
                    turn.audio_duration_secs(),
                    &ui_tx,
                )?;
            }
            FinalizerOutcome::Failed(error) => {
                with_transcript_store(&transcript_store, |store| {
                    store.mark_failed(source, &turn.turn_id, error.clone());
                })?;
                let _ = ui_tx.send(AppEvent::TurnFailed {
                    source,
                    turn_id: turn.turn_id.clone(),
                    error,
                });
                decrement_queue_metrics(
                    source,
                    &queue_metrics,
                    turn.audio_duration_secs(),
                    &ui_tx,
                )?;
            }
            FinalizerOutcome::Unfinalized(_reason) => {
                with_transcript_store(&transcript_store, |store| {
                    store.mark_unfinalized(source, &turn.turn_id);
                })?;
                decrement_queue_metrics(
                    source,
                    &queue_metrics,
                    turn.audio_duration_secs(),
                    &ui_tx,
                )?;
            }
        }
    }

    let _ = ui_tx.send(AppEvent::FinalizerStatus {
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
    let transcripts_dir =
        home_transcripts_dir().ok_or_else(|| anyhow!("application directories are unavailable"))?;
    let debug_dir =
        home_debug_dir().ok_or_else(|| anyhow!("application directories are unavailable"))?;
    println!("config_path={}", config_path.display());
    println!("logs_dir={}", logs_dir.display());
    println!("transcripts_dir={}", transcripts_dir.display());
    println!("debug_dir={}", debug_dir.display());
    Ok(())
}

fn handle_draft_event(event: TranscriptEvent, ctx: DraftEventContext<'_>) -> DraftEventOutcome {
    match event {
        TranscriptEvent::SetupComplete => DraftEventOutcome::Continue,
        TranscriptEvent::InputTranscription(text) => {
            let text = text.trim().to_owned();
            if text.is_empty() {
                return DraftEventOutcome::Continue;
            }
            let _ = ctx.ui_tx.send(AppEvent::DraftStatus {
                source: ctx.source,
                status: "capturing".into(),
            });

            if let Some(turn) = ctx.current_turn.as_mut()
                && draft_text_matches(&turn.draft_text, &text)
            {
                turn.draft_text = text.clone();
                let _ = with_transcript_store(ctx.transcript_store, |store| {
                    store.update_draft(ctx.source, &turn.turn_id, text.clone());
                });
                let _ = ctx.ui_tx.send(AppEvent::DraftTranscript {
                    source: ctx.source,
                    turn_id: turn.turn_id.clone(),
                    at: turn.started_at,
                    text,
                });
                return DraftEventOutcome::Continue;
            }

            if let Some(turn_id) = ctx.last_completed_turn_id.as_ref()
                && draft_text_matches(ctx.last_completed_draft_text, &text)
            {
                *ctx.last_completed_draft_text = text.clone();
                let _ = with_transcript_store(ctx.transcript_store, |store| {
                    store.update_draft(ctx.source, turn_id, text.clone());
                });
                let _ = ctx.ui_tx.send(AppEvent::DraftTranscript {
                    source: ctx.source,
                    turn_id: turn_id.clone(),
                    at: Instant::now(),
                    text,
                });
                return DraftEventOutcome::Continue;
            }

            let turn = ensure_current_turn(
                ctx.source,
                ctx.transcript_store,
                ctx.current_turn,
                ctx.next_turn_index,
                Instant::now(),
            );
            turn.draft_text = text.clone();
            let _ = with_transcript_store(ctx.transcript_store, |store| {
                store.update_draft(ctx.source, &turn.turn_id, text.clone());
            });
            let _ = ctx.ui_tx.send(AppEvent::DraftTranscript {
                source: ctx.source,
                turn_id: turn.turn_id.clone(),
                at: turn.started_at,
                text,
            });
            DraftEventOutcome::Continue
        }
        TranscriptEvent::GenerationComplete => {
            ctx.logger.log_lifecycle(
                "generation_complete",
                serde_json::json!({
                    "turn_id": ctx.current_turn.as_ref().map(|turn| turn.turn_id.clone()),
                }),
            );
            let _ = ctx.ui_tx.send(AppEvent::DraftStatus {
                source: ctx.source,
                status: "listening".into(),
            });
            DraftEventOutcome::Continue
        }
        TranscriptEvent::TurnComplete => {
            let Some(turn) = ctx.current_turn.take() else {
                return DraftEventOutcome::Continue;
            };

            let _ = ctx.ui_tx.send(AppEvent::DraftStatus {
                source: ctx.source,
                status: "listening".into(),
            });

            *ctx.last_completed_turn_id = Some(turn.turn_id.clone());
            *ctx.last_completed_draft_text = turn.draft_text.clone();

            let _ = with_transcript_store(ctx.transcript_store, |store| {
                store.mark_pending(ctx.source, &turn.turn_id);
            });
            let _ = ctx.ui_tx.send(AppEvent::TurnPending {
                source: ctx.source,
                turn_id: turn.turn_id.clone(),
            });

            if turn.audio_samples.is_empty() {
                let error = "draft turn completed without buffered audio".to_string();
                let _ = with_transcript_store(ctx.transcript_store, |store| {
                    store.mark_failed(ctx.source, &turn.turn_id, error.clone());
                });
                let _ = ctx.ui_tx.send(AppEvent::TurnFailed {
                    source: ctx.source,
                    turn_id: turn.turn_id,
                    error,
                });
                return DraftEventOutcome::Continue;
            }

            let pending_turn = PendingTurn {
                turn_id: turn.turn_id.clone(),
                audio_samples: turn.audio_samples,
                audio_duration: turn.audio_duration,
                attempt_count: 0,
            };

            let _ = increment_queue_metrics(
                ctx.source,
                ctx.queue_metrics,
                pending_turn.audio_duration_secs(),
                ctx.ui_tx,
            );

            if ctx.pending_turn_tx.send(pending_turn).is_err() {
                let error = "finalizer queue is closed".to_string();
                let turn_id = turn.turn_id;
                let _ = with_transcript_store(ctx.transcript_store, |store| {
                    store.mark_failed(ctx.source, &turn_id, error.clone());
                });
                let _ = ctx.ui_tx.send(AppEvent::TurnFailed {
                    source: ctx.source,
                    turn_id,
                    error,
                });
            }
            DraftEventOutcome::Continue
        }
        TranscriptEvent::UsageMetadata(usage) => {
            let _ = ctx.ui_tx.send(AppEvent::Usage {
                source: ctx.source,
                usage: usage_snapshot_from_metadata(&usage),
            });
            DraftEventOutcome::Continue
        }
        TranscriptEvent::SessionResumptionUpdate {
            new_handle,
            resumable,
        } => {
            handle_session_resumption_update(ctx.logger, ctx.resume_handle, new_handle, resumable);
            DraftEventOutcome::Continue
        }
        TranscriptEvent::GoAway { time_left } => {
            let _ = ctx.ui_tx.send(AppEvent::Notice(format!(
                "{} draft connection will renew in {time_left}",
                ctx.source.title()
            )));
            ctx.logger.log_lifecycle(
                "go_away",
                serde_json::json!({
                    "time_left": time_left,
                }),
            );
            DraftEventOutcome::Continue
        }
        TranscriptEvent::ConnectionClosed(reason) => {
            let _ = ctx.ui_tx.send(AppEvent::Notice(format!(
                "{} draft connection closed: {reason}",
                ctx.source.title()
            )));
            DraftEventOutcome::ConnectionClosed(reason)
        }
        TranscriptEvent::ModelText
        | TranscriptEvent::ToolCall(_)
        | TranscriptEvent::ToolCallCancellation(_) => DraftEventOutcome::Continue,
    }
}

async fn connect_live_session(
    request: LiveSessionRequest<'_>,
    logger: &SessionLogger,
    ui_tx: &std_mpsc::Sender<AppEvent>,
    latest_resume_handle: &mut Option<String>,
) -> Result<LiveConnection> {
    let config = TranscriberConfig {
        api_key: request.token.to_owned(),
        model: request.model.to_owned(),
        sample_rate: 16_000,
        system_instruction: request.system_instruction.map(str::to_owned),
        session_handle: request.resume_handle.map(str::to_owned),
        mode: request.mode,
    };

    let (sender, mut receiver) = transcriber::connect(config, logger.clone())
        .await
        .with_context(|| format!("connect transcriber for {}", request.source.title()))?;

    timeout(TRANSCRIBER_SETUP_TIMEOUT, async {
        loop {
            match receiver.next_event().await? {
                Some(TranscriptEvent::SetupComplete) => break Ok::<(), anyhow::Error>(()),
                Some(TranscriptEvent::SessionResumptionUpdate {
                    new_handle,
                    resumable,
                }) => handle_session_resumption_update(
                    logger,
                    latest_resume_handle,
                    new_handle,
                    resumable,
                ),
                Some(TranscriptEvent::GoAway { time_left }) => {
                    let _ = ui_tx.send(AppEvent::Notice(format!(
                        "{} connection will renew in {time_left}",
                        request.source.title()
                    )));
                    logger.log_lifecycle(
                        "go_away",
                        serde_json::json!({
                            "time_left": time_left,
                        }),
                    );
                }
                Some(TranscriptEvent::GenerationComplete)
                | Some(TranscriptEvent::InputTranscription(_))
                | Some(TranscriptEvent::ModelText)
                | Some(TranscriptEvent::TurnComplete)
                | Some(TranscriptEvent::ToolCall(_))
                | Some(TranscriptEvent::ToolCallCancellation(_))
                | Some(TranscriptEvent::UsageMetadata(_)) => {}
                Some(TranscriptEvent::ConnectionClosed(reason)) => {
                    break Err(anyhow!(
                        "Gemini Live connection closed before setup completed: {reason}"
                    ));
                }
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
    stream_label: &str,
    has_connected_once: bool,
    resume_handle: Option<&str>,
) -> Option<String> {
    if !has_connected_once {
        return None;
    }

    Some(match resume_handle {
        Some(_) => format!(
            "{} {} resuming existing Live session",
            source.title(),
            stream_label
        ),
        None => format!(
            "{} {} reconnecting without a resumable handle; session context will restart",
            source.title(),
            stream_label
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

fn load_home_config() -> Result<HomeConfig> {
    let Some(path) = ensure_home_config_exists()? else {
        return Ok(HomeConfig::default());
    };

    let raw = fs::read_to_string(&path)
        .with_context(|| format!("read config file {}", path.display()))?;

    toml::from_str(&raw).with_context(|| format!("parse config file {}", path.display()))
}

fn ensure_home_config_exists() -> Result<Option<PathBuf>> {
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
            "# draft_instruction = \"{draft_instruction}\"\n",
            "# finalizer_instruction = \"prefer speaker labels when obvious.\"\n",
            "# sources = [\"microphone\"]\n",
            "\n",
            "[transcription]\n",
            "primary_language = \"zh-Hant\"\n",
            "allowed_languages = [\"en\"]\n",
            "keep_disfluencies = true\n",
            "collapse_self_corrections = false\n",
            "dedupe_immediate_repetition = false\n",
            "numeral_policy = \"preserve\"\n",
            "\n",
            "[logs]\n",
            "max_files = {max_files}\n"
        ),
        model = DEFAULT_MODEL,
        draft_instruction = DEFAULT_DRAFT_INSTRUCTION,
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

fn compose_draft_system_instruction(custom: Option<&str>) -> Option<String> {
    Some(
        custom
            .map(str::trim)
            .filter(|custom| !custom.is_empty())
            .unwrap_or(DEFAULT_DRAFT_INSTRUCTION)
            .to_owned(),
    )
}

fn compose_finalizer_instruction(
    profile: &TranscriptionProfile,
    custom_instruction: Option<&str>,
) -> String {
    let mut instruction = profile.finalizer_instruction();
    if let Some(custom_instruction) = custom_instruction
        .map(str::trim)
        .filter(|instruction| !instruction.is_empty())
    {
        instruction.push_str("\nAdditional operator instruction:\n");
        instruction.push_str(custom_instruction);
    }
    instruction
}

fn ensure_current_turn<'a>(
    source: SourceKind,
    transcript_store: &SharedTranscriptStore,
    current_turn: &'a mut Option<CurrentTurn>,
    next_turn_index: &mut u64,
    started_at: Instant,
) -> &'a mut CurrentTurn {
    if current_turn.is_none() {
        let turn_index = *next_turn_index;
        *next_turn_index = next_turn_index.saturating_add(1);
        let turn_id = format!(
            "{}-turn-{:04}",
            source.title().replace(' ', "-"),
            turn_index
        );
        let created_at_unix_ms = unix_timestamp_ms();
        let _ = with_transcript_store(transcript_store, |store| {
            store.register_turn(source, &turn_id, turn_index, created_at_unix_ms);
        });
        *current_turn = Some(CurrentTurn {
            turn_id,
            started_at,
            draft_text: String::new(),
            audio_samples: Vec::new(),
            audio_duration: Duration::ZERO,
        });
    }

    current_turn
        .as_mut()
        .expect("current turn should be initialized")
}

fn draft_text_matches(existing: &str, incoming: &str) -> bool {
    existing.is_empty() || incoming.starts_with(existing) || existing.starts_with(incoming)
}

fn with_transcript_store<T>(
    transcript_store: &SharedTranscriptStore,
    f: impl FnOnce(&mut TranscriptStore) -> T,
) -> Result<T> {
    let mut store = transcript_store
        .lock()
        .map_err(|_| anyhow!("transcript store mutex poisoned"))?;
    Ok(f(&mut store))
}

fn mark_unfinalized_turn(
    transcript_store: &SharedTranscriptStore,
    source: SourceKind,
    turn_id: &str,
) {
    let _ = with_transcript_store(transcript_store, |store| {
        store.mark_unfinalized(source, turn_id);
    });
}

fn increment_queue_metrics(
    source: SourceKind,
    queue_metrics: &SharedQueueMetrics,
    pending_audio_secs: f32,
    ui_tx: &std_mpsc::Sender<AppEvent>,
) -> Result<()> {
    let snapshot = {
        let mut metrics = queue_metrics
            .lock()
            .map_err(|_| anyhow!("queue metrics mutex poisoned"))?;
        metrics.queue_depth = metrics.queue_depth.saturating_add(1);
        metrics.pending_audio_secs += pending_audio_secs;
        (metrics.queue_depth, metrics.pending_audio_secs)
    };
    let _ = ui_tx.send(AppEvent::QueueMetrics {
        source,
        queue_depth: snapshot.0,
        pending_audio_secs: snapshot.1,
    });
    Ok(())
}

fn decrement_queue_metrics(
    source: SourceKind,
    queue_metrics: &SharedQueueMetrics,
    pending_audio_secs: f32,
    ui_tx: &std_mpsc::Sender<AppEvent>,
) -> Result<()> {
    let snapshot = {
        let mut metrics = queue_metrics
            .lock()
            .map_err(|_| anyhow!("queue metrics mutex poisoned"))?;
        metrics.queue_depth = metrics.queue_depth.saturating_sub(1);
        metrics.pending_audio_secs = (metrics.pending_audio_secs - pending_audio_secs).max(0.0);
        (metrics.queue_depth, metrics.pending_audio_secs)
    };
    let _ = ui_tx.send(AppEvent::QueueMetrics {
        source,
        queue_depth: snapshot.0,
        pending_audio_secs: snapshot.1,
    });
    Ok(())
}

fn update_queue_metrics_after_shutdown(
    source: SourceKind,
    queue_metrics: &SharedQueueMetrics,
    ui_tx: &std_mpsc::Sender<AppEvent>,
) -> Result<()> {
    let snapshot = {
        let metrics = queue_metrics
            .lock()
            .map_err(|_| anyhow!("queue metrics mutex poisoned"))?;
        (metrics.queue_depth, metrics.pending_audio_secs)
    };
    let _ = ui_tx.send(AppEvent::QueueMetrics {
        source,
        queue_depth: snapshot.0,
        pending_audio_secs: snapshot.1,
    });
    Ok(())
}

fn finalize_transcript_tool_definition() -> ToolDefinition {
    ToolDefinition {
        name: "finalize_transcript".into(),
        description: "Finalize the transcript for the current audio turn.".into(),
        parameters: serde_json::json!({
            "type": "object",
            "required": ["text", "output_language", "detected_languages", "is_empty_or_noise", "display_action"],
            "properties": {
                "text": { "type": "string" },
                "output_language": { "type": "string" },
                "detected_languages": {
                    "type": "array",
                    "items": { "type": "string" }
                },
                "is_empty_or_noise": { "type": "boolean" },
                "display_action": {
                    "type": "string",
                    "enum": ["new-block", "append-previous-block"]
                }
            }
        }),
    }
}

async fn replay_turn_with_finalizer(
    turn: &PendingTurn,
    ctx: FinalizerReplayContext<'_>,
) -> Result<FinalizedTurnPayload, FinalizerTurnError> {
    let preamble = build_finalizer_preamble();
    let _ = ctx.ui_tx.send(AppEvent::FinalizerStatus {
        source: ctx.source,
        status: "finalizing".into(),
    });
    ctx.logger.log_lifecycle(
        "state",
        serde_json::json!({
            "status": "finalizing",
            "turn_id": turn.turn_id,
        }),
    );
    ctx.connection
        .sender
        .send_realtime_text(preamble)
        .await
        .map_err(|error| {
            FinalizerTurnError::Reconnect(format!("send finalizer preamble failed: {error:#}"))
        })?;
    ctx.connection
        .sender
        .send_activity_start()
        .await
        .map_err(|error| {
            FinalizerTurnError::Reconnect(format!("send activityStart failed: {error:#}"))
        })?;

    for chunk in turn.audio_samples.chunks(FINALIZER_REPLAY_BATCH_SAMPLES) {
        ctx.connection
            .sender
            .send_audio(chunk)
            .await
            .map_err(|error| {
                FinalizerTurnError::Reconnect(format!("send finalizer audio failed: {error:#}"))
            })?;
    }

    ctx.connection
        .sender
        .send_activity_end()
        .await
        .map_err(|error| {
            FinalizerTurnError::Reconnect(format!("send activityEnd failed: {error:#}"))
        })?;

    loop {
        let next_event = if let Some(deadline) = ctx.shutdown_deadline {
            match timeout_at(
                deadline.min(tokio::time::Instant::now() + FINALIZER_RESPONSE_TIMEOUT),
                ctx.connection.receiver.next_event(),
            )
            .await
            {
                Ok(result) => result,
                Err(_) => {
                    return Err(FinalizerTurnError::Shutdown(
                        "finalizer drain deadline reached".into(),
                    ));
                }
            }
        } else {
            match timeout(
                FINALIZER_RESPONSE_TIMEOUT,
                ctx.connection.receiver.next_event(),
            )
            .await
            {
                Ok(result) => result,
                Err(_) => {
                    return Err(FinalizerTurnError::Retryable(
                        "timed out waiting for finalizer tool call".into(),
                    ));
                }
            }
        };

        match next_event {
            Ok(Some(TranscriptEvent::ToolCall(function_calls))) => {
                let tool_call = parse_finalize_transcript_payload(function_calls)?;
                let ack = FunctionResponse {
                    id: tool_call.call_id.clone(),
                    name: "finalize_transcript".into(),
                    response: serde_json::json!({
                        "accepted": true,
                        "source": ctx.source.title(),
                        "turn_id": turn.turn_id,
                    }),
                };
                let _ = ctx.connection.sender.send_tool_response(&[ack]).await;
                let _ = ctx.ui_tx.send(AppEvent::FinalizerStatus {
                    source: ctx.source,
                    status: "ready".into(),
                });
                ctx.logger.log_lifecycle(
                    "state",
                    serde_json::json!({
                        "status": "ready",
                        "turn_id": turn.turn_id,
                    }),
                );
                return Ok(tool_call.payload);
            }
            Ok(Some(TranscriptEvent::ToolCallCancellation(ids))) => {
                ctx.logger.log_lifecycle(
                    "tool_call_cancelled",
                    serde_json::json!({
                        "turn_id": turn.turn_id,
                        "ids": ids,
                    }),
                );
            }
            Ok(Some(TranscriptEvent::SessionResumptionUpdate {
                new_handle,
                resumable,
            })) => {
                handle_session_resumption_update(
                    ctx.logger,
                    ctx.resume_handle,
                    new_handle,
                    resumable,
                );
            }
            Ok(Some(TranscriptEvent::GoAway { time_left })) => {
                let _ = ctx.ui_tx.send(AppEvent::Notice(format!(
                    "{} finalizer connection will renew in {time_left}",
                    ctx.source.title()
                )));
                ctx.logger.log_lifecycle(
                    "go_away",
                    serde_json::json!({
                        "time_left": time_left,
                        "turn_id": turn.turn_id,
                    }),
                );
            }
            Ok(Some(TranscriptEvent::GenerationComplete)) => {
                ctx.logger.log_lifecycle(
                    "generation_complete",
                    serde_json::json!({
                        "turn_id": turn.turn_id,
                    }),
                );
                let _ = ctx.ui_tx.send(AppEvent::FinalizerStatus {
                    source: ctx.source,
                    status: "awaiting-tool".into(),
                });
            }
            Ok(Some(TranscriptEvent::ConnectionClosed(reason))) => {
                return Err(FinalizerTurnError::Reconnect(reason));
            }
            Ok(Some(
                TranscriptEvent::SetupComplete
                | TranscriptEvent::InputTranscription(_)
                | TranscriptEvent::ModelText
                | TranscriptEvent::TurnComplete
                | TranscriptEvent::UsageMetadata(_),
            )) => {}
            Ok(None) => {
                return Err(FinalizerTurnError::Reconnect(
                    "connection closed without close frame".into(),
                ));
            }
            Err(error) => {
                return Err(FinalizerTurnError::Reconnect(format!(
                    "receive finalizer event failed: {error:#}"
                )));
            }
        }
    }
}

fn parse_finalize_transcript_payload(
    function_calls: Vec<FunctionCallRequest>,
) -> Result<FinalizerToolCall, FinalizerTurnError> {
    if function_calls.len() != 1 {
        return Err(FinalizerTurnError::Permanent(
            "finalizer must return exactly one tool call".into(),
        ));
    }

    let function_call = &function_calls[0];
    if function_call.name != "finalize_transcript" {
        return Err(FinalizerTurnError::Permanent(format!(
            "unexpected finalizer tool call: {}",
            function_call.name
        )));
    }

    let args: FinalizeTranscriptArgs =
        serde_json::from_value(function_call.args.clone()).map_err(|error| {
            FinalizerTurnError::Permanent(format!("invalid finalizer tool args: {error}"))
        })?;
    let text = args.text.trim().to_owned();
    let output_language = args.output_language.trim().to_owned();

    if text.is_empty() && !args.is_empty_or_noise {
        return Err(FinalizerTurnError::Permanent(
            "finalizer returned empty text without is_empty_or_noise".into(),
        ));
    }

    if output_language.is_empty() {
        return Err(FinalizerTurnError::Permanent(
            "finalizer returned empty output_language".into(),
        ));
    }

    if args.detected_languages.is_empty() && !args.is_empty_or_noise {
        return Err(FinalizerTurnError::Permanent(
            "finalizer returned no detected languages".into(),
        ));
    }

    Ok(FinalizerToolCall {
        call_id: function_call.id.clone(),
        payload: FinalizedTurnPayload {
            text,
            output_language,
            detected_languages: args.detected_languages,
            is_empty_or_noise: args.is_empty_or_noise,
            display_action: args.display_action,
        },
    })
}

fn handle_session_resumption_update(
    logger: &SessionLogger,
    resume_handle: &mut Option<String>,
    new_handle: Option<String>,
    resumable: bool,
) {
    if resumable {
        if let Some(handle) = new_handle {
            *resume_handle = Some(handle);
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
}

fn build_finalizer_preamble() -> &'static str {
    "Finalize only this replayed audio turn."
}

fn mark_remaining_unfinalized(
    transcript_store: &SharedTranscriptStore,
    pending_turn_rx: &mut mpsc::UnboundedReceiver<PendingTurn>,
    source: SourceKind,
) -> Result<()> {
    while let Ok(turn) = pending_turn_rx.try_recv() {
        with_transcript_store(transcript_store, |store| {
            store.mark_unfinalized(source, &turn.turn_id);
        })?;
    }
    Ok(())
}

fn timeout_at<T>(
    deadline: tokio::time::Instant,
    future: impl std::future::Future<Output = T>,
) -> tokio::time::Timeout<impl std::future::Future<Output = T>> {
    tokio::time::timeout_at(deadline, future)
}

fn request_shutdown(
    shutdown_requested: &mut bool,
    shutdown_deadline: &mut Option<tokio::time::Instant>,
    grace: Duration,
) {
    *shutdown_requested = true;
    if shutdown_deadline.is_none() {
        *shutdown_deadline = Some(tokio::time::Instant::now() + grace);
    }
}

enum DraftEventOutcome {
    Continue,
    ConnectionClosed(String),
}

enum FinalizerOutcome {
    Success,
    Failed(String),
    Unfinalized(String),
}

#[derive(Debug)]
enum FinalizerTurnError {
    Reconnect(String),
    Retryable(String),
    Permanent(String),
    Shutdown(String),
}

impl PendingTurn {
    fn audio_duration_secs(&self) -> f32 {
        self.audio_duration.as_secs_f32()
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::{
        DEFAULT_DRAFT_INSTRUCTION, HomeConfig, LogsConfig, build_finalizer_preamble,
        compose_draft_system_instruction, compose_finalizer_instruction, default_home_config_toml,
        ensure_home_config_file, finalize_transcript_tool_definition, init_rustls_crypto_provider,
        normalize_sources_opt, parse_finalize_transcript_payload, resolve_log_max_files,
        sanitize_instruction, sanitize_text,
    };
    use crate::capture::{SourceKind, available_sources, ensure_sources_supported};
    use crate::transcriber::FunctionCallRequest;
    use crate::transcript::{DisplayAction, TranscriptionProfile};
    use serde_json::json;

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
    fn composes_draft_system_instruction_from_default_instruction() {
        let instruction = compose_draft_system_instruction(None)
            .expect("default draft instruction should be present");

        assert_eq!(instruction, DEFAULT_DRAFT_INSTRUCTION);
    }

    #[test]
    fn finalizer_instruction_includes_profile_defaults() {
        let instruction = compose_finalizer_instruction(&TranscriptionProfile::default(), None);

        assert!(instruction.contains("Primary output language: zh-Hant."));
        assert!(instruction.contains("Keep disfluencies: true."));
    }

    #[test]
    fn finalizer_instruction_appends_custom_operator_text() {
        let instruction = compose_finalizer_instruction(
            &TranscriptionProfile::default(),
            Some("prefer speaker labels"),
        );

        assert!(instruction.contains("Additional operator instruction:"));
        assert!(instruction.contains("prefer speaker labels"));
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
    fn parses_home_config_toml() {
        let config: HomeConfig = toml::from_str(
            r#"
api_key = "test-key"
model = "gemini-3.1-flash-live-preview"
draft_instruction = "Keep filler words."
finalizer_instruction = "Prefer speaker labels."
sources = ["microphone", "system-audio"]

[transcription]
primary_language = "zh-Hant"
allowed_languages = ["en"]
keep_disfluencies = true
collapse_self_corrections = false
dedupe_immediate_repetition = false
numeral_policy = "preserve"

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
        assert_eq!(
            config.draft_instruction.as_deref(),
            Some("Keep filler words.")
        );
        assert_eq!(
            config.finalizer_instruction.as_deref(),
            Some("Prefer speaker labels.")
        );
        assert_eq!(
            config.sources,
            Some(vec![SourceKind::Microphone, SourceKind::SystemAudio])
        );
        assert!(config.transcription.is_some());
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
        assert_eq!(config.draft_instruction, None);
        assert_eq!(config.finalizer_instruction, None);
        assert!(config.transcription.is_some());
        assert_eq!(config.logs.and_then(|logs| logs.max_files), Some(100));
    }

    #[test]
    fn ensure_home_config_file_creates_default_config() {
        let dir = create_test_dir("create-default-config");
        let path = dir.join("config.toml");

        ensure_home_config_file(&path).expect("default config should be created");

        let raw = fs::read_to_string(&path).expect("config should be readable");
        assert!(raw.contains("model = \"gemini-3.1-flash-live-preview\""));
        assert!(raw.contains("[transcription]"));
        assert!(raw.contains("primary_language = \"zh-Hant\""));
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

    #[test]
    fn finalize_transcript_tool_uses_expected_schema() {
        let tool = finalize_transcript_tool_definition();

        assert_eq!(tool.name, "finalize_transcript");
        assert_eq!(
            tool.parameters["required"],
            json!([
                "text",
                "output_language",
                "detected_languages",
                "is_empty_or_noise",
                "display_action"
            ])
        );
        assert_eq!(
            tool.parameters["properties"]["output_language"],
            json!({ "type": "string" })
        );
    }

    #[test]
    fn parses_finalize_transcript_tool_payload() {
        let tool_call = parse_finalize_transcript_payload(vec![FunctionCallRequest {
            id: "call-1".into(),
            name: "finalize_transcript".into(),
            args: json!({
                "text": "hello",
                "output_language": "en",
                "detected_languages": ["en"],
                "is_empty_or_noise": false,
                "display_action": "new-block"
            }),
        }])
        .expect("payload should parse");

        assert_eq!(tool_call.call_id, "call-1");
        assert_eq!(tool_call.payload.text, "hello");
        assert_eq!(tool_call.payload.output_language, "en");
        assert_eq!(tool_call.payload.detected_languages, vec!["en".to_string()]);
        assert_eq!(tool_call.payload.display_action, DisplayAction::NewBlock);
    }

    #[test]
    fn accepts_unconfigured_finalizer_output_language_as_hint() {
        let tool_call = parse_finalize_transcript_payload(vec![FunctionCallRequest {
            id: "call-1".into(),
            name: "finalize_transcript".into(),
            args: json!({
                "text": "こんにちは",
                "output_language": "ja",
                "detected_languages": ["ja"],
                "is_empty_or_noise": false,
                "display_action": "new-block"
            }),
        }])
        .expect("output_language should be treated as a hint");

        assert_eq!(tool_call.payload.output_language, "ja");
    }

    #[test]
    fn builds_minimal_finalizer_preamble() {
        let preamble = build_finalizer_preamble();

        assert_eq!(preamble, "Finalize only this replayed audio turn.");
    }

    #[test]
    fn initializes_rustls_crypto_provider_idempotently() {
        init_rustls_crypto_provider().expect("provider should initialize");
        init_rustls_crypto_provider().expect("provider should stay initialized");
        assert!(rustls::crypto::CryptoProvider::get_default().is_some());
    }

    fn create_test_dir(label: &str) -> PathBuf {
        let counter = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("gemini-live-transcribe-{label}-{counter}"));
        fs::create_dir_all(&dir).expect("test directory should be creatable");
        dir
    }

    fn cleanup_test_dir(dir: &Path) {
        if let Err(error) = fs::remove_dir_all(dir) {
            tracing::warn!(?error, path = %dir.display(), "failed to remove test directory");
        }
    }
}
