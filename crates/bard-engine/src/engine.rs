use crate::{blocking, config::*, lyrics, song::*};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::Mutex as AsyncMutex;

#[derive(Default, Debug)]
pub struct GameplayClock {
    pub elapsed: f64,
    previous_wall: Option<f64>,
    previous_game: Option<f64>,
}
impl GameplayClock {
    pub fn sample(&mut self, wall: f64, game: Option<f64>, healthy: bool) {
        if healthy {
            if let (Some(prev_wall), Some(prev_game), Some(game)) =
                (self.previous_wall, self.previous_game, game)
            {
                let delta = wall - prev_wall;
                if delta > 0.0 && delta <= 10.0 && game > prev_game {
                    self.elapsed += delta;
                }
            }
            self.previous_wall = Some(wall);
            self.previous_game = game;
        } else {
            self.previous_wall = None;
            self.previous_game = None;
        }
    }
}
struct State {
    jobs: Value,
    last: Value,
    clock: GameplayClock,
    last_request_elapsed: Option<f64>,
}
pub struct Engine {
    pub root: PathBuf,
    state: Mutex<State>,
    compose_gate: AsyncMutex<()>,
    _lock: std::fs::File,
}
impl Engine {
    pub fn new(root: PathBuf) -> Result<Arc<Self>> {
        std::fs::create_dir_all(root.join("data"))?;
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(root.join("data/rust-engine.lock"))?;
        fs2::FileExt::try_lock_exclusive(&lock)
            .context("Another Rust music engine owns this app directory")?;
        let mut jobs = if root.join("data/native-jobs.json").exists() {
            read_json(&root.join("data/native-jobs.json"))?
        } else {
            json!({})
        };
        if !jobs.is_object() {
            bail!("Invalid native job journal");
        }
        let mut interrupted = false;
        for job in jobs.as_object_mut().unwrap().values_mut() {
            if job["state"] == "running" {
                job["state"] = json!("failed");
                job["failure_reason"] = json!("interrupted_or_unknown_not_retried");
                interrupted = true;
            }
        }
        if interrupted {
            save_json(&root.join("data/native-jobs.json"), &jobs)?;
        }
        let cooldown = if root.join("data/native-cooldown.json").exists() {
            read_json(&root.join("data/native-cooldown.json"))?
        } else {
            json!({})
        };
        Ok(Arc::new(Self {
            root,
            state: Mutex::new(State {
                jobs,
                last: json!({"state":"waiting_for_native_request"}),
                clock: GameplayClock {
                    elapsed: num(&cooldown, "elapsed", 0.0),
                    ..Default::default()
                },
                last_request_elapsed: cooldown["last_paid_elapsed"].as_f64(),
            }),
            compose_gate: AsyncMutex::new(()),
            _lock: lock,
        }))
    }
    pub fn permission(&self, s: &Value) -> Result<Value> {
        if text(s, "native_spending_mode", "") == "native_automatic"
            && truthy(&s["native_recurring_suno_authorization"])
        {
            return Ok(
                json!({"remaining":1,"recurring":true,"human_authorization":s["native_recurring_suno_authorization"],"source":"explicit_user_ongoing_native_authorization"}),
            );
        }
        let p = self.root.join("data/native-spending.json");
        if p.exists() {
            read_json(&p)
        } else {
            Ok(json!({"remaining":0}))
        }
    }
    pub fn status(&self) -> Result<Value> {
        let s = read_settings(&self.root)?;
        let song_error = validate(&s).err().map(|e| e.to_string());
        let p = self.permission(&s)?;
        let state = self.state.lock().unwrap();
        let polling = song_error.is_none()
            && flag(&s, "native_integration_enabled", true)
            && flag(&s, "native_real_interval_override", false);
        let backend = text(&s, "llm_backend", "openrouter");
        let lyrics_provider = text(&s, "lyrics_provider", "openrouter");
        let antigravity_info = crate::antigravity::health_snapshot(&s, &self.root);
        let openrouter_model = text(&s, "selected_model", "ready");
        let active_model = if backend == "antigravity" {
            antigravity_info["resolved_model"]
                .as_str()
                .unwrap_or(crate::antigravity::DEFAULT_ANTIGRAVITY_MODEL)
        } else {
            openrouter_model
        };
        let mut status = json!({"running":true,"runtime":"rust","version":env!("CARGO_PKG_VERSION"),"llm_backend":backend,"lyrics_provider":lyrics_provider,"selected_model":active_model,"openrouter_model":openrouter_model,"antigravity":antigravity_info,"native_custom_preferences":flag(&s,"native_integration_enabled",true),"suno_requests_armed":if flag(&p,"recurring",false){Value::Null}else{p["remaining"].clone()},"ongoing_native_suno_authorized":flag(&p,"recurring",false),"timing":if polling{"real_gameplay_minimum"}else{"native"},"game_polling_enabled":polling,"keep_both_variants":flag(&s,"keep_both_suno_variants",true),"last":state.last,"controls_scope":"new_native_lyrics_only","native_suno_transport":"direct_https","native_suno_proxy_active":false,"can_cancel_submitted_native_music":false,"automatic_secondary_capture":false,"secondary_import":"dashboard_only; no request-gate bypass","settings_path":self.root.join("settings.json")});
        status["song_configuration_valid"] = json!(song_error.is_none());
        status["song_configuration_error"] = json!(song_error);
        Ok(status)
    }
    pub fn arm_one(&self, consent: &str) -> Result<Value> {
        if consent != "Allow one native Suno generation, estimated 12 credits" {
            bail!("Explicit one-song authorization is required");
        }
        let _state = self.state.lock().unwrap();
        let mut s = load(&self.root)?;
        s["native_spending_mode"] = json!("one_song");
        save_json(&self.root.join("settings.json"), &s)?;
        save_json(
            &self.root.join("data/native-spending.json"),
            &json!({"remaining":1,"human_authorization":consent,"source":"human_arm_one_song_control","armed_at":now()}),
        )?;
        Ok(json!({"success":true,"remaining":1,"scope":"new_native_lyrics_only"}))
    }
    pub fn cancel(&self) -> Result<Value> {
        let _state = self.state.lock().unwrap();
        let mut s = read_settings(&self.root)?;
        s["native_spending_mode"] = json!("one_song");
        save_json(&self.root.join("settings.json"), &s)?;
        save_json(
            &self.root.join("data/native-spending.json"),
            &json!({"remaining":0}),
        )?;
        Ok(
            json!({"success":true,"remaining":0,"scope":"new_native_lyrics_only","already_submitted_music_cancelled":false}),
        )
    }
    fn request_controls(&self) -> Result<(Value, Value)> {
        // The caller holds state, the same mutex used by arm/cancel. A model/history
        // snapshot taken earlier must never revive an authorization that was paused.
        let controls = read_settings(&self.root)?;
        let permission = self.permission(&controls)?;
        Ok((controls, permission))
    }
    pub async fn compose(self: &Arc<Self>, payload: &Value) -> Result<String> {
        let payload = payload.clone();
        let (context, key) = blocking::run(move || {
            let context = native_context(&payload)?.context("Not a native bard request")?;
            Ok((context, digest(&payload)))
        })
        .await?;
        let _gate = match self.compose_gate.try_lock() {
            Ok(guard) => guard,
            Err(_) => {
                // Identical retries may wait for their saved result. Do not queue a
                // different paid lyric request beyond the native caller's timeout.
                let engine = self.clone();
                let waiting_key = key.clone();
                let same_request = blocking::run(move || {
                    Ok(engine
                        .state
                        .lock()
                        .unwrap()
                        .jobs
                        .get(&waiting_key)
                        .is_some_and(|job| job["state"] == "running"))
                })
                .await?;
                if !same_request {
                    bail!("Another native bard is already composing");
                }
                self.compose_gate.lock().await
            }
        };
        let engine = self.clone();
        let input_context = context.clone();
        let input_key = key.clone();
        let (s, cached) = blocking::run(move || {
            let s = load(&engine.root)?;
            // Validate before consuming allowance. The fresh controls read below
            // stays under the same mutex as arm/cancel, including durable writes.
            let checked = apply_voice(&s, input_context.clone())?;
            let (h, _) = load_history(&engine.root, &s)?;
            build_request(&s, &h, &checked)?;
            let mut state = engine.state.lock().unwrap();
            if let Some(job) = state.jobs.get(&input_key) {
                if job["state"] == "done" {
                    let answer = job["answer"]
                        .as_str()
                        .map(str::to_owned)
                        .context("Malformed completed native journal")?;
                    return Ok((s, Some(answer)));
                }
                bail!("Native request previously failed or is uncertain; no automatic repeat");
            }
            let (controls, mut permission) = engine.request_controls()?;
            if !flag(&controls, "native_integration_enabled", true) {
                bail!("Custom native lyrics are paused");
            }
            if num(&permission, "remaining", 0.0) < 1.0
                || !truthy(&permission["human_authorization"])
            {
                state.last = json!({"state":"waiting_for_human_to_arm_one_song"});
                bail!("Arm a song or resume explicitly authorized native mode first");
            }
            if flag(&controls, "native_real_interval_override", false)
                && state.last_request_elapsed.is_some_and(|last| {
                    state.clock.elapsed - last
                        < num(&controls, "interval_real_gameplay_seconds", 480.0)
                })
            {
                bail!("Optional real-gameplay cooldown is active");
            }
            state.jobs[&input_key] = json!({"state":"running","started_at":now(),"runtime":"rust"});
            save_json(&engine.root.join("data/native-jobs.json"), &state.jobs)?;
            if !flag(&permission, "recurring", false) {
                permission["remaining"] = json!(0);
                permission["consumed_by_native_request"] = json!(input_key);
                permission["consumed_at"] = json!(now());
                permission["scope"] = json!("lyrics_gate_before_provider_io_native_suno_direct");
                save_json(&engine.root.join("data/native-spending.json"), &permission)?;
            }
            state.last_request_elapsed = Some(state.clock.elapsed);
            save_json(
                &engine.root.join("data/native-cooldown.json"),
                &json!({"elapsed":state.clock.elapsed,"last_paid_elapsed":state.last_request_elapsed,"measurement_scope":"native_lyric_request_not_confirmed_suno_submission"}),
            )?;
            state.last = json!({"state":"composing_lyrics","singer":input_context["selected_singer"]["name"],"gender":input_context["selected_singer"]["gender"]});
            Ok((s, None))
        })
        .await?;
        if let Some(answer) = cached {
            return Ok(answer);
        }
        let result = async {
            let folder = lyrics::make_draft(&self.root, &s, context, None).await?;
            blocking::run(move || {
                let song = prepare_suno(&s, &folder)?;
                Ok((native_answer(&song), folder, song))
            })
            .await
        }
        .await;
        let engine = self.clone();
        blocking::run(move || {
            let mut state = engine.state.lock().unwrap();
            match result {
            Ok((answer, folder, song)) => {
                state.jobs[&key] = json!({"state":"done","answer":answer,"draft":folder,"title":song["title"],"runtime":"rust"});
                state.last = json!({"state":"lyrics_returned_to_native_game","title":song["title"],"draft":folder});
                save_json(&engine.root.join("data/native-jobs.json"), &state.jobs)?;
                Ok(answer)
            }
            Err(e) => {
                state.jobs[&key]["state"] = json!("failed");
                state.last = json!({"state":"lyrics_failed","error_type":"LyricsFailure"});
                save_json(&engine.root.join("data/native-jobs.json"), &state.jobs)?;
                Err(e)
            }
            }
        })
        .await
    }
    pub async fn clock_loop(self: Arc<Self>) {
        let started = tokio::time::Instant::now();
        let client = match lyrics::client() {
            Ok(c) => c,
            Err(_) => return,
        };
        loop {
            tokio::time::sleep(Duration::from_secs(2)).await;
            let root = self.root.clone();
            let s = match blocking::run(move || load(&root)).await {
                Ok(s) => s,
                Err(_) => continue,
            };
            if !(flag(&s, "native_integration_enabled", true)
                && flag(&s, "native_real_interval_override", false))
            {
                let engine = self.clone();
                let wall = started.elapsed().as_secs_f64();
                let _ = blocking::run(move || {
                    engine.state.lock().unwrap().clock.sample(wall, None, false);
                    Ok(())
                })
                .await;
                continue;
            }
            let base = text(&s, "skyrimnet_url", "");
            let url = match reqwest::Url::parse(base) {
                Ok(u)
                    if [Some("127.0.0.1"), Some("localhost")].contains(&u.host_str())
                        && u.username().is_empty()
                        && u.password().is_none() =>
                {
                    u
                }
                _ => continue,
            };
            let body = async {
                client
                    .post(format!(
                        "{}/config?api=get-game-time",
                        url.as_str().trim_end_matches('/')
                    ))
                    .timeout(Duration::from_secs(2))
                    .json(&json!({}))
                    .send()
                    .await?
                    .json::<Value>()
                    .await
            }
            .await;
            let (healthy, game) = match body {
                Ok(v) => (v["success"] == true, v["gameTime"].as_f64()),
                Err(_) => (false, None),
            };
            let engine = self.clone();
            let wall = started.elapsed().as_secs_f64();
            let _ = blocking::run(move || {
                let mut state = engine.state.lock().unwrap();
                state.clock.sample(wall, game, healthy && game.is_some());
                save_json(
                    &engine.root.join("data/native-cooldown.json"),
                    &json!({"elapsed":state.clock.elapsed,"last_paid_elapsed":state.last_request_elapsed,"measurement_scope":"native_lyric_request_not_confirmed_suno_submission"}),
                )
            }).await;
        }
    }
}
pub fn now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}
pub fn offline_status(root: &Path) -> Result<Value> {
    let s = read_settings(root)?;
    Ok(
        json!({"runtime":"rust","version":env!("CARGO_PKG_VERSION"),"running":false,"llm_backend":text(&s,"llm_backend","openrouter"),"lyrics_provider":text(&s,"lyrics_provider","openrouter"),"antigravity":crate::antigravity::health_snapshot(&s,root),"selected_model":s["selected_model"],"thinking":s["reasoning_effort"],"include_history":flag(&s,"include_history",true),"native_spending_mode":s["native_spending_mode"],"native_suno_transport":"direct_https","controls_scope":"new_native_lyrics_only","automatic_secondary_capture":false,"native_configuration_modified":false}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn clock_ignores_pause_and_time_jumps() {
        let mut c = GameplayClock::default();
        c.sample(0.0, Some(1.0), true);
        c.sample(2.0, Some(1000.0), true);
        assert_eq!(c.elapsed, 2.0);
        c.sample(4.0, Some(1000.0), true);
        assert_eq!(c.elapsed, 2.0);
        c.sample(100.0, Some(1001.0), true);
        assert_eq!(c.elapsed, 2.0);
        c.sample(102.0, None, false);
        c.sample(104.0, Some(1002.0), true);
        assert_eq!(c.elapsed, 2.0);
    }
    #[test]
    fn interrupted_jobs_latch_failed() {
        let d = tempfile::tempdir().unwrap();
        save_json(
            &d.path().join("data/native-jobs.json"),
            &json!({"x":{"state":"running"}}),
        )
        .unwrap();
        let e = Engine::new(d.path().to_owned()).unwrap();
        assert_eq!(e.state.lock().unwrap().jobs["x"]["state"], "failed");
        assert_eq!(
            read_json(&d.path().join("data/native-jobs.json")).unwrap()["x"]["state"],
            "failed"
        );
    }
    #[test]
    fn invalid_song_preferences_do_not_break_health_or_pause() {
        let d = tempfile::tempdir().unwrap();
        save_json(&d.path().join("settings.json"),&json!({"selected_model":"invalid model","native_spending_mode":"native_automatic","native_recurring_suno_authorization":"recorded approval"})).unwrap();
        let e = Engine::new(d.path().to_owned()).unwrap();
        let health = e.status().unwrap();
        assert_eq!(health["running"], true);
        assert_eq!(health["song_configuration_valid"], false);
        e.cancel().unwrap();
        assert_eq!(e.status().unwrap()["ongoing_native_suno_authorized"], false);
    }
    #[test]
    fn permission_gate_reloads_controls_after_pause() {
        let d = tempfile::tempdir().unwrap();
        save_json(&d.path().join("settings.json"),&json!({"selected_model":"z-ai/glm-5.3-flash","native_spending_mode":"native_automatic","native_recurring_suno_authorization":"recorded approval"})).unwrap();
        let e = Engine::new(d.path().to_owned()).unwrap();
        let earlier_song_snapshot = load(d.path()).unwrap();
        assert_eq!(
            e.permission(&earlier_song_snapshot).unwrap()["recurring"],
            true
        );
        e.cancel().unwrap();
        let _state = e.state.lock().unwrap();
        let (controls, permission) = e.request_controls().unwrap();
        assert_eq!(controls["native_spending_mode"], "one_song");
        assert_eq!(permission["remaining"], 0);
    }
    #[tokio::test]
    async fn completed_duplicate_returns_cache_without_new_allowance_or_credentials() {
        let d = tempfile::tempdir().unwrap();
        save_json(&d.path().join("settings.json"), &json!({"selected_model":"z-ai/glm-5.3-flash", "include_history":false, "voice_mode":"auto", "native_spending_mode":"one_song"})).unwrap();
        let e = Engine::new(d.path().to_owned()).unwrap();
        let source = format!("{MARKER}<singer_name>Sven</singer_name><singer_gender>male</singer_gender><current_location>Riverwood</current_location><recent_memories></recent_memories><recent_events></recent_events><recent_dialogue></recent_dialogue><previous_song_titles></previous_song_titles>");
        let payload = json!({"messages":[{"content":source}]});
        e.state.lock().unwrap().jobs[digest(&payload)] =
            json!({"state":"done", "answer":"cached original lyrics"});
        assert_eq!(e.compose(&payload).await.unwrap(), "cached original lyrics");
        assert!(!d.path().join("drafts").exists());
        assert_eq!(
            e.permission(&load(d.path()).unwrap()).unwrap()["remaining"],
            0
        );
    }

    #[tokio::test]
    async fn one_song_consumes_before_io_and_failure_never_retries() {
        let d = tempfile::tempdir().unwrap();
        save_json(&d.path().join("settings.json"),&json!({"lyrics_provider":"openrouter","selected_model":"z-ai/glm-5.3-flash","include_history":false,"voice_mode":"auto","native_spending_mode":"one_song","openrouter_credentials_file":d.path().join("missing-providers.yaml"),"preserved_unknown":{"value":42},"native_recurring_suno_authorization":"recorded original approval"})).unwrap();
        let e = Engine::new(d.path().to_owned()).unwrap();
        e.arm_one("Allow one native Suno generation, estimated 12 credits")
            .unwrap();
        let source=format!("{MARKER}\n<singer_name>Sven</singer_name><singer_gender>male</singer_gender><current_location>Riverwood</current_location><recent_memories></recent_memories><recent_events></recent_events><recent_dialogue></recent_dialogue><previous_song_titles></previous_song_titles>");
        let payload = json!({"messages":[{"role":"user","content":source}]});
        assert!(e.compose(&payload).await.is_err());
        assert_eq!(
            read_json(&d.path().join("data/native-spending.json")).unwrap()["remaining"],
            0
        );
        assert_eq!(
            std::fs::read_dir(d.path().join("drafts")).unwrap().count(),
            1
        );
        assert!(e.compose(&payload).await.is_err());
        assert_eq!(
            std::fs::read_dir(d.path().join("drafts")).unwrap().count(),
            1
        );
        e.cancel().unwrap();
        let s = load(d.path()).unwrap();
        assert_eq!(s["preserved_unknown"]["value"], 42);
        assert_eq!(
            s["native_recurring_suno_authorization"],
            "recorded original approval"
        );
    }

    #[test]
    fn status_reports_resolved_antigravity_model_and_effort() {
        let d = tempfile::tempdir().unwrap();
        save_json(
            &d.path().join("settings.json"),
            &json!({
                "llm_backend": "antigravity",
                "lyrics_provider": "antigravity",
                "selected_model": "z-ai/glm-5.3-flash",
                "antigravity": {
                    "model": "gemini-4-argon-low",
                    "effort": "high"
                }
            }),
        )
        .unwrap();
        let e = Engine::new(d.path().to_owned()).unwrap();
        let health = e.status().unwrap();
        assert_eq!(health["llm_backend"], "antigravity");
        assert_eq!(health["lyrics_provider"], "antigravity");
        assert_eq!(health["selected_model"], "gemini-4-argon-high");
        assert_eq!(health["openrouter_model"], "z-ai/glm-5.3-flash");
        assert_eq!(health["antigravity"]["effort"], "high");
        assert_eq!(health["antigravity"]["resolved_model"], "gemini-4-argon-high");
    }
}
