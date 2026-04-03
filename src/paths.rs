use std::env;
use std::path::PathBuf;

const APP_NAME: &str = "gemini-live-transcribe";
const CONFIG_FILE_NAME: &str = "config.toml";
const LOGS_DIR_NAME: &str = "logs";
const TRANSCRIPTS_DIR_NAME: &str = "transcripts";
const DEBUG_DIR_NAME: &str = "debug";

#[cfg(target_os = "macos")]
fn config_root_dir() -> Option<PathBuf> {
    env::var_os("HOME").map(PathBuf::from).map(|home| {
        home.join("Library")
            .join("Application Support")
            .join(APP_NAME)
    })
}

#[cfg(target_os = "macos")]
fn data_root_dir() -> Option<PathBuf> {
    config_root_dir()
}

#[cfg(target_os = "linux")]
fn config_root_dir() -> Option<PathBuf> {
    env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(PathBuf::from).map(|home| home.join(".config")))
        .map(|dir| dir.join(APP_NAME))
}

#[cfg(target_os = "linux")]
fn data_root_dir() -> Option<PathBuf> {
    env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            env::var_os("HOME")
                .map(PathBuf::from)
                .map(|home| home.join(".local").join("share"))
        })
        .map(|dir| dir.join(APP_NAME))
}

#[cfg(target_os = "windows")]
fn config_root_dir() -> Option<PathBuf> {
    env::var_os("APPDATA")
        .map(PathBuf::from)
        .map(|dir| dir.join(APP_NAME).join("config"))
}

#[cfg(target_os = "windows")]
fn data_root_dir() -> Option<PathBuf> {
    env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .map(|dir| dir.join(APP_NAME).join("data"))
}

pub fn home_config_path() -> Option<PathBuf> {
    config_root_dir().map(|dir| dir.join(CONFIG_FILE_NAME))
}

pub fn home_logs_dir() -> Option<PathBuf> {
    data_root_dir().map(|dir| dir.join(LOGS_DIR_NAME))
}

pub fn home_transcripts_dir() -> Option<PathBuf> {
    data_root_dir().map(|dir| dir.join(TRANSCRIPTS_DIR_NAME))
}

pub fn home_debug_dir() -> Option<PathBuf> {
    data_root_dir().map(|dir| dir.join(DEBUG_DIR_NAME))
}
