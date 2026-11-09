//! Antigravity ACP (Agent Client Protocol) backend for SkyrimNet and Bard lyrics.
//!
//! Spawns and manages a persistent `agy acp` child process communicating via stdio
//! JSON-RPC 2.0. Translates incoming OpenAI chat completion requests into ACP sessions
//! and prompts, converts responses into OpenAI format (both SSE streams and JSON),
//! and auto-denies any agent tool execution requests for zero-side-effect headless operation.

use anyhow::{anyhow, bail, Context, Result};
use bytes::Bytes;
use http_body_util::{combinators::UnsyncBoxBody, BodyExt, Full, StreamBody};
use hyper::{body::Frame, Response, StatusCode};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, OnceLock,
    },
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, Command},
    sync::{mpsc, oneshot, Mutex},
    time::timeout,
};

type BoxError = Box<dyn std::error::Error + Send + Sync>;
type Body = UnsyncBoxBody<Bytes, BoxError>;

pub const DEFAULT_ANTIGRAVITY_MODEL: &str = "gemini-3.8-flash-high";
pub const DEFAULT_ANTIGRAVITY_EFFORT: &str = "high";

/// Resolves the concrete Antigravity model ID based on payload, configured settings, and effort level.
pub fn resolve_model(
    payload_model: Option<&str>,
    payload_effort: Option<&str>,
    settings: &Value,
) -> String {
    // 1. If payload explicitly specifies an exact Antigravity model ID, use it directly
    if let Some(m) = payload_model {
        let m_clean = m.trim();
        if m_clean.starts_with("gemini-") || m_clean.starts_with("claude-") || m_clean.starts_with("gpt-oss-") {
            return m_clean.to_string();
        }
    }

    // 2. Resolve effort level
    let effort_raw = payload_effort
        .or_else(|| settings["antigravity"]["effort"].as_str())
        .or_else(|| settings["reasoning_effort"].as_str())
        .unwrap_or(DEFAULT_ANTIGRAVITY_EFFORT);

    let effort_tier = match effort_raw.to_lowercase().as_str() {
        "low" | "minimal" => "low",
        "medium" | "med" => "medium",
        "high" | "xhigh" | "max" => "high",
        "off" | "none" => "low", // Flash requires minimum low tier
        _ => "high",
    };

    // 3. Resolve base model
    let configured_model = settings["antigravity"]["model"]
        .as_str()
        .unwrap_or(DEFAULT_ANTIGRAVITY_MODEL);

    let base = configured_model
        .strip_suffix("-low")
        .or_else(|| configured_model.strip_suffix("-medium"))
        .or_else(|| configured_model.strip_suffix("-high"))
        .unwrap_or(configured_model);

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
        if path.exists() {
            return Ok(path);
        }
        bail!("Configured agy_path does not exist: {}", p);
    }

    // 1. Check %LOCALAPPDATA%\agy\bin\agy.exe
    if let Ok(local_app_data) = std::env::var("LOCALAPPDATA") {
        let p = PathBuf::from(local_app_data).join("agy").join("bin").join("agy.exe");
        if p.exists() {
            return Ok(p);
        }
    }

    // 2. Check %USERPROFILE%\.gemini\bin\agy.exe
    if let Ok(user_profile) = std::env::var("USERPROFILE") {
        let p = PathBuf::from(user_profile).join(".gemini").join("bin").join("agy.exe");
        if p.exists() {
            return Ok(p);
        }
    }

    // 3. Check system PATH
    if let Ok(path_var) = std::env::var("PATH") {
        for dir in std::env::split_paths(&path_var) {
            let candidate = dir.join("agy.exe");
            if candidate.exists() {
                return Ok(candidate);
            }
            let candidate_bare = dir.join("agy");
            if candidate_bare.exists() {
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

struct SessionChannels {
    sender: mpsc::UnboundedSender<AcpUpdate>,
}

struct ChildProcessHandle {
    child: Child,
    stdin_tx: mpsc::Sender<Vec<u8>>,
    initialized: bool,
}

type PendingRequestsMap = HashMap<u64, oneshot::Sender<Result<Value, String>>>;
type ActiveSessionsMap = HashMap<String, SessionChannels>;

pub struct AntigravityClient {
    sandbox_dir: PathBuf,
    configured_agy_path: Option<String>,
    inner: Mutex<Option<ChildProcessHandle>>,
    next_id: AtomicU64,
    pending_requests: Arc<Mutex<PendingRequestsMap>>,
    active_sessions: Arc<Mutex<ActiveSessionsMap>>,
    is_shutting_down: AtomicBool,
}

impl AntigravityClient {
    pub fn new(sandbox_dir: PathBuf, configured_agy_path: Option<String>) -> Self {
        Self {
            sandbox_dir,
            configured_agy_path,
            inner: Mutex::new(None),
            next_id: AtomicU64::new(1),
            pending_requests: Arc::new(Mutex::new(HashMap::new())),
            active_sessions: Arc::new(Mutex::new(HashMap::new())),
            is_shutting_down: AtomicBool::new(false),
        }
    }

    fn next_request_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::SeqCst)
    }

    /// Ensures the child `agy acp` process is running and initialized.
    async fn ensure_running(&self) -> Result<()> {
        let mut guard = self.inner.lock().await;
        if let Some(ref mut handle) = *guard {
            // Check if process has exited
            match handle.child.try_wait() {
                Ok(None) => {
                    // Still running
                    return Ok(());
                }
                _ => {
                    // Terminated; clear and respawn
                    *guard = None;
                }
            }
        }

        if self.is_shutting_down.load(Ordering::SeqCst) {
            bail!("Antigravity client is shutting down");
        }

        let agy_bin = find_agy_path(self.configured_agy_path.as_deref())?;
        std::fs::create_dir_all(&self.sandbox_dir)
            .context("Failed to create Antigravity sandbox directory")?;

        let mut cmd = Command::new(&agy_bin);
        cmd.arg("acp")
            .current_dir(&self.sandbox_dir)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null());

        let mut child = cmd
            .spawn()
            .with_context(|| format!("Failed to spawn agy acp process at {:?}", agy_bin))?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("Child stdin not captured"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("Child stdout not captured"))?;

        let pending_map = self.pending_requests.clone();
        let sessions_map = self.active_sessions.clone();

        let (stdin_tx, mut stdin_rx) = mpsc::channel::<Vec<u8>>(128);
        let mut child_stdin = stdin;
        tokio::spawn(async move {
            while let Some(bytes) = stdin_rx.recv().await {
                if child_stdin.write_all(&bytes).await.is_err() {
                    break;
                }
                if child_stdin.flush().await.is_err() {
                    break;
                }
            }
        });

        let stdin_for_replies = stdin_tx.clone();

        tokio::spawn(async move {
            let reader = BufReader::new(stdout);
            let mut lines = reader.lines();

            while let Ok(Some(line)) = lines.next_line().await {
                if line.trim().is_empty() {
                    continue;
                }
                let incoming: Value = match serde_json::from_str(&line) {
                    Ok(msg) => msg,
                    Err(_) => continue,
                };

                // Case 1: Response or Agent request with id
                if let Some(id) = incoming["id"].as_u64() {
                    // If it also has a method, it is an incoming request from the agent to us!
                    if let Some(method) = incoming["method"].as_str() {
                        // Agent is requesting permission or tool execution -> Auto-deny!
                        let denial = json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "error": {
                                "code": -32601,
                                "message": format!("Method or tool '{}' denied in headless relay mode", method)
                            }
                        });
                        let mut line_bytes = serde_json::to_vec(&denial).unwrap_or_default();
                        line_bytes.push(b'\n');
                        let _ = stdin_for_replies.send(line_bytes).await;
                        continue;
                    }

                    // Pure response with id
                    let mut pending = pending_map.lock().await;
                    if let Some(sender) = pending.remove(&id) {
                        if !incoming["error"].is_null() {
                            let err = &incoming["error"];
                            let msg = err["message"]
                                .as_str()
                                .map(str::to_owned)
                                .unwrap_or_else(|| err.to_string());
                            let _ = sender.send(Err(msg));
                        } else {
                            let _ = sender.send(Ok(incoming["result"].clone()));
                        }
                    }
                    continue;
                }

                // Case 2: Notification from child (no id)
                if let Some(method) = incoming["method"].as_str() {
                    if method == "session/update" {
                        let params = &incoming["params"];
                        let sid = params["sessionId"].as_str().unwrap_or("");
                        let update = &params["update"];
                        let update_kind = update["sessionUpdate"].as_str().unwrap_or("");

                        let sessions = sessions_map.lock().await;
                        if let Some(s) = sessions.get(sid) {
                            if update_kind == "agent_message_chunk" {
                                if let Some(text) = update["content"]["text"].as_str() {
                                    let _ = s.sender.send(AcpUpdate::Chunk(text.to_owned()));
                                }
                            } else if update_kind == "agent_thought_chunk" {
                                if let Some(text) = update["content"]["text"].as_str() {
                                    let _ = s.sender.send(AcpUpdate::Thought(text.to_owned()));
                                }
                            }
                        }
                    }
                }
            }

            // If stdout closed, reject all pending requests
            let mut pending = pending_map.lock().await;
            for (_, sender) in pending.drain() {
                let _ = sender.send(Err("Antigravity ACP process terminated unexpectedly".into()));
            }

            // And notify active sessions
            let mut sessions = sessions_map.lock().await;
            for (_, s) in sessions.drain() {
                let _ = s.sender.send(AcpUpdate::Error("ACP process closed".into()));
            }
        });

        // Store child handle
        *guard = Some(ChildProcessHandle {
            child,
            stdin_tx,
            initialized: false,
        });

        drop(guard);

        // Perform ACP initialize handshake
        let init_params = json!({
            "protocolVersion": 1,
            "clientCapabilities": {
                "fs": { "readTextFile": false, "writeTextFile": false },
                "terminal": false
            }
        });

        self.call("initialize", init_params).await?;

        let mut guard = self.inner.lock().await;
        if let Some(ref mut handle) = *guard {
            handle.initialized = true;
        }

        Ok(())
    }

    /// Sends a JSON-RPC request to the child process and waits for the response up to timeout_dur.
    pub async fn call_with_timeout(&self, method: &str, params: Value, timeout_dur: Duration) -> Result<Value> {
        let req_id = self.next_request_id();
        let (tx, rx) = oneshot::channel();

        {
            let mut pending = self.pending_requests.lock().await;
            pending.insert(req_id, tx);
        }

        let request = json!({
            "jsonrpc": "2.0",
            "id": req_id,
            "method": method,
            "params": params
        });

        let mut bytes = serde_json::to_vec(&request)?;
        bytes.push(b'\n');

        let stdin_tx = {
            let guard = self.inner.lock().await;
            let handle = guard
                .as_ref()
                .ok_or_else(|| anyhow!("ACP child process not active"))?;
            handle.stdin_tx.clone()
        };

        stdin_tx
            .send(bytes)
            .await
            .map_err(|_| anyhow!("Failed to send to ACP child process stdin"))?;

        match timeout(timeout_dur, rx).await {
            Ok(Ok(Ok(val))) => Ok(val),
            Ok(Ok(Err(err_msg))) => bail!("ACP call '{}' returned error: {}", method, err_msg),
            Ok(Err(_)) => bail!("ACP call '{}' response channel canceled", method),
            Err(_) => {
                let mut pending = self.pending_requests.lock().await;
                pending.remove(&req_id);
                bail!("ACP call '{}' timed out after {:?} waiting for response", method, timeout_dur);
            }
        }
    }

    /// Sends a JSON-RPC request to the child process with a standard 30s timeout.
    pub async fn call(&self, method: &str, params: Value) -> Result<Value> {
        self.call_with_timeout(method, params, Duration::from_secs(30)).await
    }

    /// Sends a JSON-RPC notification (no response expected).
    pub async fn notify(&self, method: &str, params: Value) -> Result<()> {
        let note = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params
        });
        let mut bytes = serde_json::to_vec(&note)?;
        bytes.push(b'\n');

        let stdin_tx = {
            let guard = self.inner.lock().await;
            guard.as_ref().map(|h| h.stdin_tx.clone())
        };

        if let Some(tx) = stdin_tx {
            let _ = tx.send(bytes).await;
        }
        Ok(())
    }

    /// Creates a new ACP session with custom model and optional system prompt extension.
    pub async fn new_session(&self, model: &str, system_prompt: Option<&str>) -> Result<String> {
        self.ensure_running().await?;

        let meta = json!({
            "antigravity": {
                "systemPrompt": {
                    "append": system_prompt.unwrap_or(DEFAULT_SYSTEM_INSTRUCTION)
                }
            }
        });

        let session_params = json!({
            "cwd": self.sandbox_dir.to_string_lossy(),
            "mcpServers": [],
            "_meta": meta
        });

        let res = self.call("session/new", session_params).await?;
        let session_id = res["sessionId"]
            .as_str()
            .ok_or_else(|| anyhow!("session/new did not return sessionId"))?
            .to_owned();

        // Switch to the requested model if different
        let current_model = res["models"]["currentModelId"].as_str().unwrap_or("");
        if !model.is_empty() && model != current_model {
            let _ = self
                .call(
                    "session/set_model",
                    json!({
                        "sessionId": &session_id,
                        "modelId": model
                    }),
                )
                .await;
        }

        Ok(session_id)
    }

    /// Initiates a prompt turn and returns a receiver for incremental update chunks.
    pub async fn prompt_stream(
        self: Arc<Self>,
        session_id: &str,
        prompt: Vec<Value>,
    ) -> Result<mpsc::UnboundedReceiver<AcpUpdate>> {
        self.ensure_running().await?;

        let (tx, rx) = mpsc::unbounded_channel();
        {
            let mut sessions = self.active_sessions.lock().await;
            sessions.insert(
                session_id.to_owned(),
                SessionChannels { sender: tx.clone() },
            );
        }

        let sid = session_id.to_owned();
        let client = self.clone();

        tokio::spawn(async move {
            let prompt_params = json!({
                "sessionId": &sid,
                "prompt": prompt
            });

            // Allow up to 300s for prompt turn (handles 30k tokens and vision encode)
            match client.call_with_timeout("session/prompt", prompt_params, Duration::from_secs(300)).await {
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
                    let _ = tx.send(AcpUpdate::Error(e.to_string()));
                }
            }

            // Cleanup session from active map
            let mut sessions = client.active_sessions.lock().await;
            sessions.remove(&sid);

            // Cleanly close ACP session to conserve quota and avoid history clutter
            let _ = client
                .notify("session/cancel", json!({ "sessionId": &sid }))
                .await;
            let _ = client
                .call("session/close", json!({ "sessionId": &sid }))
                .await;
        });

        Ok(rx)
    }

    /// Non-streaming prompt execution: buffers all chunks and returns complete string, optional reasoning & usage.
    pub async fn prompt_buffered(
        self: Arc<Self>,
        session_id: &str,
        prompt: Vec<Value>,
        timeout_duration: Duration,
    ) -> Result<(String, Option<String>, Value)> {
        let mut rx = self.clone().prompt_stream(session_id, prompt).await?;
        let mut full_text = String::new();
        let mut full_thought = String::new();
        let mut usage = json!({});

        let res = timeout(timeout_duration, async {
            while let Some(update) = rx.recv().await {
                match update {
                    AcpUpdate::Chunk(c) => full_text.push_str(&c),
                    AcpUpdate::Thought(t) => full_thought.push_str(&t),
                    AcpUpdate::Usage(u) => usage = u,
                    AcpUpdate::Completed => break,
                    AcpUpdate::Error(err) => bail!("ACP prompt failed: {}", err),
                    _ => {}
                }
            }
            Ok(())
        })
        .await;

        match res {
            Ok(Ok(())) => {
                let thought_opt = if full_thought.is_empty() {
                    None
                } else {
                    Some(full_thought)
                };
                Ok((full_text, thought_opt, usage))
            }
            Ok(Err(e)) => Err(e),
            Err(_) => {
                // Cancel prompt
                let _ = self
                    .notify("session/cancel", json!({ "sessionId": session_id }))
                    .await;
                let _ = self
                    .call("session/close", json!({ "sessionId": session_id }))
                    .await;
                bail!("Antigravity prompt execution timed out");
            }
        }
    }

    /// Graceful termination: terminates child process cleanly.
    pub async fn shutdown(&self) {
        self.is_shutting_down.store(true, Ordering::SeqCst);
        let mut guard = self.inner.lock().await;
        if let Some(mut handle) = guard.take() {
            let _ = handle.child.kill().await;
        }
    }
}

/// Global shared singleton client for the application runtime.
static GLOBAL_ANTIGRAVITY: OnceLock<Arc<AntigravityClient>> = OnceLock::new();

pub fn get_client(sandbox_dir: PathBuf, agy_path: Option<String>) -> Arc<AntigravityClient> {
    GLOBAL_ANTIGRAVITY
        .get_or_init(|| Arc::new(AntigravityClient::new(sandbox_dir, agy_path)))
        .clone()
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
            if let Some(s) = content.as_str() {
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

/// Handles an incoming `/v1/chat/completions` request using Antigravity ACP.
pub async fn handle_chat_completion(
    payload: &Value,
    settings: &Value,
    root: PathBuf,
) -> Result<Response<Body>> {
    let stream = payload["stream"].as_bool().unwrap_or(false);
    let messages = payload["messages"]
        .as_array()
        .ok_or_else(|| anyhow!("Request messages must be an array"))?;

    // Determine target model and effort
    let payload_model = payload["model"].as_str();
    let payload_effort = payload["reasoning_effort"].as_str();
    let resolved_model = resolve_model(payload_model, payload_effort, settings);
    let target_model = &resolved_model;

    let configured_path = settings["antigravity"]["agy_path"]
        .as_str()
        .map(str::to_owned);

    let sandbox_dir = settings["antigravity"]["sandbox_dir"]
        .as_str()
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join("agy-sandbox"));

    let client = get_client(sandbox_dir, configured_path);

    // Convert messages
    let (system_prompt, prompt_blocks) = openai_to_acp(messages)?;

    // Create session
    let session_id = match client.new_session(target_model, system_prompt.as_deref()).await {
        Ok(sid) => sid,
        Err(e) => {
            return Ok(json_response(
                &json!({
                    "error": format!("Antigravity session initialization failed: {}", e),
                    "code": "antigravity_unavailable"
                }),
                StatusCode::BAD_GATEWAY,
            ));
        }
    };

    let created_ts = crate::engine::now() as u64;
    let model_name = target_model.to_owned();

    if stream {
        // SSE Streaming
        let rx = match client.clone().prompt_stream(&session_id, prompt_blocks).await {
            Ok(rx) => rx,
            Err(e) => {
                return Ok(json_response(
                    &json!({
                        "error": format!("Antigravity prompt stream failed: {}", e),
                        "code": "antigravity_prompt_failed"
                    }),
                    StatusCode::BAD_GATEWAY,
                ));
            }
        };

        let stream = futures_util::stream::unfold(
            (rx, model_name, created_ts, false),
            move |(mut rx, model, created, finished)| async move {
                if finished {
                    return None;
                }
                while let Some(update) = rx.recv().await {
                    match update {
                        AcpUpdate::Chunk(delta_text) => {
                            let chunk = json!({
                                "id": format!("chatcmpl-{}", created),
                                "object": "chat.completion.chunk",
                                "created": created,
                                "model": model,
                                "choices": [{
                                    "index": 0,
                                    "delta": { "content": delta_text },
                                    "finish_reason": null
                                }]
                            });
                            let line = format!("data: {}\n\n", serde_json::to_string(&chunk).unwrap_or_default());
                            return Some((
                                Ok::<_, BoxError>(Frame::data(Bytes::from(line))),
                                (rx, model, created, false),
                            ));
                        }
                        AcpUpdate::Completed => {
                            let final_chunk = json!({
                                "id": format!("chatcmpl-{}", created),
                                "object": "chat.completion.chunk",
                                "created": created,
                                "model": model,
                                "choices": [{
                                    "index": 0,
                                    "delta": {},
                                    "finish_reason": "stop"
                                }]
                            });
                            let finish_data = format!(
                                "data: {}\n\ndata: [DONE]\n\n",
                                serde_json::to_string(&final_chunk).unwrap_or_default()
                            );
                            return Some((
                                Ok::<_, BoxError>(Frame::data(Bytes::from(finish_data))),
                                (rx, model, created, true),
                            ));
                        }
                        AcpUpdate::Error(_) => {
                            // On error, terminate stream
                            return None;
                        }
                        _ => {}
                    }
                }
                None
            },
        );

        let body = StreamBody::new(stream).boxed_unsync();
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
            .unwrap_or(300.0);
        let res = client
            .clone()
            .prompt_buffered(&session_id, prompt_blocks, Duration::from_secs_f64(timeout_secs))
            .await;

        match res {
            Ok((full_text, full_thought, usage)) => {
                let mut message = json!({
                    "role": "assistant",
                    "content": full_text
                });
                if let Some(thought) = full_thought {
                    message["reasoning_content"] = json!(thought);
                }
                let completion = json!({
                    "id": format!("chatcmpl-{}", created_ts),
                    "object": "chat.completion",
                    "created": created_ts,
                    "model": model_name,
                    "choices": [{
                        "index": 0,
                        "message": message,
                        "finish_reason": "stop"
                    }],
                    "usage": usage
                });
                Ok(json_response(&completion, StatusCode::OK))
            }
            Err(e) => Ok(json_response(
                &json!({
                    "error": format!("Antigravity completion failed: {}", e),
                    "code": "antigravity_completion_failed"
                }),
                StatusCode::BAD_GATEWAY,
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_find_agy_path_or_err() {
        // find_agy_path should find either localappdata, userprofile or PATH
        let res = find_agy_path(None);
        assert!(res.is_ok(), "find_agy_path should locate agy on this host");
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
    fn test_default_model_is_gemini_flash_high() {
        assert_eq!(DEFAULT_ANTIGRAVITY_MODEL, "gemini-3.8-flash-high");
        assert_eq!(DEFAULT_ANTIGRAVITY_EFFORT, "high");
    }

    #[test]
    fn test_resolve_model_effort_tiers() {
        let empty_settings = json!({});
        assert_eq!(resolve_model(None, None, &empty_settings), "gemini-3.8-flash-high");
        assert_eq!(resolve_model(None, Some("low"), &empty_settings), "gemini-3.8-flash-low");
        assert_eq!(resolve_model(None, Some("medium"), &empty_settings), "gemini-3.8-flash-medium");
        assert_eq!(resolve_model(None, Some("high"), &empty_settings), "gemini-3.8-flash-high");

        let custom_settings = json!({
            "antigravity": {
                "model": "gemini-4-argon",
                "effort": "low"
            }
        });
        assert_eq!(resolve_model(None, None, &custom_settings), "gemini-4-argon-low");
        assert_eq!(resolve_model(None, Some("high"), &custom_settings), "gemini-4-argon-high");
        assert_eq!(resolve_model(Some("claude-opus-5-5-high"), None, &custom_settings), "claude-opus-5-5-high");
    }
}
