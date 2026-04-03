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
    let temp_dir = TempDir::create()?;
    let archive_path = temp_dir.join(&release.asset_name);

    download_release_archive(&release.download_url, &archive_path)?;
    extract_release_archive(&release, &archive_path, temp_dir.path())?;

    let extracted_binary = temp_dir.join(release.binary_file_name());
    if !extracted_binary.is_file() {
        bail!(
            "release archive did not contain expected binary {}",
            extracted_binary.display()
        );
    }

    self_replace::self_replace(&extracted_binary)
        .with_context(|| format!("replace current executable {}", current_exe.display()))?;

    println!(
        "Updated {} in {} to {} from {}",
        BINARY_NAME,
        executable_dir.display(),
        release.target,
        release.download_url
    );
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ArchiveKind {
    TarGz,
    Zip,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ReleaseSpec {
    target: &'static str,
    archive_kind: ArchiveKind,
    asset_name: String,
    download_url: String,
}

impl ReleaseSpec {
    fn for_host() -> Result<Self> {
        Self::new(std::env::consts::OS, std::env::consts::ARCH)
    }

    fn new(os: &str, arch: &str) -> Result<Self> {
        let (target, archive_kind) = release_target_for(os, arch)?;
        let asset_name = release_asset_name(target, archive_kind);
        Ok(Self {
            target,
            archive_kind,
            download_url: release_download_url(&asset_name),
            asset_name,
        })
    }

    fn binary_file_name(&self) -> String {
        match self.archive_kind {
            ArchiveKind::TarGz => BINARY_NAME.to_string(),
            ArchiveKind::Zip => format!("{BINARY_NAME}.exe"),
        }
    }
}

fn release_target_for(os: &str, arch: &str) -> Result<(&'static str, ArchiveKind)> {
    match (os, arch) {
        ("macos", "aarch64") => Ok(("aarch64-apple-darwin", ArchiveKind::TarGz)),
        ("macos", "x86_64") => Ok(("x86_64-apple-darwin", ArchiveKind::TarGz)),
        ("linux", "x86_64") => Ok(("x86_64-unknown-linux-gnu", ArchiveKind::TarGz)),
        ("windows", "x86_64") => Ok(("x86_64-pc-windows-msvc", ArchiveKind::Zip)),
        _ => bail!("self-update is unsupported for {os} {arch}"),
    }
}

fn release_asset_name(target: &str, archive_kind: ArchiveKind) -> String {
    match archive_kind {
        ArchiveKind::TarGz => format!("{BINARY_NAME}-{target}.tar.gz"),
        ArchiveKind::Zip => format!("{BINARY_NAME}-{target}.zip"),
    }
}

fn release_download_url(asset_name: &str) -> String {
    format!("https://github.com/{REPO_OWNER}/{REPO_NAME}/releases/latest/download/{asset_name}")
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
    #[cfg(windows)]
    {
        run_command(
            powershell_command()
                .arg("-Command")
                .arg(
                    "$ProgressPreference='SilentlyContinue'; \
                     Invoke-WebRequest -Uri $env:GEMINI_UPDATE_URL -OutFile $env:GEMINI_UPDATE_OUT",
                )
                .env("GEMINI_UPDATE_URL", url)
                .env("GEMINI_UPDATE_OUT", archive_path),
            &format!("download release archive from {url}"),
        )
    }

    #[cfg(not(windows))]
    {
        run_command(
            Command::new("curl")
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
}

fn extract_release_archive(
    release: &ReleaseSpec,
    archive_path: &Path,
    temp_dir: &Path,
) -> Result<()> {
    match release.archive_kind {
        ArchiveKind::TarGz => extract_tar_gz_archive(archive_path, temp_dir),
        ArchiveKind::Zip => extract_zip_archive(archive_path, temp_dir),
    }
}

fn extract_tar_gz_archive(archive_path: &Path, temp_dir: &Path) -> Result<()> {
    run_command(
        Command::new("tar")
            .arg("-xzf")
            .arg(archive_path)
            .arg("-C")
            .arg(temp_dir),
        &format!("extract release archive {}", archive_path.display()),
    )
}

#[cfg(windows)]
fn extract_zip_archive(archive_path: &Path, temp_dir: &Path) -> Result<()> {
    run_command(
        powershell_command()
            .arg("-Command")
            .arg(
                "Expand-Archive -LiteralPath $env:GEMINI_UPDATE_ARCHIVE \
                 -DestinationPath $env:GEMINI_UPDATE_DEST -Force",
            )
            .env("GEMINI_UPDATE_ARCHIVE", archive_path)
            .env("GEMINI_UPDATE_DEST", temp_dir),
        &format!("extract release archive {}", archive_path.display()),
    )
}

#[cfg(not(windows))]
fn extract_zip_archive(_archive_path: &Path, _temp_dir: &Path) -> Result<()> {
    bail!("zip extraction is only supported on Windows hosts")
}

#[cfg(windows)]
fn powershell_command() -> Command {
    let mut command = Command::new("powershell");
    command
        .arg("-NoProfile")
        .arg("-NonInteractive")
        .arg("-ExecutionPolicy")
        .arg("Bypass");
    command
}

fn run_command(command: &mut Command, action: &str) -> Result<()> {
    let output = command
        .output()
        .with_context(|| format!("spawn command for {action}"))?;
    if output.status.success() {
        return Ok(());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let detail = if !stderr.trim().is_empty() {
        stderr.trim()
    } else {
        stdout.trim()
    };
    if detail.is_empty() {
        bail!("{action} failed with status {}", output.status);
    }
    bail!("{action} failed: {detail}");
}

#[cfg(test)]
mod tests {
    use super::{
        ArchiveKind, ReleaseSpec, release_asset_name, release_download_url, release_target_for,
    };

    #[test]
    fn maps_supported_release_targets() {
        assert_eq!(
            release_target_for("macos", "aarch64").expect("arm64 target should be supported"),
            ("aarch64-apple-darwin", ArchiveKind::TarGz)
        );
        assert_eq!(
            release_target_for("macos", "x86_64").expect("x64 target should be supported"),
            ("x86_64-apple-darwin", ArchiveKind::TarGz)
        );
        assert_eq!(
            release_target_for("linux", "x86_64").expect("linux x64 target should be supported"),
            ("x86_64-unknown-linux-gnu", ArchiveKind::TarGz)
        );
        assert_eq!(
            release_target_for("windows", "x86_64")
                .expect("windows x64 target should be supported"),
            ("x86_64-pc-windows-msvc", ArchiveKind::Zip)
        );
    }

    #[test]
    fn rejects_unsupported_release_targets() {
        let error = release_target_for("linux", "aarch64")
            .expect_err("linux arm64 should not be supported");
        assert!(error.to_string().contains("self-update is unsupported"));
    }

    #[test]
    fn builds_consistent_release_asset_names() {
        assert_eq!(
            release_asset_name("aarch64-apple-darwin", ArchiveKind::TarGz),
            "gemini-live-transcribe-aarch64-apple-darwin.tar.gz"
        );
        assert_eq!(
            release_asset_name("x86_64-pc-windows-msvc", ArchiveKind::Zip),
            "gemini-live-transcribe-x86_64-pc-windows-msvc.zip"
        );
    }

    #[test]
    fn builds_consistent_release_download_url() {
        assert_eq!(
            release_download_url("gemini-live-transcribe-x86_64-unknown-linux-gnu.tar.gz"),
            "https://github.com/JacobLinCool/gemini-live-transcribe/releases/latest/download/gemini-live-transcribe-x86_64-unknown-linux-gnu.tar.gz"
        );
    }

    #[test]
    fn release_spec_tracks_binary_name_by_archive_kind() {
        let mac = ReleaseSpec::new("macos", "aarch64").expect("spec should build");
        assert_eq!(mac.binary_file_name(), "gemini-live-transcribe");

        let windows = ReleaseSpec::new("windows", "x86_64").expect("spec should build");
        assert_eq!(windows.binary_file_name(), "gemini-live-transcribe.exe");
    }
}
