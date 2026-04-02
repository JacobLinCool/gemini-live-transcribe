use std::collections::HashMap;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
    mpsc as std_mpsc,
};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use clap::ValueEnum;
#[cfg(target_os = "macos")]
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
#[cfg(target_os = "macos")]
use cpal::{
    FromSample, Sample, SampleFormat, SizedSample, Stream, StreamConfig, SupportedStreamConfig,
};
#[cfg(target_os = "macos")]
use screencapturekit::cm::CMSampleBuffer;
#[cfg(target_os = "macos")]
use screencapturekit::prelude::{
    SCContentFilter, SCShareableContent, SCStream, SCStreamConfiguration, SCStreamOutputType,
};
#[cfg(target_os = "macos")]
use screencapturekit::stream::configuration::audio::{AudioChannelCount, AudioSampleRate};
#[cfg(target_os = "macos")]
use screencapturekit::stream::delegate_trait::StreamCallbacks;
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, watch};

use crate::ui::AppEvent;

const TARGET_SAMPLE_RATE: u32 = 16_000;
const TARGET_BATCH_DURATION_MS: u64 = 100;
const AUDIO_ACTIVITY_PEAK_THRESHOLD: f32 = 0.02;
const AUDIO_ACTIVITY_RMS_THRESHOLD: f32 = 0.005;

#[derive(Debug, Clone)]
pub struct AudioChunk {
    pub samples: Vec<f32>,
    pub started_at: Instant,
    pub duration: Duration,
    pub has_activity: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, ValueEnum, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SourceKind {
    #[value(alias = "mic")]
    Microphone,
    #[value(alias = "system")]
    SystemAudio,
}

impl SourceKind {
    pub const fn all() -> [Self; 2] {
        [Self::Microphone, Self::SystemAudio]
    }

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
}

#[cfg(target_os = "macos")]
pub fn run_capture(
    sources: Vec<SourceKind>,
    audio_routes: HashMap<SourceKind, mpsc::UnboundedSender<AudioChunk>>,
    ui_tx: std_mpsc::Sender<AppEvent>,
    shutdown_rx: watch::Receiver<bool>,
) -> Result<()> {
    if sources.is_empty() {
        return Ok(());
    }

    let mut handles = Vec::new();

    for source in sources {
        let Some(route) = audio_routes.get(&source).cloned() else {
            continue;
        };

        let ui_tx = ui_tx.clone();
        let shutdown_rx = shutdown_rx.clone();

        handles.push(thread::spawn(move || {
            let result = match source {
                SourceKind::Microphone => run_microphone_capture(route, ui_tx.clone(), shutdown_rx),
                SourceKind::SystemAudio => {
                    run_system_audio_capture(route, ui_tx.clone(), shutdown_rx)
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

#[cfg(not(target_os = "macos"))]
pub fn run_capture(
    _sources: Vec<SourceKind>,
    _audio_routes: HashMap<SourceKind, mpsc::UnboundedSender<AudioChunk>>,
    _ui_tx: std_mpsc::Sender<AppEvent>,
    _shutdown_rx: watch::Receiver<bool>,
) -> Result<()> {
    bail!("this tool currently supports macOS only");
}

#[cfg(target_os = "macos")]
fn run_microphone_capture(
    route: mpsc::UnboundedSender<AudioChunk>,
    ui_tx: std_mpsc::Sender<AppEvent>,
    shutdown_rx: watch::Receiver<bool>,
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

    let pipeline = Arc::new(Mutex::new(MicrophonePipeline::new(
        usize::from(config.channels()),
        config.sample_rate(),
        TARGET_SAMPLE_RATE,
    )));
    let stream = build_microphone_stream(
        &device,
        &config,
        Arc::clone(&pipeline),
        route.clone(),
        ui_tx.clone(),
    )?;
    stream.play().context("start microphone input stream")?;

    let _ = ui_tx.send(AppEvent::SourceStatus {
        source: SourceKind::Microphone,
        status: "capturing".into(),
    });

    while !*shutdown_rx.borrow() {
        thread::sleep(Duration::from_millis(100));
    }

    drop(stream);
    flush_batched_audio(&pipeline, &route);
    Ok(())
}

#[cfg(target_os = "macos")]
fn build_microphone_stream(
    device: &cpal::Device,
    config: &SupportedStreamConfig,
    pipeline: Arc<Mutex<MicrophonePipeline>>,
    route: mpsc::UnboundedSender<AudioChunk>,
    ui_tx: std_mpsc::Sender<AppEvent>,
) -> Result<Stream> {
    let stream_config: StreamConfig = config.clone().into();

    match config.sample_format() {
        SampleFormat::I8 => build_microphone_stream_typed::<i8>(
            device,
            &stream_config,
            Arc::clone(&pipeline),
            route,
            ui_tx,
        ),
        SampleFormat::I16 => build_microphone_stream_typed::<i16>(
            device,
            &stream_config,
            Arc::clone(&pipeline),
            route,
            ui_tx,
        ),
        SampleFormat::I24 => build_microphone_stream_typed::<cpal::I24>(
            device,
            &stream_config,
            Arc::clone(&pipeline),
            route,
            ui_tx,
        ),
        SampleFormat::I32 => build_microphone_stream_typed::<i32>(
            device,
            &stream_config,
            Arc::clone(&pipeline),
            route,
            ui_tx,
        ),
        SampleFormat::I64 => build_microphone_stream_typed::<i64>(
            device,
            &stream_config,
            Arc::clone(&pipeline),
            route,
            ui_tx,
        ),
        SampleFormat::U8 => build_microphone_stream_typed::<u8>(
            device,
            &stream_config,
            Arc::clone(&pipeline),
            route,
            ui_tx,
        ),
        SampleFormat::U16 => build_microphone_stream_typed::<u16>(
            device,
            &stream_config,
            Arc::clone(&pipeline),
            route,
            ui_tx,
        ),
        SampleFormat::U24 => build_microphone_stream_typed::<cpal::U24>(
            device,
            &stream_config,
            Arc::clone(&pipeline),
            route,
            ui_tx,
        ),
        SampleFormat::U32 => build_microphone_stream_typed::<u32>(
            device,
            &stream_config,
            Arc::clone(&pipeline),
            route,
            ui_tx,
        ),
        SampleFormat::U64 => build_microphone_stream_typed::<u64>(
            device,
            &stream_config,
            Arc::clone(&pipeline),
            route,
            ui_tx,
        ),
        SampleFormat::F32 => build_microphone_stream_typed::<f32>(
            device,
            &stream_config,
            Arc::clone(&pipeline),
            route,
            ui_tx,
        ),
        SampleFormat::F64 => {
            build_microphone_stream_typed::<f64>(device, &stream_config, pipeline, route, ui_tx)
        }
        unsupported => bail!("unsupported microphone sample format: {unsupported}"),
    }
}

#[cfg(target_os = "macos")]
fn build_microphone_stream_typed<T>(
    device: &cpal::Device,
    config: &StreamConfig,
    pipeline: Arc<Mutex<MicrophonePipeline>>,
    route: mpsc::UnboundedSender<AudioChunk>,
    ui_tx: std_mpsc::Sender<AppEvent>,
) -> Result<Stream>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let stream = device.build_input_stream(
        config,
        {
            let pipeline = Arc::clone(&pipeline);
            move |data: &[T], _info| {
                let Ok(mut pipeline) = pipeline.lock() else {
                    return;
                };
                for chunk in pipeline.push_samples_at::<T>(data, Instant::now()) {
                    let _ = route.send(chunk);
                }
            }
        },
        move |error| {
            let _ = ui_tx.send(AppEvent::SourceError {
                source: SourceKind::Microphone,
                error: format!("microphone stream error: {error}"),
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
) -> Result<()> {
    let content = SCShareableContent::get().context(
        "load shareable content; macOS may require Screen Recording permission for Terminal",
    )?;

    let displays = content.displays();
    let display = displays
        .iter()
        .max_by_key(|display| u64::from(display.width()) * u64::from(display.height()))
        .context("no display available for system audio capture")?;

    let filter = SCContentFilter::create()
        .with_display(display)
        .with_excluding_windows(&[])
        .build();

    let delegate_tx = ui_tx.clone();
    let delegate = StreamCallbacks::new()
        .on_error(move |error| {
            let _ = delegate_tx.send(AppEvent::Notice(format!("capture error: {error}")));
        })
        .on_stop({
            let delegate_tx = ui_tx.clone();
            move |error| {
                let message = error.unwrap_or_else(|| "capture stopped".into());
                let _ = delegate_tx.send(AppEvent::Notice(message));
            }
        });

    let config = SCStreamConfiguration::new()
        .with_width(display.width())
        .with_height(display.height())
        .with_captures_audio(true)
        .with_sample_rate(AudioSampleRate::Rate16000)
        .with_channel_count(AudioChannelCount::Mono)
        .with_excludes_current_process_audio(true);

    let mut stream = SCStream::new_with_delegate(&filter, &config, delegate);
    let batcher = Arc::new(Mutex::new(AudioChunkBatcher::new(
        TARGET_SAMPLE_RATE,
        Duration::from_millis(TARGET_BATCH_DURATION_MS),
    )));

    attach_system_audio_handler(
        &mut stream,
        Arc::clone(&batcher),
        route.clone(),
        ui_tx.clone(),
    );

    let _ = ui_tx.send(AppEvent::Notice(
        "system audio capture may ask for Screen Recording permission on first run".into(),
    ));

    stream
        .start_capture()
        .context("start ScreenCaptureKit system audio capture")?;

    let _ = ui_tx.send(AppEvent::SourceStatus {
        source: SourceKind::SystemAudio,
        status: "capturing".into(),
    });

    while !*shutdown_rx.borrow() {
        thread::sleep(Duration::from_millis(100));
    }

    stream
        .stop_capture()
        .context("stop ScreenCaptureKit system audio capture")?;
    flush_batched_audio(&batcher, &route);

    Ok(())
}

#[cfg(target_os = "macos")]
fn attach_system_audio_handler(
    stream: &mut SCStream,
    batcher: Arc<Mutex<AudioChunkBatcher>>,
    route: mpsc::UnboundedSender<AudioChunk>,
    ui_tx: std_mpsc::Sender<AppEvent>,
) {
    let error_sent = Arc::new(AtomicBool::new(false));

    stream.add_output_handler(
        move |sample, _type| match decode_audio_samples(&sample) {
            Ok(samples) if !samples.is_empty() => {
                let Ok(mut batcher) = batcher.lock() else {
                    return;
                };
                for chunk in batcher.push(&samples, Instant::now()) {
                    let _ = route.send(chunk);
                }
            }
            Ok(_) => {}
            Err(error) => {
                if !error_sent.swap(true, Ordering::Relaxed) {
                    let _ = ui_tx.send(AppEvent::SourceError {
                        source: SourceKind::SystemAudio,
                        error: format!("decode failed: {error:#}"),
                    });
                }
            }
        },
        SCStreamOutputType::Audio,
    );
}

#[cfg(target_os = "macos")]
fn flush_batched_audio<T>(batcher: &Arc<Mutex<T>>, route: &mpsc::UnboundedSender<AudioChunk>)
where
    T: AudioBatching,
{
    let Ok(mut batcher) = batcher.lock() else {
        return;
    };

    if let Some(chunk) = batcher.flush_audio() {
        let _ = route.send(chunk);
    }
}

#[cfg(target_os = "macos")]
fn decode_audio_samples(sample: &CMSampleBuffer) -> Result<Vec<f32>> {
    let format = sample
        .format_description()
        .context("missing audio format description")?;
    let channel_count = format
        .audio_channel_count()
        .context("missing audio channel count")? as usize;
    let bits_per_channel = format
        .audio_bits_per_channel()
        .context("missing audio bit depth")? as usize;
    let big_endian = format.audio_is_big_endian();
    let frame_count = sample.num_samples();

    let buffers = sample
        .audio_buffer_list()
        .context("missing audio buffer list")?;

    if frame_count == 0 || buffers.num_buffers() == 0 {
        return Ok(Vec::new());
    }

    match (format.audio_is_float(), bits_per_channel) {
        (true, 32) => decode_f32_buffers(&buffers, channel_count, frame_count, big_endian),
        (false, 16) => decode_i16_buffers(&buffers, channel_count, frame_count, big_endian),
        (false, 32) => decode_i32_buffers(&buffers, channel_count, frame_count, big_endian),
        (is_float, bits) => bail!(
            "unsupported sample format: float={is_float}, bits={bits}, channels={channel_count}, buffers={}",
            buffers.num_buffers()
        ),
    }
}

#[cfg(target_os = "macos")]
fn decode_f32_buffers(
    buffers: &screencapturekit::cm::AudioBufferList,
    channel_count: usize,
    frame_count: usize,
    big_endian: bool,
) -> Result<Vec<f32>> {
    decode_buffers(buffers, channel_count, frame_count, 4, move |bytes| {
        let raw = if big_endian {
            f32::from_be_bytes(bytes.try_into().unwrap())
        } else {
            f32::from_le_bytes(bytes.try_into().unwrap())
        };
        raw.clamp(-1.0, 1.0)
    })
}

#[cfg(target_os = "macos")]
fn decode_i16_buffers(
    buffers: &screencapturekit::cm::AudioBufferList,
    channel_count: usize,
    frame_count: usize,
    big_endian: bool,
) -> Result<Vec<f32>> {
    decode_buffers(buffers, channel_count, frame_count, 2, move |bytes| {
        let raw = if big_endian {
            i16::from_be_bytes(bytes.try_into().unwrap())
        } else {
            i16::from_le_bytes(bytes.try_into().unwrap())
        };
        f32::from(raw) / 32768.0
    })
}

#[cfg(target_os = "macos")]
fn decode_i32_buffers(
    buffers: &screencapturekit::cm::AudioBufferList,
    channel_count: usize,
    frame_count: usize,
    big_endian: bool,
) -> Result<Vec<f32>> {
    decode_buffers(buffers, channel_count, frame_count, 4, move |bytes| {
        let raw = if big_endian {
            i32::from_be_bytes(bytes.try_into().unwrap())
        } else {
            i32::from_le_bytes(bytes.try_into().unwrap())
        };
        (raw as f64 / i32::MAX as f64) as f32
    })
}

#[cfg(target_os = "macos")]
fn decode_buffers(
    buffers: &screencapturekit::cm::AudioBufferList,
    channel_count: usize,
    frame_count: usize,
    bytes_per_sample: usize,
    decode_sample: impl Fn(&[u8]) -> f32,
) -> Result<Vec<f32>> {
    if buffers.num_buffers() == 1 {
        let buffer = buffers
            .get(0)
            .ok_or_else(|| anyhow!("missing interleaved audio buffer"))?;
        let data = buffer.data();
        let expected = frame_count
            .checked_mul(channel_count)
            .and_then(|value| value.checked_mul(bytes_per_sample))
            .context("interleaved buffer size overflow")?;

        if data.len() < expected {
            bail!(
                "interleaved audio buffer too small: expected at least {expected} bytes, got {}",
                data.len()
            );
        }

        let mut mono = Vec::with_capacity(frame_count);
        for frame in 0..frame_count {
            let mut sum = 0.0f32;
            for channel in 0..channel_count {
                let offset = (frame * channel_count + channel) * bytes_per_sample;
                sum += decode_sample(&data[offset..offset + bytes_per_sample]);
            }
            mono.push(sum / channel_count as f32);
        }
        return Ok(mono);
    }

    if buffers.num_buffers() != channel_count {
        bail!(
            "unexpected planar audio layout: {} buffers for {channel_count} channels",
            buffers.num_buffers()
        );
    }

    let mut mono = vec![0.0f32; frame_count];

    for buffer in buffers {
        let data = buffer.data();
        let expected = frame_count
            .checked_mul(bytes_per_sample)
            .context("planar buffer size overflow")?;

        if data.len() < expected {
            bail!(
                "planar audio buffer too small: expected at least {expected} bytes, got {}",
                data.len()
            );
        }

        for (frame, sample) in mono.iter_mut().enumerate() {
            let offset = frame * bytes_per_sample;
            *sample += decode_sample(&data[offset..offset + bytes_per_sample]);
        }
    }

    for sample in &mut mono {
        *sample /= channel_count as f32;
    }

    Ok(mono)
}

#[cfg(target_os = "macos")]
#[derive(Debug)]
struct MicrophonePipeline {
    converter: MicrophoneConverter,
    batcher: AudioChunkBatcher,
}

#[cfg(target_os = "macos")]
impl MicrophonePipeline {
    fn new(channels: usize, source_sample_rate: u32, target_sample_rate: u32) -> Self {
        Self {
            converter: MicrophoneConverter::new(channels, source_sample_rate, target_sample_rate),
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
}

#[cfg(target_os = "macos")]
trait AudioBatching {
    fn flush_audio(&mut self) -> Option<AudioChunk>;
}

#[cfg(target_os = "macos")]
#[derive(Debug)]
struct AudioChunkBatcher {
    sample_rate: u32,
    target_samples: usize,
    buffered: Vec<f32>,
    stream_started_at: Option<Instant>,
    emitted_samples: u64,
}

#[cfg(target_os = "macos")]
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
            samples,
            started_at,
        }
    }
}

#[cfg(target_os = "macos")]
impl AudioBatching for AudioChunkBatcher {
    fn flush_audio(&mut self) -> Option<AudioChunk> {
        AudioChunkBatcher::flush_audio(self)
    }
}

#[cfg(target_os = "macos")]
impl AudioBatching for MicrophonePipeline {
    fn flush_audio(&mut self) -> Option<AudioChunk> {
        self.batcher.flush_audio()
    }
}

#[cfg(target_os = "macos")]
#[derive(Debug)]
struct MicrophoneConverter {
    channels: usize,
    passthrough: bool,
    resampler: LinearResampler,
}

#[cfg(target_os = "macos")]
impl MicrophoneConverter {
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

#[cfg(target_os = "macos")]
#[derive(Debug)]
struct LinearResampler {
    step: f64,
    position: f64,
    buffered: Vec<f32>,
}

#[cfg(target_os = "macos")]
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

#[cfg(target_os = "macos")]
fn detect_audio_activity(samples: &[f32]) -> bool {
    if samples.is_empty() {
        return false;
    }

    let mut peak = 0.0f32;
    let mut energy_sum = 0.0f32;

    for &sample in samples {
        let magnitude = sample.abs();
        peak = peak.max(magnitude);
        energy_sum += sample * sample;
    }

    let rms = (energy_sum / samples.len() as f32).sqrt();
    peak >= AUDIO_ACTIVITY_PEAK_THRESHOLD || rms >= AUDIO_ACTIVITY_RMS_THRESHOLD
}

#[cfg(target_os = "macos")]
fn duration_from_samples(sample_count: u64, sample_rate: u32) -> Duration {
    if sample_count == 0 {
        return Duration::ZERO;
    }

    let nanos = sample_count.saturating_mul(1_000_000_000) / u64::from(sample_rate.max(1));
    Duration::from_nanos(nanos)
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use std::time::{Duration, Instant};

    use super::{AudioChunkBatcher, LinearResampler, MicrophoneConverter, TARGET_SAMPLE_RATE};

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
        let mut converter = MicrophoneConverter::new(2, TARGET_SAMPLE_RATE, TARGET_SAMPLE_RATE);
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
        let mut batcher = AudioChunkBatcher::new(16_000, Duration::from_millis(100));
        let base = Instant::now();
        assert!(batcher.push(&vec![0.25; 800], base).is_empty());

        let chunks = batcher.push(&vec![0.25; 800], base + Duration::from_millis(50));
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].samples.len(), 1600);
        assert!(chunks[0].has_activity);
    }

    #[test]
    fn audio_chunk_batcher_flushes_remainder() {
        let mut batcher = AudioChunkBatcher::new(16_000, Duration::from_millis(100));
        assert!(batcher.push(&vec![0.25; 400], Instant::now()).is_empty());

        let tail = batcher.flush_audio().expect("tail should be present");
        assert_eq!(tail.samples.len(), 400);
    }

    #[test]
    fn audio_chunk_batcher_tracks_chunk_start_from_audio_clock() {
        let mut batcher = AudioChunkBatcher::new(16_000, Duration::from_millis(100));
        let received_at = Instant::now();

        let chunks = batcher.push(&vec![0.25; 1600], received_at);

        assert_eq!(chunks.len(), 1);
        assert_eq!(
            chunks[0].started_at + Duration::from_millis(100),
            received_at
        );
    }

    #[test]
    fn silence_chunk_is_marked_inactive() {
        let mut batcher = AudioChunkBatcher::new(16_000, Duration::from_millis(100));
        let chunks = batcher.push(&vec![0.0; 1600], Instant::now());

        assert_eq!(chunks.len(), 1);
        assert!(!chunks[0].has_activity);
    }
}
