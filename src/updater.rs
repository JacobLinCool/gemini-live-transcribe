use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};

const REPO_OWNER: &str = "JacobLinCool";
const REPO_NAME: &str = "gemini-live-transcribe";
const BINARY_NAME: &str = env!("CARGO_PKG_NAME");

pub fn run_self_update() -> Result<()> {
    let release = ReleaseSpec::for_host()?;
    let current_exe = std::env::current_exe().context("resolve current executable path")?;
    let executable_dir = current_exe
        .parent()
        .ok_or_else(|| anyhow!("current executable has no parent directory"))?;
    let stage_path = stage_path_for(&current_exe)?;
    let temp_dir = TempDir::create()?;
    let archive_path = temp_dir.join(&release.asset_name);

    download_release_archive(&release.download_url, &archive_path)?;
    extract_release_archive(&archive_path, temp_dir.path())?;

    let extracted_binary = temp_dir.join(BINARY_NAME);
    if !extracted_binary.is_file() {
        bail!(
            "release archive did not contain expected binary {}",
            extracted_binary.display()
        );
    }

    if stage_path.exists() {
        fs::remove_file(&stage_path)
            .with_context(|| format!("remove stale stage file {}", stage_path.display()))?;
    }

    fs::copy(&extracted_binary, &stage_path).with_context(|| {
        format!(
            "copy extracted binary {} to staging path {}",
            extracted_binary.display(),
            stage_path.display()
        )
    })?;

    let current_permissions = fs::metadata(&current_exe)
        .with_context(|| format!("stat current executable {}", current_exe.display()))?
        .permissions();
    fs::set_permissions(&stage_path, current_permissions)
        .with_context(|| format!("set permissions on {}", stage_path.display()))?;

    fs::rename(&stage_path, &current_exe).with_context(|| {
        format!(
            "replace current executable {} from stage {}",
            current_exe.display(),
            stage_path.display()
        )
    })?;

    println!(
        "Updated {} in {} to {} from {}",
        BINARY_NAME,
        executable_dir.display(),
        release.target,
        release.download_url
    );
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ReleaseSpec {
    target: &'static str,
    asset_name: String,
    download_url: String,
}

impl ReleaseSpec {
    fn for_host() -> Result<Self> {
        Self::new(std::env::consts::OS, std::env::consts::ARCH)
    }

    fn new(os: &str, arch: &str) -> Result<Self> {
        let target = release_target_for(os, arch)?;
        let asset_name = release_asset_name(target);
        Ok(Self {
            target,
            download_url: release_download_url(&asset_name),
            asset_name,
        })
    }
}

fn release_target_for(os: &str, arch: &str) -> Result<&'static str> {
    match (os, arch) {
        ("macos", "aarch64") => Ok("aarch64-apple-darwin"),
        ("macos", "x86_64") => Ok("x86_64-apple-darwin"),
        _ => bail!("self-update is only supported on macOS arm64 and x86_64"),
    }
}

fn release_asset_name(target: &str) -> String {
    format!("{BINARY_NAME}-{target}.tar.gz")
}

fn release_download_url(asset_name: &str) -> String {
    format!("https://github.com/{REPO_OWNER}/{REPO_NAME}/releases/latest/download/{asset_name}")
}

fn stage_path_for(current_exe: &Path) -> Result<PathBuf> {
    let parent = current_exe
        .parent()
        .ok_or_else(|| anyhow!("current executable has no parent directory"))?;
    let file_name = current_exe
        .file_name()
        .ok_or_else(|| anyhow!("current executable has no file name"))?;
    let mut stage_name = OsString::from(".");
    stage_name.push(file_name);
    stage_name.push(".update");
    Ok(parent.join(stage_name))
}

fn create_temp_dir() -> Result<PathBuf> {
    let base = std::env::temp_dir();
    for attempt in 0..32 {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .context("system clock is before unix epoch")?
            .as_nanos();
        let candidate = base.join(format!(
            "{BINARY_NAME}-update-{}-{now}-{attempt}",
            std::process::id()
        ));
        match fs::create_dir(&candidate) {
            Ok(()) => return Ok(candidate),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("create temp dir {}", candidate.display()));
            }
        }
    }

    bail!("unable to create temp dir for self-update")
}

struct TempDir(PathBuf);

impl TempDir {
    fn create() -> Result<Self> {
        Ok(Self(create_temp_dir()?))
    }

    fn join(&self, path: impl AsRef<Path>) -> PathBuf {
        self.0.join(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn download_release_archive(url: &str, archive_path: &Path) -> Result<()> {
    run_command(
        Command::new("/usr/bin/curl")
            .arg("--fail")
            .arg("--silent")
            .arg("--show-error")
            .arg("--location")
            .arg("--proto")
            .arg("=https")
            .arg("--tlsv1.2")
            .arg(url)
            .arg("--output")
            .arg(archive_path),
        &format!("download release archive from {url}"),
    )
}

fn extract_release_archive(archive_path: &Path, temp_dir: &Path) -> Result<()> {
    run_command(
        Command::new("/usr/bin/tar")
            .arg("-xzf")
            .arg(archive_path)
            .arg("-C")
            .arg(temp_dir),
        &format!("extract release archive {}", archive_path.display()),
    )
}

fn run_command(command: &mut Command, action: &str) -> Result<()> {
    let output = command
        .output()
        .with_context(|| format!("spawn command for {action}"))?;
    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    let detail = stderr.trim();
    if detail.is_empty() {
        bail!("{action} failed with status {}", output.status);
    }
    bail!("{action} failed: {detail}");
}

#[cfg(test)]
mod tests {
    use super::{
        ReleaseSpec, release_asset_name, release_download_url, release_target_for, stage_path_for,
    };

    #[test]
    fn maps_supported_release_targets() {
        assert_eq!(
            release_target_for("macos", "aarch64").expect("arm64 target should be supported"),
            "aarch64-apple-darwin"
        );
        assert_eq!(
            release_target_for("macos", "x86_64").expect("x64 target should be supported"),
            "x86_64-apple-darwin"
        );
    }

    #[test]
    fn rejects_unsupported_release_targets() {
        let error =
            release_target_for("linux", "x86_64").expect_err("linux should not be supported");
        assert!(error.to_string().contains("self-update"));
    }

    #[test]
    fn builds_consistent_release_asset_name() {
        assert_eq!(
            release_asset_name("aarch64-apple-darwin"),
            "gemini-live-transcribe-aarch64-apple-darwin.tar.gz"
        );
    }

    #[test]
    fn builds_consistent_release_download_url() {
        assert_eq!(
            release_download_url("gemini-live-transcribe-x86_64-apple-darwin.tar.gz"),
            "https://github.com/JacobLinCool/gemini-live-transcribe/releases/latest/download/gemini-live-transcribe-x86_64-apple-darwin.tar.gz"
        );
    }

    #[test]
    fn stages_update_next_to_current_executable() {
        let current_exe = std::path::Path::new("/tmp/gemini-live-transcribe");
        assert_eq!(
            stage_path_for(current_exe).expect("stage path should resolve"),
            std::path::PathBuf::from("/tmp/.gemini-live-transcribe.update")
        );
    }

    #[test]
    fn builds_release_spec_from_host_tuple() {
        let spec = ReleaseSpec::new("macos", "aarch64").expect("spec should build");
        assert_eq!(spec.target, "aarch64-apple-darwin");
        assert_eq!(
            spec.asset_name,
            "gemini-live-transcribe-aarch64-apple-darwin.tar.gz"
        );
    }
}
