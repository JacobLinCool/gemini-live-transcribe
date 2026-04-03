use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

struct PathsTestEnv {
    root: PathBuf,
    config_path: PathBuf,
    logs_dir: PathBuf,
    transcripts_dir: PathBuf,
    debug_dir: PathBuf,
    env_pairs: Vec<(&'static str, PathBuf)>,
}

#[test]
fn paths_subcommand_prints_config_and_logs_paths() {
    let env = create_test_env();
    let mut command = Command::new(env!("CARGO_BIN_EXE_gemini-live-transcribe"));
    command.arg("paths");

    for (key, value) in &env.env_pairs {
        command.env(key, value);
    }

    let output = command.output().expect("paths subcommand should run");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).expect("stdout should be utf-8");
    let lines = stdout.lines().collect::<Vec<_>>();
    assert_eq!(lines.len(), 4, "stdout was: {stdout:?}");
    assert_eq!(
        lines[0],
        format!("config_path={}", env.config_path.display())
    );
    assert_eq!(lines[1], format!("logs_dir={}", env.logs_dir.display()));
    assert_eq!(
        lines[2],
        format!("transcripts_dir={}", env.transcripts_dir.display())
    );
    assert_eq!(lines[3], format!("debug_dir={}", env.debug_dir.display()));
    assert!(output.stderr.is_empty(), "stderr should be empty");
    assert!(
        env.config_path.exists(),
        "paths subcommand should create the default config"
    );

    cleanup_test_dir(&env.root);
}

fn create_test_root() -> PathBuf {
    let counter = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir().join(format!(
        "gemini-live-transcribe-cli-{}-{counter}",
        std::process::id()
    ));
    if root.exists() {
        std::fs::remove_dir_all(&root).expect("existing test dir should be removable");
    }
    std::fs::create_dir_all(&root).expect("test root should be created");
    root
}

#[cfg(target_os = "macos")]
fn create_test_env() -> PathsTestEnv {
    let root = create_test_root();
    let home = root.join("home");
    std::fs::create_dir_all(&home).expect("test home should be created");
    let app_dir = home.join("Library/Application Support/gemini-live-transcribe");
    PathsTestEnv {
        root,
        config_path: app_dir.join("config.toml"),
        logs_dir: app_dir.join("logs"),
        transcripts_dir: app_dir.join("transcripts"),
        debug_dir: app_dir.join("debug"),
        env_pairs: vec![("HOME", home)],
    }
}

#[cfg(target_os = "linux")]
fn create_test_env() -> PathsTestEnv {
    let root = create_test_root();
    let home = root.join("home");
    let xdg_config_home = home.join(".config");
    let xdg_data_home = home.join(".local").join("share");
    std::fs::create_dir_all(&home).expect("test home should be created");
    std::fs::create_dir_all(&xdg_config_home).expect("xdg config home should be created");
    std::fs::create_dir_all(&xdg_data_home).expect("xdg data home should be created");
    PathsTestEnv {
        root,
        config_path: xdg_config_home.join("gemini-live-transcribe").join("config.toml"),
        logs_dir: xdg_data_home.join("gemini-live-transcribe").join("logs"),
        transcripts_dir: xdg_data_home.join("gemini-live-transcribe").join("transcripts"),
        debug_dir: xdg_data_home.join("gemini-live-transcribe").join("debug"),
        env_pairs: vec![
            ("HOME", home),
            ("XDG_CONFIG_HOME", xdg_config_home),
            ("XDG_DATA_HOME", xdg_data_home),
        ],
    }
}

#[cfg(target_os = "windows")]
fn create_test_env() -> PathsTestEnv {
    let root = create_test_root();
    let user_profile = root.join("home");
    let appdata = root.join("AppData").join("Roaming");
    let local_appdata = root.join("AppData").join("Local");
    std::fs::create_dir_all(&user_profile).expect("user profile should be created");
    std::fs::create_dir_all(&appdata).expect("appdata should be created");
    std::fs::create_dir_all(&local_appdata).expect("local appdata should be created");
    PathsTestEnv {
        root,
        config_path: appdata
            .join("gemini-live-transcribe")
            .join("config")
            .join("config.toml"),
        logs_dir: local_appdata
            .join("gemini-live-transcribe")
            .join("data")
            .join("logs"),
        transcripts_dir: local_appdata
            .join("gemini-live-transcribe")
            .join("data")
            .join("transcripts"),
        debug_dir: local_appdata
            .join("gemini-live-transcribe")
            .join("data")
            .join("debug"),
        env_pairs: vec![
            ("USERPROFILE", user_profile.clone()),
            ("HOME", user_profile),
            ("APPDATA", appdata),
            ("LOCALAPPDATA", local_appdata),
        ],
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn create_test_env() -> PathsTestEnv {
    panic!("unsupported test platform");
}

fn cleanup_test_dir(dir: &Path) {
    let _ = std::fs::remove_dir_all(dir);
}
