use std::collections::HashMap;
#[cfg(target_os = "macos")]
use std::ffi::c_void;
use std::fs::{self, File};
use std::io::{Seek, SeekFrom, Write};
use std::path::PathBuf;
#[cfg(target_os = "macos")]
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc as std_mpsc};
use std::thread;
use std::time::{Duration, Instant};
#[cfg(target_os = "macos")]
use std::{mem::MaybeUninit, ptr::NonNull};

use anyhow::{Context, Result, bail};
use clap::ValueEnum;
#[cfg(target_os = "macos")]
use coreaudio::audio_unit::StreamFormat as CoreAudioStreamFormat;
#[cfg(target_os = "macos")]
use coreaudio::audio_unit::audio_format::LinearPcmFlags;
#[cfg(target_os = "macos")]
use coreaudio::audio_unit::macos_helpers::{get_default_device_id, get_device_name};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{
    FromSample, Sample, SampleFormat, SizedSample, Stream, StreamConfig, SupportedStreamConfig,
};
#[cfg(target_os = "macos")]
use objc2::{AnyThread, rc::Retained};
#[cfg(target_os = "macos")]
use objc2_core_audio::{
    AudioDeviceCreateIOProcID, AudioDeviceDestroyIOProcID, AudioDeviceID, AudioDeviceIOProcID,
    AudioDeviceStart, AudioDeviceStop, AudioHardwareCreateAggregateDevice,
    AudioHardwareCreateProcessTap, AudioHardwareDestroyAggregateDevice,
    AudioHardwareDestroyProcessTap, AudioObjectGetPropertyData, AudioObjectGetPropertyDataSize,
    AudioObjectID, AudioObjectPropertyAddress, CATapDescription, CATapMuteBehavior,
    kAudioAggregateDeviceIsPrivateKey, kAudioAggregateDeviceNameKey,
    kAudioAggregateDeviceTapAutoStartKey, kAudioAggregateDeviceTapListKey,
    kAudioAggregateDeviceUIDKey, kAudioDevicePropertyStreams, kAudioEndPointDeviceIsPrivateKey,
    kAudioObjectPropertyElementMain, kAudioObjectPropertyScopeGlobal,
    kAudioObjectPropertyScopeInput, kAudioStreamPropertyPhysicalFormat,
    kAudioSubTapDriftCompensationKey, kAudioSubTapUIDKey,
};
#[cfg(target_os = "macos")]
use objc2_core_audio_types::{
    AudioBuffer, AudioBufferList, AudioStreamBasicDescription, AudioTimeStamp,
};
#[cfg(target_os = "macos")]
use objc2_core_foundation::{
    CFArray, CFDictionary, CFMutableDictionary, CFRetained, CFString, kCFAllocatorDefault,
    kCFTypeArrayCallBacks, kCFTypeDictionaryKeyCallBacks, kCFTypeDictionaryValueCallBacks,
};
#[cfg(target_os = "macos")]
use objc2_foundation::{NSArray, NSNumber, NSString, ns_string};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, watch};

use crate::paths::home_debug_dir;
use crate::ui::AppEvent;

const TARGET_SAMPLE_RATE: u32 = 16_000;
const TARGET_BATCH_DURATION_MS: u64 = 40;
const AUDIO_ACTIVITY_PEAK_THRESHOLD: f32 = 0.02;
const AUDIO_ACTIVITY_RMS_THRESHOLD: f32 = 0.005;
#[cfg(target_os = "macos")]
type SampleDecoder = fn(&[u8], bool) -> f32;

#[derive(Debug, Clone)]
pub struct AudioChunk {
    pub samples: Vec<f32>,
    pub started_at: Instant,
    pub duration: Duration,
    pub has_activity: bool,
    pub rms: f32,
}

#[derive(Debug, Clone)]
pub struct DebugCaptureConfig {
    session_id: String,
}

impl DebugCaptureConfig {
    pub fn new(session_id: String) -> Self {
        Self { session_id }
    }

    fn session_id(&self) -> &str {
        &self.session_id
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, ValueEnum, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SourceKind {
    #[value(alias = "mic")]
    Microphone,
    #[value(alias = "system")]
    #[cfg_attr(not(target_os = "macos"), value(hide = true))]
    SystemAudio,
}

impl SourceKind {
    pub const fn title(self) -> &'static str {
        match self {
            Self::Microphone => "microphone",
            Self::SystemAudio => "system audio",
        }
    }

    pub const fn prompt_label(self) -> &'static str {
        match self {
            Self::Microphone => "Microphone",
            Self::SystemAudio => "System audio",
        }
    }

    pub const fn debug_slug(self) -> &'static str {
        match self {
            Self::Microphone => "microphone",
            Self::SystemAudio => "system-audio",
        }
    }
}

#[cfg(target_os = "macos")]
const AVAILABLE_SOURCES: [SourceKind; 2] = [SourceKind::Microphone, SourceKind::SystemAudio];
#[cfg(not(target_os = "macos"))]
const AVAILABLE_SOURCES: [SourceKind; 1] = [SourceKind::Microphone];

pub fn available_sources() -> &'static [SourceKind] {
    &AVAILABLE_SOURCES
}

pub fn ensure_sources_supported(sources: &[SourceKind]) -> Result<()> {
    for &source in sources {
        if !available_sources().contains(&source) {
            bail!("{}", unsupported_source_message(source));
        }
    }

    Ok(())
}

fn unsupported_source_message(source: SourceKind) -> &'static str {
    match source {
        SourceKind::Microphone => "microphone capture is unavailable on this platform",
        SourceKind::SystemAudio => "system audio capture is only supported on macOS",
    }
}

pub fn run_capture(
    sources: Vec<SourceKind>,
    audio_routes: HashMap<SourceKind, mpsc::UnboundedSender<AudioChunk>>,
    ui_tx: std_mpsc::Sender<AppEvent>,
    shutdown_rx: watch::Receiver<bool>,
    debug_capture: Option<DebugCaptureConfig>,
) -> Result<()> {
    if sources.is_empty() {
        return Ok(());
    }
    ensure_sources_supported(&sources)?;

    let mut handles = Vec::new();

    for source in sources {
        let Some(route) = audio_routes.get(&source).cloned() else {
            continue;
        };

        let ui_tx = ui_tx.clone();
        let shutdown_rx = shutdown_rx.clone();
        let debug_capture = debug_capture.clone();

        handles.push(thread::spawn(move || {
            let debug_dump = match debug_capture {
                Some(config) => Some(
                    DebugAudioDump::create(config.session_id(), source, ui_tx.clone())
                        .context("create debug audio dump")?,
                ),
                None => None,
            };
            let result = match source {
                SourceKind::Microphone => {
                    run_microphone_capture(route, ui_tx.clone(), shutdown_rx, debug_dump)
                }
                SourceKind::SystemAudio => {
                    run_system_audio_capture(route, ui_tx.clone(), shutdown_rx, debug_dump)
                }
            };

            if let Err(error) = &result {
                let _ = ui_tx.send(AppEvent::SourceError {
                    source,
                    error: format!("{error:#}"),
                });
            }

            result
        }));
    }

    while !*shutdown_rx.borrow() {
        thread::sleep(Duration::from_millis(100));
    }

    for handle in handles {
        match handle.join() {
            Ok(Ok(())) => {}
            Ok(Err(error)) => tracing::error!(?error, "capture source failed"),
            Err(_) => tracing::error!("capture thread panicked"),
        }
    }

    Ok(())
}
fn run_microphone_capture(
    route: mpsc::UnboundedSender<AudioChunk>,
    ui_tx: std_mpsc::Sender<AppEvent>,
    shutdown_rx: watch::Receiver<bool>,
    debug_dump: Option<Arc<Mutex<DebugAudioDump>>>,
) -> Result<()> {
    let host = cpal::default_host();
    let device = host
        .default_input_device()
        .context("no default microphone input device available")?;
    let config = device
        .default_input_config()
        .context("load default microphone input config")?;

    let _ = ui_tx.send(AppEvent::Notice(format!(
        "microphone device: {} | {} Hz | {} ch | {}",
        device
            .description()
            .map(|description| description.name().to_string())
            .unwrap_or_else(|_| "unknown".into()),
        config.sample_rate(),
        config.channels(),
        config.sample_format()
    )));

    let pipeline = Arc::new(Mutex::new(AudioCapturePipeline::new(
        usize::from(config.channels()),
        config.sample_rate(),
        TARGET_SAMPLE_RATE,
    )));
    let stream = build_cpal_input_stream(
        SourceKind::Microphone,
        &device,
        &config,
        Arc::clone(&pipeline),
        route.clone(),
        ui_tx.clone(),
        debug_dump.clone(),
    )?;
    stream.play().context("start microphone input stream")?;

    let _ = ui_tx.send(AppEvent::DraftStatus {
        source: SourceKind::Microphone,
        status: "capturing".into(),
    });

    while !*shutdown_rx.borrow() {
        thread::sleep(Duration::from_millis(100));
    }

    drop(stream);
    flush_batched_audio(
        SourceKind::Microphone,
        &pipeline,
        &route,
        &ui_tx,
        debug_dump.as_ref(),
    );
    Ok(())
}

fn build_cpal_input_stream(
    source: SourceKind,
    device: &cpal::Device,
    config: &SupportedStreamConfig,
    pipeline: Arc<Mutex<AudioCapturePipeline>>,
    route: mpsc::UnboundedSender<AudioChunk>,
    ui_tx: std_mpsc::Sender<AppEvent>,
    debug_dump: Option<Arc<Mutex<DebugAudioDump>>>,
) -> Result<Stream> {
    let stream_config: StreamConfig = config.clone().into();

    match config.sample_format() {
        SampleFormat::I8 => build_cpal_input_stream_typed::<i8>(
            source,
            device,
            &stream_config,
            Arc::clone(&pipeline),
            route,
            ui_tx,
            debug_dump,
        ),
        SampleFormat::I16 => build_cpal_input_stream_typed::<i16>(
            source,
            device,
            &stream_config,
            Arc::clone(&pipeline),
            route,
            ui_tx,
            debug_dump,
        ),
        SampleFormat::I24 => build_cpal_input_stream_typed::<cpal::I24>(
            source,
            device,
            &stream_config,
            Arc::clone(&pipeline),
            route,
            ui_tx,
            debug_dump,
        ),
        SampleFormat::I32 => build_cpal_input_stream_typed::<i32>(
            source,
            device,
            &stream_config,
            Arc::clone(&pipeline),
            route,
            ui_tx,
            debug_dump,
        ),
        SampleFormat::I64 => build_cpal_input_stream_typed::<i64>(
            source,
            device,
            &stream_config,
            Arc::clone(&pipeline),
            route,
            ui_tx,
            debug_dump,
        ),
        SampleFormat::U8 => build_cpal_input_stream_typed::<u8>(
            source,
            device,
            &stream_config,
            Arc::clone(&pipeline),
            route,
            ui_tx,
            debug_dump,
        ),
        SampleFormat::U16 => build_cpal_input_stream_typed::<u16>(
            source,
            device,
            &stream_config,
            Arc::clone(&pipeline),
            route,
            ui_tx,
            debug_dump,
        ),
        SampleFormat::U24 => build_cpal_input_stream_typed::<cpal::U24>(
            source,
            device,
            &stream_config,
            Arc::clone(&pipeline),
            route,
            ui_tx,
            debug_dump,
        ),
        SampleFormat::U32 => build_cpal_input_stream_typed::<u32>(
            source,
            device,
            &stream_config,
            Arc::clone(&pipeline),
            route,
            ui_tx,
            debug_dump,
        ),
        SampleFormat::U64 => build_cpal_input_stream_typed::<u64>(
            source,
            device,
            &stream_config,
            Arc::clone(&pipeline),
            route,
            ui_tx,
            debug_dump,
        ),
        SampleFormat::F32 => build_cpal_input_stream_typed::<f32>(
            source,
            device,
            &stream_config,
            Arc::clone(&pipeline),
            route,
            ui_tx,
            debug_dump,
        ),
        SampleFormat::F64 => build_cpal_input_stream_typed::<f64>(
            source,
            device,
            &stream_config,
            pipeline,
            route,
            ui_tx,
            debug_dump,
        ),
        unsupported => bail!(
            "unsupported {} sample format: {unsupported}",
            source.title()
        ),
    }
}

fn build_cpal_input_stream_typed<T>(
    source: SourceKind,
    device: &cpal::Device,
    config: &StreamConfig,
    pipeline: Arc<Mutex<AudioCapturePipeline>>,
    route: mpsc::UnboundedSender<AudioChunk>,
    ui_tx: std_mpsc::Sender<AppEvent>,
    debug_dump: Option<Arc<Mutex<DebugAudioDump>>>,
) -> Result<Stream>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let chunk_ui_tx = ui_tx.clone();
    let chunk_debug_dump = debug_dump.clone();
    let stream = device.build_input_stream(
        config,
        {
            let pipeline = Arc::clone(&pipeline);
            move |data: &[T], _info| {
                let Ok(mut pipeline) = pipeline.lock() else {
                    return;
                };
                for chunk in pipeline.push_samples_at::<T>(data, Instant::now()) {
                    dispatch_audio_chunk(
                        source,
                        chunk,
                        &route,
                        &chunk_ui_tx,
                        chunk_debug_dump.as_ref(),
                    );
                }
            }
        },
        move |error| {
            let _ = ui_tx.send(AppEvent::SourceError {
                source,
                error: format!("{} stream error: {error}", source.title()),
            });
        },
        None,
    )?;

    Ok(stream)
}

#[cfg(target_os = "macos")]
fn run_system_audio_capture(
    route: mpsc::UnboundedSender<AudioChunk>,
    ui_tx: std_mpsc::Sender<AppEvent>,
    shutdown_rx: watch::Receiver<bool>,
    debug_dump: Option<Arc<Mutex<DebugAudioDump>>>,
) -> Result<()> {
    let output_device_id = get_default_device_id(false)
        .context("no default output device available for system audio capture")?;
    let output_device_name =
        get_device_name(output_device_id).unwrap_or_else(|_| "unknown".to_string());

    let mut session =
        SystemAudioCaptureSession::new().context("create Core Audio system audio tap")?;
    let stream_format = *session.stream_format();
    let pipeline = Arc::new(Mutex::new(AudioCapturePipeline::new(
        stream_format.channels as usize,
        stream_format.sample_rate.round() as u32,
        TARGET_SAMPLE_RATE,
    )));

    session
        .attach_callback(
            Arc::clone(&pipeline),
            route.clone(),
            ui_tx.clone(),
            debug_dump.clone(),
        )
        .context("attach Core Audio tap callback")?;

    let _ = ui_tx.send(AppEvent::Notice(format!(
        "system output: {output_device_name} | {:.0} Hz | {} ch | {:?}",
        stream_format.sample_rate, stream_format.channels, stream_format.sample_format
    )));

    let _ = ui_tx.send(AppEvent::Notice(
        "system audio capture may ask for System Audio Recording permission on first run".into(),
    ));

    session
        .start()
        .context("start Core Audio system audio capture")?;

    let _ = ui_tx.send(AppEvent::DraftStatus {
        source: SourceKind::SystemAudio,
        status: "capturing".into(),
    });

    while !*shutdown_rx.borrow() {
        thread::sleep(Duration::from_millis(100));
    }

    session
        .stop()
        .context("stop Core Audio system audio capture")?;
    flush_batched_audio(
        SourceKind::SystemAudio,
        &pipeline,
        &route,
        &ui_tx,
        debug_dump.as_ref(),
    );

    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn run_system_audio_capture(
    _route: mpsc::UnboundedSender<AudioChunk>,
    _ui_tx: std_mpsc::Sender<AppEvent>,
    _shutdown_rx: watch::Receiver<bool>,
    _debug_dump: Option<Arc<Mutex<DebugAudioDump>>>,
) -> Result<()> {
    bail!("{}", unsupported_source_message(SourceKind::SystemAudio))
}

#[cfg(target_os = "macos")]
struct SystemAudioCaptureSession {
    aggregate_device_id: AudioDeviceID,
    io_proc_id: Option<AudioDeviceIOProcID>,
    callback_context: Option<NonNull<SystemAudioCallbackContext>>,
    started: bool,
    _resources: SystemAudioTapResources,
    stream_format: CoreAudioStreamFormat,
}

#[cfg(target_os = "macos")]
impl SystemAudioCaptureSession {
    fn new() -> Result<Self> {
        let tap_description = build_tap_description();
        let tap_id = create_process_tap(&tap_description)?;
        let tap_uid = unsafe { tap_description.UUID().UUIDString() };
        let aggregate_device_id = create_aggregate_device(&tap_uid)?;
        let resources = SystemAudioTapResources {
            tap_id: Some(tap_id),
            aggregate_device_id: Some(aggregate_device_id),
        };
        let stream_format = get_aggregate_device_stream_format(aggregate_device_id)
            .context("read Core Audio tap stream format")?;

        Ok(Self {
            aggregate_device_id,
            io_proc_id: None,
            callback_context: None,
            started: false,
            _resources: resources,
            stream_format,
        })
    }

    fn stream_format(&self) -> &CoreAudioStreamFormat {
        &self.stream_format
    }

    fn attach_callback(
        &mut self,
        pipeline: Arc<Mutex<AudioCapturePipeline>>,
        route: mpsc::UnboundedSender<AudioChunk>,
        ui_tx: std_mpsc::Sender<AppEvent>,
        debug_dump: Option<Arc<Mutex<DebugAudioDump>>>,
    ) -> Result<()> {
        let callback_context = Box::new(SystemAudioCallbackContext {
            pipeline,
            route,
            ui_tx,
            debug_dump,
            stream_format: self.stream_format,
            error_sent: AtomicBool::new(false),
        });
        let callback_context = NonNull::from(Box::leak(callback_context));
        let mut io_proc_id = MaybeUninit::<AudioDeviceIOProcID>::uninit();
        check_core_audio_status(
            unsafe {
                AudioDeviceCreateIOProcID(
                    self.aggregate_device_id,
                    Some(system_audio_io_proc),
                    callback_context.as_ptr().cast(),
                    NonNull::new_unchecked(io_proc_id.as_mut_ptr()),
                )
            },
            "register Core Audio system audio callback",
        )?;
        self.io_proc_id = Some(unsafe { io_proc_id.assume_init() });
        self.callback_context = Some(callback_context);
        Ok(())
    }

    fn start(&mut self) -> Result<()> {
        let io_proc_id = self
            .io_proc_id
            .context("system audio callback must be attached before starting capture")?;
        check_core_audio_status(
            unsafe { AudioDeviceStart(self.aggregate_device_id, io_proc_id) },
            "start Core Audio tap aggregate device",
        )?;
        self.started = true;
        Ok(())
    }

    fn stop(&mut self) -> Result<()> {
        if let Some(io_proc_id) = self.io_proc_id {
            check_core_audio_status(
                unsafe { AudioDeviceStop(self.aggregate_device_id, io_proc_id) },
                "stop Core Audio tap aggregate device",
            )?;
            self.started = false;
        }
        Ok(())
    }
}

#[cfg(target_os = "macos")]
struct SystemAudioCallbackContext {
    pipeline: Arc<Mutex<AudioCapturePipeline>>,
    route: mpsc::UnboundedSender<AudioChunk>,
    ui_tx: std_mpsc::Sender<AppEvent>,
    debug_dump: Option<Arc<Mutex<DebugAudioDump>>>,
    stream_format: CoreAudioStreamFormat,
    error_sent: AtomicBool,
}

#[cfg(target_os = "macos")]
unsafe extern "C-unwind" fn system_audio_io_proc(
    _device_id: AudioObjectID,
    _now: NonNull<AudioTimeStamp>,
    input_data: NonNull<AudioBufferList>,
    _input_time: NonNull<AudioTimeStamp>,
    _output_data: NonNull<AudioBufferList>,
    _output_time: NonNull<AudioTimeStamp>,
    client_data: *mut c_void,
) -> i32 {
    let Some(context) = NonNull::new(client_data.cast::<SystemAudioCallbackContext>()) else {
        return 0;
    };
    let context = unsafe { context.as_ref() };

    match decode_core_audio_samples(input_data.as_ptr(), context.stream_format) {
        Ok(samples) if !samples.is_empty() => {
            let Ok(mut pipeline) = context.pipeline.lock() else {
                return 0;
            };
            for chunk in pipeline.push(&samples, Instant::now()) {
                dispatch_audio_chunk(
                    SourceKind::SystemAudio,
                    chunk,
                    &context.route,
                    &context.ui_tx,
                    context.debug_dump.as_ref(),
                );
            }
        }
        Ok(_) => {}
        Err(error) => {
            if !context.error_sent.swap(true, Ordering::Relaxed) {
                let _ = context.ui_tx.send(AppEvent::SourceError {
                    source: SourceKind::SystemAudio,
                    error: format!("system audio decode failed: {error:#}"),
                });
            }
        }
    }

    0
}

#[cfg(target_os = "macos")]
impl Drop for SystemAudioCaptureSession {
    fn drop(&mut self) {
        if self.started {
            let _ = self.stop();
        }
        if let Some(io_proc_id) = self.io_proc_id.take() {
            let _ = unsafe { AudioDeviceDestroyIOProcID(self.aggregate_device_id, io_proc_id) };
        }
        if let Some(callback_context) = self.callback_context.take() {
            let _ = unsafe { Box::from_raw(callback_context.as_ptr()) };
        }
    }
}

#[cfg(target_os = "macos")]
#[derive(Debug, Default)]
struct SystemAudioTapResources {
    tap_id: Option<AudioObjectID>,
    aggregate_device_id: Option<AudioObjectID>,
}

#[cfg(target_os = "macos")]
impl Drop for SystemAudioTapResources {
    fn drop(&mut self) {
        if let Some(aggregate_device_id) = self.aggregate_device_id.take() {
            let _ = unsafe { AudioHardwareDestroyAggregateDevice(aggregate_device_id) };
        }
        if let Some(tap_id) = self.tap_id.take() {
            let _ = unsafe { AudioHardwareDestroyProcessTap(tap_id) };
        }
    }
}

#[cfg(target_os = "macos")]
fn get_aggregate_device_stream_format(device_id: AudioDeviceID) -> Result<CoreAudioStreamFormat> {
    let stream_ids: Vec<AudioObjectID> = read_audio_object_vec(
        device_id,
        AudioObjectPropertyAddress {
            mSelector: kAudioDevicePropertyStreams,
            mScope: kAudioObjectPropertyScopeInput,
            mElement: kAudioObjectPropertyElementMain,
        },
        "read system audio aggregate streams",
    )?;
    let stream_id = *stream_ids
        .first()
        .context("system audio aggregate device exposed no input streams")?;
    let asbd: AudioStreamBasicDescription = read_audio_object(
        stream_id,
        AudioObjectPropertyAddress {
            mSelector: kAudioStreamPropertyPhysicalFormat,
            mScope: kAudioObjectPropertyScopeGlobal,
            mElement: kAudioObjectPropertyElementMain,
        },
        "read system audio aggregate stream format",
    )?;
    CoreAudioStreamFormat::from_asbd(asbd)
        .context("system audio aggregate stream format is unsupported")
}

#[cfg(target_os = "macos")]
fn read_audio_object<T: Copy>(
    object_id: AudioObjectID,
    property: AudioObjectPropertyAddress,
    context: &'static str,
) -> Result<T> {
    let mut value = MaybeUninit::<T>::uninit();
    let mut data_size = std::mem::size_of::<T>() as u32;
    check_core_audio_status(
        unsafe {
            AudioObjectGetPropertyData(
                object_id,
                NonNull::from(&property),
                0,
                std::ptr::null(),
                NonNull::from(&mut data_size),
                NonNull::new_unchecked(value.as_mut_ptr().cast()),
            )
        },
        context,
    )?;
    if data_size as usize != std::mem::size_of::<T>() {
        bail!(
            "{context}: expected {} bytes, received {data_size}",
            std::mem::size_of::<T>()
        );
    }
    Ok(unsafe { value.assume_init() })
}

#[cfg(target_os = "macos")]
fn read_audio_object_vec<T: Copy>(
    object_id: AudioObjectID,
    property: AudioObjectPropertyAddress,
    context: &'static str,
) -> Result<Vec<T>> {
    let mut data_size = 0u32;
    check_core_audio_status(
        unsafe {
            AudioObjectGetPropertyDataSize(
                object_id,
                NonNull::from(&property),
                0,
                std::ptr::null(),
                NonNull::from(&mut data_size),
            )
        },
        context,
    )?;
    if !(data_size as usize).is_multiple_of(std::mem::size_of::<T>()) {
        bail!(
            "{context}: property size {data_size} is not a multiple of {}",
            std::mem::size_of::<T>()
        );
    }
    let count = data_size as usize / std::mem::size_of::<T>();
    let mut values = vec![MaybeUninit::<T>::uninit(); count];
    check_core_audio_status(
        unsafe {
            AudioObjectGetPropertyData(
                object_id,
                NonNull::from(&property),
                0,
                std::ptr::null(),
                NonNull::from(&mut data_size),
                NonNull::new_unchecked(values.as_mut_ptr().cast()),
            )
        },
        context,
    )?;
    Ok(values
        .into_iter()
        .map(|value| unsafe { value.assume_init() })
        .collect())
}

#[cfg(target_os = "macos")]
fn build_tap_description() -> Retained<CATapDescription> {
    let processes = NSArray::<NSNumber>::new();
    let tap_description = unsafe {
        CATapDescription::initMonoGlobalTapButExcludeProcesses(
            CATapDescription::alloc(),
            processes.as_ref(),
        )
    };
    unsafe {
        tap_description.setMuteBehavior(CATapMuteBehavior::Unmuted);
        tap_description.setName(ns_string!("gemini-live-transcribe system audio"));
        tap_description.setPrivate(true);
    }
    tap_description
}

#[cfg(target_os = "macos")]
fn create_process_tap(tap_description: &CATapDescription) -> Result<AudioObjectID> {
    let mut tap_id = MaybeUninit::<AudioObjectID>::uninit();
    check_core_audio_status(
        unsafe { AudioHardwareCreateProcessTap(Some(tap_description), tap_id.as_mut_ptr()) },
        "create system audio process tap",
    )?;
    Ok(unsafe { tap_id.assume_init() })
}

#[cfg(target_os = "macos")]
fn create_aggregate_device(tap_uid: &NSString) -> Result<AudioObjectID> {
    let aggregate_properties = create_audio_aggregate_device_properties(tap_uid);
    let mut aggregate_device_id: AudioObjectID = 0;
    check_core_audio_status(
        unsafe {
            AudioHardwareCreateAggregateDevice(
                aggregate_properties.as_ref(),
                NonNull::from(&mut aggregate_device_id),
            )
        },
        "create private aggregate device for system audio tap",
    )?;
    Ok(aggregate_device_id)
}

#[cfg(target_os = "macos")]
fn create_audio_aggregate_device_properties(tap_uid: &NSString) -> CFRetained<CFDictionary> {
    let tap_dictionary = unsafe {
        let dictionary = CFMutableDictionary::new(
            kCFAllocatorDefault,
            2,
            &kCFTypeDictionaryKeyCallBacks,
            &kCFTypeDictionaryValueCallBacks,
        )
        .expect("Core Foundation tap dictionary allocation should succeed");

        CFMutableDictionary::set_value(
            Some(dictionary.as_ref()),
            &*CFString::from_str(kAudioSubTapUIDKey.to_str().expect("valid sub-tap uid key"))
                as *const _ as *const c_void,
            tap_uid as *const _ as *const c_void,
        );
        CFMutableDictionary::set_value(
            Some(dictionary.as_ref()),
            &*CFString::from_str(
                kAudioSubTapDriftCompensationKey
                    .to_str()
                    .expect("valid drift compensation key"),
            ) as *const _ as *const c_void,
            &*NSNumber::initWithBool(NSNumber::alloc(), true) as *const _ as *const c_void,
        );

        dictionary
    };

    let tap_list = [tap_dictionary];
    let taps = unsafe {
        CFArray::new(
            kCFAllocatorDefault,
            tap_list.as_ptr() as *mut *const c_void,
            tap_list.len() as _,
            &kCFTypeArrayCallBacks,
        )
        .expect("Core Foundation tap array allocation should succeed")
    };

    let aggregate_name = CFString::from_str("gemini-live-transcribe system audio");
    let aggregate_uid = CFString::from_str(&format!(
        "com.jacoblincool.gemini-live-transcribe.system-audio.{}",
        std::process::id()
    ));

    unsafe {
        let dictionary = CFMutableDictionary::new(
            kCFAllocatorDefault,
            5,
            &kCFTypeDictionaryKeyCallBacks,
            &kCFTypeDictionaryValueCallBacks,
        )
        .expect("Core Foundation aggregate device allocation should succeed");

        CFMutableDictionary::set_value(
            Some(dictionary.as_ref()),
            &*CFString::from_str(
                kAudioAggregateDeviceNameKey
                    .to_str()
                    .expect("valid aggregate name key"),
            ) as *const _ as *const c_void,
            &*aggregate_name as *const _ as *const c_void,
        );
        CFMutableDictionary::set_value(
            Some(dictionary.as_ref()),
            &*CFString::from_str(
                kAudioAggregateDeviceUIDKey
                    .to_str()
                    .expect("valid aggregate uid key"),
            ) as *const _ as *const c_void,
            &*aggregate_uid as *const _ as *const c_void,
        );
        CFMutableDictionary::set_value(
            Some(dictionary.as_ref()),
            &*CFString::from_str(
                kAudioAggregateDeviceTapListKey
                    .to_str()
                    .expect("valid aggregate tap list key"),
            ) as *const _ as *const c_void,
            &*taps as *const _ as *const c_void,
        );
        CFMutableDictionary::set_value(
            Some(dictionary.as_ref()),
            &*CFString::from_str(
                kAudioAggregateDeviceTapAutoStartKey
                    .to_str()
                    .expect("valid aggregate tap auto-start key"),
            ) as *const _ as *const c_void,
            &*NSNumber::initWithBool(NSNumber::alloc(), false) as *const _ as *const c_void,
        );
        CFMutableDictionary::set_value(
            Some(dictionary.as_ref()),
            &*CFString::from_str(
                kAudioAggregateDeviceIsPrivateKey
                    .to_str()
                    .expect("valid aggregate private key"),
            ) as *const _ as *const c_void,
            &*NSNumber::initWithBool(NSNumber::alloc(), true) as *const _ as *const c_void,
        );
        CFMutableDictionary::set_value(
            Some(dictionary.as_ref()),
            &*CFString::from_str(
                kAudioEndPointDeviceIsPrivateKey
                    .to_str()
                    .expect("valid endpoint private key"),
            ) as *const _ as *const c_void,
            &*NSNumber::initWithBool(NSNumber::alloc(), true) as *const _ as *const c_void,
        );

        CFRetained::cast_unchecked::<CFDictionary>(dictionary)
    }
}

#[cfg(target_os = "macos")]
fn check_core_audio_status(status: i32, context: &'static str) -> Result<()> {
    coreaudio::Error::from_os_status(status).with_context(|| context.to_string())
}

fn flush_batched_audio<T>(
    source: SourceKind,
    batcher: &Arc<Mutex<T>>,
    route: &mpsc::UnboundedSender<AudioChunk>,
    ui_tx: &std_mpsc::Sender<AppEvent>,
    debug_dump: Option<&Arc<Mutex<DebugAudioDump>>>,
) where
    T: AudioBatching,
{
    let Ok(mut batcher) = batcher.lock() else {
        return;
    };

    if let Some(chunk) = batcher.flush_audio() {
        dispatch_audio_chunk(source, chunk, route, ui_tx, debug_dump);
    }
}

fn dispatch_audio_chunk(
    source: SourceKind,
    chunk: AudioChunk,
    route: &mpsc::UnboundedSender<AudioChunk>,
    ui_tx: &std_mpsc::Sender<AppEvent>,
    debug_dump: Option<&Arc<Mutex<DebugAudioDump>>>,
) {
    if let Some(debug_dump) = debug_dump
        && let Ok(mut debug_dump) = debug_dump.lock()
        && let Err(error) = debug_dump.write_chunk(&chunk)
    {
        let _ = ui_tx.send(AppEvent::Notice(format!(
            "{} debug dump write failed: {error:#}",
            source.title()
        )));
    }
    let _ = ui_tx.send(AppEvent::CaptureLevel {
        source,
        rms: chunk.rms,
    });
    let _ = route.send(chunk);
}

#[derive(Debug)]
struct DebugAudioDump {
    file: File,
    path: PathBuf,
    data_bytes: u32,
}

impl DebugAudioDump {
    fn create(
        session_id: &str,
        source: SourceKind,
        ui_tx: std_mpsc::Sender<AppEvent>,
    ) -> Result<Arc<Mutex<Self>>> {
        let base_dir = home_debug_dir()
            .ok_or_else(|| anyhow::anyhow!("application debug directory is unavailable"))?;
        let session_dir = base_dir.join(session_id);
        fs::create_dir_all(&session_dir)
            .with_context(|| format!("create debug directory {}", session_dir.display()))?;

        let wav_path = session_dir.join(format!("{}.wav", source.debug_slug()));
        let metadata_path = session_dir.join(format!("{}.json", source.debug_slug()));
        let mut file = File::create(&wav_path)
            .with_context(|| format!("create debug wav {}", wav_path.display()))?;
        write_wav_header(&mut file, TARGET_SAMPLE_RATE, 1, 0)
            .with_context(|| format!("initialize debug wav {}", wav_path.display()))?;
        fs::write(
            &metadata_path,
            serde_json::to_vec_pretty(&serde_json::json!({
                "session_id": session_id,
                "source": source.debug_slug(),
                "sample_rate_hz": TARGET_SAMPLE_RATE,
                "channels": 1,
                "sample_format": "pcm_s16le",
                "stage": "post-resample pre-gemini",
                "wav_path": wav_path,
            }))?,
        )
        .with_context(|| format!("write debug metadata {}", metadata_path.display()))?;
        let _ = ui_tx.send(AppEvent::Notice(format!(
            "{} debug audio dump: {}",
            source.title(),
            wav_path.display()
        )));

        Ok(Arc::new(Mutex::new(Self {
            file,
            path: wav_path,
            data_bytes: 0,
        })))
    }

    fn write_chunk(&mut self, chunk: &AudioChunk) -> Result<()> {
        let pcm_bytes = encode_pcm_s16le(&chunk.samples);
        self.file
            .write_all(&pcm_bytes)
            .with_context(|| format!("append debug wav {}", self.path.display()))?;
        self.data_bytes = self
            .data_bytes
            .checked_add(pcm_bytes.len() as u32)
            .context("debug wav exceeded 4 GiB")?;
        Ok(())
    }

    fn finalize(&mut self) -> Result<()> {
        self.file
            .seek(SeekFrom::Start(0))
            .with_context(|| format!("seek debug wav {}", self.path.display()))?;
        write_wav_header(&mut self.file, TARGET_SAMPLE_RATE, 1, self.data_bytes)
            .with_context(|| format!("finalize debug wav {}", self.path.display()))?;
        self.file
            .flush()
            .with_context(|| format!("flush debug wav {}", self.path.display()))?;
        Ok(())
    }
}

impl Drop for DebugAudioDump {
    fn drop(&mut self) {
        let _ = self.finalize();
    }
}

fn encode_pcm_s16le(samples: &[f32]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(samples.len() * 2);
    for &sample in samples {
        let clamped = sample.clamp(-1.0, 1.0);
        let value = (clamped * 32767.0) as i16;
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    bytes
}

fn write_wav_header(
    file: &mut File,
    sample_rate: u32,
    channels: u16,
    data_bytes: u32,
) -> Result<()> {
    let bits_per_sample = 16u16;
    let byte_rate = sample_rate
        .checked_mul(channels as u32)
        .and_then(|value| value.checked_mul((bits_per_sample / 8) as u32))
        .context("compute wav byte rate")?;
    let block_align = channels
        .checked_mul(bits_per_sample / 8)
        .context("compute wav block align")?;
    let riff_size = 36u32
        .checked_add(data_bytes)
        .context("compute wav riff size")?;

    file.write_all(b"RIFF")?;
    file.write_all(&riff_size.to_le_bytes())?;
    file.write_all(b"WAVE")?;
    file.write_all(b"fmt ")?;
    file.write_all(&16u32.to_le_bytes())?;
    file.write_all(&1u16.to_le_bytes())?;
    file.write_all(&channels.to_le_bytes())?;
    file.write_all(&sample_rate.to_le_bytes())?;
    file.write_all(&byte_rate.to_le_bytes())?;
    file.write_all(&block_align.to_le_bytes())?;
    file.write_all(&bits_per_sample.to_le_bytes())?;
    file.write_all(b"data")?;
    file.write_all(&data_bytes.to_le_bytes())?;
    Ok(())
}

#[cfg(target_os = "macos")]
fn decode_core_audio_samples(
    buffers: *mut AudioBufferList,
    format: CoreAudioStreamFormat,
) -> Result<Vec<f32>> {
    if buffers.is_null() {
        return Ok(Vec::new());
    }
    let buffer_count = unsafe { (*buffers).mNumberBuffers as usize };
    if buffer_count == 0 {
        return Ok(Vec::new());
    }

    let channel_count = format.channels as usize;
    let interleaved = !format.flags.contains(LinearPcmFlags::IS_NON_INTERLEAVED);
    let packed = format.flags.contains(LinearPcmFlags::IS_PACKED);
    let big_endian = format.flags.contains(LinearPcmFlags::IS_BIG_ENDIAN);

    let (bytes_per_sample, decode_sample): (usize, SampleDecoder) = match format.sample_format {
        coreaudio::audio_unit::SampleFormat::F32 => (4, decode_f32_sample),
        coreaudio::audio_unit::SampleFormat::I32 => (4, decode_i32_sample),
        coreaudio::audio_unit::SampleFormat::I16 => (2, decode_i16_sample),
        coreaudio::audio_unit::SampleFormat::I8 => (1, decode_i8_sample),
        coreaudio::audio_unit::SampleFormat::I24 if packed => (3, decode_i24_sample),
        coreaudio::audio_unit::SampleFormat::I24 => {
            bail!("unsupported system audio sample layout: unpacked 24-bit PCM")
        }
    };

    if interleaved {
        return unsafe {
            decode_interleaved_core_audio_samples(
                &(*buffers).mBuffers[0],
                channel_count,
                bytes_per_sample,
                big_endian,
                decode_sample,
            )
        };
    }

    unsafe {
        decode_planar_core_audio_samples(
            buffers,
            channel_count,
            bytes_per_sample,
            big_endian,
            decode_sample,
        )
    }
}

#[cfg(target_os = "macos")]
unsafe fn decode_interleaved_core_audio_samples(
    buffer: &AudioBuffer,
    channel_count: usize,
    bytes_per_sample: usize,
    big_endian: bool,
    decode_sample: fn(&[u8], bool) -> f32,
) -> Result<Vec<f32>> {
    if buffer.mData.is_null() || channel_count == 0 {
        return Ok(Vec::new());
    }
    if buffer.mNumberChannels as usize != channel_count {
        bail!(
            "system audio channel mismatch: format expects {channel_count}, buffer reported {}",
            buffer.mNumberChannels
        );
    }

    let data_len = buffer.mDataByteSize as usize;
    let frame_width = channel_count
        .checked_mul(bytes_per_sample)
        .context("system audio frame width overflow")?;
    if frame_width == 0 || !data_len.is_multiple_of(frame_width) {
        bail!("invalid interleaved system audio buffer size: {data_len}");
    }

    let frame_count = data_len / frame_width;
    let data = unsafe { std::slice::from_raw_parts(buffer.mData.cast::<u8>(), data_len) };
    let mut mono = Vec::with_capacity(frame_count);

    for frame in 0..frame_count {
        let mut sum = 0.0f32;
        for channel in 0..channel_count {
            let offset = (frame * channel_count + channel) * bytes_per_sample;
            sum += decode_sample(&data[offset..offset + bytes_per_sample], big_endian);
        }
        mono.push(sum / channel_count as f32);
    }

    Ok(mono)
}

#[cfg(target_os = "macos")]
unsafe fn decode_planar_core_audio_samples(
    buffers: *mut AudioBufferList,
    channel_count: usize,
    bytes_per_sample: usize,
    big_endian: bool,
    decode_sample: fn(&[u8], bool) -> f32,
) -> Result<Vec<f32>> {
    if channel_count == 0 {
        return Ok(Vec::new());
    }

    let audio_buffers = unsafe {
        std::slice::from_raw_parts(
            (*buffers).mBuffers.as_ptr(),
            (*buffers).mNumberBuffers as usize,
        )
    };
    if audio_buffers.len() != channel_count {
        bail!(
            "unexpected planar system audio layout: {} buffers for {channel_count} channels",
            audio_buffers.len()
        );
    }

    let frame_count = audio_buffers
        .first()
        .map(|buffer| buffer.mDataByteSize as usize / bytes_per_sample)
        .unwrap_or(0);
    let mut mono = vec![0.0f32; frame_count];

    for buffer in audio_buffers {
        if buffer.mData.is_null() {
            return Ok(Vec::new());
        }
        if buffer.mNumberChannels != 1 {
            bail!(
                "unexpected planar system audio channel count in buffer: {}",
                buffer.mNumberChannels
            );
        }

        let data_len = buffer.mDataByteSize as usize;
        if data_len != frame_count * bytes_per_sample {
            bail!("mismatched planar system audio buffer size: {data_len}");
        }

        let data = unsafe { std::slice::from_raw_parts(buffer.mData.cast::<u8>(), data_len) };
        for (frame, sample) in mono.iter_mut().enumerate() {
            let offset = frame * bytes_per_sample;
            *sample += decode_sample(&data[offset..offset + bytes_per_sample], big_endian);
        }
    }

    for sample in &mut mono {
        *sample /= channel_count as f32;
    }

    Ok(mono)
}

#[cfg(target_os = "macos")]
fn decode_f32_sample(bytes: &[u8], big_endian: bool) -> f32 {
    let raw = if big_endian {
        f32::from_be_bytes(bytes.try_into().expect("f32 sample width"))
    } else {
        f32::from_le_bytes(bytes.try_into().expect("f32 sample width"))
    };
    raw.clamp(-1.0, 1.0)
}

#[cfg(target_os = "macos")]
fn decode_i32_sample(bytes: &[u8], big_endian: bool) -> f32 {
    let raw = if big_endian {
        i32::from_be_bytes(bytes.try_into().expect("i32 sample width"))
    } else {
        i32::from_le_bytes(bytes.try_into().expect("i32 sample width"))
    };
    (raw as f64 / i32::MAX as f64) as f32
}

#[cfg(target_os = "macos")]
fn decode_i24_sample(bytes: &[u8], big_endian: bool) -> f32 {
    let extended = if big_endian {
        let sign = if bytes[0] & 0x80 != 0 { 0xFF } else { 0x00 };
        [sign, bytes[0], bytes[1], bytes[2]]
    } else {
        let sign = if bytes[2] & 0x80 != 0 { 0xFF } else { 0x00 };
        [bytes[0], bytes[1], bytes[2], sign]
    };
    let raw = if big_endian {
        i32::from_be_bytes(extended)
    } else {
        i32::from_le_bytes(extended)
    };
    (raw as f64 / 8_388_608.0) as f32
}

#[cfg(target_os = "macos")]
fn decode_i16_sample(bytes: &[u8], big_endian: bool) -> f32 {
    let raw = if big_endian {
        i16::from_be_bytes(bytes.try_into().expect("i16 sample width"))
    } else {
        i16::from_le_bytes(bytes.try_into().expect("i16 sample width"))
    };
    f32::from(raw) / 32768.0
}

#[cfg(target_os = "macos")]
fn decode_i8_sample(bytes: &[u8], _big_endian: bool) -> f32 {
    bytes[0] as i8 as f32 / 128.0
}

#[derive(Debug)]
struct AudioCapturePipeline {
    converter: AudioConverter,
    batcher: AudioChunkBatcher,
}

impl AudioCapturePipeline {
    fn new(channels: usize, source_sample_rate: u32, target_sample_rate: u32) -> Self {
        Self {
            converter: AudioConverter::new(channels, source_sample_rate, target_sample_rate),
            batcher: AudioChunkBatcher::new(
                target_sample_rate,
                Duration::from_millis(TARGET_BATCH_DURATION_MS),
            ),
        }
    }

    fn push_samples_at<T>(&mut self, data: &[T], received_at: Instant) -> Vec<AudioChunk>
    where
        T: SizedSample,
        f32: FromSample<T>,
    {
        let converted = self.converter.push_samples::<T>(data);
        self.batcher.push(&converted, received_at)
    }

    #[cfg(target_os = "macos")]
    fn push(&mut self, data: &[f32], received_at: Instant) -> Vec<AudioChunk> {
        let converted = self.converter.push_samples::<f32>(data);
        self.batcher.push(&converted, received_at)
    }
}

trait AudioBatching {
    fn flush_audio(&mut self) -> Option<AudioChunk>;
}

#[derive(Debug)]
struct AudioChunkBatcher {
    sample_rate: u32,
    target_samples: usize,
    buffered: Vec<f32>,
    stream_started_at: Option<Instant>,
    emitted_samples: u64,
}

impl AudioChunkBatcher {
    fn new(sample_rate: u32, duration: Duration) -> Self {
        let target_samples = ((sample_rate as u128 * duration.as_millis()) / 1000).max(1) as usize;
        Self {
            sample_rate: sample_rate.max(1),
            target_samples,
            buffered: Vec::new(),
            stream_started_at: None,
            emitted_samples: 0,
        }
    }

    fn push(&mut self, input: &[f32], received_at: Instant) -> Vec<AudioChunk> {
        if input.is_empty() {
            return Vec::new();
        }

        if self.stream_started_at.is_none() {
            self.stream_started_at = Some(
                received_at
                    .checked_sub(duration_from_samples(input.len() as u64, self.sample_rate))
                    .unwrap_or(received_at),
            );
        }

        self.buffered.extend_from_slice(input);

        let mut chunks = Vec::new();
        while self.buffered.len() >= self.target_samples {
            let remainder = self.buffered.split_off(self.target_samples);
            let samples = std::mem::replace(&mut self.buffered, remainder);
            chunks.push(self.build_chunk(samples));
        }

        chunks
    }

    fn flush_audio(&mut self) -> Option<AudioChunk> {
        if self.buffered.is_empty() {
            return None;
        }

        let samples = std::mem::take(&mut self.buffered);
        Some(self.build_chunk(samples))
    }

    fn build_chunk(&mut self, samples: Vec<f32>) -> AudioChunk {
        let (_, rms) = analyze_audio_levels(&samples);
        let started_at = self
            .stream_started_at
            .and_then(|started_at| {
                started_at.checked_add(duration_from_samples(
                    self.emitted_samples,
                    self.sample_rate,
                ))
            })
            .unwrap_or_else(Instant::now);
        self.emitted_samples = self.emitted_samples.saturating_add(samples.len() as u64);
        let duration = duration_from_samples(samples.len() as u64, self.sample_rate);

        AudioChunk {
            duration,
            has_activity: detect_audio_activity(&samples),
            rms,
            samples,
            started_at,
        }
    }
}

impl AudioBatching for AudioChunkBatcher {
    fn flush_audio(&mut self) -> Option<AudioChunk> {
        AudioChunkBatcher::flush_audio(self)
    }
}

impl AudioBatching for AudioCapturePipeline {
    fn flush_audio(&mut self) -> Option<AudioChunk> {
        self.batcher.flush_audio()
    }
}

#[derive(Debug)]
struct AudioConverter {
    channels: usize,
    passthrough: bool,
    resampler: LinearResampler,
}

impl AudioConverter {
    fn new(channels: usize, source_sample_rate: u32, target_sample_rate: u32) -> Self {
        Self {
            channels: channels.max(1),
            passthrough: source_sample_rate == target_sample_rate,
            resampler: LinearResampler::new(source_sample_rate, target_sample_rate),
        }
    }

    fn push_samples<T>(&mut self, data: &[T]) -> Vec<f32>
    where
        T: SizedSample,
        f32: FromSample<T>,
    {
        if data.is_empty() {
            return Vec::new();
        }

        let mono = data
            .chunks(self.channels)
            .map(|frame| {
                let sum = frame
                    .iter()
                    .copied()
                    .map(f32::from_sample)
                    .fold(0.0f32, |acc, sample| acc + sample);
                sum / frame.len() as f32
            })
            .collect::<Vec<_>>();

        if self.passthrough {
            mono
        } else {
            self.resampler.push(&mono)
        }
    }
}

#[derive(Debug)]
struct LinearResampler {
    step: f64,
    position: f64,
    buffered: Vec<f32>,
}

impl LinearResampler {
    fn new(source_rate: u32, target_rate: u32) -> Self {
        Self {
            step: f64::from(source_rate) / f64::from(target_rate.max(1)),
            position: 0.0,
            buffered: Vec::new(),
        }
    }

    fn push(&mut self, input: &[f32]) -> Vec<f32> {
        if input.is_empty() {
            return Vec::new();
        }

        self.buffered.extend_from_slice(input);

        let mut output = Vec::new();
        while self.position + 1.0 < self.buffered.len() as f64 {
            let index = self.position.floor() as usize;
            let fraction = self.position - index as f64;
            let left = self.buffered[index];
            let right = self.buffered[index + 1];
            let sample = left + (right - left) * fraction as f32;
            output.push(sample.clamp(-1.0, 1.0));
            self.position += self.step;
        }

        let drop_count =
            (self.position.floor() as usize).min(self.buffered.len().saturating_sub(1));
        if drop_count > 0 {
            self.buffered.drain(0..drop_count);
            self.position -= drop_count as f64;
        }

        output
    }
}

fn detect_audio_activity(samples: &[f32]) -> bool {
    let (peak, rms) = analyze_audio_levels(samples);
    peak >= AUDIO_ACTIVITY_PEAK_THRESHOLD || rms >= AUDIO_ACTIVITY_RMS_THRESHOLD
}

fn analyze_audio_levels(samples: &[f32]) -> (f32, f32) {
    if samples.is_empty() {
        return (0.0, 0.0);
    }

    let mut peak = 0.0f32;
    let mut energy_sum = 0.0f32;

    for &sample in samples {
        let magnitude = sample.abs();
        peak = peak.max(magnitude);
        energy_sum += sample * sample;
    }

    let rms = (energy_sum / samples.len() as f32).sqrt();
    (peak, rms)
}

fn duration_from_samples(sample_count: u64, sample_rate: u32) -> Duration {
    if sample_count == 0 {
        return Duration::ZERO;
    }

    let nanos = sample_count.saturating_mul(1_000_000_000) / u64::from(sample_rate.max(1));
    Duration::from_nanos(nanos)
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::{
        AudioChunkBatcher, AudioConverter, LinearResampler, TARGET_BATCH_DURATION_MS,
        TARGET_SAMPLE_RATE,
    };

    fn batch_duration() -> Duration {
        Duration::from_millis(TARGET_BATCH_DURATION_MS)
    }

    fn batch_samples() -> usize {
        (TARGET_SAMPLE_RATE as u64 * TARGET_BATCH_DURATION_MS / 1000) as usize
    }

    #[test]
    fn linear_resampler_downsamples_exact_ratio() {
        let mut resampler = LinearResampler::new(48_000, 16_000);
        let input = (0..48).map(|value| value as f32 / 48.0).collect::<Vec<_>>();
        let output = resampler.push(&input);

        assert_eq!(output.len(), 16);
        assert!((output[0] - 0.0).abs() < 1e-6);
        assert!((output[1] - (3.0 / 48.0)).abs() < 1e-6);
        assert!((output[15] - (45.0 / 48.0)).abs() < 1e-6);
    }

    #[test]
    fn microphone_converter_downmixes_stereo() {
        let mut converter = AudioConverter::new(2, TARGET_SAMPLE_RATE, TARGET_SAMPLE_RATE);
        let output = converter.push_samples(&[0.5f32, -0.5f32, 1.0f32, 0.0f32]);

        assert_eq!(output.len(), 2);
        assert_eq!(output[0], 0.0);
        assert_eq!(output[1], 0.5);
    }

    #[test]
    fn linear_resampler_handles_fractional_ratio_without_overrunning() {
        let mut resampler = LinearResampler::new(44_100, 16_000);

        for _ in 0..8 {
            let input = vec![0.25f32; 512];
            let output = resampler.push(&input);
            assert!(!output.is_empty());
            assert!(resampler.buffered.len() <= 512);
        }
    }

    #[test]
    fn audio_chunk_batcher_groups_small_inputs_into_target_size() {
        let mut batcher = AudioChunkBatcher::new(TARGET_SAMPLE_RATE, batch_duration());
        let base = Instant::now();
        let half_batch = batch_samples() / 2;
        assert!(batcher.push(&vec![0.25; half_batch], base).is_empty());

        let chunks = batcher.push(
            &vec![0.25; half_batch],
            base + Duration::from_millis(TARGET_BATCH_DURATION_MS / 2),
        );
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].samples.len(), batch_samples());
        assert!(chunks[0].has_activity);
    }

    #[test]
    fn audio_chunk_batcher_flushes_remainder() {
        let mut batcher = AudioChunkBatcher::new(TARGET_SAMPLE_RATE, batch_duration());
        assert!(batcher.push(&vec![0.25; 400], Instant::now()).is_empty());

        let tail = batcher.flush_audio().expect("tail should be present");
        assert_eq!(tail.samples.len(), 400);
    }

    #[test]
    fn audio_chunk_batcher_tracks_chunk_start_from_audio_clock() {
        let mut batcher = AudioChunkBatcher::new(TARGET_SAMPLE_RATE, batch_duration());
        let received_at = Instant::now();

        let chunks = batcher.push(&vec![0.25; batch_samples()], received_at);

        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].started_at + batch_duration(), received_at);
    }

    #[test]
    fn silence_chunk_is_marked_inactive() {
        let mut batcher = AudioChunkBatcher::new(TARGET_SAMPLE_RATE, batch_duration());
        let chunks = batcher.push(&vec![0.0; batch_samples()], Instant::now());

        assert_eq!(chunks.len(), 1);
        assert!(!chunks[0].has_activity);
    }
}
