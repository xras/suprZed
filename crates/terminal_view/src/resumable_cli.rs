use anyhow::{Context as _, Result, anyhow};
use collections::HashMap;
use futures::channel::{mpsc, oneshot};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::{
    fs,
    io::{Read as _, Write as _},
    net::{IpAddr, SocketAddr, TcpStream},
    path::{Path, PathBuf},
    process::Command,
    str::FromStr as _,
    sync::{
        Arc, LazyLock, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use uuid::Uuid;

use crate::{ResumableCliEvidence, ResumableCliProvider, ResumableCliSession};

pub(crate) const HOOK_HELPER_ARGUMENT: &str = "--resumable-cli-hook";
const CALLBACK_PATH: &str = "/resumable-cli/session-start";
const CALLBACK_ADDRESS_ENV: &str = "ZED_RESUMABLE_CLI_CALLBACK_ADDRESS";
const CALLBACK_CAPABILITY_ENV: &str = "ZED_RESUMABLE_CLI_CALLBACK_CAPABILITY";
const CAPTURE_PROVIDER_ENV: &str = "ZED_RESUMABLE_CLI_PROVIDER";
const LAUNCH_ATTEMPT_ENV: &str = "ZED_RESUMABLE_CLI_LAUNCH_ATTEMPT";
const MAX_PAYLOAD_BYTES: u64 = 16_384;
const MAX_CODEX_HOOK_INPUT_BYTES: u64 = 1_048_576;
const CALLBACK_POLL_INTERVAL: Duration = Duration::from_millis(500);
const CALLBACK_TIMEOUT: Duration = Duration::from_secs(30);
const HELPER_IO_TIMEOUT: Duration = Duration::from_secs(3);
const MINIMUM_CODEX_VERSION: (u64, u64, u64) = (0, 154, 0);
const MINIMUM_CODEX_VERSION_DISPLAY: &str = "0.154.0";
const MINIMUM_CLAUDE_VERSION: (u64, u64, u64) = (2, 1, 218);
const MINIMUM_CLAUDE_VERSION_DISPLAY: &str = "2.1.218";
const MINIMUM_COPILOT_VERSION: (u64, u64, u64) = (1, 0, 86);
const MINIMUM_COPILOT_VERSION_DISPLAY: &str = "1.0.86";
const MINIMUM_HERMES_VERSION: (u64, u64, u64) = (0, 21, 1);
const MINIMUM_HERMES_VERSION_DISPLAY: &str = "0.21.1";

static ACTIVE_TARGETS: LazyLock<Mutex<HashMap<String, Uuid>>> =
    LazyLock::new(|| Mutex::new(HashMap::default()));

pub(crate) struct SessionClaim {
    target: String,
    token: Uuid,
}

impl SessionClaim {
    pub(crate) fn owns(&self, session: &ResumableCliSession) -> bool {
        self.target == target_key(session)
    }
}

impl Drop for SessionClaim {
    fn drop(&mut self) {
        let mut targets = ACTIVE_TARGETS
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if targets.get(&self.target) == Some(&self.token) {
            targets.remove(&self.target);
        }
    }
}

pub(crate) struct CodexCapture {
    pub(crate) session: ResumableCliSession,
}

#[derive(Clone)]
pub(crate) struct CaptureCancellation(Arc<AtomicBool>);

impl CaptureCancellation {
    pub(crate) fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    pub(crate) fn is_same_attempt(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

#[cfg(test)]
pub(crate) struct CodexCaptureListener {
    pub(crate) environment: HashMap<String, String>,
    pub(crate) result: oneshot::Receiver<Result<CodexCapture>>,
    pub(crate) cancellation: CaptureCancellation,
}

pub(crate) struct CliIdentity {
    pub(crate) session: ResumableCliSession,
    pub(crate) is_prompt: bool,
    pub(crate) prompt_started_at: Option<u64>,
}

pub(crate) struct CliIdentityObserver {
    _plugin: Option<ClaudeSessionPlugin>,
    pub(crate) environment: HashMap<String, String>,
    pub(crate) events: mpsc::Receiver<Result<CliIdentity>>,
    pub(crate) cancellation: CaptureCancellation,
}

#[cfg(test)]
pub(crate) fn codex_identity_fixture() -> (CliIdentityObserver, mpsc::Sender<Result<CliIdentity>>) {
    let (sender, events) = mpsc::channel(8);
    (
        CliIdentityObserver {
            _plugin: None,
            environment: HashMap::default(),
            events,
            cancellation: CaptureCancellation(Arc::new(AtomicBool::new(false))),
        },
        sender,
    )
}

#[cfg(test)]
pub(crate) fn claude_identity_fixture() -> Result<(
    CliIdentityObserver,
    mpsc::Sender<Result<CliIdentity>>,
    PathBuf,
)> {
    let (mut observer, sender) = codex_identity_fixture();
    let plugin = create_claude_session_plugin()?;
    let path = plugin.path.clone();
    observer._plugin = Some(plugin);
    Ok((observer, sender, path))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, Deserialize)]
struct CliProcessIdentity {
    pid: u32,
    created_at: u64,
}

struct CliIdentityEnvelope {
    identity: CliIdentity,
    process: CliProcessIdentity,
    submission_id: Option<u128>,
}

#[derive(Deserialize)]
struct CliIdentityPayload {
    cwd: PathBuf,
    hook_event_name: String,
    session_id: String,
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    agent_id: Option<String>,
    #[serde(default)]
    agent_type: Option<String>,
    #[serde(default)]
    zed_hook_started_at: Option<u64>,
    #[serde(default)]
    zed_codex_process: Option<CliProcessIdentity>,
    #[serde(default)]
    zed_prompt_submission_id: Option<String>,
}

pub(crate) fn start_codex_identity_observer(launch_cwd: PathBuf) -> Result<CliIdentityObserver> {
    start_cli_identity_observer(launch_cwd, ResumableCliProvider::Codex)
}

fn start_cli_identity_observer(
    launch_cwd: PathBuf,
    provider: ResumableCliProvider,
) -> Result<CliIdentityObserver> {
    let server = tiny_http::Server::http("127.0.0.1:0").map_err(|error| {
        anyhow!(error).context(format!(
            "binding {} identity observer",
            provider.display_name()
        ))
    })?;
    let address = server
        .server_addr()
        .to_ip()
        .context("observer is not TCP")?;
    anyhow::ensure!(address.ip().is_loopback(), "observer is not loopback");
    let capability = Uuid::new_v4().to_string();
    let attempt = Uuid::new_v4().to_string();
    let environment = HashMap::from_iter([
        (CALLBACK_ADDRESS_ENV.to_owned(), address.to_string()),
        (CALLBACK_CAPABILITY_ENV.to_owned(), capability.clone()),
        (LAUNCH_ATTEMPT_ENV.to_owned(), attempt.clone()),
        (
            CAPTURE_PROVIDER_ENV.to_owned(),
            provider.as_str().to_owned(),
        ),
    ]);
    let cancellation = CaptureCancellation(Arc::new(AtomicBool::new(false)));
    let thread_cancellation = cancellation.clone();
    let (mut sender, events) = mpsc::channel(8);
    std::thread::spawn(move || {
        let mut captured = false;
        let mut last_prompt_started_at = None;
        let mut last_submission_id = None;
        let mut captured_process = None;
        while !thread_cancellation.is_cancelled() && !sender.is_closed() {
            let mut request = match server.recv_timeout(CALLBACK_POLL_INTERVAL) {
                Ok(Some(request)) => request,
                Ok(None) => continue,
                Err(error) => {
                    if let Err(delivery) = sender.try_send(Err(anyhow!(error)
                        .context(format!("receiving {} identity", provider.display_name()))))
                    {
                        log::error!("CLI observer failure could not be delivered: {delivery}");
                    }
                    return;
                }
            };
            if request.method() != &tiny_http::Method::Post || request.url() != CALLBACK_PATH {
                respond(request, 404);
                continue;
            }
            if !header_matches(&request, "X-Zed-Capability", &capability)
                || !header_matches(&request, "X-Zed-Launch-Attempt", &attempt)
            {
                log::debug!("Dropped CLI identity callback: launch authentication mismatch");
                respond(request, 401);
                continue;
            }
            let capture = (|| {
                let mut bytes = Vec::new();
                request
                    .as_reader()
                    .take(MAX_PAYLOAD_BYTES + 1)
                    .read_to_end(&mut bytes)?;
                anyhow::ensure!(
                    bytes.len() as u64 <= MAX_PAYLOAD_BYTES,
                    "CLI identity exceeds the size limit"
                );
                parse_cli_identity(
                    &bytes,
                    (!captured).then_some(launch_cwd.as_path()),
                    provider,
                )
            })();
            let mut process = None;
            let mut submission_id = None;
            let mut capture = match capture {
                Ok(Some(capture)) => {
                    let rejection =
                        if captured_process.is_some_and(|saved| saved != capture.process) {
                            Some("hook caller changed")
                        } else if capture.identity.is_prompt
                            && last_submission_id.is_some()
                            && capture.submission_id.is_none()
                        {
                            Some("ordered submission ID missing")
                        } else if capture
                            .submission_id
                            .zip(last_submission_id)
                            .is_some_and(|(submission, last)| submission < last)
                        {
                            Some("older submission ID")
                        } else {
                            None
                        };
                    if let Some(reason) = rejection {
                        log::debug!("Dropped CLI identity callback: {reason}");
                        respond(request, 204);
                        continue;
                    }
                    process = Some(capture.process);
                    submission_id = capture.submission_id;
                    Ok(capture.identity)
                }
                Ok(None) => {
                    log::debug!("Dropped CLI identity callback: nested agent");
                    respond(request, 204);
                    continue;
                }
                Err(error) => {
                    log::debug!("Rejected malformed CLI identity callback");
                    Err(error)
                }
            };
            if capture
                .as_ref()
                .is_ok_and(|event| event.is_prompt && event.prompt_started_at.is_none())
            {
                capture = Err(anyhow!("CLI prompt callback has no helper timestamp"));
            }
            let prompt_started_at = capture
                .as_ref()
                .ok()
                .and_then(|event| event.prompt_started_at);
            if submission_id.is_none()
                && prompt_started_at
                    .zip(last_prompt_started_at)
                    .is_some_and(|(started_at, last)| started_at < last)
            {
                log::debug!("Dropped CLI identity callback: older hook timestamp");
                respond(request, 204);
                continue;
            }
            let valid = capture.is_ok();
            match sender.try_send(capture) {
                Ok(()) => {
                    if let Some(started_at) = prompt_started_at {
                        last_prompt_started_at = Some(started_at);
                    }
                    if valid {
                        captured_process = process;
                        if let Some(submission) = submission_id {
                            last_submission_id = Some(submission);
                        }
                    }
                    captured |= valid;
                    respond(request, if valid { 204 } else { 400 });
                }
                Err(error) if error.is_full() => {
                    log::debug!("Deferred CLI identity callback: queue full");
                    respond(request, 429);
                }
                Err(_) => {
                    respond(request, 410);
                    return;
                }
            }
        }
    });
    Ok(CliIdentityObserver {
        _plugin: None,
        environment,
        events,
        cancellation,
    })
}

#[cfg(test)]
fn parse_codex_identity(
    bytes: &[u8],
    initial_cwd: Option<&Path>,
) -> Result<Option<CliIdentityEnvelope>> {
    parse_cli_identity(bytes, initial_cwd, ResumableCliProvider::Codex)
}

fn parse_cli_identity(
    bytes: &[u8],
    initial_cwd: Option<&Path>,
    provider: ResumableCliProvider,
) -> Result<Option<CliIdentityEnvelope>> {
    let payload: CliIdentityPayload = serde_json::from_slice(bytes).context(format!(
        "parsing {} identity callback",
        provider.display_name()
    ))?;
    if payload.agent_id.is_some() || payload.agent_type.is_some() {
        // Child agents inherit the callback environment but do not own this terminal.
        return Ok(None);
    }
    match payload.hook_event_name.as_str() {
        "SessionStart" => anyhow::ensure!(
            matches!(payload.source.as_deref(), Some("startup" | "resume"))
                || (provider == ResumableCliProvider::Claude
                    && matches!(
                        payload.source.as_deref(),
                        Some("clear" | "compact" | "fork")
                    )),
            "unexpected CLI start source"
        ),
        "UserPromptSubmit" => {}
        _ => anyhow::bail!("unexpected CLI identity event"),
    }
    if payload.source.as_deref() == Some("startup")
        && let Some(cwd) = initial_cwd
    {
        anyhow::ensure!(
            same_path(&payload.cwd, cwd),
            "initial CLI CWD does not match launch context"
        );
    }
    if payload.zed_codex_process.is_none() {
        log::debug!("Rejected CLI identity callback: hook caller metadata missing");
    }
    let process = payload.zed_codex_process.context(format!(
        "{} callback has no process identity",
        provider.display_name()
    ))?;
    anyhow::ensure!(
        process.pid > 0 && process.created_at > 0,
        "invalid CLI process identity"
    );
    let submission_id = payload
        .zed_prompt_submission_id
        .as_deref()
        .map(|id| -> Result<u128> {
            let id = Uuid::parse_str(id).context("invalid CLI submission ID")?;
            anyhow::ensure!(
                id.get_version_num() == 7,
                "CLI submission ID is not ordered"
            );
            Ok(id.as_u128())
        })
        .transpose()?;
    Ok(Some(CliIdentityEnvelope {
        process,
        submission_id,
        identity: CliIdentity {
            is_prompt: payload.hook_event_name == "UserPromptSubmit",
            prompt_started_at: if payload.hook_event_name == "UserPromptSubmit"
                || provider == ResumableCliProvider::Claude
            {
                payload.zed_hook_started_at
            } else {
                None
            },
            session: ResumableCliSession::new(
                provider,
                payload.session_id,
                payload.cwd,
                ResumableCliEvidence::ProviderEvent,
            )?,
        },
    }))
}

pub(crate) struct ClaudeCaptureListener {
    pub(crate) observer: CliIdentityObserver,
    pub(crate) session_id: String,
    pub(crate) plugin_path: PathBuf,
}

pub(crate) struct CopilotCaptureListener {
    pub(crate) environment: HashMap<String, String>,
    pub(crate) result: oneshot::Receiver<Result<CodexCapture>>,
    pub(crate) session_id: String,
    pub(crate) cancellation: CaptureCancellation,
}

pub(crate) struct HermesCaptureListener {
    pub(crate) environment: HashMap<String, String>,
    pub(crate) result: oneshot::Receiver<Result<CodexCapture>>,
    pub(crate) cancellation: CaptureCancellation,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct HermesProfileContext {
    home: PathBuf,
    profile: String,
}

struct ClaudeSessionPlugin {
    path: PathBuf,
}

impl Drop for ClaudeSessionPlugin {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

struct ExpectedCapture {
    provider: ResumableCliProvider,
    session_id: Option<String>,
    hermes_profile: Option<HermesProfileContext>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CodexSessionStartPayload {
    cwd: PathBuf,
    hook_event_name: String,
    model: Value,
    permission_mode: Value,
    session_id: String,
    source: String,
    transcript_path: Value,
}

#[derive(Deserialize)]
struct CopilotSessionStartPayload {
    cwd: PathBuf,
    hook_event_name: String,
    session_id: String,
    #[serde(flatten)]
    _extra: Map<String, Value>,
}

#[cfg(test)]
pub(crate) fn start_codex_capture(resume_cwd: PathBuf) -> Result<CodexCaptureListener> {
    let (environment, result, cancellation) = start_capture(resume_cwd, None)?;
    Ok(CodexCaptureListener {
        environment,
        result,
        cancellation,
    })
}

pub(crate) fn start_claude_capture(resume_cwd: PathBuf) -> Result<ClaudeCaptureListener> {
    let session_id = Uuid::new_v4().to_string();
    let plugin = create_claude_session_plugin()?;
    let plugin_path = plugin.path.clone();
    let mut observer = start_cli_identity_observer(resume_cwd, ResumableCliProvider::Claude)?;
    observer._plugin = Some(plugin);
    Ok(ClaudeCaptureListener {
        observer,
        session_id,
        plugin_path,
    })
}

pub(crate) fn start_copilot_capture(resume_cwd: PathBuf) -> Result<CopilotCaptureListener> {
    let session_id = Uuid::new_v4().to_string();
    start_copilot_capture_in(
        resume_cwd,
        session_id,
        copilot_store_root()?,
        CALLBACK_TIMEOUT,
    )
}

fn start_copilot_capture_in(
    resume_cwd: PathBuf,
    session_id: String,
    store_root: PathBuf,
    timeout: Duration,
) -> Result<CopilotCaptureListener> {
    ResumableCliProvider::Copilot.validate_resume_id(&session_id)?;
    let marker = store_root
        .join("session-state")
        .join(&session_id)
        .join("workspace.yaml");
    anyhow::ensure!(
        !marker.try_exists()?,
        "reserved Copilot session already exists"
    );
    let cancellation = CaptureCancellation(Arc::new(AtomicBool::new(false)));
    let thread_cancellation = cancellation.clone();
    let (sender, result) = oneshot::channel();
    let expected_session_id = session_id.clone();
    let deadline = Instant::now() + timeout;
    std::thread::spawn(move || {
        loop {
            if sender.is_canceled() || thread_cancellation.is_cancelled() {
                return;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                if sender
                    .send(Err(anyhow!(
                        "Copilot session identity record timed out after {} seconds",
                        timeout.as_secs_f64()
                    )))
                    .is_err()
                {
                    return;
                }
                return;
            }
            match read_copilot_workspace(&marker, &expected_session_id, &resume_cwd) {
                Ok(Some(capture)) => {
                    if !thread_cancellation.is_cancelled() {
                        if sender.send(Ok(capture)).is_err() {
                            return;
                        }
                    }
                    return;
                }
                Ok(None) => {}
                Err(error) => {
                    if sender.send(Err(error)).is_err() {
                        return;
                    }
                    return;
                }
            }
            std::thread::sleep(CALLBACK_POLL_INTERVAL.min(remaining));
        }
    });
    Ok(CopilotCaptureListener {
        environment: HashMap::default(),
        result,
        session_id,
        cancellation,
    })
}

#[derive(Deserialize)]
struct CopilotWorkspace {
    id: String,
    cwd: PathBuf,
}

fn read_copilot_workspace(
    marker: &Path,
    session_id: &str,
    resume_cwd: &Path,
) -> Result<Option<CodexCapture>> {
    let file = match fs::File::open(marker) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("opening Copilot session identity record"),
    };
    let mut bytes = Vec::new();
    file.take(MAX_PAYLOAD_BYTES + 1)
        .read_to_end(&mut bytes)
        .context("reading Copilot session identity record")?;
    anyhow::ensure!(
        bytes.len() as u64 <= MAX_PAYLOAD_BYTES,
        "Copilot session identity record exceeds the size limit"
    );
    // Copilot can create workspace.yaml before its contents have been completely written.
    let workspace = match serde_yaml::from_slice::<CopilotWorkspace>(&bytes) {
        Ok(workspace) => workspace,
        Err(_) => return Ok(None),
    };
    anyhow::ensure!(
        workspace.id == session_id,
        "reserved Copilot session ID does not match"
    );
    anyhow::ensure!(
        same_path(&workspace.cwd, resume_cwd),
        "Copilot session identity record CWD does not match"
    );
    let session = ResumableCliSession::new(
        ResumableCliProvider::Copilot,
        workspace.id,
        resume_cwd.to_path_buf(),
        ResumableCliEvidence::ProviderWorkspace,
    )?;
    Ok(Some(CodexCapture { session }))
}

pub(crate) fn start_hermes_capture(
    resume_cwd: PathBuf,
    profile: HermesProfileContext,
) -> Result<HermesCaptureListener> {
    let (environment, result, cancellation) = start_capture(
        resume_cwd,
        Some(ExpectedCapture {
            provider: ResumableCliProvider::Hermes,
            session_id: None,
            hermes_profile: Some(profile),
        }),
    )?;
    Ok(HermesCaptureListener {
        environment,
        result,
        cancellation,
    })
}

fn start_capture(
    resume_cwd: PathBuf,
    expected_capture: Option<ExpectedCapture>,
) -> Result<(
    HashMap<String, String>,
    oneshot::Receiver<Result<CodexCapture>>,
    CaptureCancellation,
)> {
    // Hermes emits on_session_start on the first conversation turn, not when
    // its CLI opens. An idle terminal must keep its launch listener alive.
    let timeout = match expected_capture.as_ref().map(|capture| capture.provider) {
        Some(ResumableCliProvider::Hermes) => None,
        _ => Some(CALLBACK_TIMEOUT),
    };
    start_capture_with_optional_timeout(resume_cwd, expected_capture, timeout)
}

#[cfg(test)]
fn start_capture_with_timeout(
    resume_cwd: PathBuf,
    expected_capture: Option<ExpectedCapture>,
    timeout: Duration,
) -> Result<(
    HashMap<String, String>,
    oneshot::Receiver<Result<CodexCapture>>,
    CaptureCancellation,
)> {
    start_capture_with_optional_timeout(resume_cwd, expected_capture, Some(timeout))
}

fn start_capture_with_optional_timeout(
    resume_cwd: PathBuf,
    expected_capture: Option<ExpectedCapture>,
    timeout: Option<Duration>,
) -> Result<(
    HashMap<String, String>,
    oneshot::Receiver<Result<CodexCapture>>,
    CaptureCancellation,
)> {
    let server = tiny_http::Server::http("127.0.0.1:0")
        .map_err(|error| anyhow!(error).context("binding resumable CLI callback listener"))?;
    let address = server
        .server_addr()
        .to_ip()
        .ok_or_else(|| anyhow!("resumable CLI callback listener is not TCP"))?;
    anyhow::ensure!(
        address.ip().is_loopback(),
        "callback listener is not loopback"
    );

    let capability = Uuid::new_v4().to_string();
    let launch_attempt = Uuid::new_v4().to_string();
    let mut environment = HashMap::default();
    environment.insert(CALLBACK_ADDRESS_ENV.into(), address.to_string());
    environment.insert(CALLBACK_CAPABILITY_ENV.into(), capability.clone());
    environment.insert(LAUNCH_ATTEMPT_ENV.into(), launch_attempt.clone());

    let cancellation = CaptureCancellation(Arc::new(AtomicBool::new(false)));
    let thread_cancellation = cancellation.clone();
    let provider = expected_capture
        .as_ref()
        .map_or(ResumableCliProvider::Codex, |expected| expected.provider);
    environment.insert(CAPTURE_PROVIDER_ENV.into(), provider.as_str().into());
    let deadline = timeout.map(|timeout| Instant::now() + timeout);
    let (sender, result) = oneshot::channel();
    std::thread::spawn(move || {
        loop {
            if sender.is_canceled() || thread_cancellation.is_cancelled() {
                return;
            }

            let remaining =
                deadline.map(|deadline| deadline.saturating_duration_since(Instant::now()));
            if let Some(timeout) =
                timeout.filter(|_| remaining.is_some_and(|remaining| remaining.is_zero()))
            {
                if sender
                    .send(Err(anyhow!(
                        "{} session identity callback timed out after {} seconds",
                        provider.display_name(),
                        timeout.as_secs_f64(),
                    )))
                    .is_err()
                {
                    return;
                }
                return;
            }

            let poll_interval = remaining.map_or(CALLBACK_POLL_INTERVAL, |remaining| {
                CALLBACK_POLL_INTERVAL.min(remaining)
            });
            let Some(mut request) = (match server.recv_timeout(poll_interval) {
                Ok(request) => request,
                Err(error) => {
                    if sender
                        .send(Err(anyhow!(error).context(format!(
                            "receiving {} callback",
                            provider.display_name(),
                        ))))
                        .is_err()
                    {
                        return;
                    }
                    return;
                }
            }) else {
                continue;
            };

            if request.method() != &tiny_http::Method::Post || request.url() != CALLBACK_PATH {
                respond(request, 404);
                continue;
            }
            if !header_matches(&request, "X-Zed-Capability", &capability)
                || !header_matches(&request, "X-Zed-Launch-Attempt", &launch_attempt)
            {
                respond(request, 401);
                continue;
            }

            let capture = read_capture(&mut request, &resume_cwd, expected_capture.as_ref());
            respond(request, if capture.is_ok() { 204 } else { 400 });
            if sender.send(capture).is_err() {
                return;
            }
            return;
        }
    });

    Ok((environment, result, cancellation))
}

fn header_matches(request: &tiny_http::Request, name: &str, expected: &str) -> bool {
    request.headers().iter().any(|header| {
        header.field.to_string().eq_ignore_ascii_case(name)
            && constant_time_eq(header.value.as_str(), expected)
    })
}

fn constant_time_eq(actual: &str, expected: &str) -> bool {
    if actual.len() != expected.len() {
        return false;
    }
    actual
        .bytes()
        .zip(expected.bytes())
        .fold(0_u8, |difference, (left, right)| {
            difference | (left ^ right)
        })
        == 0
}

fn read_capture(
    request: &mut tiny_http::Request,
    resume_cwd: &Path,
    expected_capture: Option<&ExpectedCapture>,
) -> Result<CodexCapture> {
    let provider =
        expected_capture.map_or(ResumableCliProvider::Codex, |expected| expected.provider);
    let mut bytes = Vec::new();
    request
        .as_reader()
        .take(MAX_PAYLOAD_BYTES + 1)
        .read_to_end(&mut bytes)
        .with_context(|| {
            format!(
                "reading {} session identity callback",
                provider.display_name()
            )
        })?;
    anyhow::ensure!(
        bytes.len() as u64 <= MAX_PAYLOAD_BYTES,
        "{} session identity callback exceeds the size limit",
        provider.display_name()
    );

    match expected_capture {
        Some(expected) => parse_reserved_capture_payload(&bytes, resume_cwd, expected),
        None => parse_capture_payload(&bytes, resume_cwd),
    }
}

fn parse_capture_payload(bytes: &[u8], resume_cwd: &Path) -> Result<CodexCapture> {
    let payload: CodexSessionStartPayload =
        serde_json::from_slice(bytes).context("parsing Codex SessionStart callback")?;
    anyhow::ensure!(
        payload.hook_event_name == "SessionStart",
        "unexpected hook event"
    );
    anyhow::ensure!(
        payload.source == "startup",
        "unexpected SessionStart source"
    );
    anyhow::ensure!(
        same_path(&payload.cwd, resume_cwd),
        "SessionStart CWD does not match"
    );
    let _ = (
        payload.model,
        payload.permission_mode,
        payload.transcript_path,
    );

    let session = ResumableCliSession::new(
        ResumableCliProvider::Codex,
        payload.session_id,
        resume_cwd.to_path_buf(),
        ResumableCliEvidence::ProviderEvent,
    )?;
    Ok(CodexCapture { session })
}

#[cfg(test)]
fn parse_claude_capture_payload(bytes: &[u8], resume_cwd: &Path) -> Result<CodexCapture> {
    let mut payload: Value = serde_json::from_slice(bytes)?;
    stamp_hook_process_identity(
        &mut payload,
        Ok(Some(CliProcessIdentity {
            pid: 24,
            created_at: 2401,
        })),
    );
    let capture = parse_cli_identity(
        &serde_json::to_vec(&payload)?,
        Some(resume_cwd),
        ResumableCliProvider::Claude,
    )?
    .context("nested Claude agent")?;
    Ok(CodexCapture {
        session: capture.identity.session,
    })
}

fn parse_reserved_capture_payload(
    bytes: &[u8],
    resume_cwd: &Path,
    expected: &ExpectedCapture,
) -> Result<CodexCapture> {
    if expected.provider == ResumableCliProvider::Hermes {
        let profile = expected
            .hermes_profile
            .as_ref()
            .context("missing expected Hermes profile context")?;
        return parse_hermes_capture_payload(bytes, resume_cwd, profile);
    }
    if expected.provider == ResumableCliProvider::Copilot {
        return parse_copilot_capture_payload(bytes, resume_cwd, expected);
    }
    anyhow::bail!("unsupported one-shot capture provider")
}

fn parse_copilot_capture_payload(
    bytes: &[u8],
    resume_cwd: &Path,
    expected: &ExpectedCapture,
) -> Result<CodexCapture> {
    let payload: CopilotSessionStartPayload =
        serde_json::from_slice(bytes).context("parsing Copilot SessionStart callback")?;
    anyhow::ensure!(
        payload.hook_event_name == "SessionStart",
        "unexpected hook event"
    );
    anyhow::ensure!(
        expected.session_id.as_deref() == Some(payload.session_id.as_str()),
        "reserved session ID does not match"
    );
    anyhow::ensure!(
        same_path(&payload.cwd, resume_cwd),
        "SessionStart CWD does not match"
    );

    let session = ResumableCliSession::new(
        ResumableCliProvider::Copilot,
        payload.session_id,
        resume_cwd.to_path_buf(),
        ResumableCliEvidence::ProviderEvent,
    )?;
    Ok(CodexCapture { session })
}

#[derive(Deserialize)]
struct HermesSessionStartPayload {
    cwd: PathBuf,
    hook_event_name: String,
    profile: String,
    session_id: String,
    #[serde(flatten)]
    _extra: Map<String, Value>,
}

fn parse_hermes_capture_payload(
    bytes: &[u8],
    resume_cwd: &Path,
    expected_profile: &HermesProfileContext,
) -> Result<CodexCapture> {
    let payload: HermesSessionStartPayload =
        serde_json::from_slice(bytes).context("parsing Hermes on_session_start callback")?;
    anyhow::ensure!(
        payload.hook_event_name == "on_session_start",
        "unexpected hook event"
    );
    anyhow::ensure!(
        payload.profile == expected_profile.profile,
        "Hermes profile does not match the launched profile"
    );
    anyhow::ensure!(
        same_path(&payload.cwd, resume_cwd),
        "on_session_start CWD does not match"
    );
    let session = ResumableCliSession::new(
        ResumableCliProvider::Hermes,
        payload.session_id,
        resume_cwd.to_path_buf(),
        ResumableCliEvidence::ProviderEvent,
    )?
    .with_provider_context(
        expected_profile.home.clone(),
        expected_profile.profile.clone(),
    )?;
    Ok(CodexCapture { session })
}

fn create_claude_session_plugin() -> Result<ClaudeSessionPlugin> {
    let executable = std::env::current_exe().context("locating the Zed executable")?;
    let path = std::env::temp_dir().join(format!("zed-claude-session-plugin-{}", Uuid::new_v4()));
    fs::create_dir_all(path.join(".claude-plugin"))?;
    fs::create_dir_all(path.join("hooks"))?;

    let manifest = json!({ "name": "zed-session-capture" });
    let hooks = json!({
        "hooks": {
            "SessionStart": [{
                "matcher": "^(startup|resume|clear|compact|fork)$",
                "hooks": [{
                    "type": "command",
                    "command": executable,
                    "args": [HOOK_HELPER_ARGUMENT],
                    "timeout": 5
                }]
            }],
            "UserPromptSubmit": [{
                "hooks": [{
                    "type": "command",
                    "command": executable,
                    "args": [HOOK_HELPER_ARGUMENT],
                    "timeout": 5
                }]
            }]
        }
    });
    fs::write(
        path.join(".claude-plugin").join("plugin.json"),
        serde_json::to_vec_pretty(&manifest)?,
    )?;
    fs::write(
        path.join("hooks").join("hooks.json"),
        serde_json::to_vec_pretty(&hooks)?,
    )?;
    Ok(ClaudeSessionPlugin { path })
}

fn same_path(left: &Path, right: &Path) -> bool {
    match (left.canonicalize(), right.canonicalize()) {
        (Ok(left), Ok(right)) => left == right,
        _ => left == right,
    }
}

fn respond(request: tiny_http::Request, status: u16) {
    let response = tiny_http::Response::empty(status).with_header(
        tiny_http::Header::from_str("Connection: close")
            .expect("static callback response header is valid"),
    );
    let _ = request.respond(response);
}

pub fn run_hook_helper() -> i32 {
    match forward_hook_payload() {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("Resumable CLI hook failed: {error:#}");
            1
        }
    }
}

fn callback_environment(
    mut read: impl FnMut(&str) -> Result<Option<String>>,
) -> Result<Option<(String, String, String)>> {
    let address = read(CALLBACK_ADDRESS_ENV)?;
    let capability = read(CALLBACK_CAPABILITY_ENV)?;
    let launch_attempt = read(LAUNCH_ATTEMPT_ENV)?;
    if address.is_none() && capability.is_none() && launch_attempt.is_none() {
        return Ok(None);
    }
    Ok(Some((
        address.context("missing ZED_RESUMABLE_CLI_CALLBACK_ADDRESS")?,
        capability.context("missing ZED_RESUMABLE_CLI_CALLBACK_CAPABILITY")?,
        launch_attempt.context("missing ZED_RESUMABLE_CLI_LAUNCH_ATTEMPT")?,
    )))
}

pub(crate) fn capture_error_message(
    provider: ResumableCliProvider,
    pane: Option<&str>,
    terminal: Option<&str>,
    error: &anyhow::Error,
) -> String {
    let mut labels = Vec::new();
    if let Some(pane) = pane.filter(|name| !name.trim().is_empty()) {
        labels.push(format!("pane: {pane}"));
    }
    if let Some(terminal) = terminal.filter(|name| !name.trim().is_empty()) {
        labels.push(format!("terminal: {terminal}"));
    }
    let location = if labels.is_empty() {
        String::new()
    } else {
        format!(" ({})", labels.join(", "))
    };
    format!(
        "{} session identity capture failed{location}: {error:#}",
        provider.display_name()
    )
}

fn identity_hook_payload(payload: Vec<u8>, cwd: &Path, hook_started_at: u64) -> Result<Vec<u8>> {
    anyhow::ensure!(
        payload.len() as u64 <= MAX_CODEX_HOOK_INPUT_BYTES,
        "hook input is too large"
    );
    let mut value: Value = serde_json::from_slice(&payload).context("invalid hook input")?;
    anyhow::ensure!(
        matches!(
            value.get("hook_event_name").and_then(Value::as_str),
            Some("SessionStart" | "on_session_start" | "UserPromptSubmit")
        ) && value.get("session_id").and_then(Value::as_str).is_some(),
        "invalid SessionStart hook input"
    );
    if value.get("cwd").and_then(Value::as_str).is_none() {
        value["cwd"] = Value::String(cwd.to_string_lossy().into_owned());
    }

    let prompt = value.get("hook_event_name").and_then(Value::as_str) == Some("UserPromptSubmit");
    let submission = prompt
        .then(|| {
            value
                .get("turn_id")
                .and_then(Value::as_str)
                .and_then(|id| Uuid::parse_str(id).ok())
                .filter(|id| id.get_version_num() == 7)
        })
        .flatten();
    value
        .as_object_mut()
        .context("hook input is not an object")?
        .retain(|key, _| {
            matches!(
                key.as_str(),
                "hook_event_name" | "session_id" | "cwd" | "agent_id" | "agent_type"
            ) || (!prompt && matches!(key.as_str(), "source" | "profile"))
        });
    value["zed_hook_started_at"] = json!(hook_started_at);
    if prompt {
        if let Some(submission) = submission {
            value["zed_prompt_submission_id"] = json!(submission.to_string());
        }
    } else if value.get("hook_event_name").and_then(Value::as_str) == Some("SessionStart") {
        // Claude requires the field, but capture never needs its filesystem path.
        value["transcript_path"] = Value::Null;
    }
    let payload = serde_json::to_vec(&value)?;
    anyhow::ensure!(
        payload.len() as u64 <= MAX_PAYLOAD_BYTES,
        "identity callback is too large"
    );

    Ok(payload)
}

fn is_hook_command_shell(name: &str) -> bool {
    ["cmd.exe", "powershell.exe", "pwsh.exe"]
        .iter()
        .any(|shell| name.eq_ignore_ascii_case(shell))
}

fn hook_caller_pid(
    helper_pid: u32,
    mut process: impl FnMut(u32) -> Option<(Option<u32>, String)>,
) -> Result<Option<u32>> {
    let mut pid = helper_pid;
    for _ in 0..64 {
        let Some((parent, name)) = process(pid) else {
            return Ok(None);
        };
        if pid != helper_pid && !is_hook_command_shell(&name) {
            return Ok(Some(pid));
        }
        let Some(parent) = parent else {
            return Ok(None);
        };
        if parent == pid {
            return Ok(None);
        }
        pid = parent;
    }
    anyhow::bail!("Codex hook process ancestry exceeds the limit")
}

fn hook_process_identity() -> Result<Option<CliProcessIdentity>> {
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};
    // The owned hook command uses cmd.exe or PowerShell. Its first ancestor
    // outside those wrappers is the dispatching process, regardless of its name.
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing().without_tasks(),
    );
    let Some(pid) = hook_caller_pid(std::process::id(), |pid| {
        system.process(Pid::from_u32(pid)).map(|process| {
            (
                process.parent().map(|parent| parent.as_u32()),
                process.name().to_string_lossy().into_owned(),
            )
        })
    })?
    else {
        return Ok(None);
    };
    use windows::Win32::{
        Foundation::{CloseHandle, FILETIME},
        System::Threading::{GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION},
    };
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }
        .context("opening the Codex hook process")?;
    let mut created = FILETIME::default();
    let mut exited = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    let result =
        unsafe { GetProcessTimes(handle, &mut created, &mut exited, &mut kernel, &mut user) };
    if let Err(error) = unsafe { CloseHandle(handle) } {
        log::debug!("Could not close the identity process handle: {error}");
    }
    result.context("reading the Codex process creation time")?;
    Ok(Some(CliProcessIdentity {
        pid,
        created_at: (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime),
    }))
}

fn stamp_hook_process_identity(identity: &mut Value, process: Result<Option<CliProcessIdentity>>) {
    match process {
        Ok(Some(process)) => identity["zed_codex_process"] = json!(process),
        Ok(None) => {
            log::debug!("Hook caller process identity unavailable; forwarding for rejection");
        }
        Err(_) => {
            log::debug!("Hook caller process lookup failed; forwarding for rejection");
        }
    }
}

fn forward_hook_payload() -> Result<()> {
    let Some((address, capability, launch_attempt)) =
        callback_environment(|name| match std::env::var(name) {
            Ok(value) => Ok(Some(value)),
            Err(std::env::VarError::NotPresent) => Ok(None),
            Err(std::env::VarError::NotUnicode(_)) => Err(anyhow!("{name} is not valid UTF-8")),
        })?
    else {
        return Ok(());
    };

    let deadline = Instant::now() + HELPER_IO_TIMEOUT;
    let address: SocketAddr = address.parse().context("invalid callback address")?;
    anyhow::ensure!(
        address.ip() == IpAddr::from([127, 0, 0, 1]),
        "callback is not loopback"
    );
    Uuid::parse_str(&capability).context("invalid callback capability")?;
    Uuid::parse_str(&launch_attempt).context("invalid launch attempt")?;

    // Stamp before reading stdin or opening the connection: delayed delivery
    // must not make an older prompt look newer than a resumed prompt.
    let mut hook_started_at = 0;
    // QPC is shared across processes and independent of wall-clock adjustments.
    unsafe { windows::Win32::System::Performance::QueryPerformanceCounter(&mut hook_started_at) }
        .context("reading the identity hook clock")?;
    let hook_started_at = hook_started_at
        .try_into()
        .context("invalid identity hook timestamp")?;
    let mut payload = Vec::new();
    std::io::stdin()
        .take(MAX_CODEX_HOOK_INPUT_BYTES + 1)
        .read_to_end(&mut payload)
        .context("reading hook input")?;
    anyhow::ensure!(
        payload.len() as u64 <= MAX_CODEX_HOOK_INPUT_BYTES,
        "hook input is too large"
    );
    let payload = identity_hook_payload(
        payload,
        &std::env::current_dir().context("locating the hook working directory")?,
        hook_started_at,
    )?;

    let mut identity: Value = serde_json::from_slice(&payload)?;
    // Older running Zed builds did not set the provider marker. Preserve their
    // callbacks until those terminals are relaunched with explicit provider scope.
    if std::env::var(CAPTURE_PROVIDER_ENV).map_or(true, |provider| {
        matches!(provider.as_str(), "codex" | "claude")
    }) {
        // Forward lookup failures so the observer can report them without binding a session.
        stamp_hook_process_identity(&mut identity, hook_process_identity());
    }
    let payload = serde_json::to_vec(&identity)?;
    anyhow::ensure!(
        payload.len() as u64 <= MAX_PAYLOAD_BYTES,
        "identity callback is too large"
    );
    send_hook_payload_before(address, &capability, &launch_attempt, &payload, deadline)
}

#[cfg(test)]
fn send_hook_payload(
    address: SocketAddr,
    capability: &str,
    launch_attempt: &str,
    payload: &[u8],
) -> Result<()> {
    send_hook_payload_before(
        address,
        capability,
        launch_attempt,
        payload,
        Instant::now() + HELPER_IO_TIMEOUT,
    )
}

fn remaining_helper_time(deadline: Instant) -> Result<Duration> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    anyhow::ensure!(!remaining.is_zero(), "identity callback deadline exceeded");
    Ok(remaining)
}

fn send_hook_payload_before(
    address: SocketAddr,
    capability: &str,
    launch_attempt: &str,
    payload: &[u8],
    deadline: Instant,
) -> Result<()> {
    loop {
        remaining_helper_time(deadline)?;
        if send_hook_payload_once(address, capability, launch_attempt, payload, deadline)? {
            return Ok(());
        }
        std::thread::sleep(
            Duration::from_millis(20).min(deadline.saturating_duration_since(Instant::now())),
        );
    }
}

fn send_hook_payload_once(
    address: SocketAddr,
    capability: &str,
    launch_attempt: &str,
    payload: &[u8],
    deadline: Instant,
) -> Result<bool> {
    let mut stream = match TcpStream::connect_timeout(&address, remaining_helper_time(deadline)?) {
        Ok(stream) => stream,
        // Forget and terminal closure cancel the listener while the CLI may still
        // have its launch environment. Those later hooks must remain harmless.
        Err(error) if error.kind() == std::io::ErrorKind::ConnectionRefused => {
            log::debug!("Resumable CLI listener is closed; skipping callback");
            return Ok(true);
        }
        Err(error) => return Err(error).context("connecting to callback listener"),
    };
    let header = format!(
        "POST {CALLBACK_PATH} HTTP/1.1\r\nHost: {address}\r\nX-Zed-Capability: {capability}\r\nX-Zed-Launch-Attempt: {launch_attempt}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        payload.len()
    );
    write_hook_bytes(&mut stream, header.as_bytes(), deadline)?;
    write_hook_bytes(&mut stream, payload, deadline)?;
    stream.set_read_timeout(Some(remaining_helper_time(deadline)?))?;

    let mut response = [0_u8; 32];
    let bytes_read = stream.read(&mut response)?;
    let status =
        std::str::from_utf8(&response[..bytes_read]).context("invalid callback response")?;
    if status.starts_with("HTTP/1.1 429") {
        return Ok(false);
    }
    anyhow::ensure!(
        status.starts_with("HTTP/1.1 204") || status.starts_with("HTTP/1.1 410"),
        "callback was rejected"
    );
    Ok(true)
}

fn write_hook_bytes(stream: &mut TcpStream, mut bytes: &[u8], deadline: Instant) -> Result<()> {
    while !bytes.is_empty() {
        stream.set_write_timeout(Some(remaining_helper_time(deadline)?))?;
        let written = stream.write(bytes).context("writing callback payload")?;
        anyhow::ensure!(written > 0, "callback listener stopped accepting data");
        bytes = bytes
            .get(written..)
            .context("invalid callback write size")?;
    }
    Ok(())
}

pub(crate) struct HookMerge {
    pub(crate) content: String,
    pub(crate) changed: bool,
    pub(crate) entry_preview: String,
}

pub(crate) fn codex_hooks_path() -> Result<PathBuf> {
    Ok(dirs::home_dir()
        .ok_or_else(|| anyhow!("home directory is unavailable"))?
        .join(".codex")
        .join("hooks.json"))
}

pub(crate) fn merge_codex_hook(existing: Option<&str>) -> Result<HookMerge> {
    let executable = std::env::current_exe().context("locating the Zed executable")?;
    let executable = executable
        .to_str()
        .ok_or_else(|| anyhow!("the Zed executable path is not valid UTF-8"))?;
    anyhow::ensure!(
        !executable.contains('"'),
        "the Zed executable path contains a quote"
    );
    let command = format!("\"{executable}\" {HOOK_HELPER_ARGUMENT}");
    let command_windows = if executable.contains('%') {
        let executable = executable.replace('\'', "''");
        format!(
            "powershell.exe -NoProfile -NonInteractive -Command \"& '{executable}' {HOOK_HELPER_ARGUMENT}\""
        )
    } else {
        format!("cmd.exe /D /V:OFF /S /C \"\"{executable}\" {HOOK_HELPER_ARGUMENT}\"")
    };
    let owned_hook = json!({
        "type": "command",
        "command": command,
        "commandWindows": command_windows,
        "timeout": 5
    });
    let owned_group = json!({
        "matcher": "^(startup|resume)$",
        "hooks": [owned_hook]
    });

    let mut root = match existing {
        Some(existing) if !existing.trim().is_empty() => {
            serde_json::from_str::<Value>(existing).context("parsing existing Codex hooks.json")?
        }
        _ => Value::Object(Map::new()),
    };
    let root = root
        .as_object_mut()
        .ok_or_else(|| anyhow!("Codex hooks.json root must be an object"))?;
    let hooks = root
        .entry("hooks")
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .ok_or_else(|| anyhow!("Codex hooks.json 'hooks' must be an object"))?;
    let session_start = hooks
        .entry("SessionStart")
        .or_insert_with(|| Value::Array(Vec::new()))
        .as_array_mut()
        .ok_or_else(|| anyhow!("Codex SessionStart hooks must be an array"))?;

    let mut changed = false;
    let mut found_owned_hook = false;
    for group in session_start.iter_mut() {
        let Some(hooks) = group.get_mut("hooks").and_then(Value::as_array_mut) else {
            continue;
        };
        hooks.retain_mut(|hook| {
            if !is_owned_hook(hook) {
                return true;
            }

            if !found_owned_hook {
                found_owned_hook = true;
                if hook != &owned_hook {
                    *hook = owned_hook.clone();
                    changed = true;
                }
                true
            } else {
                changed = true;
                false
            }
        });
    }
    if !found_owned_hook {
        session_start.push(owned_group.clone());
        changed = true;
    }
    let prompt_groups = hooks
        .entry("UserPromptSubmit")
        .or_insert_with(|| Value::Array(Vec::new()))
        .as_array_mut()
        .context("Codex UserPromptSubmit hooks must be an array")?;
    let mut found_prompt = false;
    for group in prompt_groups.iter_mut() {
        let filtered = group
            .get("matcher")
            .and_then(Value::as_str)
            .is_some_and(|matcher| !matcher.is_empty());
        if let Some(entries) = group.get_mut("hooks").and_then(Value::as_array_mut) {
            entries.retain_mut(|hook| {
                if !is_owned_hook(hook) {
                    return true;
                }
                if filtered || found_prompt {
                    changed = true;
                    return false;
                }
                found_prompt = true;
                if hook != &owned_hook {
                    *hook = owned_hook.clone();
                    changed = true;
                }
                true
            });
        }
    }
    if !found_prompt {
        prompt_groups.push(json!({"hooks": [owned_hook]}));
        changed = true;
    }
    let entry_preview = serde_json::to_string_pretty(&json!({"SessionStart": owned_group,
        "UserPromptSubmit": {"hooks": [owned_hook]}}))?;
    Ok(HookMerge {
        content: serde_json::to_string_pretty(&root)?,
        changed,
        entry_preview,
    })
}

pub(crate) fn hermes_config_path(profile: &HermesProfileContext) -> PathBuf {
    profile.home.join("config.yaml")
}

fn default_hermes_root() -> Result<PathBuf> {
    #[cfg(target_os = "windows")]
    {
        if let Some(local_app_data) =
            std::env::var_os("LOCALAPPDATA").filter(|value| !value.is_empty())
        {
            return Ok(PathBuf::from(local_app_data).join("hermes"));
        }
        return Ok(dirs::home_dir()
            .ok_or_else(|| anyhow!("home directory is unavailable"))?
            .join("AppData")
            .join("Local")
            .join("hermes"));
    }
    #[cfg(not(target_os = "windows"))]
    {
        Ok(dirs::home_dir()
            .ok_or_else(|| anyhow!("home directory is unavailable"))?
            .join(".hermes"))
    }
}

fn process_hermes_home() -> Result<PathBuf> {
    Ok(std::env::var_os("HERMES_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or(default_hermes_root()?))
}

fn hermes_root_for_home(home: &Path) -> &Path {
    if home
        .parent()
        .and_then(Path::file_name)
        .is_some_and(|name| name.eq_ignore_ascii_case("profiles"))
    {
        home.parent().and_then(Path::parent).unwrap_or(home)
    } else {
        home
    }
}

fn uses_hermes_profile_argument(profile: &HermesProfileContext) -> bool {
    profile
        .home
        .parent()
        .and_then(Path::file_name)
        .is_some_and(|name| name.eq_ignore_ascii_case("profiles"))
        || profile.profile != "custom"
}

fn validate_hermes_profile(profile: &str) -> Result<()> {
    anyhow::ensure!(
        matches!(profile, "default" | "custom")
            || (profile.len() <= 64
                && profile
                    .bytes()
                    .enumerate()
                    .all(|(index, byte)| byte.is_ascii_lowercase()
                        || byte.is_ascii_digit()
                        || (index > 0 && matches!(byte, b'-' | b'_')))),
        "invalid Hermes profile identifier"
    );
    Ok(())
}

pub(crate) fn active_hermes_profile() -> Result<HermesProfileContext> {
    let configured_home = process_hermes_home()?;
    let default_root = default_hermes_root()?;
    if configured_home
        .parent()
        .and_then(Path::file_name)
        .is_some_and(|name| name.eq_ignore_ascii_case("profiles"))
    {
        let profile = configured_home
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| anyhow!("Hermes profile path is not valid UTF-8"))?
            .to_owned();
        validate_hermes_profile(&profile)?;
        return Ok(HermesProfileContext {
            home: configured_home,
            profile,
        });
    }

    let requested_profile = match std::env::var("HERMES_PROFILE") {
        Ok(profile) if !profile.trim().is_empty() => Some(profile),
        Ok(_) | Err(std::env::VarError::NotPresent) => {
            match fs::read_to_string(configured_home.join("active_profile")) {
                Ok(profile) => Some(profile),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => return Err(error).context("reading the active Hermes profile"),
            }
        }
        Err(error) => return Err(error).context("reading HERMES_PROFILE"),
    }
    .and_then(|profile| {
        let profile = profile.trim().to_ascii_lowercase();
        (!profile.is_empty()).then_some(profile)
    });
    let profile = requested_profile.unwrap_or_else(|| {
        if same_path(&configured_home, &default_root) {
            "default".to_owned()
        } else {
            "custom".to_owned()
        }
    });
    validate_hermes_profile(&profile)?;
    let home = match profile.as_str() {
        "default" | "custom" => configured_home,
        _ => configured_home.join("profiles").join(&profile),
    };
    anyhow::ensure!(
        home.is_dir(),
        "Hermes profile home is unavailable: {}",
        home.display()
    );
    Ok(HermesProfileContext { home, profile })
}

pub(crate) fn merge_hermes_hook(existing: Option<&str>) -> Result<HookMerge> {
    let executable = std::env::current_exe().context("locating the Zed executable")?;
    let executable = executable
        .to_str()
        .ok_or_else(|| anyhow!("the Zed executable path is not valid UTF-8"))?;
    anyhow::ensure!(
        !executable.contains('"'),
        "the Zed executable path contains a quote"
    );
    let command = format!("\"{executable}\" {HOOK_HELPER_ARGUMENT}");
    let owned_hook = serde_yaml::to_value(serde_json::json!({
        "command": command,
        "timeout": 5
    }))?;
    let entry_preview = serde_yaml::to_string(&owned_hook)?;

    let mut root = match existing {
        Some(existing) if !existing.trim().is_empty() => {
            serde_yaml::from_str(existing).context("parsing existing Hermes config.yaml")?
        }
        _ => serde_yaml::Value::Mapping(Default::default()),
    };
    let root = root
        .as_mapping_mut()
        .ok_or_else(|| anyhow!("Hermes config.yaml root must be a mapping"))?;
    let hooks = root
        .entry(serde_yaml::Value::String("hooks".into()))
        .or_insert_with(|| serde_yaml::Value::Mapping(Default::default()))
        .as_mapping_mut()
        .ok_or_else(|| anyhow!("Hermes config.yaml 'hooks' must be a mapping"))?;
    let session_start = hooks
        .entry(serde_yaml::Value::String("on_session_start".into()))
        .or_insert_with(|| serde_yaml::Value::Sequence(Vec::new()))
        .as_sequence_mut()
        .ok_or_else(|| anyhow!("Hermes on_session_start hooks must be a sequence"))?;

    let mut found_owned_hook = false;
    let mut changed = false;
    session_start.retain_mut(|hook| {
        if !is_owned_hermes_hook(hook) {
            return true;
        }
        if found_owned_hook {
            changed = true;
            return false;
        }
        found_owned_hook = true;
        if hook != &owned_hook {
            *hook = owned_hook.clone();
            changed = true;
        }
        true
    });
    if !found_owned_hook {
        session_start.push(owned_hook);
        changed = true;
    }

    Ok(HookMerge {
        content: serde_yaml::to_string(&root)?,
        changed,
        entry_preview,
    })
}

fn is_owned_hermes_hook(hook: &serde_yaml::Value) -> bool {
    hook.get("command")
        .and_then(serde_yaml::Value::as_str)
        .is_some_and(|command| {
            command
                .trim_end_matches('"')
                .ends_with(HOOK_HELPER_ARGUMENT)
        })
}

fn is_owned_hook(hook: &Value) -> bool {
    hook.get("type").and_then(Value::as_str) == Some("command")
        && ["command", "commandWindows"].into_iter().any(|field| {
            hook.get(field)
                .and_then(Value::as_str)
                .is_some_and(|command| {
                    command
                        .trim_end_matches('"')
                        .ends_with(HOOK_HELPER_ARGUMENT)
                })
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn post_identity(observer: &CliIdentityObserver, mut payload: Value) -> String {
        if payload.get("zed_codex_process").is_none() {
            payload["zed_codex_process"] = json!({"pid":24,"created_at":2401});
        }
        post_identity_bytes(observer, &serde_json::to_vec(&payload).unwrap())
    }

    fn post_identity_bytes(observer: &CliIdentityObserver, bytes: &[u8]) -> String {
        let address = &observer.environment[CALLBACK_ADDRESS_ENV];
        let mut stream = TcpStream::connect(address).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        write!(stream, "POST {CALLBACK_PATH} HTTP/1.1\r\nHost: {address}\r\nX-Zed-Capability: {}\r\nX-Zed-Launch-Attempt: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            observer.environment[CALLBACK_CAPABILITY_ENV], observer.environment[LAUNCH_ATTEMPT_ENV], bytes.len()).unwrap();
        stream.write_all(&bytes).unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        response
    }

    #[test]
    fn codex_observer_survives_the_old_idle_deadline() {
        use futures::StreamExt as _;
        let cwd = std::env::current_dir().unwrap();
        let mut observer = start_codex_identity_observer(cwd.clone()).unwrap();
        let (_, old_result, _) = start_capture_with_timeout(cwd, None, CALLBACK_TIMEOUT).unwrap();
        let error = futures::executor::block_on(old_result)
            .unwrap()
            .err()
            .unwrap();
        assert!(error.to_string().contains("timed out"));
        let accepted = post_identity(
            &observer,
            json!({
                "hook_event_name":"SessionStart","source":"startup",
                "session_id":"019f1234-5678-7abc-8def-0123456789ab",
                "cwd":std::env::current_dir().unwrap(),
            }),
        );
        assert!(accepted.starts_with("HTTP/1.1 204"));
        let capture = futures::executor::block_on(observer.events.next())
            .unwrap()
            .unwrap();
        assert_eq!(
            capture.session.exact_resume_id(),
            "019f1234-5678-7abc-8def-0123456789ab"
        );
        let address = observer.environment[CALLBACK_ADDRESS_ENV].clone();
        observer.cancellation.cancel();
        assert_listener_closes(&address);
    }

    #[test]
    fn codex_observer_recovers_and_reports_repeated_and_switched_identity() {
        use futures::StreamExt as _;
        let cwd = std::env::current_dir().unwrap();
        let mut observer = start_codex_identity_observer(cwd.clone()).unwrap();
        use futures::FutureExt as _;
        let forged = post_session_start(
            &observer.environment[CALLBACK_ADDRESS_ENV],
            "invalid",
            &observer.environment[LAUNCH_ATTEMPT_ENV],
            &cwd,
        );
        assert!(forged.starts_with("HTTP/1.1 401"));
        assert!(observer.events.next().now_or_never().is_none());
        assert!(post_identity_bytes(&observer, b"{").starts_with("HTTP/1.1 400"));
        assert!(
            futures::executor::block_on(observer.events.next())
                .unwrap()
                .is_err()
        );
        let oversized = json!({"hook_event_name":"SessionStart", "source":"startup",
            "session_id":"019f1234-5678-7abc-8def-0123456789ab", "cwd":cwd,
            "extra":"x".repeat(MAX_PAYLOAD_BYTES as usize)});
        assert!(post_identity(&observer, oversized).starts_with("HTTP/1.1 400"));
        assert!(
            futures::executor::block_on(observer.events.next())
                .unwrap()
                .is_err()
        );
        let invalid =
            json!({"hook_event_name":"UserPromptSubmit", "session_id":"not-a-session", "cwd":cwd});
        assert!(post_identity(&observer, invalid).starts_with("HTTP/1.1 400"));
        assert!(
            futures::executor::block_on(observer.events.next())
                .unwrap()
                .is_err()
        );
        let first = "019f1234-5678-7abc-8def-0123456789aa";
        let second = "019f1234-5678-7abc-8def-0123456789bb";
        let other_cwd = cwd.join("other-project");
        for (id, directory, event, source) in [
            (first, &cwd, "SessionStart", Some("startup")),
            (first, &cwd, "UserPromptSubmit", None),
            (second, &other_cwd, "SessionStart", Some("resume")),
            (second, &other_cwd, "UserPromptSubmit", None),
            (first, &cwd, "UserPromptSubmit", None),
        ] {
            let payload = json!({"hook_event_name":event,"source":source,"session_id":id,"cwd":directory,"zed_hook_started_at":2401});
            assert!(post_identity(&observer, payload).starts_with("HTTP/1.1 204"));
            let capture = futures::executor::block_on(observer.events.next())
                .unwrap()
                .unwrap();
            assert_eq!(capture.session.exact_resume_id(), id);
            assert_eq!(capture.session.resume_cwd(), directory);
        }
        observer.cancellation.cancel();
        assert_listener_closes(&observer.environment[CALLBACK_ADDRESS_ENV]);
    }

    #[test]
    fn codex_observer_acknowledges_delayed_old_prompts_without_rebinding() {
        use futures::{FutureExt as _, StreamExt as _};
        let cwd = std::env::current_dir().unwrap();
        let mut observer = start_codex_identity_observer(cwd.clone()).unwrap();
        for (id, timestamp) in [
            ("019f1234-5678-7abc-8def-0123456789bb", 2402),
            ("019f1234-5678-7abc-8def-0123456789aa", 2401),
        ] {
            let payload = json!({"hook_event_name":"UserPromptSubmit","session_id":id,
                "cwd":cwd,"zed_hook_started_at":timestamp});
            assert!(post_identity(&observer, payload).starts_with("HTTP/1.1 204"));
            if timestamp == 2402 {
                let capture = futures::executor::block_on(observer.events.next())
                    .unwrap()
                    .unwrap();
                assert_eq!(capture.session.exact_resume_id(), id);
            } else {
                assert!(observer.events.next().now_or_never().is_none());
            }
        }
        observer.cancellation.cancel();
    }

    #[test]
    fn codex_observer_ignores_other_root_processes_and_reused_pids() {
        use futures::{FutureExt as _, StreamExt as _};
        let cwd = std::env::current_dir().unwrap();
        let mut observer = start_codex_identity_observer(cwd.clone()).unwrap();
        let initial = json!({"hook_event_name":"SessionStart","source":"startup",
            "session_id":"019f1234-5678-7abc-8def-0123456789aa","cwd":cwd});
        assert!(post_identity(&observer, initial).starts_with("HTTP/1.1 204"));
        assert!(
            futures::executor::block_on(observer.events.next())
                .unwrap()
                .is_ok()
        );
        for process in [
            json!({"pid":25,"created_at":2401}),
            json!({"pid":24,"created_at":2402}),
        ] {
            let foreign = json!({"hook_event_name":"UserPromptSubmit",
                "session_id":"019f1234-5678-7abc-8def-0123456789bb","cwd":cwd,
                "zed_hook_started_at":9000,"zed_codex_process":process});
            assert!(post_identity(&observer, foreign).starts_with("HTTP/1.1 204"));
            assert!(observer.events.next().now_or_never().is_none());
        }
        let resumed = json!({"hook_event_name":"UserPromptSubmit",
            "session_id":"019f1234-5678-7abc-8def-0123456789bb","cwd":cwd.join("resumed-project"),
            "zed_hook_started_at":2402});
        assert!(post_identity(&observer, resumed).starts_with("HTTP/1.1 204"));
        let capture = futures::executor::block_on(observer.events.next())
            .unwrap()
            .unwrap();
        assert_eq!(
            capture.session.exact_resume_id(),
            "019f1234-5678-7abc-8def-0123456789bb"
        );
        assert_eq!(capture.session.resume_cwd(), cwd.join("resumed-project"));
        observer.cancellation.cancel();
    }

    #[test]
    fn codex_observer_orders_provider_submissions_before_hook_dispatch() {
        use futures::{FutureExt as _, StreamExt as _};
        let cwd = std::env::current_dir().unwrap();
        let mut observer = start_codex_identity_observer(cwd.clone()).unwrap();
        for (submission, hook, delivered) in [
            (Some("019f1234-5678-7abc-8def-012345678902"), 2402, true),
            (Some("019f1234-5678-7abc-8def-012345678901"), 9000, false),
            (None, 9001, false),
            (Some("019f1234-5678-7abc-8def-012345678903"), 2401, true),
        ] {
            let payload = json!({"hook_event_name":"UserPromptSubmit",
                "session_id":"019f1234-5678-7abc-8def-0123456789ab","cwd":cwd,
                "zed_hook_started_at":hook,"zed_prompt_submission_id":submission});
            assert!(post_identity(&observer, payload).starts_with("HTTP/1.1 204"));
            if delivered {
                assert!(
                    futures::executor::block_on(observer.events.next())
                        .unwrap()
                        .is_ok()
                );
            } else {
                assert!(observer.events.next().now_or_never().is_none());
            }
        }
        observer.cancellation.cancel();
    }

    #[test]
    fn identity_helper_retries_queue_pressure_without_changing_payload() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let address = server.server_addr().to_ip().unwrap();
        let worker = std::thread::spawn(move || {
            for status in [429, 429, 204] {
                let mut request = server
                    .recv_timeout(Duration::from_secs(2))
                    .unwrap()
                    .unwrap();
                let mut bytes = Vec::new();
                request.as_reader().read_to_end(&mut bytes).unwrap();
                assert_eq!(bytes, b"same stamped identity");
                respond(request, status);
            }
        });
        send_hook_payload(address, "unused", "unused", b"same stamped identity").unwrap();
        worker.join().unwrap();
    }

    #[test]
    fn identity_helper_deadline_bounds_a_stalled_response() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let address = server.server_addr().to_ip().unwrap();
        let (release, released) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let request = server
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .unwrap();
            released.recv_timeout(Duration::from_secs(2)).unwrap();
            drop(request);
        });
        let started = Instant::now();
        let result = send_hook_payload_before(
            address,
            "unused",
            "unused",
            b"{}",
            started + Duration::from_millis(150),
        );
        assert!(result.is_err());
        assert!(started.elapsed() < Duration::from_secs(2));
        release.send(()).unwrap();
        worker.join().unwrap();
    }

    #[test]
    fn hook_caller_pid_skips_wrappers_and_accepts_other_host_names() {
        for host in ["codex.exe", "codex-code-mode-host.exe", "python.exe"] {
            let processes = HashMap::from_iter([
                (1, (Some(2), "zed.exe".to_owned())),
                (2, (Some(3), "CMD.EXE".to_owned())),
                (3, (Some(4), "powershell.exe".to_owned())),
                (4, (Some(5), "pwsh.exe".to_owned())),
                (5, (None, host.to_owned())),
            ]);
            assert_eq!(
                hook_caller_pid(1, |pid| processes.get(&pid).cloned()).unwrap(),
                Some(5),
            );
        }
    }

    #[test]
    fn hook_caller_pid_accepts_a_direct_host() {
        let processes = HashMap::from_iter([
            (1, (Some(2), "zed.exe".to_owned())),
            (2, (None, "another-host.exe".to_owned())),
        ]);
        assert_eq!(
            hook_caller_pid(1, |pid| processes.get(&pid).cloned()).unwrap(),
            Some(2),
        );
    }

    #[test]
    fn hook_caller_pid_handles_missing_and_terminated_parent_chains() {
        let cases = [
            HashMap::default(),
            HashMap::from_iter([(1, (None, "zed.exe".to_owned()))]),
            HashMap::from_iter([(1, (Some(2), "zed.exe".to_owned()))]),
            HashMap::from_iter([
                (1, (Some(2), "zed.exe".to_owned())),
                (2, (None, "cmd.exe".to_owned())),
            ]),
            HashMap::from_iter([(1, (Some(1), "zed.exe".to_owned()))]),
        ];
        for processes in cases {
            assert_eq!(
                hook_caller_pid(1, |pid| processes.get(&pid).cloned()).unwrap(),
                None,
            );
        }
    }

    #[test]
    fn hook_caller_pid_bounds_cycles_and_deep_wrapper_chains() {
        let cycle = HashMap::from_iter([
            (1, (Some(2), "zed.exe".to_owned())),
            (2, (Some(3), "cmd.exe".to_owned())),
            (3, (Some(2), "pwsh.exe".to_owned())),
        ]);
        assert!(hook_caller_pid(1, |pid| cycle.get(&pid).cloned()).is_err());
        assert!(hook_caller_pid(1, |pid| Some((Some(pid + 1), "cmd.exe".to_owned()))).is_err());
    }

    #[test]
    fn hook_lookup_failures_reach_the_observer_without_binding_and_allow_recovery() {
        use futures::StreamExt as _;
        let cwd = std::env::current_dir().unwrap();
        let mut observer = start_codex_identity_observer(cwd.clone()).unwrap();
        let input = json!({
            "hook_event_name":"SessionStart", "source":"startup",
            "session_id":"019f1234-5678-7abc-8def-0123456789ab", "cwd":cwd,
            "prompt":"PRIVATE_PROMPT", "transcript_path":"PRIVATE_PATH",
            "zed_codex_process":{"pid":999,"created_at":999},
        });
        let normalized =
            identity_hook_payload(serde_json::to_vec(&input).unwrap(), &cwd, 1).unwrap();
        for lookup in [Ok(None), Err(anyhow!("simulated process lookup failure"))] {
            let mut identity: Value = serde_json::from_slice(&normalized).unwrap();
            stamp_hook_process_identity(&mut identity, lookup);
            assert!(identity.get("zed_codex_process").is_none());
            let bytes = serde_json::to_vec(&identity).unwrap();
            let serialized = String::from_utf8(bytes.clone()).unwrap();
            assert!(!serialized.contains("PRIVATE_PROMPT"));
            assert!(!serialized.contains("PRIVATE_PATH"));
            let result = send_hook_payload(
                observer.environment[CALLBACK_ADDRESS_ENV].parse().unwrap(),
                &observer.environment[CALLBACK_CAPABILITY_ENV],
                &observer.environment[LAUNCH_ATTEMPT_ENV],
                &bytes,
            );
            assert!(result.is_err());
            let error = futures::executor::block_on(observer.events.next())
                .unwrap()
                .err()
                .unwrap();
            assert!(error.to_string().contains("no process identity"));
        }
        let mut identity: Value = serde_json::from_slice(&normalized).unwrap();
        stamp_hook_process_identity(
            &mut identity,
            Ok(Some(CliProcessIdentity {
                pid: 24,
                created_at: 2401,
            })),
        );
        assert!(
            post_identity_bytes(&observer, &serde_json::to_vec(&identity).unwrap())
                .starts_with("HTTP/1.1 204")
        );
        let capture = futures::executor::block_on(observer.events.next())
            .unwrap()
            .unwrap();
        assert_eq!(
            capture.session.exact_resume_id(),
            input["session_id"].as_str().unwrap()
        );
        observer.cancellation.cancel();
    }

    #[test]
    fn startup_helper_filters_extra_fields_and_keeps_provider_contracts() {
        let cwd = std::env::current_dir().unwrap();
        let id = "019f1234-5678-7abc-8def-0123456789ab";
        let payload = identity_hook_payload(
            serde_json::to_vec(&json!({
                "hook_event_name":"SessionStart","source":"startup","session_id":id,
                "cwd":cwd,"model":"PRIVATE_MODEL","prompt":"PRIVATE_PROMPT",
                "transcript_path":"PRIVATE_PATH","unknown":"PRIVATE_EXTRA",
                "zed_codex_process":{"pid":999,"created_at":999}
            }))
            .unwrap(),
            &cwd,
            2401,
        )
        .unwrap();
        let normalized: Value = serde_json::from_slice(&payload).unwrap();
        assert!(
            !String::from_utf8(payload.clone())
                .unwrap()
                .contains("PRIVATE")
        );
        assert!(normalized["transcript_path"].is_null());
        assert!(normalized.get("zed_codex_process").is_none());
        for provider in [ResumableCliProvider::Claude, ResumableCliProvider::Copilot] {
            let expected = ExpectedCapture {
                provider,
                session_id: Some(id.to_owned()),
                hermes_profile: None,
            };
            let capture = match provider {
                ResumableCliProvider::Claude => parse_claude_capture_payload(&payload, &cwd),
                _ => parse_copilot_capture_payload(&payload, &cwd, &expected),
            }
            .unwrap();
            assert_eq!(capture.session.exact_resume_id(), id);
        }
        let profile = HermesProfileContext {
            home: cwd.clone(),
            profile: "test".into(),
        };
        let hermes_id = "20261006_000001_a1b2c3";
        let hermes = identity_hook_payload(
            serde_json::to_vec(&json!({
                "hook_event_name":"on_session_start","session_id":hermes_id,"cwd":cwd,
                "profile":"test","prompt":"PRIVATE_PROMPT"
            }))
            .unwrap(),
            &cwd,
            2401,
        )
        .unwrap();
        assert_eq!(
            parse_hermes_capture_payload(&hermes, &cwd, &profile)
                .unwrap()
                .session
                .exact_resume_id(),
            hermes_id
        );
        let prompt = identity_hook_payload(
            serde_json::to_vec(&json!({
                "hook_event_name":"UserPromptSubmit","session_id":id,"cwd":cwd,
                "turn_id":"019f1234-5678-7abc-8def-012345678902","prompt":"PRIVATE_PROMPT"
            }))
            .unwrap(),
            &cwd,
            2401,
        )
        .unwrap();
        let prompt: Value = serde_json::from_slice(&prompt).unwrap();
        assert_eq!(
            prompt["zed_prompt_submission_id"],
            "019f1234-5678-7abc-8def-012345678902"
        );
        assert!(prompt.get("prompt").is_none());
    }

    #[test]
    fn codex_observer_ignores_children_and_rejects_unrelated_events() {
        use futures::{FutureExt as _, StreamExt as _};
        let cwd = std::env::current_dir().unwrap();
        let mut observer = start_codex_identity_observer(cwd.clone()).unwrap();
        for extra in [
            json!({"agent_id":"child"}),
            json!({"agent_type":"explorer"}),
        ] {
            let mut payload = json!({"hook_event_name":"UserPromptSubmit",
                "session_id":"019f1234-5678-7abc-8def-0123456789ab","cwd":cwd});
            payload
                .as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            assert!(post_identity(&observer, payload).starts_with("HTTP/1.1 204"));
            assert!(observer.events.next().now_or_never().is_none());
        }
        for extra in [
            json!({"hook_event_name":"Stop"}),
            json!({"source":"unknown"}),
        ] {
            let mut payload = json!({"hook_event_name":"SessionStart","source":"startup",
                "session_id":"019f1234-5678-7abc-8def-0123456789ab","cwd":cwd});
            payload
                .as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            assert!(
                parse_codex_identity(&serde_json::to_vec(&payload).unwrap(), Some(&cwd)).is_err()
            );
        }
        let root = json!({"hook_event_name":"SessionStart","source":"startup",
            "session_id":"019f1234-5678-7abc-8def-0123456789ab","cwd":cwd});
        assert!(post_identity(&observer, root).starts_with("HTTP/1.1 204"));
        assert!(
            futures::executor::block_on(observer.events.next())
                .unwrap()
                .is_ok()
        );
        observer.cancellation.cancel();
    }

    #[test]
    fn identity_helper_tolerates_closed_listeners_and_preserves_authentication_errors() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        assert!(send_hook_payload(address, "unused", "unused", b"{}").is_ok());

        for (status, accepted) in [(204, true), (410, true), (400, false), (401, false)] {
            let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
            let address = server.server_addr().to_ip().unwrap();
            let responder = std::thread::spawn(move || {
                let request = server
                    .recv_timeout(Duration::from_secs(2))
                    .unwrap()
                    .unwrap();
                respond(request, status);
            });
            assert_eq!(
                send_hook_payload(address, "unused", "unused", b"{}").is_ok(),
                accepted
            );
            responder.join().unwrap();
        }
    }

    #[test]
    fn codex_prompt_helper_strips_long_prompts_and_unknown_fields() {
        let cwd = std::env::current_dir().unwrap();
        let input = serde_json::to_vec(&json!({"hook_event_name":"UserPromptSubmit",
            "session_id":"019f1234-5678-7abc-8def-0123456789ab","cwd":cwd,
            "prompt":"PR24_PROMPT_SENTINEL".repeat(5_000), "transcript_path":"PR24_TRANSCRIPT_SENTINEL",
            "agent_id":"child", "agent_type":"explorer", "turn_id":"not-needed"}))
        .unwrap();
        assert!(input.len() as u64 > MAX_PAYLOAD_BYTES);
        let normalized = identity_hook_payload(input, &cwd, 2401).unwrap();
        let value: Value = serde_json::from_slice(&normalized).unwrap();
        assert_eq!(value.as_object().unwrap().len(), 6);
        assert_eq!(value["zed_hook_started_at"], 2401);
        assert_eq!(value["agent_id"], "child");
        assert_eq!(value["agent_type"], "explorer");
        assert!(value.get("prompt").is_none());
        assert!(value.get("transcript_path").is_none());
        assert!(value.get("turn_id").is_none());
        assert_eq!(value["cwd"], json!(cwd));
        assert!(!String::from_utf8(normalized).unwrap().contains("SENTINEL"));
        assert!(
            identity_hook_payload(
                vec![b' '; MAX_CODEX_HOOK_INPUT_BYTES as usize + 1],
                &cwd,
                2401
            )
            .is_err()
        );
    }

    #[test]
    fn codex_observer_drop_or_cancellation_closes_its_listener() {
        let observer = start_codex_identity_observer(std::env::current_dir().unwrap()).unwrap();
        let address = observer.environment[CALLBACK_ADDRESS_ENV].clone();
        drop(observer);
        assert_listener_closes(&address);
        let observer = start_codex_identity_observer(std::env::current_dir().unwrap()).unwrap();
        let address = observer.environment[CALLBACK_ADDRESS_ENV].clone();
        observer.cancellation.cancel();
        assert_listener_closes(&address);
    }

    fn assert_listener_closes(address: &str) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while TcpStream::connect(address).is_ok() {
            assert!(
                Instant::now() < deadline,
                "callback listener remained reachable after cancellation"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn post_session_start(
        address: &str,
        capability: &str,
        launch_attempt: &str,
        cwd: &Path,
    ) -> String {
        let payload = serde_json::to_vec(&json!({
            "cwd": cwd,
            "hook_event_name": "SessionStart",
            "model": "gpt-5",
            "permission_mode": "default",
            "session_id": "019f1234-5678-7abc-8def-0123456789ab",
            "source": "startup",
            "transcript_path": cwd.join("session.jsonl")
        }))
        .unwrap();
        let mut stream = TcpStream::connect(address).unwrap();
        write!(
            stream,
            "POST {CALLBACK_PATH} HTTP/1.1\r\nHost: {address}\r\nX-Zed-Capability: {capability}\r\nX-Zed-Launch-Attempt: {launch_attempt}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            payload.len()
        )
        .unwrap();
        stream.write_all(&payload).unwrap();
        stream.flush().unwrap();

        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        response
    }

    fn post_claude_session_start(
        address: &str,
        capability: &str,
        launch_attempt: &str,
        cwd: &Path,
        session_id: &str,
    ) -> String {
        let payload = serde_json::to_vec(&json!({
            "cwd": cwd,
            "hook_event_name": "SessionStart",
            "model": "claude-opus-4-6",
            "permission_mode": "default",
            "session_id": session_id,
            "source": "startup",
            "transcript_path": cwd.join("session.jsonl"),
            "zed_codex_process": {"pid":24, "created_at":2401},
            "zed_hook_started_at": 2401
        }))
        .unwrap();
        let mut stream = TcpStream::connect(address).unwrap();
        write!(
            stream,
            "POST {CALLBACK_PATH} HTTP/1.1\r\nHost: {address}\r\nX-Zed-Capability: {capability}\r\nX-Zed-Launch-Attempt: {launch_attempt}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            payload.len()
        )
        .unwrap();
        stream.write_all(&payload).unwrap();
        stream.flush().unwrap();

        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        response
    }

    fn post_hermes_session_start(
        address: &str,
        capability: &str,
        launch_attempt: &str,
        cwd: &Path,
        profile: &str,
        session_id: &str,
    ) -> String {
        let payload = serde_json::to_vec(&json!({
            "cwd": cwd,
            "hook_event_name": "on_session_start",
            "profile": profile,
            "session_id": session_id,
            "extra": {}
        }))
        .unwrap();
        let mut stream = TcpStream::connect(address).unwrap();
        write!(
            stream,
            "POST {CALLBACK_PATH} HTTP/1.1\r\nHost: {address}\r\nX-Zed-Capability: {capability}\r\nX-Zed-Launch-Attempt: {launch_attempt}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            payload.len()
        )
        .unwrap();
        stream.write_all(&payload).unwrap();
        stream.flush().unwrap();

        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        response
    }

    #[test]
    fn loopback_capture_rejects_forgery_then_accepts_exact_launch() {
        let cwd = std::env::current_dir().unwrap().canonicalize().unwrap();
        let listener = start_codex_capture(cwd.clone()).unwrap();
        assert!(!listener.cancellation.is_cancelled());
        let address = &listener.environment[CALLBACK_ADDRESS_ENV];
        let capability = &listener.environment[CALLBACK_CAPABILITY_ENV];
        let launch_attempt = &listener.environment[LAUNCH_ATTEMPT_ENV];

        let forged = post_session_start(
            address,
            "019f1234-5678-7abc-8def-0123456789ab",
            launch_attempt,
            &cwd,
        );
        assert!(forged.starts_with("HTTP/1.1 401"));

        let accepted = post_session_start(address, capability, launch_attempt, &cwd);
        assert!(accepted.starts_with("HTTP/1.1 204"));
        let capture = futures::executor::block_on(listener.result)
            .unwrap()
            .unwrap();
        assert_eq!(
            capture.session.exact_resume_id(),
            "019f1234-5678-7abc-8def-0123456789ab"
        );
        assert_eq!(capture.session.resume_cwd(), cwd);
    }

    #[test]
    fn loopback_capture_deadline_closes_listener() {
        let cwd = std::env::current_dir().unwrap().canonicalize().unwrap();
        let (environment, result, _cancellation) =
            start_capture_with_timeout(cwd, None, Duration::from_millis(50)).unwrap();
        let address = environment[CALLBACK_ADDRESS_ENV].clone();

        let error = match futures::executor::block_on(result).unwrap() {
            Ok(_) => panic!("capture unexpectedly succeeded"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("timed out"));
        assert_listener_closes(&address);
    }

    #[test]
    fn cancelling_loopback_capture_closes_listener() {
        let cwd = std::env::current_dir().unwrap().canonicalize().unwrap();
        let (environment, result, cancellation) =
            start_capture_with_timeout(cwd, None, Duration::from_secs(30)).unwrap();
        let address = environment[CALLBACK_ADDRESS_ENV].clone();

        cancellation.cancel();
        assert!(futures::executor::block_on(result).is_err());
        assert_listener_closes(&address);
    }

    #[test]
    fn repeated_cancelled_captures_release_their_ports() {
        let cwd = std::env::current_dir().unwrap().canonicalize().unwrap();
        for _ in 0..8 {
            let (environment, result, cancellation) =
                start_capture_with_timeout(cwd.clone(), None, Duration::from_secs(30)).unwrap();
            let address = environment[CALLBACK_ADDRESS_ENV].clone();

            cancellation.cancel();
            assert!(futures::executor::block_on(result).is_err());
            assert_listener_closes(&address);
        }
    }

    #[test]
    fn invalid_callbacks_do_not_extend_capture_deadline() {
        let cwd = std::env::current_dir().unwrap().canonicalize().unwrap();
        let (environment, result, _cancellation) =
            start_capture_with_timeout(cwd.clone(), None, Duration::from_millis(500)).unwrap();
        let address = &environment[CALLBACK_ADDRESS_ENV];
        let launch_attempt = &environment[LAUNCH_ATTEMPT_ENV];

        let forged = post_session_start(address, "invalid", launch_attempt, &cwd);
        assert!(forged.starts_with("HTTP/1.1 401"));
        let error = match futures::executor::block_on(result).unwrap() {
            Ok(_) => panic!("capture unexpectedly succeeded"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("timed out"));
    }

    #[test]
    fn session_claim_releases_on_drop() {
        let session = ResumableCliSession::new(
            ResumableCliProvider::Codex,
            Uuid::new_v4().to_string(),
            std::env::current_dir().unwrap().canonicalize().unwrap(),
            ResumableCliEvidence::ProviderEvent,
        )
        .unwrap();

        let claim = try_claim_target(&session).expect("first owner should acquire target");
        assert!(try_claim_target(&session).is_none());
        drop(claim);
        assert!(try_claim_target(&session).is_some());
    }

    #[test]
    fn replacement_claim_is_not_released_by_displaced_owner() {
        let session = ResumableCliSession::new(
            ResumableCliProvider::Codex,
            Uuid::new_v4().to_string(),
            std::env::current_dir().unwrap().canonicalize().unwrap(),
            ResumableCliEvidence::ProviderEvent,
        )
        .unwrap();

        let first = try_claim_target(&session).expect("first owner should acquire target");
        let replacement = replace_target_claim(&session);
        drop(first);
        assert!(try_claim_target(&session).is_none());
        drop(replacement);
        assert!(try_claim_target(&session).is_some());
    }

    #[test]
    fn claude_loopback_capture_requires_launch_authentication() {
        let cwd = std::env::current_dir().unwrap().canonicalize().unwrap();
        let mut listener = start_claude_capture(cwd.clone()).unwrap();
        let address = &listener.observer.environment[CALLBACK_ADDRESS_ENV];
        let capability = &listener.observer.environment[CALLBACK_CAPABILITY_ENV];
        let launch_attempt = &listener.observer.environment[LAUNCH_ATTEMPT_ENV];

        let forged = post_claude_session_start(
            address,
            "019f1234-5678-7abc-8def-0123456789ab",
            launch_attempt,
            &cwd,
            &listener.session_id,
        );
        assert!(forged.starts_with("HTTP/1.1 401"));

        let accepted = post_claude_session_start(
            address,
            capability,
            launch_attempt,
            &cwd,
            &listener.session_id,
        );
        assert!(accepted.starts_with("HTTP/1.1 204"));
        let capture =
            futures::executor::block_on(futures::StreamExt::next(&mut listener.observer.events))
                .unwrap()
                .unwrap();
        assert_eq!(capture.session.provider(), ResumableCliProvider::Claude);
        assert_eq!(capture.session.exact_resume_id(), listener.session_id);
    }

    #[test]
    fn hermes_capture_accepts_first_turn_after_launch_timeout() {
        let cwd = std::env::current_dir().unwrap().canonicalize().unwrap();
        let profile = HermesProfileContext {
            home: cwd.clone(),
            profile: "default".into(),
        };
        let mut listener = start_hermes_capture(cwd.clone(), profile).unwrap();

        // Exercise the production launch deadline: a user may leave Hermes at
        // its prompt longer than other providers take to create a session.
        std::thread::sleep(CALLBACK_TIMEOUT + Duration::from_secs(1));
        assert!(matches!(listener.result.try_recv(), Ok(None)));
        let id = "20261009_100001_a1b2c3";
        let response = post_hermes_session_start(
            &listener.environment[CALLBACK_ADDRESS_ENV],
            &listener.environment[CALLBACK_CAPABILITY_ENV],
            &listener.environment[LAUNCH_ATTEMPT_ENV],
            &cwd,
            "default",
            id,
        );
        assert!(response.starts_with("HTTP/1.1 204"));
        let capture = futures::executor::block_on(listener.result)
            .unwrap()
            .unwrap();
        assert_eq!(capture.session.provider(), ResumableCliProvider::Hermes);
        assert_eq!(capture.session.exact_resume_id(), id);
        assert_eq!(capture.session.resume_cwd(), cwd);
        assert_eq!(capture.session.provider_home(), Some(cwd.as_path()));
        assert_eq!(capture.session.provider_profile(), Some("default"));
        assert_listener_closes(&listener.environment[CALLBACK_ADDRESS_ENV]);
    }

    #[test]
    fn idle_hermes_capture_closes_when_cancelled_or_receiver_dropped() {
        let cwd = std::env::current_dir().unwrap().canonicalize().unwrap();
        for cancel in [true, false] {
            let profile = HermesProfileContext {
                home: cwd.clone(),
                profile: "default".into(),
            };
            let listener = start_hermes_capture(cwd.clone(), profile).unwrap();
            let address = listener.environment[CALLBACK_ADDRESS_ENV].clone();
            if cancel {
                listener.cancellation.cancel();
                assert!(futures::executor::block_on(listener.result).is_err());
            } else {
                drop(listener.result);
            }
            assert_listener_closes(&address);
        }
    }

    #[test]
    fn hermes_loopback_capture_requires_the_expected_profile_and_persists_its_home() {
        let cwd = std::env::current_dir().unwrap().canonicalize().unwrap();
        let default_profile = HermesProfileContext {
            home: cwd.clone(),
            profile: "default".into(),
        };
        let listener = start_hermes_capture(cwd.clone(), default_profile).unwrap();
        let address = &listener.environment[CALLBACK_ADDRESS_ENV];
        let capability = &listener.environment[CALLBACK_CAPABILITY_ENV];
        let launch_attempt = &listener.environment[LAUNCH_ATTEMPT_ENV];
        let id = "20260922_123456_a1b2c3";

        let rejected =
            post_hermes_session_start(address, capability, launch_attempt, &cwd, "named", id);
        assert!(rejected.starts_with("HTTP/1.1 400"));
        let capture = futures::executor::block_on(listener.result).unwrap();
        match capture {
            Ok(_) => panic!("unexpected Hermes profile capture was accepted"),
            Err(error) => assert!(error.to_string().contains("does not match")),
        }

        let named_home = cwd.join("profiles").join("named");
        let named_profile = HermesProfileContext {
            home: named_home.clone(),
            profile: "named".into(),
        };
        let listener = start_hermes_capture(cwd.clone(), named_profile).unwrap();
        let accepted = post_hermes_session_start(
            &listener.environment[CALLBACK_ADDRESS_ENV],
            &listener.environment[CALLBACK_CAPABILITY_ENV],
            &listener.environment[LAUNCH_ATTEMPT_ENV],
            &cwd,
            "named",
            id,
        );
        assert!(accepted.starts_with("HTTP/1.1 204"));
        let capture = futures::executor::block_on(listener.result)
            .unwrap()
            .unwrap();
        assert_eq!(capture.session.provider(), ResumableCliProvider::Hermes);
        assert_eq!(capture.session.exact_resume_id(), id);
        assert_eq!(capture.session.provider_home(), Some(named_home.as_path()));
        assert_eq!(capture.session.provider_profile(), Some("named"));
    }

    #[test]
    fn claude_capture_accepts_session_title_metadata_without_weakening_identity_checks() {
        let cwd = std::env::current_dir().unwrap().canonicalize().unwrap();
        let session_id = Uuid::new_v4().to_string();
        let mut payload = json!({
            "cwd": cwd,
            "hook_event_name": "SessionStart",
            "session_id": session_id,
            "source": "startup",
            "transcript_path": cwd.join("session.jsonl"),
            "session_title": "zed-capture-probe"
        });
        let capture =
            parse_claude_capture_payload(&serde_json::to_vec(&payload).unwrap(), &cwd).unwrap();
        assert_eq!(capture.session.exact_resume_id(), session_id);
        for (field, value) in [
            ("session_id", json!("invalid")),
            ("source", json!("unexpected")),
            ("hook_event_name", json!("SessionEnd")),
            ("cwd", json!(cwd.join("different"))),
        ] {
            let previous = payload[field].clone();
            payload[field] = value;
            assert!(
                parse_claude_capture_payload(&serde_json::to_vec(&payload).unwrap(), &cwd).is_err(),
                "invalid {field} was accepted"
            );
            payload[field] = previous;
        }
    }

    #[test]
    fn codex_commands_keep_session_hooks_out_of_the_shared_daemon() {
        assert_eq!(codex_new_command(), b"codex --no-daemon\r");
        let cwd = std::env::current_dir().unwrap().canonicalize().unwrap();
        let session_id = Uuid::new_v4().to_string();
        let session = ResumableCliSession::new(
            ResumableCliProvider::Codex,
            session_id.clone(),
            cwd,
            ResumableCliEvidence::ProviderEvent,
        )
        .unwrap();
        assert_eq!(
            codex_resume_command(&session).unwrap(),
            format!("codex --no-daemon resume {session_id}\r").into_bytes()
        );
    }

    #[test]
    fn incomplete_callback_context_is_reported_without_exposing_credentials() {
        assert!(callback_environment(|_| Ok(None)).unwrap().is_none());
        for missing in [
            CALLBACK_ADDRESS_ENV,
            CALLBACK_CAPABILITY_ENV,
            LAUNCH_ATTEMPT_ENV,
        ] {
            let error = callback_environment(|name| {
                Ok((name != missing).then(|| "private-callback-value".to_owned()))
            })
            .unwrap_err();
            let message = format!("{error:#}");
            assert!(message.contains(missing));
            assert!(!message.contains("private-callback-value"));
        }
    }

    #[test]
    fn capture_failure_preserves_provider_location_and_safe_parser_details() {
        let error = anyhow!("missing field hook_event_name")
            .context("parsing Claude SessionStart callback");
        let message = capture_error_message(
            ResumableCliProvider::Claude,
            Some("Center 2"),
            Some("review"),
            &error,
        );
        assert!(message.contains("Claude session identity capture failed"));
        assert!(message.contains("pane: Center 2, terminal: review"));
        assert!(
            message.contains("parsing Claude SessionStart callback: missing field hook_event_name")
        );
        for provider in [
            ResumableCliProvider::Codex,
            ResumableCliProvider::Claude,
            ResumableCliProvider::Copilot,
            ResumableCliProvider::Hermes,
        ] {
            let message = capture_error_message(provider, Some(" "), None, &anyhow!("timed out"));
            assert!(message.starts_with(provider.display_name()));
            assert!(!message.contains("pane:"));
        }
    }

    #[test]
    fn concurrent_captures_cannot_exchange_session_identity() {
        let cwd = std::env::current_dir().unwrap().canonicalize().unwrap();
        let first = start_claude_capture(cwd.clone()).unwrap();
        let second = start_claude_capture(cwd.clone()).unwrap();
        let forged = post_claude_session_start(
            &first.observer.environment[CALLBACK_ADDRESS_ENV],
            &second.observer.environment[CALLBACK_CAPABILITY_ENV],
            &second.observer.environment[LAUNCH_ATTEMPT_ENV],
            &cwd,
            &second.session_id,
        );
        assert!(forged.starts_with("HTTP/1.1 401"));
        for mut listener in [first, second] {
            let accepted = post_claude_session_start(
                &listener.observer.environment[CALLBACK_ADDRESS_ENV],
                &listener.observer.environment[CALLBACK_CAPABILITY_ENV],
                &listener.observer.environment[LAUNCH_ATTEMPT_ENV],
                &cwd,
                &listener.session_id,
            );
            assert!(accepted.starts_with("HTTP/1.1 204"));
            let capture = futures::executor::block_on(futures::StreamExt::next(
                &mut listener.observer.events,
            ))
            .unwrap()
            .unwrap();
            assert_eq!(capture.session.exact_resume_id(), listener.session_id);
        }
    }

    #[test]
    fn capture_timeout_identifies_the_provider_and_closes_late_delivery() {
        assert_eq!(CALLBACK_TIMEOUT, Duration::from_secs(30));
        let cwd = std::env::current_dir().unwrap().canonicalize().unwrap();
        for provider in [
            ResumableCliProvider::Codex,
            ResumableCliProvider::Claude,
            ResumableCliProvider::Copilot,
            ResumableCliProvider::Hermes,
        ] {
            let (environment, result, _cancellation) = start_capture_with_timeout(
                cwd.clone(),
                Some(ExpectedCapture {
                    provider,
                    session_id: None,
                    hermes_profile: None,
                }),
                Duration::from_millis(10),
            )
            .unwrap();
            let error = match futures::executor::block_on(result).unwrap() {
                Ok(_) => panic!("capture unexpectedly succeeded"),
                Err(error) => error,
            };
            let message = error.to_string();
            assert!(message.starts_with(provider.display_name()));
            assert!(message.contains("timed out after 0.01 seconds"));
            assert_listener_closes(&environment[CALLBACK_ADDRESS_ENV]);
        }
    }

    #[test]
    fn merges_without_replacing_unrelated_codex_hooks() {
        let existing = r#"{
            "description": "mine",
            "hooks": {
                "SessionStart": [{
                    "matcher": "startup",
                    "hooks": [{"type": "command", "commandWindows": "mine.exe"}]
                }]
            }
        }"#;
        let merged = merge_codex_hook(Some(existing)).unwrap();
        let value: Value = serde_json::from_str(&merged.content).unwrap();
        assert!(merged.changed);
        assert_eq!(value["description"], "mine");
        assert_eq!(value["hooks"]["SessionStart"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn prompt_hook_merge_preserves_unrelated_hooks_and_removes_filtered_owned_duplicates() {
        let existing = json!({"description":"keep", "hooks":{"UserPromptSubmit":[
            {"matcher":"filtered","hooks":[
                {"type":"command","commandWindows":"old-zed.exe --resumable-cli-hook"},
                {"type":"command","commandWindows":"mine.exe"}]},
            {"hooks":[{"type":"command","commandWindows":"duplicate.exe --resumable-cli-hook"}]}
        ]}})
        .to_string();
        let merged = merge_codex_hook(Some(&existing)).unwrap();
        let value: Value = serde_json::from_str(&merged.content).unwrap();
        assert_eq!(value["description"], "keep");
        let groups = value["hooks"]["UserPromptSubmit"].as_array().unwrap();
        assert_eq!(groups[0]["matcher"], "filtered");
        assert_eq!(groups[0]["hooks"].as_array().unwrap().len(), 1);
        assert_eq!(groups[0]["hooks"][0]["commandWindows"], "mine.exe");
        assert_eq!(
            groups
                .iter()
                .flat_map(|group| group["hooks"].as_array().unwrap())
                .filter(|hook| is_owned_hook(hook))
                .count(),
            1
        );
        assert!(groups[1].get("matcher").is_none());
        let again = merge_codex_hook(Some(&merged.content)).unwrap();
        assert!(!again.changed);
        assert_eq!(again.content, merged.content);
    }

    #[test]
    fn merge_is_idempotent() {
        let first = merge_codex_hook(None).unwrap();
        let second = merge_codex_hook(Some(&first.content)).unwrap();
        assert!(!second.changed);
        assert_eq!(first.content, second.content);
    }

    #[test]
    fn windows_hook_uses_an_explicit_shell_independent_launcher() {
        let merged = merge_codex_hook(None).unwrap();
        let value: Value = serde_json::from_str(&merged.content).unwrap();
        let hook = &value["hooks"]["SessionStart"][0]["hooks"][0];
        let command = hook["commandWindows"].as_str().unwrap();

        assert!(
            command.starts_with("cmd.exe /D /V:OFF /S /C ")
                || (std::env::current_exe()
                    .unwrap()
                    .to_string_lossy()
                    .contains('%')
                    && command.starts_with("powershell.exe -NoProfile -NonInteractive -Command "))
        );
        assert!(command.contains(HOOK_HELPER_ARGUMENT));
        assert!(is_owned_hook(hook));
    }

    #[test]
    fn merge_updates_only_a_stale_owned_hook() {
        let existing = r#"{
            "hooks": {
                "SessionStart": [{
                    "matcher": "^(startup|resume)$",
                    "hooks": [
                        {"type": "command", "commandWindows": "\"old-zed.exe\" --resumable-cli-hook", "timeout": 5},
                        {"type": "command", "commandWindows": "mine.exe"}
                    ]
                }]
            }
        }"#;
        let merged = merge_codex_hook(Some(existing)).unwrap();
        let value: Value = serde_json::from_str(&merged.content).unwrap();
        let hooks = value["hooks"]["SessionStart"][0]["hooks"]
            .as_array()
            .unwrap();
        assert!(merged.changed);
        assert!(is_owned_hook(&hooks[0]));
        assert_eq!(hooks[1]["commandWindows"], "mine.exe");
    }

    #[test]
    fn merge_removes_duplicate_owned_hooks_across_groups() {
        let existing = r#"{
            "hooks": {
                "SessionStart": [
                    {
                        "matcher": "startup",
                        "hooks": [
                            {"type": "command", "commandWindows": "\"old-zed.exe\" --resumable-cli-hook"},
                            {"type": "command", "commandWindows": "mine.exe"}
                        ]
                    },
                    {
                        "matcher": "resume",
                        "hooks": [
                            {"type": "command", "commandWindows": "\"other-zed.exe\" --resumable-cli-hook"},
                            {"type": "command", "commandWindows": "theirs.exe"}
                        ]
                    }
                ]
            }
        }"#;
        let merged = merge_codex_hook(Some(existing)).unwrap();
        let value: Value = serde_json::from_str(&merged.content).unwrap();
        let groups = value["hooks"]["SessionStart"].as_array().unwrap();
        let owned_count = groups
            .iter()
            .flat_map(|group| group["hooks"].as_array().unwrap())
            .filter(|hook| is_owned_hook(hook))
            .count();
        assert!(merged.changed);
        assert_eq!(owned_count, 1);
        assert_eq!(groups[0]["hooks"][1]["commandWindows"], "mine.exe");
        assert_eq!(groups[1]["hooks"][0]["commandWindows"], "theirs.exe");
    }

    #[test]
    fn merges_hermes_hook_without_replacing_unrelated_config() {
        let existing = r#"
model:
  provider: anthropic
hooks:
  on_session_start:
    - command: mine.exe
      timeout: 12
  post_tool_call:
    - command: formatter.exe
"#;
        let merged = merge_hermes_hook(Some(existing)).unwrap();
        let value: serde_yaml::Value = serde_yaml::from_str(&merged.content).unwrap();
        let session_hooks = value["hooks"]["on_session_start"].as_sequence().unwrap();

        assert!(merged.changed);
        assert_eq!(value["model"]["provider"], "anthropic");
        assert_eq!(
            value["hooks"]["post_tool_call"][0]["command"],
            "formatter.exe"
        );
        assert_eq!(session_hooks.len(), 2);
        assert_eq!(session_hooks[0]["command"], "mine.exe");
        assert!(is_owned_hermes_hook(&session_hooks[1]));
    }

    #[test]
    fn hermes_hook_merge_is_idempotent_and_removes_owned_duplicates() {
        let first = merge_hermes_hook(None).unwrap();
        let second = merge_hermes_hook(Some(&first.content)).unwrap();
        assert!(!second.changed);
        assert_eq!(first.content, second.content);

        let existing = "hooks:\n  on_session_start:\n    - command: old-zed.exe --resumable-cli-hook\n      timeout: 9\n    - command: mine.exe\n    - command: other-zed.exe --resumable-cli-hook\n      timeout: 12\n";
        let merged = merge_hermes_hook(Some(existing)).unwrap();
        let value: serde_yaml::Value = serde_yaml::from_str(&merged.content).unwrap();
        let hooks = value["hooks"]["on_session_start"].as_sequence().unwrap();
        let owned = value["hooks"]["on_session_start"]
            .as_sequence()
            .unwrap()
            .iter()
            .filter(|hook| is_owned_hermes_hook(hook))
            .count();
        assert!(merged.changed);
        assert_eq!(owned, 1);
        assert_eq!(hooks.len(), 2);
        assert_eq!(hooks[1]["command"], "mine.exe");
        assert_eq!(hooks[0]["timeout"], 5);
    }

    #[test]
    fn hermes_hook_merge_rejects_malformed_config_shapes() {
        assert!(merge_hermes_hook(Some("[not, a, mapping]")).is_err());
        assert!(merge_hermes_hook(Some("hooks: []")).is_err());
        assert!(merge_hermes_hook(Some("hooks:\n  on_session_start: {}\n")).is_err());
    }

    #[test]
    fn resume_command_contains_only_the_validated_exact_id() {
        let id = "019f1234-5678-7abc-8def-0123456789ab";
        let session = ResumableCliSession::new(
            ResumableCliProvider::Codex,
            id.into(),
            PathBuf::from(r"C:\workspace"),
            ResumableCliEvidence::ProviderEvent,
        )
        .unwrap();
        assert_eq!(
            codex_resume_command(&session).unwrap(),
            format!("codex --no-daemon resume {id}\r").into_bytes()
        );
    }

    #[test]
    fn accepts_the_versioned_codex_session_start_payload() {
        let cwd = PathBuf::from(r"C:\workspace");
        let payload = br#"{
            "cwd":"C:\\workspace",
            "hook_event_name":"SessionStart",
            "model":"gpt-5",
            "permission_mode":"default",
            "session_id":"019f1234-5678-7abc-8def-0123456789ab",
            "source":"startup",
            "transcript_path":"C:\\private\\session.jsonl"
        }"#;
        let capture = parse_capture_payload(payload, &cwd).unwrap();
        assert_eq!(
            capture.session.exact_resume_id(),
            "019f1234-5678-7abc-8def-0123456789ab"
        );
        assert_eq!(capture.session.resume_cwd(), cwd);
    }

    #[test]
    fn rejects_changed_or_misattributed_codex_payloads() {
        let cwd = PathBuf::from(r"C:\workspace");
        let unknown_field = br#"{
            "cwd":"C:\\workspace",
            "hook_event_name":"SessionStart",
            "model":"gpt-5",
            "permission_mode":"default",
            "session_id":"019f1234-5678-7abc-8def-0123456789ab",
            "source":"startup",
            "transcript_path":"C:\\private\\session.jsonl",
            "unexpected":true
        }"#;
        assert!(parse_capture_payload(unknown_field, &cwd).is_err());

        let wrong_cwd = br#"{
            "cwd":"C:\\another-workspace",
            "hook_event_name":"SessionStart",
            "model":"gpt-5",
            "permission_mode":"default",
            "session_id":"019f1234-5678-7abc-8def-0123456789ab",
            "source":"startup",
            "transcript_path":"C:\\private\\session.jsonl"
        }"#;
        assert!(parse_capture_payload(wrong_cwd, &cwd).is_err());
    }

    #[test]
    fn accepts_supported_codex_versions() {
        assert!(is_supported_codex_version("0.154.0"));
        assert!(is_supported_codex_version("0.155.1"));
        assert!(is_supported_codex_version("0.200.0"));

        assert!(!is_supported_codex_version("0.153.99"));
        assert!(!is_supported_codex_version("1.0.0"));
        assert!(!is_supported_codex_version("0.155"));
        assert!(!is_supported_codex_version("0.155.1-beta"));
    }

    #[test]
    fn claude_identity_requires_a_valid_session_and_launch_context() {
        let cwd = PathBuf::from(r"C:\workspace");
        let id = "019f1234-5678-7abc-8def-0123456789ab";
        let payload = br#"{
            "cwd":"C:\\workspace",
            "hook_event_name":"SessionStart",
            "model":"claude-opus-4-6",
            "permission_mode":"default",
            "session_id":"019f1234-5678-7abc-8def-0123456789ab",
            "source":"startup",
            "transcript_path":"C:\\private\\session.jsonl"
        }"#;
        let capture = parse_claude_capture_payload(payload, &cwd).unwrap();
        assert_eq!(capture.session.provider(), ResumableCliProvider::Claude);
        assert_eq!(capture.session.exact_resume_id(), id);
        let mut invalid: Value = serde_json::from_slice(payload).unwrap();
        invalid["session_id"] = json!("invalid");
        assert!(
            parse_claude_capture_payload(&serde_json::to_vec(&invalid).unwrap(), &cwd).is_err()
        );
    }

    #[test]
    fn accepts_copilot_session_without_claude_only_fields() {
        let cwd = PathBuf::from(r"C:\workspace");
        let id = "019f1234-5678-7abc-8def-0123456789ab";
        let expected = ExpectedCapture {
            provider: ResumableCliProvider::Copilot,
            session_id: Some(id.into()),
            hermes_profile: None,
        };
        let payload = br#"{
            "cwd":"C:\\workspace",
            "hook_event_name":"SessionStart",
            "session_id":"019f1234-5678-7abc-8def-0123456789ab",
            "timestamp":"2026-09-22T00:00:00Z"
        }"#;

        let capture = parse_reserved_capture_payload(payload, &cwd, &expected).unwrap();
        assert_eq!(capture.session.provider(), ResumableCliProvider::Copilot);
        assert_eq!(capture.session.exact_resume_id(), id);
    }

    #[test]
    fn claude_commands_contain_only_validated_launch_arguments() {
        let id = "019f1234-5678-7abc-8def-0123456789ab";
        let plugin = Path::new(r"C:\Temp\zed plugin");
        assert_eq!(
            claude_new_command(id, plugin).unwrap(),
            format!(
                "claude --session-id {id} --plugin-dir '{}'\r",
                plugin.display()
            )
            .into_bytes()
        );
        let session = ResumableCliSession::new(
            ResumableCliProvider::Claude,
            id.into(),
            PathBuf::from(r"C:\workspace"),
            ResumableCliEvidence::ProviderEvent,
        )
        .unwrap();
        assert_eq!(
            claude_resume_command(&session).unwrap(),
            format!("claude --resume {id}\r").into_bytes()
        );
    }

    #[cfg(windows)]
    #[test]
    fn claude_plugin_paths_are_literal_powershell_arguments() {
        let id = "019f1234-5678-7abc-8def-0123456789ab";
        for path in [
            r"C:\Temp\ordinary plugin",
            r"C:\Temp\O'Brien",
            r"C:\Temp\$(throw 'evaluated')",
            r"C:\Temp\$env:USERNAME",
            r"C:\Temp\back`tick",
        ] {
            let command =
                String::from_utf8(claude_new_command(id, Path::new(path)).unwrap()).unwrap();
            let script = format!("function claude {{ [Console]::Write($args[3]) }}; {command}");
            let output = std::process::Command::new("powershell.exe")
                .args(["-NoProfile", "-NonInteractive", "-Command", &script])
                .output()
                .expect("PowerShell is required for the Windows command regression");
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(String::from_utf8(output.stdout).unwrap(), path);
        }
        for path in [
            concat!(r"C:\Temp\line", "\nnext"),
            concat!(r"C:\Temp\line", "\rnext"),
            concat!(r"C:\Temp\nul", "\0next"),
        ] {
            assert!(claude_new_command(id, Path::new(path)).is_err());
        }
        assert!(
            !String::from_utf8(copilot_new_command(id).unwrap())
                .unwrap()
                .contains("plugin-dir")
        );
    }

    #[test]
    fn accepts_supported_claude_versions() {
        assert!(is_supported_claude_version("2.1.218"));
        assert!(is_supported_claude_version("2.2.0"));
        assert!(!is_supported_claude_version("2.1.217"));
        assert!(!is_supported_claude_version("3.0.0"));
        assert_eq!(
            claude_version_from_output("2.1.218 (Claude Code)"),
            Some("2.1.218")
        );
        assert_eq!(
            claude_version_from_output("Claude Code 2.1.218"),
            Some("2.1.218")
        );
    }

    #[test]
    fn claude_observer_captures_resumed_conversations_and_retains_plugin() {
        use futures::StreamExt as _;
        let cwd = std::env::current_dir().unwrap();
        let listener = start_claude_capture(cwd.clone()).unwrap();
        let path = listener.plugin_path.clone();
        let mut observer = listener.observer;
        for (source, id) in [
            ("startup", listener.session_id.as_str()),
            ("resume", "b969468e-7853-45b5-8c67-c6de21692503"),
        ] {
            let response = post_identity(
                &observer,
                json!({
                    "hook_event_name": "SessionStart", "source": source,
                    "session_id": id, "cwd": cwd, "zed_hook_started_at": 2501,
                }),
            );
            assert!(response.starts_with("HTTP/1.1 204"));
            let identity = futures::executor::block_on(observer.events.next())
                .unwrap()
                .unwrap();
            assert_eq!(identity.session.provider(), ResumableCliProvider::Claude);
            assert_eq!(identity.session.exact_resume_id(), id);
            assert!(path.is_dir());
        }
        let resumed = ResumableCliSession::new(
            ResumableCliProvider::Claude,
            listener.session_id,
            cwd,
            ResumableCliEvidence::ProviderEvent,
        )
        .unwrap();
        let command =
            String::from_utf8(claude_resume_command_with_plugin(&resumed, &path).unwrap()).unwrap();
        assert!(command.contains("--resume"));
        assert!(command.contains("--plugin-dir"));
        observer.cancellation.cancel();
        drop(observer);
        assert!(!path.exists());
    }

    #[test]
    fn claude_session_plugin_has_the_validated_layout_and_is_ephemeral() {
        let plugin = create_claude_session_plugin().unwrap();
        let path = plugin.path.clone();
        let manifest: Value = serde_json::from_slice(
            &fs::read(path.join(".claude-plugin").join("plugin.json")).unwrap(),
        )
        .unwrap();
        let hooks: Value =
            serde_json::from_slice(&fs::read(path.join("hooks").join("hooks.json")).unwrap())
                .unwrap();
        let hook = &hooks["hooks"]["SessionStart"][0];

        assert_eq!(manifest["name"], "zed-session-capture");
        assert_eq!(hook["matcher"], "^(startup|resume|clear|compact|fork)$");
        assert_eq!(hook["hooks"][0]["type"], "command");
        assert_eq!(hook["hooks"][0]["args"][0], HOOK_HELPER_ARGUMENT);
        drop(plugin);
        assert!(!path.exists());
    }

    #[test]
    fn copilot_workspace_capture_validates_identity_directory_and_size() {
        let store = std::env::temp_dir().join(format!("zed-copilot-capture-{}", Uuid::new_v4()));
        fs::create_dir_all(&store).unwrap();
        let marker = store.join("workspace.yaml");
        let id = Uuid::new_v4().to_string();
        assert!(
            read_copilot_workspace(&marker, &id, &store)
                .unwrap()
                .is_none()
        );
        fs::write(&marker, "id:").unwrap();
        assert!(
            read_copilot_workspace(&marker, &id, &store)
                .unwrap()
                .is_none()
        );
        let metadata =
            |id: &str, cwd: &Path| serde_yaml::to_string(&json!({"id": id, "cwd": cwd})).unwrap();
        fs::write(&marker, metadata(&Uuid::new_v4().to_string(), &store)).unwrap();
        assert!(read_copilot_workspace(&marker, &id, &store).is_err());
        fs::write(&marker, metadata(&id, &store.join("other"))).unwrap();
        assert!(read_copilot_workspace(&marker, &id, &store).is_err());
        fs::write(&marker, vec![b' '; MAX_PAYLOAD_BYTES as usize + 1]).unwrap();
        assert!(read_copilot_workspace(&marker, &id, &store).is_err());
        fs::write(&marker, metadata(&id, &store)).unwrap();
        let capture = read_copilot_workspace(&marker, &id, &store)
            .unwrap()
            .unwrap();
        assert_eq!(capture.session.exact_resume_id(), id);
        assert_eq!(
            capture.session.evidence(),
            ResumableCliEvidence::ProviderWorkspace
        );
        fs::remove_dir_all(store).unwrap();
    }

    #[test]
    fn copilot_captures_an_idle_session_without_a_hook_or_first_prompt() {
        let store = std::env::temp_dir().join(format!("zed-copilot-idle-{}", Uuid::new_v4()));
        let id = Uuid::new_v4().to_string();
        let listener = start_copilot_capture_in(
            store.clone(),
            id.clone(),
            store.clone(),
            Duration::from_secs(3),
        )
        .unwrap();
        let marker = store.join("session-state").join(&id).join("workspace.yaml");
        fs::create_dir_all(marker.parent().unwrap()).unwrap();
        fs::write(
            &marker,
            serde_yaml::to_string(&json!({"id": id, "cwd": store})).unwrap(),
        )
        .unwrap();
        let capture = futures::executor::block_on(listener.result)
            .unwrap()
            .unwrap();
        assert_eq!(capture.session.exact_resume_id(), id);
        assert!(
            start_copilot_capture_in(store.clone(), id, store.clone(), Duration::from_secs(3))
                .is_err()
        );
        fs::remove_dir_all(store).unwrap();
    }

    #[test]
    fn copilot_record_capture_cancels_and_times_out_without_binding() {
        let store = std::env::temp_dir().join(format!("zed-copilot-cancel-{}", Uuid::new_v4()));
        let id = Uuid::new_v4().to_string();
        let listener =
            start_copilot_capture_in(store.clone(), id, store.clone(), Duration::from_secs(3))
                .unwrap();
        listener.cancellation.cancel();
        assert!(futures::executor::block_on(listener.result).is_err());
        let listener = start_copilot_capture_in(
            store.clone(),
            Uuid::new_v4().to_string(),
            store,
            Duration::from_millis(50),
        )
        .unwrap();
        let error = futures::executor::block_on(listener.result)
            .unwrap()
            .err()
            .unwrap();
        assert!(
            error
                .to_string()
                .contains("Copilot session identity record timed out")
        );
    }

    #[test]
    fn copilot_commands_require_an_exact_existing_session() {
        let id = "019f1234-5678-7abc-8def-0123456789ab";
        assert_eq!(
            copilot_new_command(id).unwrap(),
            format!("copilot --session-id {id}\r").into_bytes()
        );

        let session = ResumableCliSession::new(
            ResumableCliProvider::Copilot,
            id.into(),
            PathBuf::from(r"C:\workspace"),
            ResumableCliEvidence::ProviderEvent,
        )
        .unwrap();
        let store = std::env::temp_dir().join(format!("zed-copilot-store-{}", Uuid::new_v4()));
        assert!(verify_copilot_session_exists_in(&session, &store).is_err());

        let session_dir = store.join("session-state").join(id);
        fs::create_dir_all(&session_dir).unwrap();
        fs::write(session_dir.join("workspace.yaml"), "cwd: C:\\workspace\n").unwrap();
        verify_copilot_session_exists_in(&session, &store).unwrap();
        fs::remove_dir_all(store).unwrap();
    }

    #[test]
    fn accepts_supported_copilot_versions() {
        assert!(is_supported_copilot_version("1.0.86"));
        assert!(is_supported_copilot_version("1.0.88"));
        assert!(is_supported_copilot_version("1.1.0"));
        assert!(!is_supported_copilot_version("1.0.85"));
        assert!(!is_supported_copilot_version("2.0.0"));
        assert_eq!(
            claude_version_from_output(
                "GitHub Copilot CLI 1.0.88.\nRun 'copilot update' to check for updates."
            ),
            Some("1.0.88")
        );
    }

    #[test]
    fn hermes_commands_require_a_canonical_existing_session() {
        assert!(is_supported_hermes_version("0.21.1"));
        assert!(is_supported_hermes_version("0.22.0"));
        assert!(!is_supported_hermes_version("0.21.0"));
        assert!(!is_supported_hermes_version("1.0.0"));
        assert_eq!(
            hermes_new_command(&HermesProfileContext {
                home: PathBuf::from(r"C:\Hermes"),
                profile: "default".into(),
            }),
            b"hermes -p default\r"
        );
        assert_eq!(
            hermes_new_command(&HermesProfileContext {
                home: PathBuf::from(r"C:\CustomHermes"),
                profile: "custom".into(),
            }),
            b"hermes\r"
        );
        assert_eq!(
            hermes_new_command(&HermesProfileContext {
                home: PathBuf::from(r"C:\Hermes\profiles\custom"),
                profile: "custom".into(),
            }),
            b"hermes -p custom\r"
        );

        let id = "20260922_123456_a1b2c3";
        let session = ResumableCliSession::new(
            ResumableCliProvider::Hermes,
            id.into(),
            PathBuf::from(r"C:\workspace"),
            ResumableCliEvidence::ProviderEvent,
        )
        .unwrap();
        let home = std::env::temp_dir().join(format!("zed-hermes-store-{}", Uuid::new_v4()));
        fs::create_dir_all(&home).unwrap();
        assert!(verify_hermes_session_exists_in(&session, &home).is_err());

        let db_path = home.join("state.db");
        let connection = db::sqlez::connection::Connection::open_file(&db_path.to_string_lossy());
        connection
            .exec("CREATE TABLE sessions (id TEXT PRIMARY KEY)")
            .unwrap()()
        .unwrap();
        assert!(verify_hermes_session_exists_in(&session, &home).is_err());
        connection
            .exec(&format!("INSERT INTO sessions (id) VALUES ('{id}')"))
            .unwrap()()
        .unwrap();
        drop(connection);

        verify_hermes_session_exists_in(&session, &home).unwrap();
        fs::remove_dir_all(home).unwrap();
    }
}

#[allow(
    clippy::disallowed_methods,
    reason = "provider contract checks run on the background executor"
)]
pub(crate) fn verify_codex_contract() -> Result<()> {
    let output = Command::new("cmd.exe")
        .args(["/D", "/S", "/C", "codex --version"])
        .output()
        .context("Codex is not installed or is unavailable on PATH")?;
    anyhow::ensure!(output.status.success(), "Codex version check failed");
    let version = String::from_utf8(output.stdout).context("Codex version output is not UTF-8")?;
    let version = version
        .split_whitespace()
        .last()
        .ok_or_else(|| anyhow!("Codex returned an empty version"))?;
    anyhow::ensure!(
        is_supported_codex_version(version),
        "Codex {version} is unsupported; this integration requires {MINIMUM_CODEX_VERSION_DISPLAY} or newer within the 0.x release line"
    );
    let output = Command::new("cmd.exe")
        .args(["/D", "/S", "/C", "codex --help"])
        .output()
        .context("checking Codex support for session-local hooks")?;
    anyhow::ensure!(output.status.success(), "Codex help check failed");
    let help = String::from_utf8(output.stdout).context("Codex help output is not UTF-8")?;
    anyhow::ensure!(
        help.split_whitespace()
            .any(|argument| argument == "--no-daemon"),
        "This Codex version cannot isolate resumable session hooks; update Codex to a version supporting --no-daemon",
    );
    Ok(())
}

fn is_supported_codex_version(version: &str) -> bool {
    let mut components = version.split('.');
    let parsed = (
        components
            .next()
            .and_then(|value| value.parse::<u64>().ok()),
        components
            .next()
            .and_then(|value| value.parse::<u64>().ok()),
        components
            .next()
            .and_then(|value| value.parse::<u64>().ok()),
    );
    let (Some(major), Some(minor), Some(patch)) = parsed else {
        return false;
    };
    components.next().is_none()
        && major == MINIMUM_CODEX_VERSION.0
        && (major, minor, patch) >= MINIMUM_CODEX_VERSION
}

#[allow(
    clippy::disallowed_methods,
    reason = "provider contract checks run on the background executor"
)]
pub(crate) fn verify_claude_contract() -> Result<()> {
    let output = Command::new("cmd.exe")
        .args(["/D", "/S", "/C", "claude --version"])
        .output()
        .context("Claude Code is not installed or is unavailable on PATH")?;
    anyhow::ensure!(output.status.success(), "Claude Code version check failed");
    let version =
        String::from_utf8(output.stdout).context("Claude Code version output is not UTF-8")?;
    let version = claude_version_from_output(&version)
        .ok_or_else(|| anyhow!("Claude Code returned an empty version"))?;
    anyhow::ensure!(
        is_supported_claude_version(version),
        "Claude Code {version} is unsupported; this integration requires {MINIMUM_CLAUDE_VERSION_DISPLAY} or newer within the 2.x release line"
    );
    Ok(())
}

fn claude_version_from_output(output: &str) -> Option<&str> {
    output.split_whitespace().find_map(|value| {
        let value = value.trim_end_matches('.');
        value
            .chars()
            .next()
            .is_some_and(|character| character.is_ascii_digit())
            .then_some(value)
    })
}

fn is_supported_claude_version(version: &str) -> bool {
    let mut components = version.split('.');
    let parsed = (
        components
            .next()
            .and_then(|value| value.parse::<u64>().ok()),
        components
            .next()
            .and_then(|value| value.parse::<u64>().ok()),
        components
            .next()
            .and_then(|value| value.parse::<u64>().ok()),
    );
    let (Some(major), Some(minor), Some(patch)) = parsed else {
        return false;
    };
    components.next().is_none()
        && major == MINIMUM_CLAUDE_VERSION.0
        && (major, minor, patch) >= MINIMUM_CLAUDE_VERSION
}

#[allow(
    clippy::disallowed_methods,
    reason = "provider contract checks run on the background executor"
)]
pub(crate) fn verify_copilot_contract() -> Result<()> {
    let output = Command::new("cmd.exe")
        .args(["/D", "/S", "/C", "copilot --version"])
        .output()
        .context("Copilot CLI is not installed or is unavailable on PATH")?;
    anyhow::ensure!(output.status.success(), "Copilot CLI version check failed");
    let version =
        String::from_utf8(output.stdout).context("Copilot CLI version output is not UTF-8")?;
    let version = claude_version_from_output(&version)
        .ok_or_else(|| anyhow!("Copilot CLI returned an empty version"))?;
    anyhow::ensure!(
        is_supported_copilot_version(version),
        "Copilot CLI {version} is unsupported; this integration requires {MINIMUM_COPILOT_VERSION_DISPLAY} or newer within the 1.x release line"
    );
    Ok(())
}

fn is_supported_copilot_version(version: &str) -> bool {
    let mut components = version.split('.');
    let parsed = (
        components
            .next()
            .and_then(|value| value.parse::<u64>().ok()),
        components
            .next()
            .and_then(|value| value.parse::<u64>().ok()),
        components
            .next()
            .and_then(|value| value.parse::<u64>().ok()),
    );
    let (Some(major), Some(minor), Some(patch)) = parsed else {
        return false;
    };
    components.next().is_none()
        && major == MINIMUM_COPILOT_VERSION.0
        && (major, minor, patch) >= MINIMUM_COPILOT_VERSION
}

#[allow(
    clippy::disallowed_methods,
    reason = "provider contract checks run on the background executor"
)]
pub(crate) fn verify_hermes_contract() -> Result<()> {
    active_hermes_profile()?;
    let output = Command::new("cmd.exe")
        .args(["/D", "/S", "/C", "hermes --version"])
        .output()
        .context("Hermes Agent CLI is not installed or is unavailable on PATH")?;
    anyhow::ensure!(
        output.status.success(),
        "Hermes Agent CLI version check failed"
    );
    let version =
        String::from_utf8(output.stdout).context("Hermes Agent CLI version output is not UTF-8")?;
    let version = version
        .split_whitespace()
        .find_map(|value| {
            value.strip_prefix('v').or(Some(value)).filter(|value| {
                value
                    .chars()
                    .next()
                    .is_some_and(|character| character.is_ascii_digit())
            })
        })
        .ok_or_else(|| anyhow!("Hermes Agent CLI returned an empty version"))?;
    anyhow::ensure!(
        is_supported_hermes_version(version),
        "Hermes Agent CLI {version} is unsupported; this integration requires {MINIMUM_HERMES_VERSION_DISPLAY} or newer within the 0.x release line"
    );
    Ok(())
}

fn is_supported_hermes_version(version: &str) -> bool {
    let mut components = version.split('.');
    let parsed = (
        components
            .next()
            .and_then(|value| value.parse::<u64>().ok()),
        components
            .next()
            .and_then(|value| value.parse::<u64>().ok()),
        components
            .next()
            .and_then(|value| value.parse::<u64>().ok()),
    );
    let (Some(major), Some(minor), Some(patch)) = parsed else {
        return false;
    };
    components.next().is_none()
        && major == MINIMUM_HERMES_VERSION.0
        && (major, minor, patch) >= MINIMUM_HERMES_VERSION
}

pub(crate) fn codex_new_command() -> Vec<u8> {
    b"codex --no-daemon\r".to_vec()
}

pub(crate) fn codex_resume_command(session: &ResumableCliSession) -> Result<Vec<u8>> {
    anyhow::ensure!(
        session.provider() == ResumableCliProvider::Codex,
        "unsupported resumable CLI provider"
    );
    session
        .provider()
        .validate_resume_id(session.exact_resume_id())?;
    Ok(format!("codex --no-daemon resume {}\r", session.exact_resume_id()).into_bytes())
}

pub(crate) fn claude_new_command(session_id: &str, plugin_path: &Path) -> Result<Vec<u8>> {
    claude_command_with_plugin(session_id, plugin_path, "--session-id")
}

fn claude_command_with_plugin(
    session_id: &str,
    plugin_path: &Path,
    argument: &str,
) -> Result<Vec<u8>> {
    ResumableCliProvider::Claude.validate_resume_id(session_id)?;
    let plugin_path = plugin_path
        .to_str()
        .ok_or_else(|| anyhow!("the Claude session plugin path is not valid UTF-8"))?;
    anyhow::ensure!(
        !plugin_path.contains('"') && !plugin_path.chars().any(char::is_control),
        "the Claude session plugin path contains a quote or control character"
    );
    Ok(format!(
        "claude {argument} {session_id} --plugin-dir '{}'\r",
        plugin_path.replace('\'', "''")
    )
    .into_bytes())
}

pub(crate) fn claude_resume_command(session: &ResumableCliSession) -> Result<Vec<u8>> {
    anyhow::ensure!(
        session.provider() == ResumableCliProvider::Claude,
        "unsupported resumable CLI provider"
    );
    session
        .provider()
        .validate_resume_id(session.exact_resume_id())?;
    Ok(format!("claude --resume {}\r", session.exact_resume_id()).into_bytes())
}

pub(crate) fn claude_resume_command_with_plugin(
    session: &ResumableCliSession,
    plugin_path: &Path,
) -> Result<Vec<u8>> {
    anyhow::ensure!(
        session.provider() == ResumableCliProvider::Claude,
        "unsupported resumable CLI provider"
    );
    claude_command_with_plugin(session.exact_resume_id(), plugin_path, "--resume")
}

pub(crate) fn copilot_new_command(session_id: &str) -> Result<Vec<u8>> {
    ResumableCliProvider::Copilot.validate_resume_id(session_id)?;
    Ok(format!("copilot --session-id {session_id}\r").into_bytes())
}

fn copilot_store_root() -> Result<PathBuf> {
    if let Some(root) = std::env::var_os("COPILOT_HOME") {
        return Ok(PathBuf::from(root));
    }
    Ok(dirs::home_dir()
        .ok_or_else(|| anyhow!("home directory is unavailable"))?
        .join(".copilot"))
}

fn verify_copilot_session_exists_in(
    session: &ResumableCliSession,
    store_root: &Path,
) -> Result<()> {
    anyhow::ensure!(
        session.provider() == ResumableCliProvider::Copilot,
        "unsupported resumable CLI provider"
    );
    session
        .provider()
        .validate_resume_id(session.exact_resume_id())?;
    let marker = store_root
        .join("session-state")
        .join(session.exact_resume_id())
        .join("workspace.yaml");
    anyhow::ensure!(
        marker.is_file(),
        "Copilot session {} is unavailable; refusing to open a fresh session",
        session.exact_resume_id()
    );
    Ok(())
}

pub(crate) fn copilot_resume_command(session: &ResumableCliSession) -> Result<Vec<u8>> {
    verify_copilot_session_exists_in(session, &copilot_store_root()?)?;
    Ok(format!("copilot --resume={}\r", session.exact_resume_id()).into_bytes())
}

pub(crate) fn hermes_new_command(profile: &HermesProfileContext) -> Vec<u8> {
    if uses_hermes_profile_argument(profile) {
        format!("hermes -p {}\r", profile.profile).into_bytes()
    } else {
        b"hermes\r".to_vec()
    }
}

pub(crate) fn hermes_resume_command(session: &ResumableCliSession) -> Result<Vec<u8>> {
    let active_profile = active_hermes_profile()?;
    let profile = match (session.provider_home(), session.provider_profile()) {
        (Some(home), Some(profile)) => HermesProfileContext {
            home: home.to_path_buf(),
            profile: profile.to_owned(),
        },
        (None, None) => active_profile.clone(),
        _ => anyhow::bail!("Hermes session has incomplete provider context"),
    };
    validate_hermes_profile(&profile.profile)?;
    let active_root = hermes_root_for_home(&active_profile.home);
    let persisted_root = hermes_root_for_home(&profile.home);
    anyhow::ensure!(
        same_path(active_root, persisted_root),
        "The saved Hermes profile belongs to a different HERMES_HOME"
    );
    if !uses_hermes_profile_argument(&profile) {
        anyhow::ensure!(
            same_path(&active_profile.home, &profile.home),
            "The saved custom Hermes home is not active"
        );
    }
    verify_hermes_session_exists_in(session, &profile.home)?;
    let profile_argument = if uses_hermes_profile_argument(&profile) {
        format!(" -p {}", profile.profile)
    } else {
        String::new()
    };
    Ok(format!(
        "hermes{profile_argument} --resume {}\r",
        session.exact_resume_id()
    )
    .into_bytes())
}

fn verify_hermes_session_exists_in(
    session: &ResumableCliSession,
    hermes_home: &Path,
) -> Result<()> {
    anyhow::ensure!(
        session.provider() == ResumableCliProvider::Hermes,
        "unsupported resumable CLI provider"
    );
    session
        .provider()
        .validate_resume_id(session.exact_resume_id())?;
    let db_path = hermes_home.join("state.db");
    anyhow::ensure!(
        db_path.is_file(),
        "Hermes session store is unavailable; refusing to open a fresh session"
    );
    let connection =
        db::sqlez::connection::Connection::open_file_read_only(&db_path.to_string_lossy())
            .context("opening the Hermes session store read-only")?;
    let mut select = connection
        .select_row_bound::<String, bool>("SELECT 1 FROM sessions WHERE id = ? LIMIT 1")
        .context("preparing exact Hermes session lookup")?;
    anyhow::ensure!(
        select(session.exact_resume_id().to_owned())?.unwrap_or(false),
        "Hermes session {} is unavailable; refusing to open a fresh session",
        session.exact_resume_id()
    );
    Ok(())
}

fn target_key(session: &ResumableCliSession) -> String {
    format!(
        "{}:{}",
        session.provider().as_str(),
        session.exact_resume_id()
    )
}

pub(crate) fn try_claim_target(session: &ResumableCliSession) -> Option<SessionClaim> {
    let mut targets = ACTIVE_TARGETS
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let target = target_key(session);
    if targets.contains_key(&target) {
        return None;
    }
    let token = Uuid::new_v4();
    targets.insert(target.clone(), token);
    Some(SessionClaim { target, token })
}

pub(crate) fn replace_target_claim(session: &ResumableCliSession) -> SessionClaim {
    let mut targets = ACTIVE_TARGETS
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let target = target_key(session);
    let token = Uuid::new_v4();
    targets.insert(target.clone(), token);
    SessionClaim { target, token }
}
