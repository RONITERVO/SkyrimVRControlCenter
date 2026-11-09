//! Dedicated hardware watchdog: never waits for music HTTP, launchers or file hashing.
use crate::{
    config::{self, Config, Result},
    feedback::{self, Capi, Engine, Profile},
    platform,
    service::{Request, Shared},
};
use serde_json::{Value, json};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    thread,
    time::{Duration, Instant},
};
pub struct FeedbackWorker {
    pub status: Shared,
    tx: Sender<Request>,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}
impl FeedbackWorker {
    pub fn start(c: Config, preview: bool, tx: Sender<Request>, rx: Receiver<Request>) -> Self {
        let status = Arc::new(Mutex::new(
            json!({"feedback":{},"settings":c.feedback_settings().ok()}),
        ));
        let shared = status.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let worker = thread::spawn(move || {
            let mut engine = None;
            let mut error = String::new();
            if !preview {
                match create(&c) {
                    Ok(e) => engine = Some(e),
                    Err(e) => error = e,
                }
            }
            let mut action_id = 0u64;
            let mut action_message = String::new();
            let mut processes = platform::processes();
            let mut refreshed = Instant::now();
            while !stopped.load(Ordering::Relaxed) {
                while let Ok(r) = rx.try_recv() {
                    let result = if preview {
                        Err("Hardware is inactive in preview".into())
                    } else {
                        perform(&c, &mut engine, &r.action, &r.body)
                    };
                    if let Err(e) = &result {
                        error = e.clone();
                    } else {
                        error.clear();
                    }
                    action_id += 1;
                    action_message = match &result {
                        Ok(v) => v["message"].as_str().unwrap_or("Done").to_owned(),
                        Err(e) => format!("Action stopped: {e}"),
                    };
                    if let Some(reply) = r.reply {
                        let _ = reply.send(result);
                    }
                }
                if refreshed.elapsed() >= Duration::from_secs(1) {
                    processes = platform::processes();
                    refreshed = Instant::now();
                }
                let state = if let Some(e) = engine.as_mut() {
                    let now = Instant::now();
                    e.poll(now);
                    let alive = e.state.as_ref().is_none_or(|s| {
                        processes.iter().any(|(pid, name)| {
                            *pid == s.pid && name.eq_ignore_ascii_case("SkyrimVR.exe")
                        })
                    });
                    e.step(now, alive)
                } else {
                    json!({"driverActive":false,"enabled":false,"leftProfile":"off","rightProfile":"off","bridgeFresh":false,"error":error})
                };
                if let Ok(mut s) = shared.lock() {
                    *s = json!({"actionId":action_id,"lastAction":action_message,"feedback":state,"settings":engine.as_ref().map(|e|json!(e.settings)).or_else(||c.feedback_settings().ok().map(|v|json!(v))).unwrap_or(json!({}))});
                }
                thread::sleep(Duration::from_millis(100));
            }
            if let Some(e) = engine.as_mut() {
                e.stop();
            }
        });
        Self {
            status,
            tx,
            stop,
            worker: Some(worker),
        }
    }
    pub fn command(&self, action: &str, body: &Value) -> Result<Value> {
        let (tx, rx) = mpsc::channel();
        self.tx
            .send(Request {
                action: action.to_owned(),
                body: body.clone(),
                reply: Some(tx),
            })
            .map_err(|_| "Feedback worker unavailable")?;
        rx.recv_timeout(Duration::from_secs(4))
            .map_err(|_| "Feedback command timed out; check status before retrying")?
    }
}
impl Drop for FeedbackWorker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
    }
}
fn create(c: &Config) -> Result<Engine<Capi>> {
    Engine::new(
        c.feedback_settings()?,
        Capi::new(feedback::capi_path(&c.feedback_root)?),
        Some(19462),
    )
}
fn perform(
    c: &Config,
    engine: &mut Option<Engine<Capi>>,
    action: &str,
    body: &Value,
) -> Result<Value> {
    match action {
        "feedback-shutdown" => {
            if let Some(e) = engine.as_mut() {
                e.stop();
            }
            Ok(json!({"message":"Trigger effects released for window shutdown"}))
        }
        "feedback-start" => {
            if engine.is_none() {
                *engine = Some(create(c)?)
            }
            let e = engine.as_mut().unwrap();
            e.arm();
            config::write_json(
                &c.feedback_root.join(".local/settings.json"),
                &json!(e.settings),
            )?;
            Ok(json!({"message":"Feedback armed; effects require fresh Skyrim state"}))
        }
        "feedback-stop" => {
            if let Some(e) = engine.as_mut() {
                e.stop();
                config::write_json(
                    &c.feedback_root.join(".local/settings.json"),
                    &json!(e.settings),
                )?;
            }
            Ok(json!({"message":"Custom trigger effects released and disarmed"}))
        }
        "feedback-save" => {
            let mut s: feedback::Settings =
                serde_json::from_value(body.clone()).map_err(|e| e.to_string())?;
            s.validate()?;
            if let Some(e) = engine.as_mut() {
                e.settings(s)?;
                s = e.settings.clone();
            }
            config::write_json(&c.feedback_root.join(".local/settings.json"), &json!(s))?;
            Ok(json!({"message":"Trigger feel saved; use Start to rearm after a driver error"}))
        }
        "feedback-test" => {
            let p: Profile = serde_json::from_value(body["profile"].clone())
                .map_err(|_| "Unknown test profile".to_string())?;
            engine
                .as_mut()
                .ok_or("Feedback engine is unavailable")?
                .test(p, Instant::now())?;
            Ok(json!({"message":"Visible two-second trigger test started"}))
        }
        _ => Err("Unknown feedback action".into()),
    }
}
