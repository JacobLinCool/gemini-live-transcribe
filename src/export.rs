use std::path::PathBuf;

use anyhow::{Context, Result, anyhow};

use crate::paths::home_transcripts_dir;
use crate::transcript::{TranscriptStore, validate_export_path};

pub fn transcript_export_path(session_id: &str) -> Result<PathBuf> {
    let dir = home_transcripts_dir()
        .ok_or_else(|| anyhow!("application transcript directory is unavailable"))?;
    Ok(dir.join(format!("{session_id}.json")))
}

pub fn write_transcript_export(store: &mut TranscriptStore) -> Result<PathBuf> {
    let path = transcript_export_path(store.session_id())?;
    validate_export_path(&path)?;
    store
        .write_json(&path)
        .with_context(|| format!("write transcript export {}", path.display()))?;
    Ok(path)
}
