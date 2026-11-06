use anyhow::{Context as _, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};

/// Contents of the registration file the shared OpenCode service writes when
/// it starts with `opencode serve --service`.
#[derive(Clone, Deserialize)]
pub struct ServiceRegistration {
    pub id: String,
    pub version: String,
    pub url: String,
    pub pid: u32,
    pub password: String,
}

impl std::fmt::Debug for ServiceRegistration {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ServiceRegistration")
            .field("id", &self.id)
            .field("version", &self.version)
            .field("url", &self.url)
            .field("pid", &self.pid)
            .finish_non_exhaustive()
    }
}

/// OpenCode resolves this path the same way on every platform
/// (`client/src/service-probe.ts`), so it intentionally differs from Zed's own
/// `paths::state_dir()`.
pub fn registration_path() -> PathBuf {
    let state_home = std::env::var_os("XDG_STATE_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| util::paths::home_dir().join(".local").join("state"));
    state_home.join("opencode").join("service.json")
}

pub fn parse_registration(contents: &str) -> Result<ServiceRegistration> {
    serde_json::from_str(contents).context("parsing OpenCode service registration")
}

pub fn read_registration(path: &Path) -> Result<Option<ServiceRegistration>> {
    match std::fs::read_to_string(path) {
        Ok(contents) => parse_registration(&contents).map(Some),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
    }
}

/// The system OpenCode, never Zed's ACP registry download: the service is shared
/// with the TUI and desktop app, so it should run from the install they use.
pub fn find_system_binary() -> Option<PathBuf> {
    if let Ok(path) = which::which("opencode") {
        return Some(path);
    }
    let installer_path = util::paths::home_dir()
        .join(".opencode")
        .join("bin")
        .join(if cfg!(windows) {
            "opencode.exe"
        } else {
            "opencode"
        });
    installer_path.is_file().then_some(installer_path)
}

/// Arguments that start the shared background service. The service outlives
/// Zed by design, so callers must not kill it.
pub const SERVICE_ARGS: [&str; 2] = ["serve", "--service"];

#[derive(Debug)]
pub struct NotInstalled;

impl std::fmt::Display for NotInstalled {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(
            "OpenCode isn't installed. Install it with `curl -fsSL https://opencode.ai/install | bash`, then try again.",
        )
    }
}

impl std::error::Error for NotInstalled {}

/// Starts the shared service with the system `opencode` and waits until it
/// registers. Never replaces a running service: OpenCode's own clients may be
/// using it.
pub async fn start_service(executor: gpui::BackgroundExecutor) -> Result<ServiceRegistration> {
    let binary = find_system_binary().ok_or(NotInstalled)?;
    let path = registration_path();
    let previous = read_registration(&path).ok().flatten().map(|registration| registration.id);
    log::info!("starting the OpenCode service with {}", binary.display());
    smol::process::Command::new(&binary)
        .args(SERVICE_ARGS)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .with_context(|| format!("starting {}", binary.display()))?;
    for _ in 0..60 {
        executor.timer(std::time::Duration::from_millis(250)).await;
        if let Some(registration) = read_registration(&path)?
            && Some(&registration.id) != previous.as_ref()
        {
            return Ok(registration);
        }
    }
    anyhow::bail!("the OpenCode service didn't start within 15 seconds")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_registration_without_exposing_password_in_debug() {
        let registration = parse_registration(
            r#"{"id":"a","version":"2.2.5","url":"http://127.0.0.1:49374","pid":42,"password":"secret"}"#,
        )
        .expect("valid registration");
        assert_eq!(registration.url, "http://127.0.0.1:49374");
        assert_eq!(registration.password, "secret");
        assert!(!format!("{registration:?}").contains("secret"));
    }
}
