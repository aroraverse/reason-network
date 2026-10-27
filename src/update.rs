//! Release artifacts, version checks and explicit self-update (M3).
//!
//! Every release publishes, per target:
//!
//! * `reason-network-<version>-<target>.zip` (Windows) or `.tar.gz` (others):
//!   the executable plus README, INSTALL and VERSION files;
//! * `reason-network-<version>-<target>[.exe]`: the bare executable, used by
//!   `reason-network update`;
//!
//! and one `SHA256SUMS` file covering all of them (`sha256sum` format).
//!
//! A release source is either an HTTP base with the GitHub Releases layout
//! (`<base>/download/v<version>/<asset>`, `<base>/latest/download/<asset>`)
//! or a local directory holding `SHA256SUMS` and the assets. The local form
//! lets the whole update path be exercised before anything is published.
//!
//! Updating is always explicit. The new executable is downloaded next to the
//! current one, verified against `SHA256SUMS`, smoke-tested, and only then
//! swapped in with two renames; the previous executable is kept as
//! `<exe>.old` and restored if the swap fails. Configuration and the data
//! directory are never touched.

use std::cmp::Ordering;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::error::{Error, Result};
use crate::util;

/// Name of the checksum file in every release.
pub const SUMS_FILE: &str = "SHA256SUMS";
/// Executable and package name prefix.
pub const PRODUCT: &str = "reason-network";
/// Environment variable naming an explicit release source (URL or directory).
pub const ENV_RELEASE_URL: &str = "REASON_NETWORK_RELEASE_URL";
/// Environment variable naming the GitHub repository (`owner/name`).
pub const ENV_REPO: &str = "REASON_NETWORK_REPO";

/// The version of this build.
pub fn current_version() -> Version {
    Version::parse(env!("CARGO_PKG_VERSION")).expect("the crate version is valid")
}

// ---------------------------------------------------------------------------
// Versions
// ---------------------------------------------------------------------------

/// A `MAJOR.MINOR.PATCH[-PRERELEASE]` version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    /// Major, minor, patch.
    pub numbers: [u64; 3],
    /// Pre-release label, e.g. `rc.1`.
    pub pre: Option<String>,
}

impl Version {
    /// Parse `1.2.3`, `v1.2.3` or `1.2.3-rc.1`.
    pub fn parse(text: &str) -> Result<Self> {
        let text = text.trim();
        let text = text.strip_prefix('v').unwrap_or(text);
        let (core, pre) = match text.split_once('-') {
            Some((core, pre)) if !pre.is_empty() => (core, Some(pre.to_string())),
            Some(_) => return Err(invalid_version(text)),
            None => (text, None),
        };
        let parts: Vec<&str> = core.split('.').collect();
        if parts.len() != 3 {
            return Err(invalid_version(text));
        }
        let mut numbers = [0u64; 3];
        for (slot, part) in numbers.iter_mut().zip(&parts) {
            if part.is_empty() || !part.chars().all(|c| c.is_ascii_digit()) {
                return Err(invalid_version(text));
            }
            *slot = part.parse().map_err(|_| invalid_version(text))?;
        }
        Ok(Self { numbers, pre })
    }
}

fn invalid_version(text: &str) -> Error {
    Error::InvalidRequest(format!(
        "invalid version `{text}`: expected MAJOR.MINOR.PATCH, e.g. 0.2.0"
    ))
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let [major, minor, patch] = self.numbers;
        write!(f, "{major}.{minor}.{patch}")?;
        if let Some(pre) = &self.pre {
            write!(f, "-{pre}")?;
        }
        Ok(())
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        self.numbers
            .cmp(&other.numbers)
            .then_with(|| match (&self.pre, &other.pre) {
                (None, None) => Ordering::Equal,
                // A release sorts after its pre-releases.
                (None, Some(_)) => Ordering::Greater,
                (Some(_), None) => Ordering::Less,
                (Some(a), Some(b)) => a.cmp(b),
            })
    }
}

// ---------------------------------------------------------------------------
// Targets and artifact names
// ---------------------------------------------------------------------------

/// Release targets, as `(os, arch, rust target triple)`.
pub const TARGETS: &[(&str, &str, &str)] = &[
    ("windows", "x86_64", "x86_64-pc-windows-msvc"),
    ("linux", "x86_64", "x86_64-unknown-linux-gnu"),
    ("macos", "aarch64", "aarch64-apple-darwin"),
    ("macos", "x86_64", "x86_64-apple-darwin"),
];

/// The release target for an OS and architecture (`std::env::consts` names).
pub fn target_for(os: &str, arch: &str) -> Option<&'static str> {
    TARGETS
        .iter()
        .find(|(o, a, _)| *o == os && *a == arch)
        .map(|(_, _, target)| *target)
}

/// The release target of the running build.
pub fn current_target() -> Option<&'static str> {
    target_for(std::env::consts::OS, std::env::consts::ARCH)
}

fn is_windows_target(target: &str) -> bool {
    target.contains("windows")
}

/// `reason-network-<version>-<target>.zip|.tar.gz`.
pub fn package_name(version: &Version, target: &str) -> String {
    let extension = if is_windows_target(target) {
        "zip"
    } else {
        "tar.gz"
    };
    format!("{PRODUCT}-{version}-{target}.{extension}")
}

/// `reason-network-<version>-<target>[.exe]`, the bare executable.
pub fn binary_name(version: &Version, target: &str) -> String {
    let extension = if is_windows_target(target) {
        ".exe"
    } else {
        ""
    };
    format!("{PRODUCT}-{version}-{target}{extension}")
}

/// Split an asset name into `(version, target)`, if it follows the scheme.
pub fn parse_asset_name(name: &str) -> Option<(Version, String)> {
    let rest = name.strip_prefix(&format!("{PRODUCT}-"))?;
    for (_, _, target) in TARGETS {
        for suffix in [".zip", ".tar.gz", ".exe", ""] {
            let tail = format!("-{target}{suffix}");
            if let Some(version) = rest.strip_suffix(&tail)
                && let Ok(version) = Version::parse(version)
            {
                return Some((version, target.to_string()));
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Checksums
// ---------------------------------------------------------------------------

/// One line of `SHA256SUMS`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checksum {
    /// Lowercase hex SHA-256.
    pub sha256: String,
    /// Asset file name.
    pub name: String,
}

/// Parse `sha256sum` output: `<64 hex>  <name>` (or `<hex> *<name>`).
pub fn parse_sums(text: &str) -> Result<Vec<Checksum>> {
    let mut sums = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let malformed = || {
            Error::InvalidRequest(format!(
                "{SUMS_FILE} line {} is malformed: `{line}`",
                index + 1
            ))
        };
        let (hash, name) = line.split_once(char::is_whitespace).ok_or_else(malformed)?;
        let name = name.trim_start().trim_start_matches('*').trim();
        if hash.len() != 64 || !hash.chars().all(|c| c.is_ascii_hexdigit()) || name.is_empty() {
            return Err(malformed());
        }
        if name.contains(['/', '\\']) || name.contains("..") {
            return Err(Error::InvalidRequest(format!(
                "{SUMS_FILE} names an unsafe path: `{name}`"
            )));
        }
        sums.push(Checksum {
            sha256: hash.to_ascii_lowercase(),
            name: name.to_string(),
        });
    }
    if sums.is_empty() {
        return Err(Error::InvalidRequest(format!("{SUMS_FILE} is empty")));
    }
    Ok(sums)
}

/// Fail unless `bytes` hash to `expected`.
pub fn verify(bytes: &[u8], expected: &str, name: &str) -> Result<()> {
    let actual = util::sha256_hex(bytes);
    if actual.eq_ignore_ascii_case(expected) {
        Ok(())
    } else {
        Err(Error::Other(format!(
            "checksum mismatch for {name}: expected {expected}, got {actual}; the download is corrupt or was tampered with, nothing was installed"
        )))
    }
}

/// The newest version listed in `SHA256SUMS`, or the requested one.
pub fn select_version(sums: &[Checksum], wanted: Option<&Version>) -> Result<Version> {
    let mut versions: Vec<Version> = sums
        .iter()
        .filter_map(|sum| parse_asset_name(&sum.name).map(|(version, _)| version))
        .collect();
    versions.sort();
    versions.dedup();
    match wanted {
        Some(wanted) => versions
            .into_iter()
            .find(|version| version == wanted)
            .ok_or_else(|| {
                Error::NotFound(format!("version {wanted} is not listed in {SUMS_FILE}"))
            }),
        None => versions
            .pop()
            .ok_or_else(|| Error::NotFound(format!("{SUMS_FILE} lists no release assets"))),
    }
}

// ---------------------------------------------------------------------------
// Release sources
// ---------------------------------------------------------------------------

/// Where releases come from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// An HTTP(S) base with the GitHub Releases layout.
    Http(String),
    /// A local directory with `SHA256SUMS` and the assets.
    Dir(PathBuf),
}

impl Source {
    /// Parse `--source`: a URL, `owner/name` for GitHub, or a directory.
    pub fn parse(text: &str) -> Result<Self> {
        let text = text.trim();
        if text.starts_with("https://") || text.starts_with("http://") {
            return Ok(Source::Http(text.trim_end_matches('/').to_string()));
        }
        let path = PathBuf::from(text);
        if path.is_dir() {
            return Ok(Source::Dir(path));
        }
        let parts: Vec<&str> = text.split('/').collect();
        if parts.len() == 2 && parts.iter().all(|part| is_repo_part(part)) {
            return Ok(Source::Http(format!("https://github.com/{text}/releases")));
        }
        Err(Error::InvalidRequest(format!(
            "release source `{text}` is neither a URL, an existing directory, nor a GitHub `owner/name`"
        )))
    }

    /// The configured default: `REASON_NETWORK_RELEASE_URL`, then
    /// `REASON_NETWORK_REPO` (runtime, then build time).
    pub fn from_env() -> Result<Self> {
        if let Ok(value) = std::env::var(ENV_RELEASE_URL)
            && !value.trim().is_empty()
        {
            return Self::parse(&value);
        }
        let repo = std::env::var(ENV_REPO)
            .ok()
            .filter(|value| !value.trim().is_empty())
            .or_else(|| option_env!("REASON_NETWORK_REPO").map(str::to_string))
            .filter(|value| !value.trim().is_empty());
        match repo {
            Some(repo) => Self::parse(&repo),
            None => Err(Error::Other(format!(
                "no release source is configured for this build: pass --source <owner/name|URL|directory>, or set {ENV_REPO}=owner/name"
            ))),
        }
    }

    /// Human readable form.
    pub fn display(&self) -> String {
        match self {
            Source::Http(base) => base.clone(),
            Source::Dir(dir) => dir.display().to_string(),
        }
    }

    fn sums_location(&self, version: Option<&Version>) -> String {
        match (self, version) {
            (Source::Http(base), None) => format!("{base}/latest/download/{SUMS_FILE}"),
            (Source::Http(base), Some(version)) => {
                format!("{base}/download/v{version}/{SUMS_FILE}")
            }
            (Source::Dir(dir), _) => dir.join(SUMS_FILE).display().to_string(),
        }
    }

    fn asset_location(&self, version: &Version, name: &str) -> String {
        match self {
            Source::Http(base) => format!("{base}/download/v{version}/{name}"),
            Source::Dir(dir) => dir.join(name).display().to_string(),
        }
    }

    async fn fetch(&self, location: &str) -> Result<Vec<u8>> {
        match self {
            Source::Dir(_) => std::fs::read(location)
                .map_err(|error| Error::Other(format!("cannot read {location}: {error}"))),
            Source::Http(_) => http_get(location).await,
        }
    }
}

fn is_repo_part(part: &str) -> bool {
    !part.is_empty()
        && part
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
}

fn http_client() -> Result<reqwest::Client> {
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let provider = std::sync::Arc::new(rustls::crypto::ring::default_provider());
    let mut tls = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|error| Error::Other(format!("TLS setup failed: {error}")))?
        .with_root_certificates(roots)
        .with_no_client_auth();
    tls.alpn_protocols = vec![b"http/1.1".to_vec()];
    reqwest::Client::builder()
        .tls_backend_preconfigured(tls)
        .user_agent(format!("{PRODUCT}/{}", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(20))
        .timeout(Duration::from_secs(300))
        .build()
        .map_err(|error| Error::Other(format!("cannot create the HTTP client: {error}")))
}

async fn http_get(url: &str) -> Result<Vec<u8>> {
    let response = http_client()?
        .get(url)
        .send()
        .await
        .map_err(|error| Error::Other(format!("cannot download {url}: {error}")))?;
    let status = response.status();
    if !status.is_success() {
        let hint = if status.as_u16() == 404 {
            " (no published release, or the version/asset does not exist)"
        } else {
            ""
        };
        return Err(Error::Other(format!(
            "cannot download {url}: HTTP {status}{hint}"
        )));
    }
    response
        .bytes()
        .await
        .map(|bytes| bytes.to_vec())
        .map_err(|error| Error::Other(format!("cannot download {url}: {error}")))
}

// ---------------------------------------------------------------------------
// Check and update
// ---------------------------------------------------------------------------

/// What a release source offers for this platform.
#[derive(Debug, Clone, serde::Serialize)]
pub struct CheckResult {
    /// This build's version.
    pub current: String,
    /// The selected release version.
    pub available: String,
    /// Whether `available` is newer than `current`.
    pub update_available: bool,
    /// This platform's target triple.
    pub target: String,
    /// The bare executable asset for this platform.
    pub asset: String,
    /// Its published SHA-256.
    pub sha256: String,
    /// Where the release was found.
    pub source: String,
}

/// Look up the release for this platform without downloading the binary.
pub async fn check(source: &Source, wanted: Option<&Version>, target: &str) -> Result<CheckResult> {
    let location = source.sums_location(wanted);
    let sums = parse_sums(&String::from_utf8_lossy(&source.fetch(&location).await?))?;
    let version = select_version(&sums, wanted)?;
    let asset = binary_name(&version, target);
    let sum = sums.iter().find(|sum| sum.name == asset).ok_or_else(|| {
        Error::NotFound(format!(
            "release {version} has no executable for {target} (`{asset}` is not in {SUMS_FILE})"
        ))
    })?;
    let current = current_version();
    Ok(CheckResult {
        current: current.to_string(),
        available: version.to_string(),
        update_available: version > current,
        target: target.to_string(),
        asset,
        sha256: sum.sha256.clone(),
        source: source.display(),
    })
}

/// The result of an update.
#[derive(Debug, Clone, serde::Serialize)]
pub struct UpdateResult {
    /// Version before the update.
    pub from: String,
    /// Version installed.
    pub to: String,
    /// The executable that was replaced.
    pub executable: String,
    /// The previous executable, kept for rollback.
    pub previous: Option<String>,
    /// False when the installation was already current.
    pub changed: bool,
}

/// Download, verify and install the selected release over `exe`.
///
/// Without `wanted`, nothing happens unless a newer version exists. With
/// `wanted`, that exact version is installed (allowing a downgrade).
pub async fn update(
    source: &Source,
    wanted: Option<&Version>,
    target: &str,
    exe: &Path,
) -> Result<UpdateResult> {
    let found = check(source, wanted, target).await?;
    if wanted.is_none() && !found.update_available {
        return Ok(UpdateResult {
            from: found.current.clone(),
            to: found.current,
            executable: exe.display().to_string(),
            previous: None,
            changed: false,
        });
    }
    let version = Version::parse(&found.available)?;
    let bytes = source
        .fetch(&source.asset_location(&version, &found.asset))
        .await?;
    verify(&bytes, &found.sha256, &found.asset)?;
    let previous = install_executable(exe, &bytes)?;
    Ok(UpdateResult {
        from: found.current,
        to: found.available,
        executable: exe.display().to_string(),
        previous: Some(previous.display().to_string()),
        changed: true,
    })
}

/// Replace `exe` with `bytes`, keeping the old executable as `<exe>.old`.
///
/// The new file is written and smoke-tested next to the target first, so a
/// failure at any step leaves the current executable in place. Renaming a
/// running executable is allowed on every supported OS, so this works while
/// the daemon is running; the daemon keeps the old code until restarted.
pub fn install_executable(exe: &Path, bytes: &[u8]) -> Result<PathBuf> {
    let dir = exe
        .parent()
        .ok_or_else(|| Error::Other(format!("{} has no parent directory", exe.display())))?;
    let file_name = exe
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| PRODUCT.to_string());
    // Keep the real extension last so Windows will execute the staged file.
    let (stem, extension) = match file_name.rsplit_once('.') {
        Some((stem, extension)) if !stem.is_empty() => (stem, format!(".{extension}")),
        _ => (file_name.as_str(), String::new()),
    };
    let staged = dir.join(format!(".{stem}.new-{}{extension}", std::process::id()));
    std::fs::write(&staged, bytes).map_err(|error| {
        Error::Other(format!(
            "cannot write {} ({error}); is the directory writable?",
            staged.display()
        ))
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))?;
    }
    if let Err(error) = smoke_test(&staged) {
        let _ = std::fs::remove_file(&staged);
        return Err(error);
    }

    let mut previous = exe.with_file_name(format!("{file_name}.old"));
    if previous.exists() && std::fs::remove_file(&previous).is_err() {
        // A still-running older executable may hold `.old` open on Windows.
        previous = exe.with_file_name(format!("{file_name}.old-{}", std::process::id()));
    }
    let had_current = exe.exists();
    if had_current && let Err(error) = std::fs::rename(exe, &previous) {
        let _ = std::fs::remove_file(&staged);
        return Err(Error::Other(format!(
            "cannot move {} aside ({error}); nothing was changed",
            exe.display()
        )));
    }
    if let Err(error) = std::fs::rename(&staged, exe) {
        if had_current {
            let _ = std::fs::rename(&previous, exe);
        }
        let _ = std::fs::remove_file(&staged);
        return Err(Error::Other(format!(
            "cannot install the new executable ({error}); the previous one was restored"
        )));
    }
    Ok(previous)
}

/// Run `<exe> --version` and require it to identify as reason-network.
fn smoke_test(exe: &Path) -> Result<String> {
    let output = std::process::Command::new(exe)
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|error| {
            Error::Other(format!(
                "the downloaded executable does not run on this machine ({error}); nothing was installed"
            ))
        })?;
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if output.status.success() && text.starts_with(PRODUCT) {
        Ok(text)
    } else {
        Err(Error::Other(format!(
            "the downloaded executable failed its self-check (`--version` printed `{text}`); nothing was installed"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(text: &str) -> Version {
        Version::parse(text).unwrap()
    }

    #[test]
    fn versions_parse_and_order() {
        assert_eq!(v("v1.2.3"), v("1.2.3"));
        assert_eq!(v("0.10.0").to_string(), "0.10.0");
        assert!(v("0.10.0") > v("0.9.9"));
        assert!(v("1.0.0") > v("1.0.0-rc.1"));
        assert!(v("1.0.0-rc.2") > v("1.0.0-rc.1"));
        for bad in ["", "1.2", "1.2.3.4", "a.b.c", "1.2.x", "1.2.3-", "-1.2.3"] {
            assert!(Version::parse(bad).is_err(), "{bad}");
        }
        assert_eq!(current_version().to_string(), env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn platforms_map_to_release_targets() {
        assert_eq!(
            target_for("windows", "x86_64"),
            Some("x86_64-pc-windows-msvc")
        );
        assert_eq!(
            target_for("linux", "x86_64"),
            Some("x86_64-unknown-linux-gnu")
        );
        assert_eq!(target_for("macos", "aarch64"), Some("aarch64-apple-darwin"));
        assert_eq!(target_for("macos", "x86_64"), Some("x86_64-apple-darwin"));
        assert_eq!(target_for("linux", "aarch64"), None);
        assert_eq!(target_for("freebsd", "x86_64"), None);
        assert!(
            current_target().is_some(),
            "the CI platforms are release targets"
        );
    }

    #[test]
    fn artifact_names_round_trip() {
        let version = v("0.2.0");
        assert_eq!(
            package_name(&version, "x86_64-pc-windows-msvc"),
            "reason-network-0.2.0-x86_64-pc-windows-msvc.zip"
        );
        assert_eq!(
            package_name(&version, "aarch64-apple-darwin"),
            "reason-network-0.2.0-aarch64-apple-darwin.tar.gz"
        );
        assert_eq!(
            binary_name(&version, "x86_64-pc-windows-msvc"),
            "reason-network-0.2.0-x86_64-pc-windows-msvc.exe"
        );
        assert_eq!(
            binary_name(&version, "x86_64-unknown-linux-gnu"),
            "reason-network-0.2.0-x86_64-unknown-linux-gnu"
        );
        for (_, _, target) in TARGETS {
            for name in [
                package_name(&version, target),
                binary_name(&version, target),
            ] {
                assert_eq!(
                    parse_asset_name(&name),
                    Some((version.clone(), target.to_string())),
                    "{name}"
                );
            }
        }
        assert_eq!(parse_asset_name("SHA256SUMS"), None);
        assert_eq!(
            parse_asset_name("reason-network-x-aarch64-apple-darwin"),
            None
        );
    }

    #[test]
    fn sums_parse_and_reject_garbage() {
        let hash = "a".repeat(64);
        let sums = parse_sums(&format!(
            "{hash}  reason-network-0.1.0-x86_64-apple-darwin\n\n{hash} *reason-network-0.2.0-x86_64-apple-darwin.tar.gz\n"
        ))
        .unwrap();
        assert_eq!(sums.len(), 2);
        assert_eq!(
            sums[1].name,
            "reason-network-0.2.0-x86_64-apple-darwin.tar.gz"
        );
        assert_eq!(select_version(&sums, None).unwrap(), v("0.2.0"));
        assert_eq!(
            select_version(&sums, Some(&v("0.1.0"))).unwrap(),
            v("0.1.0")
        );
        assert!(select_version(&sums, Some(&v("9.9.9"))).is_err());

        assert!(parse_sums("").is_err());
        assert!(parse_sums("abc  file").is_err());
        assert!(parse_sums(&format!("{hash}  ../evil")).is_err());
        assert!(parse_sums(&format!("{hash}  dir/evil")).is_err());
    }

    #[test]
    fn verification_rejects_modified_bytes() {
        let expected = util::sha256_hex(b"release");
        verify(b"release", &expected, "asset").unwrap();
        let error = verify(b"tampered", &expected, "asset").unwrap_err();
        assert!(error.to_string().contains("checksum mismatch"), "{error}");
    }

    #[test]
    fn sources_parse() {
        assert_eq!(
            Source::parse("acme/reason-network").unwrap(),
            Source::Http("https://github.com/acme/reason-network/releases".into())
        );
        assert_eq!(
            Source::parse("https://example.com/rel/").unwrap(),
            Source::Http("https://example.com/rel".into())
        );
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            Source::parse(&dir.path().display().to_string()).unwrap(),
            Source::Dir(dir.path().to_path_buf())
        );
        assert!(Source::parse("not a source").is_err());
        let http = Source::parse("acme/rn").unwrap();
        assert_eq!(
            http.sums_location(None),
            "https://github.com/acme/rn/releases/latest/download/SHA256SUMS"
        );
        assert_eq!(
            http.asset_location(&v("0.2.0"), "x"),
            "https://github.com/acme/rn/releases/download/v0.2.0/x"
        );
    }
}
