//! Antigravity ACP (Agent Client Protocol) backend for SkyrimNet and Bard lyrics.
//!
//! Spawns and manages a persistent `agy acp` child process communicating via stdio
//! JSON-RPC 2.0. Translates incoming OpenAI chat completion requests into ACP sessions
//! and prompts, converts responses into OpenAI format (both SSE streams and JSON),
//! and auto-denies any agent tool execution requests for zero-side-effect headless operation.
//!
//! Each spawned child is a separate *generation* with its own pending-request and
//! session maps, so a recycled child can never fail or mis-route requests that belong
//! to its replacement. The `initialize` handshake runs while the process lock is held,
//! so concurrent requests wait for it instead of racing ahead of it.

use anyhow::{anyhow, bail, Context, Result};
use bytes::Bytes;
use http_body_util::{combinators::UnsyncBoxBody, BodyExt, Full, StreamBody};
use hyper::{body::Frame, Response, StatusCode};
use serde_json::{json, Value};
use std::{
    collections::{HashMap, VecDeque},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex as StdMutex, OnceLock,
    },
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdout, Command},
    sync::{mpsc, oneshot, Mutex},
    task::JoinHandle,
    time::timeout,
};

type BoxError = Box<dyn std::error::Error + Send + Sync>;
type Body = UnsyncBoxBody<Bytes, BoxError>;

pub const DEFAULT_ANTIGRAVITY_MODEL: &str = "gemini-3.8-flash-low";
pub const DEFAULT_ANTIGRAVITY_EFFORT: &str = "low";

/// Timeout for ordinary ACP calls (`initialize`, `session/new`, `session/set_model`).
const CALL_TIMEOUT: Duration = Duration::from_secs(30);
/// Upper bound for a single prompt turn (handles 30k-token contexts and vision encode).
const PROMPT_TIMEOUT: Duration = Duration::from_secs(300);
/// Best-effort `session/close` must never hold up the caller for long.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Normalizes an arbitrary effort string into one of Antigravity's supported tiers.
pub fn normalize_effort(effort_raw: &str) -> &'static str {
    match effort_raw.trim().to_lowercase().as_str() {
        "low" | "minimal" | "off" | "none" => "low",
        "medium" | "med" => "medium",
        "high" | "xhigh" | "max" => "high",
        _ => "low",
    }
}

/// Resolves the effective Antigravity effort tier from payload override or settings.
pub fn resolve_effort(payload_effort: Option<&str>, settings: &Value) -> &'static str {
    let effort_raw = payload_effort
        .or_else(|| settings["antigravity"]["effort"].as_str())
        .or_else(|| settings["reasoning_effort"].as_str())
        .unwrap_or(DEFAULT_ANTIGRAVITY_EFFORT);
    normalize_effort(effort_raw)
}

/// Resolves the concrete Antigravity model ID based on payload, configured settings, and effort level.
pub fn resolve_model(
    payload_model: Option<&str>,
    payload_effort: Option<&str>,
    settings: &Value,
) -> String {
    // 1. If payload explicitly specifies an exact Antigravity model ID, use it directly.
    if let Some(m) = payload_model {
        let m_clean = m.trim();
        if m_clean.starts_with("gemini-") || m_clean.starts_with("claude-") || m_clean.starts_with("gpt-oss-") {
            return m_clean.to_string();
        }
    }

    // 2. Resolve effort level
    let effort_tier = resolve_effort(payload_effort, settings);

    // 3. Resolve base model
    let configured_model = settings["antigravity"]["model"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(DEFAULT_ANTIGRAVITY_MODEL);

    let base = configured_model
        .strip_suffix("-low")
        .or_else(|| configured_model.strip_suffix("-medium"))
        .or_else(|| configured_model.strip_suffix("-high"))
        .unwrap_or(configured_model);

    // GPT-OSS 120B is published only at the -medium tier.
    if base == "gpt-oss-120b" {
        return "gpt-oss-120b-medium".to_owned();
    }

    format!("{}-{}", base, effort_tier)
}
pub const DEFAULT_SYSTEM_INSTRUCTION: &str =
    "You are generating raw output for SkyrimNet (Skyrim VR AI). \
     Respond ONLY with the direct dialogue or JSON requested by the prompt. \
     Do NOT use tools, do NOT add meta-commentary, preambles, or conversational filler. \
     Do NOT prefix dialogue with character name headers (such as 'Name: '); output only the direct spoken words or the raw JSON. \
     Do NOT wrap dialogue in markdown code blocks or quotes unless explicitly instructed in the prompt.";

/// Helper to build an HTTP response from bytes and content-type.
fn make_response(bytes: impl Into<Bytes>, kind: &str, status: StatusCode) -> Response<Body> {
    Response::builder()
        .status(status)
        .header("Content-Type", kind)
        .body(
            Full::new(bytes.into())
                .map_err(|never| match never {})
                .boxed_unsync(),
        )
        .unwrap()
}

pub fn json_response(v: &Value, status: StatusCode) -> Response<Body> {
    make_response(serde_json::to_vec(v).unwrap(), "application/json", status)
}

/// Discovers the path to the `agy` CLI binary on the host machine.
pub fn find_agy_path(configured_path: Option<&str>) -> Result<PathBuf> {
    if let Some(p) = configured_path {
        let path = PathBuf::from(p);
        if path.is_file() {
            return Ok(path);
        }
        bail!("Configured agy_path does not exist: {}", p);
    }

    // 1. Check %LOCALAPPDATA%\agy\bin\agy.exe
    if let Ok(local_app_data) = std::env::var("LOCALAPPDATA") {
        let p = PathBuf::from(local_app_data).join("agy").join("bin").join("agy.exe");
        if p.is_file() {
            return Ok(p);
        }
    }

    // 2. Check %USERPROFILE%\.gemini\bin\agy.exe
    if let Ok(user_profile) = std::env::var("USERPROFILE") {
        let p = PathBuf::from(user_profile).join(".gemini").join("bin").join("agy.exe");
        if p.is_file() {
            return Ok(p);
        }
    }

    // 3. Check system PATH
    if let Ok(path_var) = std::env::var("PATH") {
        for dir in std::env::split_paths(&path_var) {
            let candidate = dir.join("agy.exe");
            if candidate.is_file() {
                return Ok(candidate);
            }
            let candidate_bare = dir.join("agy");
            if candidate_bare.is_file() {
                return Ok(candidate_bare);
            }
        }
    }

    bail!("Antigravity CLI (agy) binary not found. Please install Antigravity CLI or set 'agy_path' in settings.json.")
}

/// Represents an event update from ACP `session/update`.
#[derive(Debug, Clone)]
pub enum AcpUpdate {
    Chunk(String),
    Thought(String),
    StopReason(String),
    Usage(Value),
    Completed,
    Error(String),
}

/// Maps an ACP `stopReason` onto the closest OpenAI `finish_reason`.
pub fn finish_reason(stop_reason: Option<&str>) -> &'static str {
    match stop_reason {
        Some("max_tokens") | Some("max_turn_requests") => "length",
        Some("refusal") => "content_filter",
        _ => "stop",
    }
}

struct SessionChannels {
    sender: mpsc::UnboundedSender<AcpUpdate>,
}

type PendingRequestsMap = HashMap<u64, oneshot::Sender<std::result::Result<Value, String>>>;
type ActiveSessionsMap = HashMap<String, SessionChannels>;

fn encode_line(v: &Value) -> Vec<u8> {
    let mut bytes = serde_json::to_vec(v).unwrap_or_default();
    bytes.push(b'\n');
    bytes
}

/// One `agy acp` child generation: its stdin queue plus the requests and sessions that belong to it.
struct Connection {
    generation: u64,
    stdin_tx: mpsc::Sender<Vec<u8>>,
    next_id: AtomicU64,
    pending: StdMutex<PendingRequestsMap>,
    sessions: StdMutex<ActiveSessionsMap>,
    closed: AtomicBool,
}

impl Connection {
    fn new(generation: u64, stdin_tx: mpsc::Sender<Vec<u8>>) -> Self {
        Self {
            generation,
            stdin_tx,
            next_id: AtomicU64::new(1),
            pending: StdMutex::new(HashMap::new()),
            sessions: StdMutex::new(HashMap::new()),
            closed: AtomicBool::new(false),
        }
    }

    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    fn active_sessions(&self) -> usize {
        self.sessions.lock().unwrap().len()
    }

    /// Sends a JSON-RPC request to this child and waits for the response up to `timeout_dur`.
    async fn call_with_timeout(&self, method: &str, params: Value, timeout_dur: Duration) -> Result<Value> {
        let req_id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        {
            // `closed` is set under this lock by the reader, so a request can never be
            // registered after the reader has drained the map.
            let mut pending = self.pending.lock().unwrap();
            if self.is_closed() {
                bail!("Antigravity ACP process is not running");
            }
            pending.insert(req_id, tx);
        }

        let request = json!({
            "jsonrpc": "2.0",
            "id": req_id,
            "method": method,
            "params": params
        });
        if self.stdin_tx.send(encode_line(&request)).await.is_err() {
            self.pending.lock().unwrap().remove(&req_id);
            bail!("Failed to send '{}' to the ACP child process", method);
        }

        match timeout(timeout_dur, rx).await {
            Ok(Ok(Ok(val))) => Ok(val),
            Ok(Ok(Err(err_msg))) => bail!("ACP call '{}' returned error: {}", method, err_msg),
            Ok(Err(_)) => bail!("ACP call '{}' response channel canceled", method),
            Err(_) => {
                self.pending.lock().unwrap().remove(&req_id);
                bail!("ACP call '{}' timed out after {:?} waiting for response", method, timeout_dur);
            }
        }
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value> {
        self.call_with_timeout(method, params, CALL_TIMEOUT).await
    }

    /// Queues a JSON-RPC notification without waiting (usable from `Drop`).
    fn notify_now(&self, method: &str, params: Value) -> bool {
        let note = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params
        });
        self.stdin_tx.try_send(encode_line(&note)).is_ok()
    }

    /// Best-effort session cleanup; errors (including unsupported method) are ignored.
    async fn close_session(&self, session_id: &str) {
        if self.is_closed() {
            return;
        }
        let _ = self
            .call_with_timeout("session/close", json!({ "sessionId": session_id }), CLOSE_TIMEOUT)
            .await;
    }
}

/// Builds the reply to a request the agent sent to us. Permission prompts get the ACP
/// `cancelled` outcome (a clean denial); every other client method is unsupported.
fn agent_request_reply(id: &Value, method: &str) -> Value {
    if method == "session/request_permission" {
        json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": { "outcome": { "outcome": "cancelled" } }
        })
    } else {
        json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {
                "code": -32601,
                "message": format!("Method or tool '{}' denied in headless relay mode", method)
            }
        })
    }
}

fn response_id(id: &Value) -> Option<u64> {
    id.as_u64().or_else(|| id.as_str().and_then(|s| s.parse().ok()))
}

fn route_session_update(conn: &Connection, params: &Value) {
    let sid = params["sessionId"].as_str().unwrap_or("");
    let update = &params["update"];
    let text = update["content"]["text"].as_str();
    let message = match (update["sessionUpdate"].as_str(), text) {
        (Some("agent_message_chunk"), Some(t)) => AcpUpdate::Chunk(t.to_owned()),
        (Some("agent_thought_chunk"), Some(t)) => AcpUpdate::Thought(t.to_owned()),
        _ => return,
    };
    if let Some(s) = conn.sessions.lock().unwrap().get(sid) {
        let _ = s.sender.send(message);
    }
}

/// Reads one child's stdout until EOF, then fails only that child's outstanding work.
async fn read_loop(conn: Arc<Connection>, stdout: ChildStdout, telemetry: Arc<StdMutex<TelemetryState>>) {
    let mut lines = BufReader::new(stdout).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(incoming) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let id = incoming.get("id").filter(|v| !v.is_null());
        match (incoming["method"].as_str(), id) {
            // Request from the agent (numeric or string id): always answer so the turn never hangs.
            (Some(method), Some(id)) => {
                let reply = agent_request_reply(id, method);
                let _ = conn.stdin_tx.send(encode_line(&reply)).await;
            }
            // Response to one of our requests.
            (None, Some(id)) => {
                let Some(req_id) = response_id(id) else {
                    continue;
                };
                let sender = conn.pending.lock().unwrap().remove(&req_id);
                if let Some(sender) = sender {
                    let outcome = if incoming["error"].is_null() {
                        Ok(incoming["result"].clone())
                    } else {
                        let err = &incoming["error"];
                        Err(err["message"]
                            .as_str()
                            .map(str::to_owned)
                            .unwrap_or_else(|| err.to_string()))
                    };
                    let _ = sender.send(outcome);
                }
            }
            (Some("session/update"), None) => route_session_update(&conn, &incoming["params"]),
            _ => {}
        }
    }

    let drained: Vec<_> = {
        let mut pending = conn.pending.lock().unwrap();
        conn.closed.store(true, Ordering::SeqCst);
        pending.drain().map(|(_, s)| s).collect()
    };
    for sender in drained {
        let _ = sender.send(Err("Antigravity ACP process terminated unexpectedly".into()));
    }
    let sessions: Vec<_> = conn.sessions.lock().unwrap().drain().map(|(_, s)| s).collect();
    for s in sessions {
        let _ = s.sender.send(AcpUpdate::Error("ACP process closed".into()));
    }
    let mut t = telemetry.lock().unwrap();
    if t.generation == conn.generation {
        t.acp_running = false;
        t.acp_initialized = false;
        t.active_sessions = 0;
    }
}

struct ChildProcessHandle {
    child: Child,
    conn: Arc<Connection>,
    writer: JoinHandle<()>,
    sandbox_dir: PathBuf,
    configured_agy_path: Option<String>,
}

impl ChildProcessHandle {
    /// Stops the stdin writer (closing the child's stdin) and kills the process.
    async fn terminate(mut self) {
        self.writer.abort();
        let _ = self.child.kill().await;
    }
}

#[derive(Clone, Debug)]
struct ClientConfig {
    sandbox_dir: PathBuf,
    configured_agy_path: Option<String>,
}

#[derive(Clone, Debug, Default)]
struct TelemetryState {
    generation: u64,
    acp_running: bool,
    acp_initialized: bool,
    active_sessions: u64,
    total_requests: u64,
    failed_requests: u64,
    last_latency_ms: Option<u64>,
    last_model: Option<String>,
    last_error: Option<String>,
    last_completed_at: Option<f64>,
}

/// An ACP session bound to the child generation that created it.
pub struct AcpSession {
    conn: Arc<Connection>,
    id: String,
}

impl AcpSession {
    pub fn id(&self) -> &str {
        &self.id
    }
}

/// Sends `session/cancel` when a prompt stream is dropped before the turn finished
/// (client disconnect, caller timeout), so abandoned turns stop consuming quota.
struct CancelOnDrop {
    conn: Arc<Connection>,
    session_id: String,
    finished: Arc<AtomicBool>,
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if !self.finished.load(Ordering::SeqCst) {
            self.conn
                .notify_now("session/cancel", json!({ "sessionId": &self.session_id }));
        }
    }
}

/// Incremental updates for one prompt turn. Dropping it early cancels the turn.
pub struct PromptStream {
    rx: mpsc::UnboundedReceiver<AcpUpdate>,
    _cancel: CancelOnDrop,
}

impl PromptStream {
    pub async fn recv(&mut self) -> Option<AcpUpdate> {
        self.rx.recv().await
    }
}

/// Result of a fully buffered (non-streaming) prompt turn.
#[derive(Debug, Clone)]
pub struct BufferedCompletion {
    pub text: String,
    pub thought: Option<String>,
    pub usage: Value,
    pub stop_reason: Option<String>,
}

pub struct AntigravityClient {
    config: StdMutex<ClientConfig>,
    inner: Mutex<Option<ChildProcessHandle>>,
    generation: AtomicU64,
    telemetry: Arc<StdMutex<TelemetryState>>,
    is_shutting_down: AtomicBool,
}

impl AntigravityClient {
    pub fn new(sandbox_dir: PathBuf, configured_agy_path: Option<String>) -> Self {
        Self {
            config: StdMutex::new(ClientConfig {
                sandbox_dir,
                configured_agy_path,
            }),
            inner: Mutex::new(None),
            generation: AtomicU64::new(0),
            telemetry: Arc::new(StdMutex::new(TelemetryState::default())),
            is_shutting_down: AtomicBool::new(false),
        }
    }

    /// Updates sandbox/binary configuration for subsequent child launches.
    pub fn update_config(&self, sandbox_dir: PathBuf, configured_agy_path: Option<String>) {
        let mut cfg = self.config.lock().unwrap();
        cfg.sandbox_dir = sandbox_dir;
        cfg.configured_agy_path = configured_agy_path;
    }

    fn current_config(&self) -> ClientConfig {
        self.config.lock().unwrap().clone()
    }

    pub fn record_outcome(&self, model: &str, elapsed: Duration, error: Option<&str>) {
        let mut t = self.telemetry.lock().unwrap();
        t.total_requests += 1;
        t.last_model = Some(model.to_owned());
        t.last_latency_ms = Some(elapsed.as_millis().min(u64::MAX as u128) as u64);
        t.last_completed_at = Some(crate::engine::now());
        if let Some(err) = error {
            t.failed_requests += 1;
            t.last_error = Some(err.to_owned());
        } else {
            t.last_error = None;
        }
    }

    fn record_error_only(&self, model: Option<&str>, error: &str) {
        let mut t = self.telemetry.lock().unwrap();
        t.total_requests += 1;
        t.failed_requests += 1;
        if let Some(m) = model {
            t.last_model = Some(m.to_owned());
        }
        t.last_error = Some(error.to_owned());
        t.last_completed_at = Some(crate::engine::now());
    }

    fn mark_stopped(&self) {
        let mut t = self.telemetry.lock().unwrap();
        t.acp_running = false;
        t.acp_initialized = false;
        t.active_sessions = 0;
    }

    fn sync_active_sessions(&self, conn: &Connection) {
        let count = conn.active_sessions() as u64;
        let mut t = self.telemetry.lock().unwrap();
        if t.generation == conn.generation {
            t.active_sessions = count;
        }
    }

    /// Ensures an initialized `agy acp` child is running and returns its connection.
    ///
    /// The process lock is held through spawn *and* the `initialize` handshake, so
    /// concurrent callers block until the child is ready instead of sending
    /// `session/new` before `initialize` has completed.
    async fn ensure_running(&self) -> Result<Arc<Connection>> {
        let cfg = self.current_config();
        let mut guard = self.inner.lock().await;
        if let Some(handle) = guard.as_mut() {
            let alive = matches!(handle.child.try_wait(), Ok(None)) && !handle.conn.is_closed();
            let config_changed = handle.sandbox_dir != cfg.sandbox_dir
                || handle.configured_agy_path != cfg.configured_agy_path;
            // Settings changes recycle the child only once it is idle.
            if alive && (!config_changed || handle.conn.active_sessions() > 0) {
                return Ok(handle.conn.clone());
            }
            if let Some(old) = guard.take() {
                old.terminate().await;
            }
            self.mark_stopped();
        }

        if self.is_shutting_down.load(Ordering::SeqCst) {
            bail!("Antigravity client is shutting down");
        }

        let agy_bin = find_agy_path(cfg.configured_agy_path.as_deref())?;
        std::fs::create_dir_all(&cfg.sandbox_dir)
            .context("Failed to create Antigravity sandbox directory")?;

        let mut cmd = Command::new(&agy_bin);
        cmd.arg("acp")
            .current_dir(&cfg.sandbox_dir)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        #[cfg(windows)]
        cmd.creation_flags(CREATE_NO_WINDOW);

        let mut child = cmd
            .spawn()
            .with_context(|| format!("Failed to spawn agy acp process at {:?}", agy_bin))?;
        #[cfg(windows)]
        job::bind_to_relay_lifetime(&child);

        let mut child_stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("Child stdin not captured"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("Child stdout not captured"))?;

        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        let (stdin_tx, mut stdin_rx) = mpsc::channel::<Vec<u8>>(128);
        let conn = Arc::new(Connection::new(generation, stdin_tx));

        let writer = tokio::spawn(async move {
            while let Some(bytes) = stdin_rx.recv().await {
                if child_stdin.write_all(&bytes).await.is_err() {
                    break;
                }
                if child_stdin.flush().await.is_err() {
                    break;
                }
            }
        });

        {
            let mut t = self.telemetry.lock().unwrap();
            t.generation = generation;
            t.acp_running = true;
            t.acp_initialized = false;
            t.active_sessions = 0;
        }
        tokio::spawn(read_loop(conn.clone(), stdout, self.telemetry.clone()));

        let init_params = json!({
            "protocolVersion": 1,
            "clientCapabilities": {
                "fs": { "readTextFile": false, "writeTextFile": false },
                "terminal": false
            }
        });
        if let Err(err) = conn.call("initialize", init_params).await {
            writer.abort();
            let _ = child.kill().await;
            let mut t = self.telemetry.lock().unwrap();
            if t.generation == generation {
                t.acp_running = false;
                t.acp_initialized = false;
            }
            t.last_error = Some(err.to_string());
            return Err(err.context("Antigravity ACP initialize failed"));
        }

        {
            let mut t = self.telemetry.lock().unwrap();
            if t.generation == generation {
                t.acp_initialized = true;
            }
        }
        *guard = Some(ChildProcessHandle {
            child,
            conn: conn.clone(),
            writer,
            sandbox_dir: cfg.sandbox_dir,
            configured_agy_path: cfg.configured_agy_path,
        });
        Ok(conn)
    }

    /// Creates a new ACP session with custom model and optional system prompt extension.
    pub async fn new_session(&self, model: &str, system_prompt: Option<&str>) -> Result<AcpSession> {
        let conn = self.ensure_running().await?;
        let sandbox_dir = self.current_config().sandbox_dir;

        let meta = json!({
            "antigravity": {
                "systemPrompt": {
                    "append": system_prompt.unwrap_or(DEFAULT_SYSTEM_INSTRUCTION)
                }
            }
        });

        let session_params = json!({
            "cwd": sandbox_dir.to_string_lossy(),
            "mcpServers": [],
            "_meta": meta
        });

        let res = conn.call("session/new", session_params).await?;
        let session_id = res["sessionId"]
            .as_str()
            .ok_or_else(|| anyhow!("session/new did not return sessionId"))?
            .to_owned();

        // Switch to the requested model if different
        let current_model = res["models"]["currentModelId"].as_str().unwrap_or("");
        if !model.is_empty() && model != current_model {
            if let Err(err) = conn
                .call(
                    "session/set_model",
                    json!({
                        "sessionId": &session_id,
                        "modelId": model
                    }),
                )
                .await
            {
                conn.close_session(&session_id).await;
                return Err(err).context(format!("Failed to select Antigravity model '{model}'"));
            }
        }

        Ok(AcpSession { conn, id: session_id })
    }

    /// Starts a prompt turn and returns a stream of incremental updates.
    ///
    /// The stream always ends with exactly one `Completed` or `Error`. Dropping it before
    /// then sends `session/cancel`; the session is closed once the turn ends.
    pub fn prompt_stream(self: &Arc<Self>, session: AcpSession, prompt: Vec<Value>) -> PromptStream {
        let AcpSession { conn, id } = session;
        let (tx, rx) = mpsc::unbounded_channel();
        conn.sessions
            .lock()
            .unwrap()
            .insert(id.clone(), SessionChannels { sender: tx.clone() });
        self.sync_active_sessions(&conn);

        let finished = Arc::new(AtomicBool::new(false));
        let task_conn = conn.clone();
        let task_id = id.clone();
        let task_finished = finished.clone();
        let client = self.clone();

        tokio::spawn(async move {
            let prompt_params = json!({
                "sessionId": &task_id,
                "prompt": prompt
            });
            let result = task_conn
                .call_with_timeout("session/prompt", prompt_params, PROMPT_TIMEOUT)
                .await;
            task_finished.store(true, Ordering::SeqCst);
            task_conn.sessions.lock().unwrap().remove(&task_id);
            client.sync_active_sessions(&task_conn);

            match result {
                Ok(result) => {
                    if let Some(stop) = result["stopReason"].as_str() {
                        let _ = tx.send(AcpUpdate::StopReason(stop.to_owned()));
                    }
                    if !result["usage"].is_null() {
                        let _ = tx.send(AcpUpdate::Usage(result["usage"].clone()));
                    }
                    let _ = tx.send(AcpUpdate::Completed);
                }
                Err(e) => {
                    // Covers the prompt timeout: make sure the agent stops working on it.
                    task_conn.notify_now("session/cancel", json!({ "sessionId": &task_id }));
                    let _ = tx.send(AcpUpdate::Error(e.to_string()));
                }
            }

            // Close the session to conserve quota and avoid history clutter.
            task_conn.close_session(&task_id).await;
        });

        PromptStream {
            rx,
            _cancel: CancelOnDrop {
                conn,
                session_id: id,
                finished,
            },
        }
    }

    /// Non-streaming prompt execution: buffers all chunks and returns the complete answer.
    pub async fn prompt_buffered(
        self: &Arc<Self>,
        session: AcpSession,
        prompt: Vec<Value>,
        timeout_duration: Duration,
    ) -> Result<BufferedCompletion> {
        let mut stream = self.prompt_stream(session, prompt);
        let mut completion = BufferedCompletion {
            text: String::new(),
            thought: None,
            usage: json!({}),
            stop_reason: None,
        };
        let mut thought = String::new();

        let collected = timeout(timeout_duration, async {
            while let Some(update) = stream.recv().await {
                match update {
                    AcpUpdate::Chunk(c) => completion.text.push_str(&c),
                    AcpUpdate::Thought(t) => thought.push_str(&t),
                    AcpUpdate::Usage(u) => completion.usage = u,
                    AcpUpdate::StopReason(s) => completion.stop_reason = Some(s),
                    AcpUpdate::Completed => return Ok(()),
                    AcpUpdate::Error(err) => return Err(anyhow!("ACP prompt failed: {}", err)),
                }
            }
            Err(anyhow!("ACP prompt stream ended without completing"))
        })
        .await;

        match collected {
            Ok(Ok(())) => {
                if !thought.is_empty() {
                    completion.thought = Some(thought);
                }
                Ok(completion)
            }
            Ok(Err(e)) => Err(e),
            Err(_) => {
                // Dropping the stream sends a single `session/cancel`; the prompt task closes the session.
                drop(stream);
                bail!("Antigravity prompt execution timed out after {:?}", timeout_duration);
            }
        }
    }

    /// Restarts/recycles the idle ACP child process without stopping the HTTP server.
    pub async fn restart_if_idle(&self) -> Result<bool> {
        let mut guard = self.inner.lock().await;
        if let Some(handle) = guard.as_ref() {
            let active = handle.conn.active_sessions();
            if active > 0 {
                bail!(
                    "Cannot restart Antigravity ACP worker while {active} request(s) are in flight"
                );
            }
        }
        let stopped = if let Some(handle) = guard.take() {
            handle.terminate().await;
            true
        } else {
            false
        };
        self.mark_stopped();
        self.telemetry.lock().unwrap().last_error = None;
        Ok(stopped)
    }

    /// Graceful termination: terminates child process cleanly.
    pub async fn shutdown(&self) {
        self.is_shutting_down.store(true, Ordering::SeqCst);
        let mut guard = self.inner.lock().await;
        if let Some(handle) = guard.take() {
            handle.terminate().await;
        }
        self.mark_stopped();
    }
}

/// Ties `agy acp` to the relay's lifetime: the child is placed in a kill-on-close job, so
/// Windows terminates it (and anything it spawned) if the relay exits or crashes without
/// running `shutdown`.
#[cfg(windows)]
mod job {
    use std::sync::OnceLock;
    use windows_sys::Win32::{
        Foundation::CloseHandle,
        System::JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
            SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        },
    };

    static JOB: OnceLock<usize> = OnceLock::new();

    fn job_handle() -> Option<usize> {
        let handle = *JOB.get_or_init(|| unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if job.is_null() {
                return 0;
            }
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let ok = SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const core::ffi::c_void,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            );
            if ok == 0 {
                CloseHandle(job);
                return 0;
            }
            // Intentionally never closed: the OS closes it when the relay exits.
            job as usize
        });
        (handle != 0).then_some(handle)
    }

    /// Best effort; failure only means the child is not auto-killed on relay crash.
    pub fn bind_to_relay_lifetime(child: &tokio::process::Child) {
        if let (Some(job), Some(raw)) = (job_handle(), child.raw_handle()) {
            unsafe {
                AssignProcessToJobObject(job as *mut core::ffi::c_void, raw as *mut core::ffi::c_void);
            }
        }
    }
}

/// Global shared singleton client for the application runtime.
static GLOBAL_ANTIGRAVITY: OnceLock<Arc<AntigravityClient>> = OnceLock::new();

/// Resolves Antigravity sandbox directory and optional `agy.exe` override from settings.
pub fn configured_paths(settings: &Value, root: &Path) -> (PathBuf, Option<String>) {
    let configured_path = settings["antigravity"]["agy_path"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let sandbox_dir = settings["antigravity"]["sandbox_dir"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| {
            let p = PathBuf::from(s);
            if p.is_relative() {
                root.join(p)
            } else {
                p
            }
        })
        .unwrap_or_else(|| root.join("agy-sandbox"));
    (sandbox_dir, configured_path)
}

pub fn get_client(sandbox_dir: PathBuf, agy_path: Option<String>) -> Arc<AntigravityClient> {
    let client = GLOBAL_ANTIGRAVITY
        .get_or_init(|| Arc::new(AntigravityClient::new(sandbox_dir.clone(), agy_path.clone())))
        .clone();
    client.update_config(sandbox_dir, agy_path);
    client
}

/// Records an external completion outcome (e.g. from Bard lyric drafting) into telemetry.
pub fn record_outcome(model: &str, elapsed: Duration, error: Option<&str>) {
    if let Some(client) = GLOBAL_ANTIGRAVITY.get() {
        client.record_outcome(model, elapsed, error);
    }
}

/// Restarts the global ACP child worker if idle; keeps the HTTP server listening.
pub async fn restart_global() -> Result<Value> {
    let was_running = if let Some(client) = GLOBAL_ANTIGRAVITY.get() {
        client.restart_if_idle().await?
    } else {
        false
    };
    Ok(json!({
        "success": true,
        "restarted": was_running,
        "message": if was_running {
            "Stopped idle Antigravity ACP worker; a fresh agy acp process will start on the next request"
        } else {
            "Antigravity ACP worker was already idle; ready for the next request"
        }
    }))
}

/// Gracefully shuts down the global ACP child process if running.
pub async fn shutdown_global() {
    if let Some(client) = GLOBAL_ANTIGRAVITY.get() {
        client.shutdown().await;
    }
}

/// Builds a non-blocking sanitized telemetry snapshot for `GET /health`.
pub fn health_snapshot(settings: &Value, root: &Path) -> Value {
    let (sandbox_dir, configured_path) = configured_paths(settings, root);
    let agy_lookup = find_agy_path(configured_path.as_deref());
    let configured_model = settings["antigravity"]["model"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(DEFAULT_ANTIGRAVITY_MODEL);
    let resolved_effort = resolve_effort(None, settings);
    let resolved_model = resolve_model(None, Some(resolved_effort), settings);
    let telemetry = GLOBAL_ANTIGRAVITY
        .get()
        .map(|c| c.telemetry.lock().unwrap().clone())
        .unwrap_or_default();

    json!({
        "configured_model": configured_model,
        "resolved_model": resolved_model,
        "effort": resolved_effort,
        "agy_found": agy_lookup.is_ok(),
        "agy_path": agy_lookup.as_ref().ok().map(|p| p.to_string_lossy().into_owned()),
        "agy_error": agy_lookup.err().map(|e| e.to_string()),
        "sandbox_dir": sandbox_dir.to_string_lossy(),
        "acp_running": telemetry.acp_running,
        "acp_initialized": telemetry.acp_initialized,
        "active_sessions": telemetry.active_sessions,
        "total_requests": telemetry.total_requests,
        "failed_requests": telemetry.failed_requests,
        "last_latency_ms": telemetry.last_latency_ms,
        "last_model": telemetry.last_model,
        "last_error": telemetry.last_error,
        "last_completed_at": telemetry.last_completed_at,
    })
}

/// Extracts plain text from OpenAI message content (string or array of `text` parts).
fn content_text(content: &Value) -> Option<String> {
    if let Some(s) = content.as_str() {
        return Some(s.to_owned());
    }
    let parts: Vec<&str> = content
        .as_array()?
        .iter()
        .filter(|p| p["type"].as_str() == Some("text"))
        .filter_map(|p| p["text"].as_str())
        .collect();
    (!parts.is_empty()).then(|| parts.join("\n"))
}

/// Translates OpenAI Chat Completion messages into an ACP system instruction and prompt blocks.
pub fn openai_to_acp(messages: &[Value]) -> Result<(Option<String>, Vec<Value>)> {
    let mut system_parts = Vec::new();
    let mut prompt_blocks = Vec::new();
    let mut dialogue_transcript = String::new();

    let non_system_count = messages
        .iter()
        .filter(|m| m["role"].as_str().unwrap_or("user") != "system")
        .count();

    for msg in messages {
        let role = msg["role"].as_str().unwrap_or("user");
        let content = &msg["content"];

        if role == "system" {
            if let Some(s) = content_text(content) {
                if !s.trim().is_empty() {
                    system_parts.push(s.trim().to_owned());
                }
            }
            continue;
        }

        let explicit_speaker = msg
            .get("name")
            .and_then(|n| n.as_str())
            .filter(|n| !n.trim().is_empty());

        // Handle string content
        if let Some(text) = content.as_str() {
            let formatted_turn = match explicit_speaker {
                Some(s) => format!("{}: {}", s, text),
                None => {
                    if non_system_count <= 1 {
                        text.to_owned()
                    } else if role == "assistant" {
                        if text.starts_with("Assistant:") {
                            text.to_owned()
                        } else {
                            format!("Assistant: {}", text)
                        }
                    } else if text.starts_with('[') || text.starts_with('*') || text.contains(" (to ") {
                        text.to_owned()
                    } else {
                        format!("User: {}", text)
                    }
                }
            };
            if !dialogue_transcript.is_empty() {
                dialogue_transcript.push('\n');
            }
            dialogue_transcript.push_str(&formatted_turn);
            continue;
        }

        // Handle array content (multimodal / vision blocks)
        if let Some(parts) = content.as_array() {
            for part in parts {
                let part_type = part["type"].as_str().unwrap_or("");
                if part_type == "text" {
                    if let Some(t) = part["text"].as_str() {
                        let formatted_turn = match explicit_speaker {
                            Some(s) => format!("{}: {}", s, t),
                            None => {
                                if non_system_count <= 1 {
                                    t.to_owned()
                                } else if role == "assistant" {
                                    format!("Assistant: {}", t)
                                } else {
                                    format!("User: {}", t)
                                }
                            }
                        };
                        if !dialogue_transcript.is_empty() {
                            dialogue_transcript.push('\n');
                        }
                        dialogue_transcript.push_str(&formatted_turn);
                    }
                } else if part_type == "image_url" {
                    if let Some(url) = part["image_url"]["url"].as_str() {
                        // Support data:image/png;base64,... URIs
                        if let Some(idx) = url.find(";base64,") {
                            let mime = url.strip_prefix("data:").and_then(|s| s.split(';').next()).unwrap_or("image/jpeg");
                            let b64 = &url[idx + 8..];
                            prompt_blocks.push(json!({
                                "type": "image",
                                "data": b64,
                                "mimeType": mime
                            }));
                        } else {
                            bail!("Remote HTTP/HTTPS image URLs are not supported in offline relay mode");
                        }
                    }
                } else if part_type == "image" {
                    prompt_blocks.push(part.clone());
                }
            }
        }
    }

    // Build the system prompt
    let system_prompt = if system_parts.is_empty() {
        DEFAULT_SYSTEM_INSTRUCTION.to_owned()
    } else {
        format!(
            "{}\n\nStrict Output Requirements:\n{}",
            system_parts.join("\n\n"),
            DEFAULT_SYSTEM_INSTRUCTION
        )
    };

    if !dialogue_transcript.trim().is_empty() {
        prompt_blocks.push(json!({
            "type": "text",
            "text": dialogue_transcript.trim()
        }));
    }

    if prompt_blocks.is_empty() {
        prompt_blocks.push(json!({
            "type": "text",
            "text": "Hello"
        }));
    }

    Ok((Some(system_prompt), prompt_blocks))
}

/// State for converting a [`PromptStream`] into OpenAI-style SSE frames.
struct SseState {
    stream: PromptStream,
    buffered: VecDeque<AcpUpdate>,
    model: String,
    created: u64,
    client: Arc<AntigravityClient>,
    started: Instant,
    stop_reason: Option<String>,
    usage: Option<Value>,
    done: bool,
}

impl SseState {
    async fn next_update(&mut self) -> Option<AcpUpdate> {
        if let Some(update) = self.buffered.pop_front() {
            return Some(update);
        }
        self.stream.recv().await
    }

    fn chunk(&self, delta: Value, finish: Option<&str>) -> Value {
        json!({
            "id": format!("chatcmpl-{}", self.created),
            "object": "chat.completion.chunk",
            "created": self.created,
            "model": self.model,
            "choices": [{
                "index": 0,
                "delta": delta,
                "finish_reason": finish
            }]
        })
    }

    /// Mid-stream failure: an OpenRouter-style error chunk followed by `[DONE]`, so the
    /// client sees an explicit error instead of a silently truncated answer.
    fn error_frame(&mut self, message: &str) -> Bytes {
        self.client
            .record_outcome(&self.model, self.started.elapsed(), Some(message));
        self.done = true;
        let mut chunk = self.chunk(json!({ "content": "" }), Some("error"));
        chunk["error"] = json!({
            "code": 502,
            "message": format!("Antigravity completion failed: {}", message)
        });
        Bytes::from(format!("data: {}\n\ndata: [DONE]\n\n", chunk))
    }
}

async fn next_sse_frame(mut st: SseState) -> Option<(std::result::Result<Frame<Bytes>, BoxError>, SseState)> {
    if st.done {
        return None;
    }
    loop {
        match st.next_update().await {
            Some(AcpUpdate::Chunk(text)) => {
                let chunk = st.chunk(json!({ "content": text }), None);
                let bytes = Bytes::from(format!("data: {}\n\n", chunk));
                return Some((Ok(Frame::data(bytes)), st));
            }
            Some(AcpUpdate::Thought(_)) => {}
            Some(AcpUpdate::StopReason(s)) => st.stop_reason = Some(s),
            Some(AcpUpdate::Usage(u)) => st.usage = Some(u),
            Some(AcpUpdate::Completed) => {
                st.client.record_outcome(&st.model, st.started.elapsed(), None);
                st.done = true;
                let mut chunk = st.chunk(json!({}), Some(finish_reason(st.stop_reason.as_deref())));
                if let Some(usage) = st.usage.take() {
                    chunk["usage"] = usage;
                }
                let bytes = Bytes::from(format!("data: {}\n\ndata: [DONE]\n\n", chunk));
                return Some((Ok(Frame::data(bytes)), st));
            }
            Some(AcpUpdate::Error(err)) => {
                let bytes = st.error_frame(&err);
                return Some((Ok(Frame::data(bytes)), st));
            }
            None => {
                let bytes = st.error_frame("ACP prompt stream ended without completing");
                return Some((Ok(Frame::data(bytes)), st));
            }
        }
    }
}

/// Handles an incoming `/v1/chat/completions` request using Antigravity ACP.
pub async fn handle_chat_completion(
    payload: &Value,
    settings: &Value,
    root: PathBuf,
) -> Result<Response<Body>> {
    let (sandbox_dir, configured_path) = configured_paths(settings, &root);
    let client = get_client(sandbox_dir, configured_path);
    handle_chat_completion_with(client, payload, settings).await
}

/// Chat completion against an explicit client (the global one in production, mocks in tests).
async fn handle_chat_completion_with(
    client: Arc<AntigravityClient>,
    payload: &Value,
    settings: &Value,
) -> Result<Response<Body>> {
    let started = Instant::now();
    let stream = payload["stream"].as_bool().unwrap_or(false);
    let messages = payload["messages"]
        .as_array()
        .ok_or_else(|| anyhow!("Request messages must be an array"))?;

    // Determine target model and effort
    let payload_model = payload["model"].as_str();
    let payload_effort = payload["reasoning_effort"].as_str();
    let resolved_model = resolve_model(payload_model, payload_effort, settings);
    let target_model = &resolved_model;

    // Convert messages
    let (system_prompt, prompt_blocks) = openai_to_acp(messages)?;

    // Create session
    let session = match client.new_session(target_model, system_prompt.as_deref()).await {
        Ok(session) => session,
        Err(e) => {
            client.record_error_only(Some(target_model), &format!("{e:#}"));
            return Ok(json_response(
                &json!({
                    "error": format!("Antigravity session initialization failed: {:#}", e),
                    "code": "antigravity_unavailable"
                }),
                StatusCode::BAD_GATEWAY,
            ));
        }
    };

    let created_ts = crate::engine::now() as u64;
    let model_name = target_model.to_owned();

    if stream {
        let mut prompt = client.prompt_stream(session, prompt_blocks);

        // Hold the HTTP status until the turn either produces output or fails, so failures
        // before the first token are reported as 502 instead of an empty 200 stream.
        let mut buffered = VecDeque::new();
        let early_failure = loop {
            match prompt.recv().await {
                Some(AcpUpdate::Error(err)) => break Some(err),
                None => break Some("ACP prompt stream ended before producing output".to_owned()),
                Some(update @ (AcpUpdate::Chunk(_) | AcpUpdate::Completed)) => {
                    buffered.push_back(update);
                    break None;
                }
                Some(AcpUpdate::Thought(_)) => {}
                Some(other) => buffered.push_back(other),
            }
        };
        if let Some(err) = early_failure {
            client.record_outcome(&model_name, started.elapsed(), Some(&err));
            return Ok(json_response(
                &json!({
                    "error": format!("Antigravity completion failed: {}", err),
                    "code": "antigravity_prompt_failed"
                }),
                StatusCode::BAD_GATEWAY,
            ));
        }

        let state = SseState {
            stream: prompt,
            buffered,
            model: model_name,
            created: created_ts,
            client: client.clone(),
            started,
            stop_reason: None,
            usage: None,
            done: false,
        };
        let body = StreamBody::new(futures_util::stream::unfold(state, next_sse_frame)).boxed_unsync();
        Ok(Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", "text/event-stream")
            .header("Cache-Control", "no-cache")
            .header("Connection", "keep-alive")
            .body(body)
            .unwrap())
    } else {
        // Non-streaming
        let timeout_secs = settings["completion_timeout_seconds"]
            .as_f64()
            .filter(|s| s.is_finite() && *s > 0.0)
            .unwrap_or(300.0);
        let res = client
            .prompt_buffered(session, prompt_blocks, Duration::from_secs_f64(timeout_secs))
            .await;

        match res {
            Ok(completion) => {
                client.record_outcome(&model_name, started.elapsed(), None);
                let mut message = json!({
                    "role": "assistant",
                    "content": completion.text
                });
                if let Some(thought) = completion.thought {
                    message["reasoning_content"] = json!(thought);
                }
                let response = json!({
                    "id": format!("chatcmpl-{}", created_ts),
                    "object": "chat.completion",
                    "created": created_ts,
                    "model": model_name,
                    "choices": [{
                        "index": 0,
                        "message": message,
                        "finish_reason": finish_reason(completion.stop_reason.as_deref())
                    }],
                    "usage": completion.usage
                });
                Ok(json_response(&response, StatusCode::OK))
            }
            Err(e) => {
                let msg = e.to_string();
                client.record_outcome(&model_name, started.elapsed(), Some(&msg));
                Ok(json_response(
                    &json!({
                        "error": format!("Antigravity completion failed: {}", msg),
                        "code": "antigravity_completion_failed"
                    }),
                    StatusCode::BAD_GATEWAY,
                ))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_find_agy_path_respects_configured_path() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing-agy.exe");
        assert!(find_agy_path(Some(&missing.to_string_lossy())).is_err());
        let present = dir.path().join("agy.exe");
        std::fs::write(&present, b"").unwrap();
        assert_eq!(find_agy_path(Some(&present.to_string_lossy())).unwrap(), present);
        // A directory is not an executable.
        assert!(find_agy_path(Some(&dir.path().to_string_lossy())).is_err());
    }

    #[test]
    fn test_openai_to_acp_translation() {
        let messages = vec![
            json!({"role": "system", "content": "You are Lydia, sworn to carry burdens."}),
            json!({"role": "user", "content": "Where are we going?"}),
            json!({"role": "assistant", "content": "To the top of High Hrothgar."}),
            json!({"role": "user", "content": "Lead the way."}),
        ];

        let (sys, prompt) = openai_to_acp(&messages).unwrap();
        let sys = sys.unwrap();
        assert!(sys.contains("You are Lydia"));
        assert!(sys.contains("Strict Output Requirements:"));

        assert_eq!(prompt.len(), 1);
        let text = prompt[0]["text"].as_str().unwrap();
        assert!(text.contains("User: Where are we going?"));
        assert!(text.contains("Assistant: To the top of High Hrothgar."));
        assert!(text.contains("User: Lead the way."));
    }

    #[test]
    fn test_openai_system_array_content_is_kept() {
        let messages = vec![
            json!({"role": "system", "content": [{"type": "text", "text": "You are Serana."}, {"type": "text", "text": "Stay in character."}]}),
            json!({"role": "user", "content": "Hello"}),
        ];
        let (sys, _) = openai_to_acp(&messages).unwrap();
        let sys = sys.unwrap();
        assert!(sys.contains("You are Serana.\nStay in character."));
    }

    #[test]
    fn test_openai_vision_remote_url_rejected() {
        let messages = vec![
            json!({
                "role": "user",
                "content": [
                    {"type": "image_url", "image_url": {"url": "https://example.com/image.png"}}
                ]
            })
        ];
        let res = openai_to_acp(&messages);
        assert!(res.is_err(), "Remote HTTP/HTTPS image URLs should be rejected");
    }

    #[test]
    fn test_empty_messages_fallback() {
        let messages: Vec<Value> = vec![];
        let (_, prompt) = openai_to_acp(&messages).unwrap();
        assert!(!prompt.is_empty(), "Prompt should never be completely empty");
    }

    #[test]
    fn test_default_model_is_gemini_flash_low() {
        assert_eq!(DEFAULT_ANTIGRAVITY_MODEL, "gemini-3.8-flash-low");
        assert_eq!(DEFAULT_ANTIGRAVITY_EFFORT, "low");
    }

    #[test]
    fn test_resolve_model_effort_tiers() {
        let empty_settings = json!({});
        assert_eq!(resolve_model(None, None, &empty_settings), "gemini-3.8-flash-low");
        assert_eq!(resolve_model(None, Some("low"), &empty_settings), "gemini-3.8-flash-low");
        assert_eq!(resolve_model(None, Some("medium"), &empty_settings), "gemini-3.8-flash-medium");
        assert_eq!(resolve_model(None, Some("high"), &empty_settings), "gemini-3.8-flash-high");

        let custom_settings = json!({
            "antigravity": {
                "model": "gemini-4-argon",
                "effort": "high"
            }
        });
        assert_eq!(resolve_model(None, None, &custom_settings), "gemini-4-argon-high");
        assert_eq!(resolve_model(None, Some("low"), &custom_settings), "gemini-4-argon-low");
        assert_eq!(resolve_model(Some("claude-opus-5-5-high"), None, &custom_settings), "claude-opus-5-5-high");

        let gpt_oss_settings = json!({
            "antigravity": {
                "model": "gpt-oss-120b-medium",
                "effort": "low"
            }
        });
        assert_eq!(resolve_model(None, None, &gpt_oss_settings), "gpt-oss-120b-medium");
    }

    #[test]
    fn test_configured_paths_ignores_blank_strings() {
        let root = PathBuf::from(r"D:\example\root");
        let s = json!({
            "antigravity": {
                "agy_path": "  ",
                "sandbox_dir": ""
            }
        });
        let (sandbox, agy) = configured_paths(&s, &root);
        assert_eq!(sandbox, root.join("agy-sandbox"));
        assert!(agy.is_none());
    }

    #[test]
    fn test_finish_reason_mapping() {
        assert_eq!(finish_reason(Some("end_turn")), "stop");
        assert_eq!(finish_reason(Some("max_tokens")), "length");
        assert_eq!(finish_reason(Some("refusal")), "content_filter");
        assert_eq!(finish_reason(None), "stop");
    }

    #[test]
    fn test_agent_request_replies() {
        let perm = agent_request_reply(&json!("perm-7"), "session/request_permission");
        assert_eq!(perm["id"], "perm-7");
        assert_eq!(perm["result"]["outcome"]["outcome"], "cancelled");
        assert!(perm.get("error").is_none());
        let other = agent_request_reply(&json!(9), "fs/read_text_file");
        assert_eq!(other["id"], 9);
        assert_eq!(other["error"]["code"], -32601);
    }

    // ---------------------------------------------------------------------------------
    // Mock ACP agent: the test binary re-invokes itself (through a .cmd wrapper that sets
    // MOCK_ENV) and runs `mock_acp_child_entry` as a scripted stdio JSON-RPC agent.
    // No network, no real agy, no model requests.
    // ---------------------------------------------------------------------------------

    const MOCK_ENV: &str = "SKYRIM_BARD_MOCK_ACP";
    const MOCK_LOG_ENV: &str = "SKYRIM_BARD_MOCK_ACP_LOG";

    #[test]
    fn mock_acp_child_entry() {
        if std::env::var_os(MOCK_ENV).is_none() {
            return;
        }
        run_mock_acp();
        std::process::exit(0);
    }

    fn run_mock_acp() {
        use std::collections::HashSet;
        use std::io::{BufRead, Write};
        use std::thread;

        let out = Arc::new(StdMutex::new(std::io::stdout()));
        let send = {
            let out = out.clone();
            move |v: Value| {
                let mut o = out.lock().unwrap();
                let _ = writeln!(o, "{}", v);
                let _ = o.flush();
            }
        };
        let log_path = std::env::var_os(MOCK_LOG_ENV).map(PathBuf::from);
        let log = move |line: &str| {
            if let Some(p) = &log_path {
                if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(p) {
                    let _ = writeln!(f, "{}", line);
                }
            }
        };
        let initialized = Arc::new(AtomicBool::new(false));
        let cancelled: Arc<StdMutex<HashSet<String>>> = Arc::default();
        let replies: Arc<StdMutex<HashMap<String, Value>>> = Arc::default();
        let mut session_counter = 0u32;

        for line in std::io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            let Ok(msg) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            let id = msg["id"].clone();
            let Some(method) = msg["method"].as_str().map(str::to_owned) else {
                // Reply from the client to one of our requests.
                let key = id.as_str().map(str::to_owned).unwrap_or_else(|| id.to_string());
                log(&format!("reply {}", key));
                replies.lock().unwrap().insert(key, msg);
                continue;
            };
            log(&method);

            if method == "initialize" {
                let send = send.clone();
                let initialized = initialized.clone();
                thread::spawn(move || {
                    thread::sleep(Duration::from_millis(300));
                    initialized.store(true, Ordering::SeqCst);
                    send(json!({"jsonrpc": "2.0", "id": id, "result": {"protocolVersion": 1}}));
                });
                continue;
            }
            if !initialized.load(Ordering::SeqCst) {
                log("ERROR request before initialize");
                if !id.is_null() {
                    send(json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32002, "message": "not initialized"}}));
                }
                continue;
            }

            match method.as_str() {
                "session/new" => {
                    session_counter += 1;
                    send(json!({"jsonrpc": "2.0", "id": id, "result": {
                        "sessionId": format!("s{}", session_counter),
                        "models": {"currentModelId": DEFAULT_ANTIGRAVITY_MODEL}
                    }}));
                }
                "session/set_model" | "session/close" => {
                    send(json!({"jsonrpc": "2.0", "id": id, "result": {}}));
                }
                "session/cancel" => {
                    if let Some(sid) = msg["params"]["sessionId"].as_str() {
                        cancelled.lock().unwrap().insert(sid.to_owned());
                    }
                }
                "session/prompt" => {
                    let send = send.clone();
                    let cancelled = cancelled.clone();
                    let replies = replies.clone();
                    thread::spawn(move || {
                        let sid = msg["params"]["sessionId"].as_str().unwrap_or("").to_owned();
                        let text: String = msg["params"]["prompt"]
                            .as_array()
                            .map(|blocks| blocks.iter().filter_map(|b| b["text"].as_str()).collect::<Vec<_>>().join(" "))
                            .unwrap_or_default();
                        let chunk = |t: &str| {
                            json!({"jsonrpc": "2.0", "method": "session/update", "params": {
                                "sessionId": sid,
                                "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": t}}
                            }})
                        };
                        if text.contains("MIDFAIL") {
                            send(chunk("partial "));
                            thread::sleep(Duration::from_millis(50));
                            send(json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32000, "message": "upstream dropped"}}));
                        } else if text.contains("FAIL") {
                            send(json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32000, "message": "quota exhausted"}}));
                        } else if text.contains("PERM") {
                            send(json!({"jsonrpc": "2.0", "id": "perm-1", "method": "session/request_permission", "params": {
                                "sessionId": sid, "toolCall": {"toolCallId": "t1"}, "options": []
                            }}));
                            let deadline = Instant::now() + Duration::from_secs(3);
                            let reply = loop {
                                if let Some(r) = replies.lock().unwrap().remove("perm-1") {
                                    break Some(r);
                                }
                                if Instant::now() > deadline {
                                    break None;
                                }
                                thread::sleep(Duration::from_millis(10));
                            };
                            let ok = reply
                                .map(|r| r["result"]["outcome"]["outcome"] == "cancelled")
                                .unwrap_or(false);
                            send(chunk(if ok { "permission denied cleanly" } else { "bad permission reply" }));
                            send(json!({"jsonrpc": "2.0", "id": id, "result": {"stopReason": "end_turn"}}));
                        } else if text.contains("SLOW") {
                            let deadline = Instant::now() + Duration::from_secs(10);
                            while Instant::now() < deadline && !cancelled.lock().unwrap().contains(&sid) {
                                thread::sleep(Duration::from_millis(10));
                            }
                            send(json!({"jsonrpc": "2.0", "id": id, "result": {"stopReason": "cancelled"}}));
                        } else {
                            send(chunk("Hello, "));
                            send(chunk("traveler."));
                            send(json!({"jsonrpc": "2.0", "id": id, "result": {"stopReason": "end_turn"}}));
                        }
                    });
                }
                _ => {
                    if !id.is_null() {
                        send(json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "unknown"}}));
                    }
                }
            }
        }
    }

    struct MockAgent {
        client: Arc<AntigravityClient>,
        log: PathBuf,
        _dir: tempfile::TempDir,
    }

    impl MockAgent {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let log = dir.path().join("mock-acp.log");
            let exe = std::env::current_exe().unwrap();
            let script = dir.path().join("mock-agy.cmd");
            std::fs::write(
                &script,
                format!(
                    "@echo off\r\nset {MOCK_ENV}=1\r\nset \"{MOCK_LOG_ENV}={}\"\r\n\"{}\" antigravity::tests::mock_acp_child_entry --exact --nocapture --test-threads=1 -q\r\n",
                    log.display(),
                    exe.display()
                ),
            )
            .unwrap();
            let client = Arc::new(AntigravityClient::new(
                dir.path().join("sandbox"),
                Some(script.to_string_lossy().into_owned()),
            ));
            Self { client, log, _dir: dir }
        }

        fn log_lines(&self) -> Vec<String> {
            std::fs::read_to_string(&self.log)
                .unwrap_or_default()
                .lines()
                .map(str::to_owned)
                .collect()
        }

        async fn wait_for_log(&self, needle: &str) -> bool {
            for _ in 0..300 {
                if self.log_lines().iter().any(|l| l == needle) {
                    return true;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            false
        }
    }

    fn user_payload(text: &str, stream: bool) -> Value {
        json!({"stream": stream, "messages": [{"role": "user", "content": text}]})
    }

    async fn body_text(resp: Response<Body>) -> String {
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        String::from_utf8_lossy(&bytes).into_owned()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_requests_wait_for_initialize() {
        let mock = MockAgent::new();
        let tasks: Vec<_> = (0..4)
            .map(|_| {
                let c = mock.client.clone();
                tokio::spawn(async move { c.new_session(DEFAULT_ANTIGRAVITY_MODEL, None).await.map(|s| s.id) })
            })
            .collect();
        for task in tasks {
            let result = task.await.unwrap();
            assert!(result.is_ok(), "session/new failed: {:?}", result.err());
        }
        let lines = mock.log_lines();
        assert_eq!(lines.iter().filter(|l| *l == "initialize").count(), 1);
        assert!(!lines.iter().any(|l| l.starts_with("ERROR")), "requests raced initialize: {lines:?}");
        mock.client.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn streaming_success_ends_with_done() {
        let mock = MockAgent::new();
        let resp = handle_chat_completion_with(mock.client.clone(), &user_payload("hi", true), &json!({}))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_text(resp).await;
        assert!(body.contains("Hello, ") && body.contains("traveler."), "{body}");
        assert!(body.contains("\"finish_reason\":\"stop\""), "{body}");
        assert!(body.trim_end().ends_with("data: [DONE]"), "{body}");
        mock.client.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn streaming_failure_before_output_is_bad_gateway() {
        let mock = MockAgent::new();
        let resp = handle_chat_completion_with(mock.client.clone(), &user_payload("FAIL", true), &json!({}))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
        assert!(body_text(resp).await.contains("quota exhausted"));
        mock.client.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn streaming_failure_mid_turn_emits_error_event() {
        let mock = MockAgent::new();
        let resp = handle_chat_completion_with(mock.client.clone(), &user_payload("MIDFAIL", true), &json!({}))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_text(resp).await;
        assert!(body.contains("partial "), "{body}");
        assert!(body.contains("\"finish_reason\":\"error\""), "{body}");
        assert!(body.contains("upstream dropped"), "{body}");
        assert!(body.trim_end().ends_with("data: [DONE]"), "{body}");
        mock.client.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn permission_requests_get_cancelled_outcome() {
        let mock = MockAgent::new();
        let resp = handle_chat_completion_with(mock.client.clone(), &user_payload("PERM", false), &json!({}))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body: Value = serde_json::from_str(&body_text(resp).await).unwrap();
        assert_eq!(body["choices"][0]["message"]["content"], "permission denied cleanly");
        mock.client.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn dropping_a_stream_cancels_the_turn() {
        let mock = MockAgent::new();
        let session = mock.client.new_session(DEFAULT_ANTIGRAVITY_MODEL, None).await.unwrap();
        let blocks = vec![json!({"type": "text", "text": "SLOW"})];
        let stream = mock.client.prompt_stream(session, blocks);
        assert!(mock.wait_for_log("session/prompt").await);
        drop(stream);
        assert!(mock.wait_for_log("session/cancel").await, "{:?}", mock.log_lines());
        assert!(mock.wait_for_log("session/close").await, "{:?}", mock.log_lines());
        mock.client.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn restart_does_not_poison_the_next_generation() {
        let mock = MockAgent::new();
        let blocks = || vec![json!({"type": "text", "text": "hi"})];
        let first = mock.client.new_session(DEFAULT_ANTIGRAVITY_MODEL, None).await.unwrap();
        mock.client
            .prompt_buffered(first, blocks(), Duration::from_secs(10))
            .await
            .unwrap();
        assert!(mock.client.restart_if_idle().await.unwrap());

        // The new generation starts while the old child's stdout is still closing.
        let second = mock.client.new_session(DEFAULT_ANTIGRAVITY_MODEL, None).await.unwrap();
        let answer = mock
            .client
            .prompt_buffered(second, blocks(), Duration::from_secs(10))
            .await
            .unwrap();
        assert_eq!(answer.text, "Hello, traveler.");
        assert_eq!(answer.stop_reason.as_deref(), Some("end_turn"));

        // Give the old reader time to hit EOF; it must not mark the new child as stopped.
        tokio::time::sleep(Duration::from_millis(800)).await;
        let t = mock.client.telemetry.lock().unwrap().clone();
        assert!(t.acp_running && t.acp_initialized, "{t:?}");
        drop(t);
        mock.client.shutdown().await;
    }
}
