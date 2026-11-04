//! Installs OpenCode the way its official installer (`install`, served at
//! `https://opencode.ai/v2/install`) does: the latest CLI package for this
//! platform from npm, unpacked into `~/.opencode/bin`. The package is checked
//! against the SHA-512 integrity hash npm publishes for it.
//!
//! Unlike the script, this never edits shell profiles: Zed finds the binary
//! in `~/.opencode/bin` itself.

use crate::connection::{AGENT_ID, Hub};
use acp_thread::{AcpThread, AgentConnection, AuthRequired};
use agent_client_protocol::v2::schema as acp;
use anyhow::{Context as _, Result, anyhow, bail};
use async_compression::futures::bufread::GzipDecoder;
use async_tar::Archive;
use base64::Engine as _;
use futures::{AsyncReadExt as _, StreamExt as _, io::BufReader};
use http_client::{AsyncBody, HttpClient};
use serde::Deserialize;
use sha2::{Digest as _, Sha512};
use gpui::{App, AppContext as _, Entity, SharedString, Task};
use project::{AgentId, Project};
use std::any::Any;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use util::path_list::PathList;

const LATEST_URL: &str = "https://opencode.ai/update/api/latest/cli/npm";
const NPM_REGISTRY: &str = "https://registry.npmjs.org";

pub fn install_dir() -> PathBuf {
    util::paths::home_dir().join(".opencode").join("bin")
}

fn binary_name() -> &'static str {
    if cfg!(windows) { "opencode.exe" } else { "opencode" }
}

/// The npm package suffix for this machine, as the install script picks it.
fn target() -> Result<String> {
    let os = match std::env::consts::OS {
        "linux" => "linux",
        "macos" => "darwin",
        "windows" => "windows",
        other => bail!("OpenCode doesn't publish builds for {other}"),
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        other => bail!("OpenCode doesn't publish builds for {other}"),
    };
    let mut target = format!("{os}-{arch}");
    if arch == "x64" && !has_avx2() {
        target.push_str("-baseline");
    }
    if os == "linux" && is_musl() {
        target.push_str("-musl");
    }
    Ok(target)
}

#[cfg(target_arch = "x86_64")]
fn has_avx2() -> bool {
    std::arch::is_x86_feature_detected!("avx2")
}

#[cfg(not(target_arch = "x86_64"))]
fn has_avx2() -> bool {
    true
}

fn is_musl() -> bool {
    if std::path::Path::new("/etc/alpine-release").exists() {
        return true;
    }
    std::process::Command::new("ldd")
        .arg("--version")
        .output()
        .map(|output| {
            let text = [output.stdout, output.stderr].concat();
            String::from_utf8_lossy(&text).to_lowercase().contains("musl")
        })
        .unwrap_or(false)
}

#[derive(Deserialize)]
struct Latest {
    version: String,
    metadata: LatestMetadata,
}

#[derive(Deserialize)]
struct LatestMetadata {
    package: String,
}

#[derive(Deserialize)]
struct PackageVersion {
    dist: Dist,
}

#[derive(Deserialize)]
struct Dist {
    tarball: String,
    integrity: String,
}

async fn get_bytes(http: &Arc<dyn HttpClient>, url: &str) -> Result<Vec<u8>> {
    let mut response = http
        .get(url, AsyncBody::empty(), true)
        .await
        .with_context(|| format!("downloading {url}"))?;
    let mut body = Vec::new();
    response.body_mut().read_to_end(&mut body).await?;
    if !response.status().is_success() {
        bail!("downloading {url} failed with status {}", response.status());
    }
    Ok(body)
}

fn verify_integrity(bytes: &[u8], integrity: &str) -> Result<()> {
    let expected = integrity
        .split_whitespace()
        .find_map(|entry| entry.strip_prefix("sha512-"))
        .ok_or_else(|| anyhow!("npm didn't publish a SHA-512 hash for the package"))?;
    let expected = base64::engine::general_purpose::STANDARD
        .decode(expected)
        .context("decoding the package hash")?;
    if Sha512::digest(bytes).as_slice() != expected.as_slice() {
        bail!("the downloaded OpenCode package doesn't match its published hash");
    }
    Ok(())
}

/// Downloads and installs the latest OpenCode, returning the binary's path.
pub async fn install(http: Arc<dyn HttpClient>) -> Result<PathBuf> {
    let target = target()?;
    let latest: Latest = serde_json::from_slice(&get_bytes(&http, LATEST_URL).await?)
        .context("reading the latest OpenCode version")?;
    let scope = latest
        .metadata
        .package
        .strip_suffix("/cli")
        .ok_or_else(|| anyhow!("unexpected package name {}", latest.metadata.package))?;
    let package = format!("{scope}/cli-{target}");
    let metadata_url = format!(
        "{NPM_REGISTRY}/{}/{}",
        package.replace('/', "%2f"),
        latest.version
    );
    let metadata: PackageVersion = serde_json::from_slice(&get_bytes(&http, &metadata_url).await?)
        .with_context(|| format!("reading {package} {}", latest.version))?;

    log::info!("installing OpenCode {} ({package})", latest.version);
    let archive_bytes = get_bytes(&http, &metadata.dist.tarball).await?;
    verify_integrity(&archive_bytes, &metadata.dist.integrity)?;

    let wanted = PathBuf::from("package").join("bin").join(binary_name());
    let mut entries = Archive::new(GzipDecoder::new(BufReader::new(archive_bytes.as_slice())))
        .entries()
        .context("reading the OpenCode package")?;
    let mut binary = None;
    while let Some(entry) = entries.next().await {
        let mut entry = entry?;
        if entry.path()?.as_os_str() == wanted.as_os_str() {
            let mut bytes = Vec::new();
            entry.read_to_end(&mut bytes).await?;
            binary = Some(bytes);
            break;
        }
    }
    let binary = binary.ok_or_else(|| anyhow!("the OpenCode package has no {}", wanted.display()))?;

    let directory = install_dir();
    smol::fs::create_dir_all(&directory)
        .await
        .with_context(|| format!("creating {}", directory.display()))?;
    let destination = directory.join(binary_name());
    // Written beside the destination and renamed, so a failed write never
    // leaves a broken binary in place.
    let partial = directory.join(format!("{}.partial", binary_name()));
    smol::fs::write(&partial, &binary)
        .await
        .with_context(|| format!("writing {}", partial.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        smol::fs::set_permissions(&partial, std::fs::Permissions::from_mode(0o755)).await?;
    }
    smol::fs::rename(&partial, &destination)
        .await
        .with_context(|| format!("installing {}", destination.display()))?;
    Ok(destination)
}

const INSTALL_METHOD: &str = "install-opencode";

/// Stands in for OpenCode D while OpenCode isn't installed. Every session
/// asks to "authenticate", which Zed shows as a prompt with an Install
/// button; installing counts as authenticating, after which Zed reconnects
/// and gets the real connection.
pub(crate) struct InstallConnection {
    http: Arc<dyn HttpClient>,
    methods: Vec<acp::AuthMethod>,
}

impl InstallConnection {
    pub(crate) fn new(http: Arc<dyn HttpClient>) -> Self {
        Self {
            http,
            methods: vec![acp::AuthMethod::Agent(
                acp::AuthMethodAgent::new(INSTALL_METHOD, "Install OpenCode").description(
                    format!(
                        "Downloads the latest OpenCode to {} and starts its service",
                        install_dir().display()
                    ),
                ),
            )],
        }
    }

    fn prompt() -> anyhow::Error {
        anyhow!(AuthRequired::new().with_description(format!(
            "OpenCode D needs OpenCode, which isn't installed. Zed can install the latest \
             release to `{}`, the same place OpenCode's own installer uses.",
            install_dir().display()
        )))
    }
}

impl AgentConnection for InstallConnection {
    fn agent_id(&self) -> AgentId {
        AgentId::new(AGENT_ID)
    }

    fn telemetry_id(&self) -> SharedString {
        "opencode-direct".into()
    }

    fn new_session(
        self: Rc<Self>,
        _project: Entity<Project>,
        _work_dirs: PathList,
        _cx: &mut App,
    ) -> Task<Result<Entity<AcpThread>>> {
        Task::ready(Err(Self::prompt()))
    }

    fn supports_load_session(&self) -> bool {
        true
    }

    fn load_session(
        self: Rc<Self>,
        _session_id: acp::SessionId,
        _project: Entity<Project>,
        _work_dirs: PathList,
        _title: Option<SharedString>,
        _cx: &mut App,
    ) -> Task<Result<Entity<AcpThread>>> {
        Task::ready(Err(Self::prompt()))
    }

    fn auth_methods(&self) -> &[acp::AuthMethod] {
        &self.methods
    }

    fn authenticate(&self, _method: acp::AuthMethodId, cx: &mut App) -> Task<Result<()>> {
        let http = self.http.clone();
        cx.spawn(async move |cx| {
            let binary = cx.background_spawn(install(http.clone())).await?;
            log::info!("installed OpenCode at {}", binary.display());
            cx.update(|cx| Hub::get(http, cx)).await?;
            Ok(())
        })
    }

    fn cancel(&self, _session_id: &acp::SessionId, _cx: &mut App) -> Result<()> {
        Ok(())
    }

    fn into_any(self: Rc<Self>) -> Rc<dyn Any> {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checks_the_package_hash() {
        let bytes = b"opencode";
        let hash = base64::engine::general_purpose::STANDARD.encode(Sha512::digest(bytes));
        assert!(verify_integrity(bytes, &format!("sha512-{hash}")).is_ok());
        assert!(verify_integrity(b"tampered", &format!("sha512-{hash}")).is_err());
        assert!(verify_integrity(bytes, "sha1-abc").is_err());
    }
}
