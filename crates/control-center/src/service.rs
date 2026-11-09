use crate::{
    config::{self, Config, Result},
    feedback_worker::FeedbackWorker,
    http, platform,
    shader_precompile::PrecompileManager,
};
use serde_json::{Value, json};
use std::{
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        mpsc::{self, Sender},
    },
    thread,
    time::{Duration, Instant},
};

pub type Shared = Arc<Mutex<Value>>;
pub struct Request {
    pub action: String,
    pub body: Value,
    pub reply: Option<Sender<Result<Value>>>,
}
pub struct Service {
    pub tx: Sender<Request>,
    feedback_tx: Sender<Request>,
    pub status: Shared,
    pub stop: Arc<std::sync::atomic::AtomicBool>,
    precompile: Arc<PrecompileManager>,
    worker: Option<thread::JoinHandle<()>>,
    server: Option<thread::JoinHandle<()>>,
}
fn random_token() -> Result<String> {
    let mut b = [0u8; 32];
    let code = unsafe {
        windows_sys::Win32::Security::Cryptography::BCryptGenRandom(
            std::ptr::null_mut(),
            b.as_mut_ptr(),
            b.len() as u32,
            windows_sys::Win32::Security::Cryptography::BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        )
    };
    if code != 0 {
        return Err("Cannot create local control credential".into());
    }
    Ok(b.iter().map(|x| format!("{x:02x}")).collect())
}
impl Service {
    pub fn start(
        config: Config,
        root: PathBuf,
        preview: bool,
        start_session: bool,
    ) -> Result<Self> {
        let (tx, rx) = mpsc::channel();
        let (feedback_tx, feedback_rx) = mpsc::channel();
        let status = Arc::new(Mutex::new(
            json!({"runtime":"rust","preview":preview,"lastAction":"Ready","music":config::public_music(&config.music_settings()?),"feedbackSettings":config.feedback_settings()?,"feedback":{"enabled":false,"leftProfile":"off","rightProfile":"off"}}),
        ));
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let server = if !preview {
            let listener = TcpListener::bind(("127.0.0.1", config.agent_port)).map_err(|_| {
                "Control Center is already running or its agent port is occupied".to_string()
            })?;
            listener.set_nonblocking(true).map_err(|e| e.to_string())?;
            fs::create_dir_all(root.join(".local")).map_err(|e| e.to_string())?;
            let token = random_token()?;
            fs::write(root.join(".local/agent-token"), &token).map_err(|e| e.to_string())?;
            let tx = tx.clone();
            let status = status.clone();
            let direct_feedback = feedback_tx.clone();
            let stopped = stop.clone();
            Some(thread::spawn(move || {
                while !stopped.load(std::sync::atomic::Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((s, addr)) if addr.ip().is_loopback() => {
                            let token = token.clone();
                            let tx = tx.clone();
                            let feedback = direct_feedback.clone();
                            let status = status.clone();
                            thread::spawn(move || {
                                serve_request(s, &token, &tx, &feedback, &status)
                            });
                        }
                        _ => thread::sleep(Duration::from_millis(70)),
                    }
                }
            }))
        } else {
            None
        };
        let shared = status.clone();
        let stopped = stop.clone();
        let feedback_sender = feedback_tx.clone();
        let precompile = Arc::new(PrecompileManager::new());
        let precompile_worker = precompile.clone();
        let worker = thread::spawn(move || {
            let feedback_worker =
                FeedbackWorker::start(config.clone(), preview, feedback_sender, feedback_rx);
            let mut feedback_action_id = 0u64;
            let mut last_refresh = Instant::now() - Duration::from_secs(4);
            let mut music_health = json!({"available":false});
            let mut action_message = if preview {
                "Preview: services and hardware are inactive".to_string()
            } else {
                "Native controls ready".to_string()
            };
            let mut recovery_note: Option<String> = None;
            if !preview {
                recovery_note = crate::shader_precompile::recover_stale_headless_steamvr(&root);
            }
            let mut known_processes = platform::processes();
            let shader_cfg = config.effective_shader_cache();
            let mut nr_status = crate::nr_guard::status(config.nr_guard.as_ref());
            let mut kat_stub_status = crate::kat::stub_status(&root);
            let mut shader_cache_status = crate::shader_cache::status(shader_cfg.as_ref());
            let mut was_precompiling = false;
            if !preview {
                match start_music(&config, &root) {
                    Ok(v) => {
                        action_message = v["message"]
                            .as_str()
                            .unwrap_or("Music relay ready")
                            .to_owned()
                    }
                    Err(e) => action_message = e,
                }
                if let Some(note) = recovery_note.take() {
                    action_message = format!("{action_message} · {note}");
                }
            }
            if !preview && start_session {
                let _ = launch_session(&config);
            }
            while !stopped.load(std::sync::atomic::Ordering::Relaxed) {
                while let Ok(req) = rx.try_recv() {
                    let result = if preview {
                        Err(
                            "Preview is read-only; launch without --preview to activate controls"
                                .into(),
                        )
                    } else {
                        perform(
                            &req.action,
                            &req.body,
                            &config,
                            &root,
                            &feedback_worker,
                            &precompile_worker,
                        )
                    };
                    action_message = match &result {
                        Ok(v) => v["message"].as_str().unwrap_or("Done").to_owned(),
                        Err(e) => format!("Action stopped: {e}"),
                    };
                    if matches!(
                        req.action.as_str(),
                        "nr-disable"
                            | "nr-restore"
                            | "kat-stub-install"
                            | "kat-stub-restore"
                            | "shader-cache-clear"
                            | "shader-cache-restore"
                            | "shader-cache-enforce-settings"
                            | "shader-precompile-start"
                            | "shader-precompile-cancel"
                    ) {
                        last_refresh = Instant::now() - Duration::from_secs(4);
                    }
                    if let Some(reply) = req.reply {
                        let _ = reply.send(result);
                    }
                }
                let precompile_snap = precompile_worker.snapshot_json();
                let now_precompiling = precompile_snap["active"] == true;
                if was_precompiling && !now_precompiling {
                    last_refresh = Instant::now() - Duration::from_secs(4);
                    if let Some(msg) = precompile_snap["message"].as_str() {
                        action_message = msg.to_owned();
                    }
                }
                was_precompiling = now_precompiling;
                if last_refresh.elapsed() >= Duration::from_secs(2) {
                    music_health = match http::request(8097, "GET", "/health", None, None) {
                        Ok(mut v) => {
                            v["available"] = json!(true);
                            v
                        }
                        Err(_) => json!({"available":false,"message":"Music relay is unavailable"}),
                    };
                    known_processes = platform::processes();
                    nr_status = crate::nr_guard::status(config.nr_guard.as_ref());
                    kat_stub_status = crate::kat::stub_status(&root);
                    shader_cache_status = crate::shader_cache::status(shader_cfg.as_ref());
                    last_refresh = Instant::now();
                }
                let feedback_state = feedback_worker.status.lock().unwrap().clone();
                if feedback_state["actionId"].as_u64().unwrap_or(0) != feedback_action_id {
                    feedback_action_id = feedback_state["actionId"].as_u64().unwrap_or(0);
                    action_message = feedback_state["lastAction"]
                        .as_str()
                        .unwrap_or("Done")
                        .to_owned();
                }
                let dependencies:Vec<Value>=config.dependencies.iter().map(|d|json!({"id":d.id,"label":d.label,"running":known_processes.iter().any(|(_,n)|n.eq_ignore_ascii_case(&d.process)),"exists":d.target.starts_with("steam://")||Path::new(&d.target).is_file(),"session":d.session})).collect();
                let snapshot = json!({"runtime":"rust","version":env!("CARGO_PKG_VERSION"),"preview":preview,"pid":std::process::id(),"updatedAt":config::now(),"lastAction":action_message,"music":config.music_settings().map(|v|config::public_music(&v)).unwrap_or(json!({})),"musicHealth":music_health,"feedback":feedback_state["feedback"],"feedbackSettings":feedback_state["settings"],"dependencies":dependencies,"gameRunning":known_processes.iter().any(|(_,n)|n.eq_ignore_ascii_case("SkyrimVR.exe"))});
                if let Ok(mut s) = shared.lock() {
                    *s = snapshot;
                    s["nrGuard"] = nr_status.clone();
                    s["katOverlayStubs"] = kat_stub_status.clone();
                    let mut sc_merged = shader_cache_status.clone();
                    sc_merged["precompile"] = precompile_snap;
                    s["shaderCache"] = sc_merged;
                }
                thread::sleep(Duration::from_millis(200));
            }
            drop(feedback_worker);
            if !preview {
                let _ = config::write_json(
                    &root.join(".local/last-status.json"),
                    &json!({"closedAt":config::now(),"feedback":"released","music":"background relay retained for shared NPC dialogue route"}),
                );
            }
        });
        Ok(Self {
            tx,
            feedback_tx,
            status,
            stop,
            precompile,
            worker: Some(worker),
            server,
        })
    }
    pub fn send(&self, action: &str, body: Value) {
        let target = if action.starts_with("feedback-") {
            &self.feedback_tx
        } else {
            &self.tx
        };
        let _ = target.send(Request {
            action: action.into(),
            body,
            reply: None,
        });
    }
}
impl Drop for Service {
    fn drop(&mut self) {
        self.precompile.cancel_and_wait(Duration::from_secs(35));
        let _ = self.feedback_tx.send(Request {
            action: "feedback-shutdown".into(),
            body: json!({}),
            reply: None,
        });
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
        if let Some(w) = self.server.take() {
            let _ = w.join();
        }
    }
}
fn start_music(config: &Config, root: &Path) -> Result<Value> {
    if let Ok(health) = http::request(8097, "GET", "/health", None, None) {
        return Ok(
            json!({"message":if health["runtime"]=="rust"{"Existing native Rust music relay is running"}else{"Existing music relay is running; preserved until it can be safely replaced"},"reused":true}),
        );
    }
    let path = root.join("skyrim-bard-engine.exe");
    let pid = platform::spawn_engine(
        &path,
        &config.music_root,
        &root.join(".local/music-engine.log"),
    )?;
    for _ in 0..15 {
        thread::sleep(Duration::from_millis(200));
        if let Ok(v) = http::request(8097, "GET", "/health", None, None) {
            return Ok(
                json!({"message":"Native Rust music relay started in background","pid":pid,"runtime":v["runtime"]}),
            );
        }
    }
    Err("Music relay did not become ready. Inspect .local/music-engine.log; generation was not requested".into())
}
fn launch_dependency(c: &Config, id: &str) -> Result<Value> {
    let d = c
        .dependencies
        .iter()
        .find(|d| d.id == id)
        .ok_or("Unknown dependency")?;
    if platform::is_running(&d.process) {
        return Ok(json!({"message":format!("{} is already running",d.label)}));
    }
    platform::open(&d.target)?;
    Ok(json!({"message":format!("Started {}",d.label)}))
}
fn launch_session(c: &Config) -> Result<Value> {
    let mut results = vec![];
    for d in c.dependencies.iter().filter(|d| d.session) {
        match launch_dependency(c, &d.id) {
            Ok(v) => results.push(v),
            Err(e) => results.push(json!({"error":e,"dependency":d.id})),
        }
    }
    let mut message = "VR dependency launch completed; check process readiness below.".to_owned();
    if c.close_kat_overlays {
        let overlays = crate::kat::close(true)?;
        message.push(' ');
        message.push_str(
            overlays["message"]
                .as_str()
                .unwrap_or("Overlay close pass complete"),
        );
        results.push(overlays);
    }
    Ok(json!({"message":message,"results":results}))
}
fn perform(
    action: &str,
    body: &Value,
    c: &Config,
    root: &Path,
    feedback_worker: &FeedbackWorker,
    precompile: &PrecompileManager,
) -> Result<Value> {
    if precompile.is_active()
        && matches!(
            action,
            "start-session"
                | "launch"
                | "driver-options"
                | "nr-disable"
                | "nr-restore"
                | "shader-cache-clear"
                | "shader-cache-restore"
                | "shader-cache-enforce-settings"
                | "feedback-start"
                | "feedback-test"
        )
    {
        return Err(
            "Headless shader pre-compile is currently running; wait for completion or cancel pre-compile first."
                .into(),
        );
    }
    match action {
        "music-downloads-auto" => {
            let enabled = body["enabled"]
                .as_bool()
                .ok_or("Automatic downloads must be boolean")?;
            let mut settings = c.music_settings()?;
            settings["keep_both_suno_variants"] = json!(enabled);
            config::write_json(&c.music_root.join("settings.json"), &settings)?;
            Ok(
                json!({"message":if enabled{"Automatic existing-song downloads enabled"}else{"Automatic downloads disabled; manual Download now remains available"}}),
            )
        }
        "music-downloads-open" => {
            let folder = match body["folder"].as_str() {
                Some("to_import") => "To import into SkyrimNet",
                Some("cached") => "Already cached by SkyrimNet",
                Some("unconfirmed") => "Unconfirmed",
                None | Some("") => "",
                _ => return Err("Unknown song folder".into()),
            };
            let path = c.music_root.join("Song Downloads").join(folder);
            fs::create_dir_all(&path).map_err(|_| "Song folder could not be created")?;
            platform::open(&path.to_string_lossy())?;
            Ok(json!({"message":"Song download folder opened"}))
        }
        "music-downloads-sync"
        | "music-downloads-task"
        | "music-downloads-recover"
        | "music-downloads-credits" => {
            let token = fs::read_to_string(c.music_root.join("data/native-adapter-token.txt"))
                .map_err(|_| "Local music credential unavailable")?;
            let endpoint = match action {
                "music-downloads-task" => "/archive/task",
                "music-downloads-recover" => "/archive/recover",
                "music-downloads-credits" => "/archive/credits",
                _ => "/archive/sync",
            };
            http::request(8097, "POST", endpoint, Some(body), Some(&token))
        }
        "music-start" => start_music(c, root),
        "music-save" => {
            let current = c.music_settings()?;
            let next = config::patch_music(&current, body)?;
            config::write_json(&c.music_root.join("settings.json"), &next)?;
            Ok(
                json!({"message":"Song preferences saved for the next native request; no generation started"}),
            )
        }
        "music-pause" | "music-arm-one" => {
            let token = fs::read_to_string(c.music_root.join("data/native-adapter-token.txt"))
                .map_err(|_| "Local music credential is unavailable".to_string())?;
            let endpoint = if action == "music-pause" {
                "/controls/cancel-allowance"
            } else {
                "/controls/arm-one"
            };
            let payload = if action == "music-pause" {
                json!({})
            } else {
                json!({"consent":"Allow one native Suno generation, estimated 12 credits"})
            };
            http::request(8097, "POST", endpoint, Some(&payload), Some(&token))?;
            Ok(
                json!({"message":if action=="music-pause"{"New custom lyric requests paused; existing music tasks may finish"}else{"One new native song request armed (about 12 Suno credits when generated)"}}),
            )
        }
        "music-resume" => {
            let mut v = c.music_settings()?;
            if v["native_recurring_suno_authorization"]
                .as_str()
                .is_none_or(|s| s.is_empty())
            {
                return Err("Ongoing mode has no existing explicit human authorization".into());
            }
            v["native_spending_mode"] = json!("native_automatic");
            config::write_json(&c.music_root.join("settings.json"), &v)?;
            Ok(
                json!({"message":"Previously authorized ongoing native requests resumed; about 12 Suno credits per generation"}),
            )
        }
        "feedback-start" | "feedback-stop" | "feedback-save" | "feedback-test" => {
            feedback_worker.command(action, body)
        }
        "start-session" => launch_session(&Config::load(root)?),
        "kat-close-overlays" => crate::kat::close(false),
        "kat-stub-install" => crate::kat::install_or_elevate(root),
        "kat-stub-restore" => crate::kat::restore_or_elevate(root),
        "kat-overlay-setting" => {
            let enabled = body["enabled"]
                .as_bool()
                .ok_or("Overlay option must be boolean")?;
            let path = root.join("control-center.json");
            let mut v = config::read_json(&path)?;
            v["closeKatOverlays"] = json!(enabled);
            config::write_json(&path, &v)?;
            Ok(json!({"message":"KAT overlay startup preference saved"}))
        }
        "launch" => launch_dependency(c, body["id"].as_str().ok_or("Dependency ID required")?),
        "dashboard" => {
            platform::open("http://127.0.0.1:8080")?;
            Ok(json!({"message":"SkyrimNet dashboard opened"}))
        }
        "driver-options" => crate::runtime::apply(c, body),
        "nr-disable" => crate::nr_guard::disable(c.nr_guard.as_ref(), root),
        "nr-restore" => crate::nr_guard::restore(c.nr_guard.as_ref(), root),
        "shader-cache-clear" => {
            let enforce = body["enforceBootCompile"].as_bool().unwrap_or(true);
            crate::shader_cache::clear_for_reload(c.effective_shader_cache().as_ref(), root, enforce)
        }
        "shader-cache-restore" => {
            crate::shader_cache::restore_latest_backup(c.effective_shader_cache().as_ref(), root)
        }
        "shader-cache-enforce-settings" => {
            crate::shader_cache::enforce_upfront_settings(c.effective_shader_cache().as_ref(), root)
        }
        "shader-cache-open-backups" => {
            crate::shader_cache::open_backups_folder(c.effective_shader_cache().as_ref())
        }
        "shader-precompile-start" => {
            let _ = feedback_worker.command("feedback-stop", &json!({}));
            let clear = body["clearCache"].as_bool().unwrap_or(true);
            precompile.start(c, root, clear)
        }
        "shader-precompile-cancel" => precompile.cancel(),
        "headset-setup" | "hardware-tester" => {
            if action == "hardware-tester" {
                feedback_worker.command("feedback-stop", &json!({}))?;
            }
            let key = if action == "headset-setup" {
                "unitySetup"
            } else {
                "hardwareTester"
            };
            let tool = config::feedback_install_path(
                &c.feedback_root,
                key,
                "Tool path missing from installation record",
            )?;
            platform::open(&tool.to_string_lossy())?;
            Ok(
                json!({"message":"Official toolkit tool opened; trigger feedback stays stopped during hardware testing"}),
            )
        }
        _ => Err("Unknown control action".into()),
    }
}
fn serve_request(
    mut stream: TcpStream,
    token: &str,
    tx: &Sender<Request>,
    feedback_tx: &Sender<Request>,
    status: &Shared,
) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
    let result = (|| -> Result<Value> {
        let mut raw = Vec::new();
        let mut chunk = [0; 4096];
        let split = loop {
            let n = stream
                .read(&mut chunk)
                .map_err(|_| "Request timed out".to_string())?;
            if n == 0 {
                return Err("Incomplete request".into());
            }
            raw.extend_from_slice(&chunk[..n]);
            if raw.len() > 65536 {
                return Err("Request too large".into());
            }
            if let Some(n) = raw.windows(4).position(|v| v == b"\r\n\r\n") {
                break n;
            }
        };
        let head = String::from_utf8_lossy(&raw[..split]).into_owned();
        let mut lines = head.lines();
        let first = lines.next().unwrap_or("");
        let mut parts = first.split_whitespace();
        let method = parts.next().unwrap_or("");
        let path = parts.next().unwrap_or("");
        if method == "GET" && path == "/status" {
            return Ok(status.lock().map_err(|_| "Status unavailable")?.clone());
        }
        if method != "POST" || path != "/command" {
            return Err("Unsupported local endpoint".into());
        }
        let auth = format!("Bearer {token}");
        let authorized = lines.clone().any(|l| {
            l.split_once(':')
                .is_some_and(|(k, v)| k.eq_ignore_ascii_case("Authorization") && v.trim() == auth)
        });
        if !authorized {
            return Err("Unauthorized".into());
        }
        let len = lines
            .find_map(|l| {
                l.split_once(':').and_then(|(k, v)| {
                    if k.eq_ignore_ascii_case("Content-Length") {
                        v.trim().parse::<usize>().ok()
                    } else {
                        None
                    }
                })
            })
            .ok_or("Content-Length required")?;
        if len > 60000 {
            return Err("Request too large".into());
        }
        while raw.len() < split + 4 + len {
            let n = stream
                .read(&mut chunk)
                .map_err(|_| "Request body timed out".to_string())?;
            if n == 0 {
                return Err("Incomplete request body".into());
            }
            raw.extend_from_slice(&chunk[..n]);
        }
        let value: Value = serde_json::from_slice(&raw[split + 4..split + 4 + len])
            .map_err(|_| "Invalid JSON".to_string())?;
        let action = value["action"]
            .as_str()
            .ok_or("Action required")?
            .to_owned();
        let (reply, rx) = mpsc::channel();
        let target = if action.starts_with("feedback-") {
            feedback_tx
        } else {
            tx
        };
        target
            .send(Request {
                action,
                body: value["body"].clone(),
                reply: Some(reply),
            })
            .map_err(|_| "Application is closing")?;
        rx.recv_timeout(Duration::from_secs(25))
            .map_err(|_| "Control action timed out; check status before retrying")?
    })();
    let (code, value) = match result {
        Ok(v) => (200, v),
        Err(e) => (400, json!({"error":e})),
    };
    let body = serde_json::to_vec(&value).unwrap();
    let head = format!(
        "HTTP/1.0 {code} Result\r\nContent-Type: application/json\r\nCache-Control: no-store\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream
        .write_all(head.as_bytes())
        .and_then(|_| stream.write_all(&body));
}

#[cfg(test)]
mod tests {
    use super::*;
    fn server(tx: Sender<Request>, feedback: Sender<Request>) -> (u16, thread::JoinHandle<()>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let thread = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            serve_request(
                stream,
                "test-token",
                &tx,
                &feedback,
                &Arc::new(Mutex::new(
                    json!({"runtime":"rust","feedback":{"leftProfile":"off"}}),
                )),
            );
        });
        (port, thread)
    }
    #[test]
    fn local_status_is_read_only_and_has_no_credential() {
        let (tx, rx) = mpsc::channel();
        let (f, fr) = mpsc::channel();
        let (p, t) = server(tx, f);
        let v = http::request(p, "GET", "/status", None, None).unwrap();
        assert_eq!(v["runtime"], "rust");
        assert!(!v.to_string().contains("test-token"));
        t.join().unwrap();
        assert!(rx.try_recv().is_err());
        assert!(fr.try_recv().is_err());
    }
    #[test]
    fn unauthenticated_commands_never_reach_workers() {
        let (tx, rx) = mpsc::channel();
        let (f, fr) = mpsc::channel();
        let (p, t) = server(tx, f);
        let v = http::request(
            p,
            "POST",
            "/command",
            Some(&json!({"action":"feedback-start","body":{}})),
            None,
        );
        assert_eq!(v.unwrap_err(), "Unauthorized");
        t.join().unwrap();
        assert!(rx.try_recv().is_err());
        assert!(fr.try_recv().is_err());
    }
    #[test]
    fn emergency_stop_bypasses_general_worker() {
        let (tx, rx) = mpsc::channel();
        let (f, fr) = mpsc::channel();
        let (p, t) = server(tx, f);
        let worker = thread::spawn(move || {
            let r = fr.recv_timeout(Duration::from_secs(2)).unwrap();
            assert_eq!(r.action, "feedback-stop");
            r.reply
                .unwrap()
                .send(Ok(json!({"message":"released"})))
                .unwrap();
        });
        let v = http::request(
            p,
            "POST",
            "/command",
            Some(&json!({"action":"feedback-stop","body":{}})),
            Some("test-token"),
        )
        .unwrap();
        assert_eq!(v["message"], "released");
        worker.join().unwrap();
        t.join().unwrap();
        assert!(rx.try_recv().is_err());
    }
}
