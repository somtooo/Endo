//! Finds the shared OpenCode service, or starts one, and waits until it's
//! ready. The service is shared with OpenCode's TUI and desktop app, so a
//! running one is never replaced.

use crate::client::OpenCodeClient;
use crate::install;
use crate::types::ServerInfo;
use anyhow::{Context as _, Result, bail};
use futures::FutureExt as _;
use http_client::HttpClient;
use serde::Deserialize;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

const START_TIMEOUT: Duration = Duration::from_secs(15);
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);
const POLL_INTERVAL: Duration = Duration::from_millis(250);
const MINIMUM_SERVICE_VERSION: u64 = 2;

#[derive(Debug)]
pub struct NotInstalled;

impl std::fmt::Display for NotInstalled {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("OpenCode isn't installed.")
    }
}

impl std::error::Error for NotInstalled {}

/// Fails with [`NotInstalled`] when no service is running and there's no
/// OpenCode to start one with.
pub async fn connect(
    http: Arc<dyn HttpClient>,
    executor: gpui::BackgroundExecutor,
) -> Result<OpenCodeClient> {
    let mut started: Option<StartedService> = None;
    let mut waiting_since = Instant::now();
    loop {
        let registration = match read_registration() {
            Ok(registration) => registration,
            // OpenCode replaces the file with a rename, and on Windows reading
            // it during the rename can fail, so for a service just started a
            // failed read means "not yet".
            Err(error) if started.is_some() => {
                log::debug!("OpenCode service registration not readable yet: {error:#}");
                None
            }
            Err(error) => return Err(error),
        };
        let registration = registration.filter(|registration| {
            started
                .as_ref()
                .is_none_or(|started| started.previous_id.as_ref() != Some(&registration.id))
        });
        let readiness = match &registration {
            Some(registration) => {
                let client = OpenCodeClient::new(http.clone(), registration)?;
                match check_readiness(&client, registration, &executor).await? {
                    Readiness::Ready => return Ok(client),
                    readiness => readiness,
                }
            }
            None => Readiness::Absent,
        };
        if readiness == Readiness::Absent && started.is_none() {
            started = Some(start_service(
                registration.map(|registration| registration.id),
            )?);
            waiting_since = Instant::now();
        }
        if waiting_since.elapsed() > START_TIMEOUT {
            return Err(match &started {
                Some(started) => started.timeout_error(),
                None => anyhow::anyhow!("the OpenCode service is still starting after 15 seconds"),
            });
        }
        executor.timer(POLL_INTERVAL).await;
    }
}

#[derive(PartialEq)]
enum Readiness {
    Ready,
    Starting,
    /// Nothing answers for the registration, so a service may be started.
    Absent,
}

async fn check_readiness(
    client: &OpenCodeClient,
    registration: &ServiceRegistration,
    executor: &gpui::BackgroundExecutor,
) -> Result<Readiness> {
    let health = futures::select_biased! {
        health = fetch_health(client, registration).fuse() => match health {
            Ok(health) => Some(health),
            // Something that speaks our protocol answered, so starting
            // another service next to it would only make things worse.
            Err(error) if error.downcast_ref::<serde_json::Error>().is_some() => {
                return Err(error);
            }
            Err(_) => None,
        },
        _ = executor.timer(PROBE_TIMEOUT).fuse() => None,
    };
    match health {
        Some(ServiceHealth::Ready) => Ok(Readiness::Ready),
        Some(ServiceHealth::Starting) => Ok(Readiness::Starting),
        Some(ServiceHealth::NotRegistered) | None => Ok(Readiness::Absent),
        Some(ServiceHealth::Failed) => bail!(
            "the OpenCode service failed to start. Check its logs, then run \
             `opencode service restart`."
        ),
        Some(ServiceHealth::TooOld(version)) => bail!(
            "OpenCode {version} is too old for OpenCode D, which needs OpenCode \
             {MINIMUM_SERVICE_VERSION}.0 or newer. Update OpenCode, then run \
             `opencode service restart`."
        ),
        Some(ServiceHealth::Incompatible) => bail!(
            "the running OpenCode service is a version OpenCode D can't talk to. \
             Update OpenCode, then run `opencode service restart`."
        ),
    }
}

/// What the registered service says about itself, checked the way OpenCode's
/// own clients check it (`client/src/service-probe.ts`).
#[derive(Clone, Debug, PartialEq)]
enum ServiceHealth {
    Ready,
    Starting,
    Failed,
    /// It has no `/api/info`, so it speaks a protocol this client doesn't.
    Incompatible,
    TooOld(String),
    /// Something else answered: the registration is stale.
    NotRegistered,
}

async fn fetch_health(
    client: &OpenCodeClient,
    registration: &ServiceRegistration,
) -> Result<ServiceHealth> {
    let response = client.fetch_service_info().await?;
    Ok(service_health(
        response.status,
        response.info.as_ref(),
        registration,
    ))
}

fn service_health(
    status: u16,
    info: Option<&ServerInfo>,
    registration: &ServiceRegistration,
) -> ServiceHealth {
    if status == 404 {
        return ServiceHealth::Incompatible;
    }
    // A 401 means the password is wrong, so it's another service.
    let Some(info) = info.filter(|_| status != 401) else {
        return ServiceHealth::NotRegistered;
    };
    if info.pid != Some(registration.pid) || info.version != registration.version {
        return ServiceHealth::NotRegistered;
    }
    if is_too_old(&info.version) {
        return ServiceHealth::TooOld(info.version.clone());
    }
    match status {
        200..=299 => ServiceHealth::Ready,
        500 => ServiceHealth::Failed,
        _ => ServiceHealth::Starting,
    }
}

/// Builds from source report `local` rather than a number and are let through.
fn is_too_old(version: &str) -> bool {
    let major = version.trim_start_matches('v').split(['.', '-']).next();
    major
        .and_then(|major| major.parse::<u64>().ok())
        .is_some_and(|major| major < MINIMUM_SERVICE_VERSION)
}

struct StartedService {
    /// The registration the new service replaces, until it writes its own.
    previous_id: Option<String>,
    output_path: PathBuf,
}

impl StartedService {
    fn timeout_error(&self) -> anyhow::Error {
        let output = std::fs::read_to_string(&self.output_path).unwrap_or_default();
        let tail: Vec<&str> = output.lines().rev().take(10).collect();
        if tail.is_empty() {
            return anyhow::anyhow!("the OpenCode service didn't start within 15 seconds");
        }
        let tail: Vec<&str> = tail.into_iter().rev().collect();
        anyhow::anyhow!(
            "the OpenCode service didn't start within 15 seconds. Its last output ({}):\n{}",
            self.output_path.display(),
            tail.join("\n")
        )
    }
}

fn start_service(previous_id: Option<String>) -> Result<StartedService> {
    let binary = install::find_binary().ok_or(NotInstalled)?;
    log::info!("starting the OpenCode service with {}", binary.display());
    // The service outlives Zed, so its output goes to a file rather than a
    // pipe. It explains a service that doesn't start.
    let output_path = paths::logs_dir().join("opencode-service.log");
    let output = std::fs::File::create(&output_path)
        .with_context(|| format!("creating {}", output_path.display()))?;
    util::command::new_command(&binary)
        .args(["serve", "--service"])
        .stdin(std::process::Stdio::null())
        .stdout(output.try_clone()?)
        .stderr(output)
        .spawn()
        .with_context(|| format!("starting {}", binary.display()))?;
    Ok(StartedService {
        previous_id,
        output_path,
    })
}

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

pub fn read_registration() -> Result<Option<ServiceRegistration>> {
    // OpenCode's troubleshooting docs give this one path on every platform, so
    // it intentionally differs from Zed's own `paths::state_dir()`. A set
    // `XDG_STATE_HOME` replaces `~/.local/state`, as OpenCode's own client does.
    let path = std::env::var_os("XDG_STATE_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| util::paths::home_dir().join(".local").join("state"))
        .join("opencode")
        .join("service.json");
    match std::fs::read_to_string(&path) {
        Ok(contents) => serde_json::from_str(&contents)
            .map(Some)
            .context("parsing OpenCode service registration"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_registration_without_exposing_password_in_debug() {
        let registration: ServiceRegistration = serde_json::from_str(
            r#"{"id":"a","version":"2.2.5","url":"http://127.0.0.1:49374","pid":42,"password":"secret"}"#,
        )
        .expect("valid registration");
        assert_eq!(registration.url, "http://127.0.0.1:49374");
        assert_eq!(registration.password, "secret");
        assert!(!format!("{registration:?}").contains("secret"));
    }

    #[test]
    fn service_health_matches_opencodes_own_check() {
        let info = |pid: u32, version: &str| ServerInfo {
            version: version.to_string(),
            pid: Some(pid),
        };
        let registration = |version: &str| ServiceRegistration {
            id: "a".into(),
            version: version.into(),
            url: "http://127.0.0.1:1".into(),
            pid: 42,
            password: String::new(),
        };
        let ours = info(42, "2.2.5");
        let registered = registration("2.2.5");
        let health = |status, info: Option<&ServerInfo>| service_health(status, info, &registered);
        assert_eq!(health(200, Some(&ours)), ServiceHealth::Ready);
        assert_eq!(health(503, Some(&ours)), ServiceHealth::Starting);
        assert_eq!(health(500, Some(&ours)), ServiceHealth::Failed);
        assert_eq!(health(404, None), ServiceHealth::Incompatible);
        assert_eq!(health(401, None), ServiceHealth::NotRegistered);
        assert_eq!(health(200, None), ServiceHealth::NotRegistered);
        // Another process on the registered port, or a newer service that
        // hasn't rewritten the file yet.
        assert_eq!(
            health(200, Some(&info(7, "2.2.5"))),
            ServiceHealth::NotRegistered
        );
        assert_eq!(
            health(200, Some(&info(42, "2.3.0"))),
            ServiceHealth::NotRegistered
        );
        assert_eq!(
            service_health(200, Some(&info(42, "1.14.2")), &registration("1.14.2")),
            ServiceHealth::TooOld("1.14.2".to_string())
        );
    }

    #[test]
    fn versions_below_two_are_too_old() {
        assert!(is_too_old("1.14.2"));
        assert!(is_too_old("0.0.0-dev-202601"));
        assert!(!is_too_old("2.0.0"));
        assert!(!is_too_old("2.0.0-beta.1"));
        assert!(!is_too_old("v2.2.5"));
        assert!(!is_too_old("10.1.0"));
        assert!(!is_too_old("local"));
    }
}
