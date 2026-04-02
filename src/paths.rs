use std::path::PathBuf;

const APP_DIR_NAME: &str = ".gemini-live-transcribe";
const CONFIG_FILE_NAME: &str = "config.toml";
const LOGS_DIR_NAME: &str = "logs";

pub fn home_app_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(APP_DIR_NAME))
}

pub fn home_config_path() -> Option<PathBuf> {
    home_app_dir().map(|dir| dir.join(CONFIG_FILE_NAME))
}

pub fn home_logs_dir() -> Option<PathBuf> {
    home_app_dir().map(|dir| dir.join(LOGS_DIR_NAME))
}
