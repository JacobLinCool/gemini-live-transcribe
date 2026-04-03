use std::path::PathBuf;

use directories::ProjectDirs;

const APP_NAME: &str = "gemini-live-transcribe";
const CONFIG_FILE_NAME: &str = "config.toml";
const LOGS_DIR_NAME: &str = "logs";
const TRANSCRIPTS_DIR_NAME: &str = "transcripts";
const DEBUG_DIR_NAME: &str = "debug";

fn project_dirs() -> Option<ProjectDirs> {
    ProjectDirs::from("", "", APP_NAME)
}

pub fn home_config_path() -> Option<PathBuf> {
    project_dirs().map(|dirs| dirs.config_dir().join(CONFIG_FILE_NAME))
}

pub fn home_logs_dir() -> Option<PathBuf> {
    project_dirs().map(|dirs| dirs.data_local_dir().join(LOGS_DIR_NAME))
}

pub fn home_transcripts_dir() -> Option<PathBuf> {
    project_dirs().map(|dirs| dirs.data_local_dir().join(TRANSCRIPTS_DIR_NAME))
}

pub fn home_debug_dir() -> Option<PathBuf> {
    project_dirs().map(|dirs| dirs.data_local_dir().join(DEBUG_DIR_NAME))
}
