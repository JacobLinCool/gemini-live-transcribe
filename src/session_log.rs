use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow};
use serde_json::{Map, Value, json};

use crate::capture::SourceKind;
use crate::paths::home_logs_dir;

const LOG_FLUSH_RECORD_THRESHOLD: usize = 32;
const LOG_FLUSH_INTERVAL: Duration = Duration::from_millis(250);

#[derive(Clone)]
pub struct SessionLogger {
    inner: Arc<Mutex<LogWriter>>,
    path: Arc<PathBuf>,
    source: SourceKind,
    stream_label: Arc<String>,
}

struct LogWriter {
    writer: BufWriter<File>,
    pending_records: usize,
    flush_record_threshold: usize,
    flush_interval: Duration,
    last_flush_at: Instant,
}

impl SessionLogger {
    pub fn create(
        source: SourceKind,
        stream_label: &str,
        model: &str,
        max_files: usize,
    ) -> Result<Self> {
        let log_dir =
            home_logs_dir().ok_or_else(|| anyhow!("application log directory is unavailable"))?;
        fs::create_dir_all(&log_dir)
            .with_context(|| format!("create log directory {}", log_dir.display()))?;

        let timestamp_ms = unix_timestamp_ms();
        let source_name = source.title().replace(' ', "-");
        let stream_name = stream_label.trim().replace(' ', "-");
        let path = log_dir.join(format!("{timestamp_ms}-{source_name}-{stream_name}.jsonl"));
        let file = OpenOptions::new()
            .create_new(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("open session log {}", path.display()))?;
        enforce_retention(&log_dir, &path, max_files)?;

        let logger = Self {
            inner: Arc::new(Mutex::new(LogWriter::new(file))),
            path: Arc::new(path),
            source,
            stream_label: Arc::new(stream_name),
        };

        logger.log_record(
            "lifecycle",
            "session_start",
            json!({
                "model": model,
                "stream_label": logger.stream_label.as_ref(),
                "log_path": logger.path().display().to_string(),
            }),
        );

        Ok(logger)
    }

    pub fn path(&self) -> &Path {
        self.path.as_ref().as_path()
    }

    pub fn log_lifecycle(&self, event: &str, payload: Value) {
        self.log_record("lifecycle", event, payload);
    }

    pub fn log_outbound_json(&self, event: &str, payload: &Value) {
        self.log_record("outbound", event, sanitize_json(payload));
    }

    pub fn log_inbound_message_text(&self, text: &str) {
        let payload = match serde_json::from_str::<Value>(text) {
            Ok(value) => sanitize_json(&value),
            Err(_) => json!({ "raw_text": text }),
        };
        self.log_record("inbound", "message", payload);
    }

    pub fn log_close(&self, reason: &str) {
        self.log_record("inbound", "close", json!({ "reason": reason }));
    }

    pub fn flush(&self) -> Result<()> {
        let mut writer = self
            .inner
            .lock()
            .map_err(|_| anyhow!("session log mutex poisoned"))?;
        writer.flush().context("flush session log writer")
    }

    fn log_record(&self, direction: &str, event: &str, payload: Value) {
        let record = json!({
            "ts_unix_ms": unix_timestamp_ms(),
            "source": self.source,
            "stream_label": self.stream_label.as_ref(),
            "direction": direction,
            "event": event,
            "payload": payload,
        });

        if let Err(error) = self.write_record(&record, should_force_flush(direction, event)) {
            tracing::error!(?error, path = %self.path().display(), "failed to write session log");
        }
    }

    fn write_record(&self, record: &Value, force_flush: bool) -> Result<()> {
        let serialized = serde_json::to_string(record).context("serialize session log record")?;
        let mut writer = self
            .inner
            .lock()
            .map_err(|_| anyhow!("session log mutex poisoned"))?;
        writer
            .write_record(&serialized, force_flush)
            .context("append session log record")
    }
}

impl LogWriter {
    fn new(file: File) -> Self {
        Self::with_policy(file, LOG_FLUSH_RECORD_THRESHOLD, LOG_FLUSH_INTERVAL)
    }

    fn with_policy(file: File, flush_record_threshold: usize, flush_interval: Duration) -> Self {
        Self {
            writer: BufWriter::new(file),
            pending_records: 0,
            flush_record_threshold: flush_record_threshold.max(1),
            flush_interval,
            last_flush_at: Instant::now(),
        }
    }

    fn write_record(&mut self, serialized: &str, force_flush: bool) -> Result<()> {
        writeln!(self.writer, "{serialized}").context("write serialized session log record")?;
        self.pending_records += 1;

        let threshold_reached = self.pending_records >= self.flush_record_threshold;
        let interval_reached = self.last_flush_at.elapsed() >= self.flush_interval;

        if force_flush || threshold_reached || interval_reached {
            self.flush()?;
        }

        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        self.writer.flush().context("flush buffered session log")?;
        self.pending_records = 0;
        self.last_flush_at = Instant::now();
        Ok(())
    }
}

impl Drop for LogWriter {
    fn drop(&mut self) {
        if let Err(error) = self.writer.flush() {
            tracing::error!(?error, "failed to flush session log writer on drop");
        }
    }
}

fn should_force_flush(direction: &str, event: &str) -> bool {
    matches!(
        (direction, event),
        ("lifecycle", "session_start" | "client_close") | ("inbound", "close")
    )
}

fn enforce_retention(log_dir: &Path, current_path: &Path, max_files: usize) -> Result<()> {
    let mut log_files = fs::read_dir(log_dir)
        .with_context(|| format!("read log directory {}", log_dir.display()))?
        .map(|entry| entry.context("read log directory entry"))
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .filter_map(|entry| {
            let path = entry.path();
            if path == current_path || !is_jsonl_file(&path) {
                return None;
            }
            Some(path)
        })
        .collect::<Vec<_>>();

    log_files.sort();

    let total_files = log_files.len() + 1;
    let files_to_remove = total_files.saturating_sub(max_files);

    for path in log_files.into_iter().take(files_to_remove) {
        fs::remove_file(&path).with_context(|| format!("remove old log {}", path.display()))?;
    }

    Ok(())
}

fn is_jsonl_file(path: &Path) -> bool {
    path.extension().and_then(|extension| extension.to_str()) == Some("jsonl")
}

fn sanitize_json(value: &Value) -> Value {
    match value {
        Value::Object(object) => sanitize_object(object),
        Value::Array(array) => Value::Array(array.iter().map(sanitize_json).collect()),
        _ => value.clone(),
    }
}

fn sanitize_object(object: &Map<String, Value>) -> Value {
    let mut sanitized = Map::with_capacity(object.len());

    for (key, value) in object {
        sanitized.insert(key.clone(), sanitize_json(value));
    }

    if let Some(mime_type) = object.get("mimeType").and_then(Value::as_str)
        && mime_type.starts_with("audio/")
        && let Some(data) = object.get("data").and_then(Value::as_str)
    {
        sanitized.insert(
            "data".into(),
            Value::String(format!("<omitted audio payload: {} chars>", data.len())),
        );
    }

    Value::Object(sanitized)
}

fn unix_timestamp_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time should be after unix epoch")
        .as_millis()
}

#[cfg(test)]
mod tests {
    use std::fs::{self, File};
    use std::io::Read;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    use serde_json::json;

    use super::{LogWriter, enforce_retention, sanitize_json, unix_timestamp_ms};

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn strips_audio_base64_from_logged_payloads() {
        let sanitized = sanitize_json(&json!({
            "inlineData": {
                "mimeType": "audio/pcm",
                "data": "AAAA"
            }
        }));

        assert_eq!(
            sanitized["inlineData"]["data"],
            "<omitted audio payload: 4 chars>"
        );
    }

    #[test]
    fn retention_keeps_all_logs_when_under_cap() {
        let dir = create_test_dir("under-cap");
        let current = touch(&dir, "003-current.jsonl");
        touch(&dir, "001-old.jsonl");
        touch(&dir, "002-mid.jsonl");

        enforce_retention(&dir, &current, 3).expect("retention should succeed");

        assert_eq!(
            file_names(&dir),
            vec!["001-old.jsonl", "002-mid.jsonl", "003-current.jsonl"]
        );

        cleanup_test_dir(&dir);
    }

    #[test]
    fn retention_removes_oldest_logs_when_over_cap() {
        let dir = create_test_dir("over-cap");
        let current = touch(&dir, "004-current.jsonl");
        touch(&dir, "001-oldest.jsonl");
        touch(&dir, "002-old.jsonl");
        touch(&dir, "003-mid.jsonl");

        enforce_retention(&dir, &current, 2).expect("retention should succeed");

        assert_eq!(file_names(&dir), vec!["003-mid.jsonl", "004-current.jsonl"]);

        cleanup_test_dir(&dir);
    }

    #[test]
    fn retention_ignores_non_jsonl_files() {
        let dir = create_test_dir("ignore-non-jsonl");
        let current = touch(&dir, "003-current.jsonl");
        touch(&dir, "001-old.jsonl");
        touch(&dir, "notes.txt");

        enforce_retention(&dir, &current, 1).expect("retention should succeed");

        assert_eq!(file_names(&dir), vec!["003-current.jsonl", "notes.txt"]);

        cleanup_test_dir(&dir);
    }

    #[test]
    fn log_writer_flushes_when_threshold_is_reached() {
        let dir = create_test_dir("threshold-flush");
        let path = dir.join("session.jsonl");
        let file = File::create(&path).expect("log file should be created");
        let mut writer = LogWriter::with_policy(file, 2, Duration::from_secs(3600));

        writer
            .write_record(r#"{"event":"one"}"#, false)
            .expect("first record should be buffered");
        assert_eq!(fs::read_to_string(&path).expect("read log file"), "");

        writer
            .write_record(r#"{"event":"two"}"#, false)
            .expect("second record should trigger flush");

        let contents = fs::read_to_string(&path).expect("read flushed log file");
        assert!(contents.contains(r#"{"event":"one"}"#));
        assert!(contents.contains(r#"{"event":"two"}"#));

        cleanup_test_dir(&dir);
    }

    #[test]
    fn log_writer_explicit_flush_persists_buffered_records() {
        let dir = create_test_dir("explicit-flush");
        let path = dir.join("session.jsonl");
        let file = File::create(&path).expect("log file should be created");
        let mut writer = LogWriter::with_policy(file, 99, Duration::from_secs(3600));

        writer
            .write_record(r#"{"event":"buffered"}"#, false)
            .expect("record should be buffered");
        assert_eq!(fs::read_to_string(&path).expect("read log file"), "");

        writer.flush().expect("flush should succeed");

        let mut contents = String::new();
        File::open(&path)
            .expect("log file should reopen")
            .read_to_string(&mut contents)
            .expect("read flushed log file");
        assert!(contents.contains(r#"{"event":"buffered"}"#));

        cleanup_test_dir(&dir);
    }

    fn create_test_dir(name: &str) -> PathBuf {
        let counter = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "gemini-live-transcribe-{name}-{}-{counter}",
            unix_timestamp_ms()
        ));
        fs::create_dir_all(&dir).expect("test dir should be created");
        dir
    }

    fn cleanup_test_dir(dir: &Path) {
        let _ = fs::remove_dir_all(dir);
    }

    fn touch(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        File::create(&path).expect("test file should be created");
        path
    }

    fn file_names(dir: &Path) -> Vec<String> {
        let mut names = fs::read_dir(dir)
            .expect("read dir should succeed")
            .map(|entry| entry.expect("entry should be readable").file_name())
            .map(|name| name.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        names.sort();
        names
    }
}
