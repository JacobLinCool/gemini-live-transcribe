use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

#[test]
fn paths_subcommand_prints_config_and_logs_paths() {
    let home = create_test_home();
    let output = Command::new(env!("CARGO_BIN_EXE_gemini-live-transcribe"))
        .arg("paths")
        .env("HOME", &home)
        .output()
        .expect("paths subcommand should run");

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).expect("stdout should be utf-8");
    let lines = stdout.lines().collect::<Vec<_>>();
    assert_eq!(lines.len(), 2, "stdout was: {stdout:?}");
    assert_eq!(
        lines[0],
        format!(
            "config_path={}",
            home.join(".gemini-live-transcribe/config.toml").display()
        )
    );
    assert_eq!(
        lines[1],
        format!(
            "logs_dir={}",
            home.join(".gemini-live-transcribe/logs").display()
        )
    );
    assert!(output.stderr.is_empty(), "stderr should be empty");
    assert!(
        home.join(".gemini-live-transcribe/config.toml").exists(),
        "paths subcommand should create the default config"
    );
}

fn create_test_home() -> PathBuf {
    let counter = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "gemini-live-transcribe-cli-{}-{counter}",
        std::process::id()
    ));
    if path.exists() {
        std::fs::remove_dir_all(&path).expect("existing test dir should be removable");
    }
    std::fs::create_dir_all(&path).expect("test home should be created");
    path
}
