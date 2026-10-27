//! Update checks and self-update against local release fixtures.
//!
//! No release is published yet, so a "release" here is a temporary directory
//! with `SHA256SUMS` and assets laid out exactly like a GitHub release. The
//! executable being replaced is a copy in a temporary directory, never the
//! real installation.

use std::path::{Path, PathBuf};

use reason_network::update::{self, Source, Version};
use reason_network::util;

const REAL_EXE: &str = env!("CARGO_BIN_EXE_reason-network");

fn target() -> &'static str {
    update::current_target().expect("tests run on a release target")
}

struct Release {
    dir: tempfile::TempDir,
    sums: Vec<String>,
}

impl Release {
    fn new() -> Self {
        Self {
            dir: tempfile::tempdir().unwrap(),
            sums: Vec::new(),
        }
    }

    /// Publish an executable asset for `version` with the given bytes.
    fn add(&mut self, version: &str, bytes: &[u8]) -> PathBuf {
        let name = update::binary_name(&Version::parse(version).unwrap(), target());
        let path = self.dir.path().join(&name);
        std::fs::write(&path, bytes).unwrap();
        self.sums
            .push(format!("{}  {name}", util::sha256_hex(bytes)));
        self.write_sums();
        path
    }

    fn write_sums(&self) {
        std::fs::write(
            self.dir.path().join(update::SUMS_FILE),
            self.sums.join("\n") + "\n",
        )
        .unwrap();
    }

    fn source(&self) -> Source {
        Source::Dir(self.dir.path().to_path_buf())
    }
}

/// An "installed" copy of the executable plus a data directory.
struct Install {
    dir: tempfile::TempDir,
    exe: PathBuf,
    data: PathBuf,
}

impl Install {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let exe = bin.join(Path::new(REAL_EXE).file_name().unwrap());
        std::fs::copy(REAL_EXE, &exe).unwrap();
        let data = dir.path().join("data");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::write(data.join("reason-network.db"), b"user data").unwrap();
        std::fs::write(data.join("machine.key"), b"secret").unwrap();
        Self { dir, exe, data }
    }

    fn exe_hash(&self) -> String {
        util::sha256_file(&self.exe).unwrap()
    }

    fn assert_data_intact(&self) {
        assert_eq!(
            std::fs::read(self.data.join("reason-network.db")).unwrap(),
            b"user data"
        );
        assert_eq!(
            std::fs::read(self.data.join("machine.key")).unwrap(),
            b"secret"
        );
    }

    fn stray_files(&self) -> Vec<String> {
        std::fs::read_dir(self.exe.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().to_string())
            .filter(|name| name.contains(".new-"))
            .collect()
    }
}

/// A runnable reason-network binary whose bytes differ from the installed
/// one, standing in for a newer build.
fn newer_build() -> Vec<u8> {
    let mut bytes = std::fs::read(REAL_EXE).unwrap();
    // Trailing bytes after a PE/ELF image do not affect execution. macOS
    // validates code signatures, so the bytes stay unmodified there.
    if !cfg!(target_os = "macos") {
        bytes.extend_from_slice(b"\0reason-network-test-release-marker");
    }
    bytes
}

#[tokio::test]
async fn check_reports_newer_and_current_releases() {
    let mut release = Release::new();
    release.add("0.0.1", b"old");
    let current = update::current_version();
    let found = update::check(&release.source(), None, target())
        .await
        .unwrap();
    assert_eq!(found.available, "0.0.1");
    assert!(!found.update_available);

    release.add("99.0.0", b"new");
    let found = update::check(&release.source(), None, target())
        .await
        .unwrap();
    assert_eq!(found.current, current.to_string());
    assert_eq!(found.available, "99.0.0");
    assert!(found.update_available);
    assert_eq!(
        found.asset,
        update::binary_name(&Version::parse("99.0.0").unwrap(), target())
    );

    let pinned = update::check(
        &release.source(),
        Some(&Version::parse("0.0.1").unwrap()),
        target(),
    )
    .await
    .unwrap();
    assert_eq!(pinned.available, "0.0.1");

    let error = update::check(
        &release.source(),
        Some(&Version::parse("5.5.5").unwrap()),
        target(),
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("5.5.5 is not listed"), "{error}");
}

#[tokio::test]
async fn a_release_without_this_platform_is_reported() {
    let mut release = Release::new();
    let other = if target().contains("windows") {
        "x86_64-unknown-linux-gnu"
    } else {
        "x86_64-pc-windows-msvc"
    };
    let name = update::binary_name(&Version::parse("99.0.0").unwrap(), other);
    std::fs::write(release.dir.path().join(&name), b"x").unwrap();
    release
        .sums
        .push(format!("{}  {name}", util::sha256_hex(b"x")));
    release.write_sums();
    let error = update::check(&release.source(), None, target())
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("has no executable for"),
        "{error}"
    );
}

#[tokio::test]
async fn update_installs_a_verified_release_and_keeps_data() {
    let mut release = Release::new();
    let bytes = newer_build();
    release.add("99.0.0", &bytes);
    let install = Install::new();
    let original = install.exe_hash();

    let result = update::update(&release.source(), None, target(), &install.exe)
        .await
        .unwrap();
    assert!(result.changed);
    assert_eq!(result.to, "99.0.0");
    assert_eq!(
        install.exe_hash(),
        util::sha256_hex(&bytes),
        "new executable installed"
    );
    let previous = PathBuf::from(result.previous.unwrap());
    assert_eq!(
        util::sha256_file(&previous).unwrap(),
        original,
        "old one kept for rollback"
    );
    install.assert_data_intact();
    assert!(install.stray_files().is_empty());

    // The installed executable still runs.
    let output = std::process::Command::new(&install.exe)
        .arg("--version")
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&output.stdout).starts_with("reason-network"));

    // Running it again is safe: nothing newer, nothing changes.
    let again = update::update(&release.source(), None, target(), &install.exe).await;
    // 99.0.0 is still newer than this build, so it reinstalls the same bytes.
    assert!(again.unwrap().changed);
    assert_eq!(install.exe_hash(), util::sha256_hex(&bytes));
    install.assert_data_intact();
}

#[tokio::test]
async fn up_to_date_installations_are_left_alone() {
    let mut release = Release::new();
    release.add(&update::current_version().to_string(), b"same version");
    let install = Install::new();
    let before = install.exe_hash();
    let result = update::update(&release.source(), None, target(), &install.exe)
        .await
        .unwrap();
    assert!(!result.changed);
    assert_eq!(before, install.exe_hash());
}

#[tokio::test]
async fn tampered_artifacts_are_rejected_without_touching_the_install() {
    let mut release = Release::new();
    let asset = release.add("99.0.0", &newer_build());
    // Corrupt the asset after the checksums were published.
    let mut bytes = std::fs::read(&asset).unwrap();
    bytes.push(0);
    std::fs::write(&asset, bytes).unwrap();

    let install = Install::new();
    let before = install.exe_hash();
    let error = update::update(&release.source(), None, target(), &install.exe)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("checksum mismatch"), "{error}");
    assert_eq!(before, install.exe_hash());
    assert!(install.stray_files().is_empty());
    assert!(
        !install
            .exe
            .with_file_name(format!(
                "{}.old",
                install.exe.file_name().unwrap().to_string_lossy()
            ))
            .exists()
    );
    install.assert_data_intact();
}

#[tokio::test]
async fn an_artifact_that_does_not_run_is_not_installed() {
    let mut release = Release::new();
    // Correct checksum, but not an executable.
    release.add("99.0.0", b"this is not a program");
    let install = Install::new();
    let before = install.exe_hash();
    let error = update::update(&release.source(), None, target(), &install.exe)
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("nothing was installed"),
        "{error}"
    );
    assert_eq!(before, install.exe_hash());
    assert!(install.stray_files().is_empty());
    let _keep = &install.dir;
}

#[tokio::test]
async fn an_explicit_version_can_downgrade() {
    let mut release = Release::new();
    let old = newer_build();
    release.add("0.0.1", &old);
    release.add("99.0.0", b"never downloaded");
    let install = Install::new();
    let result = update::update(
        &release.source(),
        Some(&Version::parse("0.0.1").unwrap()),
        target(),
        &install.exe,
    )
    .await
    .unwrap();
    assert!(result.changed);
    assert_eq!(result.to, "0.0.1");
    assert_eq!(install.exe_hash(), util::sha256_hex(&old));
}

#[test]
fn cli_update_check_against_a_local_release() {
    let mut release = Release::new();
    release.add("99.0.0", b"new");
    let output = std::process::Command::new(REAL_EXE)
        .args(["update", "--check", "--source"])
        .arg(release.dir.path())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout}");
    assert!(stdout.contains("available: 99.0.0"), "{stdout}");
    assert!(stdout.contains("an update is available"), "{stdout}");

    let output = std::process::Command::new(REAL_EXE)
        .args(["--version"])
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        format!("reason-network {}", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn cli_update_without_a_source_explains_how_to_configure_one() {
    if option_env!("REASON_NETWORK_REPO").is_some() {
        // Release builds have a baked-in repository; nothing to test here.
        return;
    }
    let output = std::process::Command::new(REAL_EXE)
        .args(["update", "--check"])
        .env_remove("REASON_NETWORK_REPO")
        .env_remove("REASON_NETWORK_RELEASE_URL")
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("--source"), "{stderr}");
}
