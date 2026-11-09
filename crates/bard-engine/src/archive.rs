//! Existing Suno results only. No music-generation endpoint or game API polling.
use crate::{blocking, config::*, delivery, lyrics};
use anyhow::{bail, Context, Result};
use futures_util::StreamExt;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    fs::{self, File},
    io::{Read, Seek, SeekFrom},
    path::{Component, Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, UNIX_EPOCH},
};
use tokio::{io::AsyncWriteExt, sync::Notify};

const AGENT: &str = "SkyrimVRControlCenter/0.2.0";
pub const CACHED: &str = "Already cached by SkyrimNet";
pub const IMPORT: &str = "To import into SkyrimNet";
pub const UNKNOWN: &str = "Unconfirmed";
const LIMIT: u64 = 100 * 1024 * 1024;

pub struct Archive {
    root: PathBuf,
    status: Mutex<Value>,
    wake: Notify,
    manual: AtomicBool,
    credits: AtomicBool,
    queued: Mutex<Vec<(String, bool)>>,
}

pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"-_".contains(&c))
}
fn output(root: &Path) -> PathBuf {
    root.join("Song Downloads")
}
fn index_path(root: &Path) -> PathBuf {
    root.join("data/song-downloads-index.json")
}
fn stamp(path: &Path) -> Result<Value> {
    let m = fs::metadata(path)?;
    Ok(
        json!({"bytes":m.len(),"modified":m.modified()?.duration_since(UNIX_EPOCH)?.as_nanos().to_string()}),
    )
}
fn file_sha(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut hash = Sha256::new();
    let mut bytes = [0; 65536];
    loop {
        let n = file.read(&mut bytes)?;
        if n == 0 {
            break;
        }
        hash.update(&bytes[..n]);
    }
    Ok(format!("{:x}", hash.finalize()))
}
fn owned_path(root: &Path, relative: &str) -> Result<PathBuf> {
    let p = Path::new(relative);
    if p.components().any(|c| !matches!(c, Component::Normal(_))) {
        bail!("Invalid archive path");
    }
    let first = p
        .components()
        .next()
        .context("Empty archive path")?
        .as_os_str()
        .to_string_lossy();
    if ![CACHED, IMPORT, UNKNOWN, ".incoming"].contains(&first.as_ref()) {
        bail!("Unknown archive folder");
    }
    let target = output(root).join(p);
    for ancestor in target.ancestors().take_while(|a| *a != root) {
        if let Ok(m) = fs::symlink_metadata(ancestor) {
            #[cfg(windows)]
            {
                use std::os::windows::fs::MetadataExt;
                if m.file_attributes() & 0x400 != 0 {
                    bail!("Archive reparse path refused");
                }
            }
            if m.file_type().is_symlink() {
                bail!("Archive symlink refused");
            }
        }
    }
    Ok(target)
}
fn load_index(root: &Path) -> Result<Value> {
    let path = index_path(root);
    let v = if path.exists() {
        read_json(&path)?
    } else {
        json!({"version":1,"tasks":{},"tracks":{},"logs":{},"cache_files":{},"recoveries":{}})
    };
    if v["version"] != 1
        || ["tasks", "tracks", "logs", "cache_files", "recoveries"]
            .iter()
            .any(|k| !v[*k].is_object())
    {
        bail!("Invalid download index; preserve it for inspection");
    }
    for (id, track) in v["tracks"].as_object().unwrap() {
        if !valid_id(id) {
            bail!("Invalid saved track ID");
        }
        if let Some(p) = track["relative"].as_str() {
            owned_path(root, p)?;
        }
    }
    Ok(v)
}
fn save_index(root: &Path, index: &Value) -> Result<()> {
    let path = index_path(root);
    if path.exists() && read_json(&path)? == *index {
        return Ok(());
    }
    save_json(&path, index)
}

fn discover(index: &mut Value, directory: &Path) -> Result<bool> {
    let mut backlog = false;
    let submitted = regex::Regex::new(
        r"^\[[^\]]+\] \[\d+\] \[(?:info|debug)\] \[SunoInterface\.cpp:\d+\] BardSinging/Suno: Generation submitted, taskId=([A-Za-z0-9_-]+)\s*$",
    )?;
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        if !(name == "SkyrimNet.log" || name.starts_with("SkyrimNet.")) || !name.ends_with(".log") {
            continue;
        }
        let mut file = File::open(entry.path())?;
        let size = file.metadata()?.len();
        let mut prefix = vec![0; size.min(256) as usize];
        file.read_exact(&mut prefix)?;
        let fingerprint = sha(&prefix);
        let cursor = &index["logs"][&name];
        let offset =
            if cursor["prefix"] == fingerprint && cursor["offset"].as_u64().unwrap_or(0) <= size {
                cursor["offset"].as_u64().unwrap_or(0)
            } else {
                0
            };
        file.seek(SeekFrom::Start(offset))?;
        let mut bytes = Vec::new();
        file.take(2 * 1024 * 1024).read_to_end(&mut bytes)?;
        let consumed = bytes
            .iter()
            .rposition(|b| *b == b'\n')
            .map(|n| n + 1)
            .unwrap_or_else(|| {
                if bytes.len() == 2 * 1024 * 1024 {
                    bytes.len()
                } else {
                    0
                }
            });
        for line in String::from_utf8_lossy(&bytes[..consumed]).lines() {
            if !submitted.is_match(line) {
                continue;
            }
            if let Some((_, id)) = line.split_once("taskId=") {
                let id = id.split_whitespace().next().unwrap_or("");
                if valid_id(id) && index["tasks"].get(id).is_none() {
                    index["tasks"][id] = json!({"state":"discovered","next_check":0});
                }
            }
        }
        let end = offset + consumed as u64;
        backlog |= end < size && consumed > 0;
        index["logs"][&name] = json!({"prefix":fingerprint,"offset":end});
    }
    Ok(backlog)
}

fn cache_snapshot(
    index: &mut Value,
    directory: &Path,
) -> Result<(HashSet<String>, HashSet<String>)> {
    let mut ids = HashSet::new();
    let mut hashes = HashSet::new();
    let mut current = json!({});
    if !directory.exists() {
        return Ok((ids, hashes));
    }
    for entry in fs::read_dir(directory)? {
        let p = entry?.path();
        if !p.is_file()
            || !["mp3", "wav", "flac", "ogg", "m4a"]
                .contains(&p.extension().and_then(|v| v.to_str()).unwrap_or(""))
        {
            continue;
        }
        let name = p.file_name().unwrap().to_string_lossy().to_string();
        let before = stamp(&p)?;
        let hash = if index["cache_files"][&name]["stamp"] == before {
            index["cache_files"][&name]["sha256"]
                .as_str()
                .unwrap_or("")
                .to_owned()
        } else {
            let hash = file_sha(&p)?;
            if stamp(&p)? != before {
                continue;
            } // A cache download is still being written.
            hash
        };
        if hash.is_empty() {
            continue;
        }
        current[&name] = json!({"stamp":before,"sha256":hash});
        ids.insert(p.file_stem().unwrap().to_string_lossy().to_string());
        hashes.insert(hash);
    }
    index["cache_files"] = current;
    Ok((ids, hashes))
}
fn category(
    track: &Value,
    siblings: &[String],
    index: &Value,
    ids: &HashSet<String>,
    hashes: &HashSet<String>,
) -> &'static str {
    let cached = |id: &str, t: &Value| {
        ids.contains(id) || t["sha256"].as_str().is_some_and(|h| hashes.contains(h))
    };
    if cached(text(track, "id", ""), track) {
        CACHED
    } else if siblings.iter().any(|id| cached(id, &index["tracks"][id])) {
        IMPORT
    } else {
        UNKNOWN
    }
}
fn filename(title: &str, id: &str, extension: &str) -> String {
    let title: String = title
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .take(72)
        .collect();
    format!("song_{}--{}.{}", title.trim_matches('_'), id, extension)
}
fn find_saved(root: &Path, id: &str) -> Result<Option<(String, PathBuf)>> {
    if !valid_id(id) {
        bail!("Invalid track identity");
    }
    let suffix = format!("--{id}.");
    let mut found = None;
    for bucket in [CACHED, IMPORT, UNKNOWN] {
        let folder = owned_path(root, &format!("{bucket}/placeholder"))?
            .parent()
            .unwrap()
            .to_path_buf();
        if !folder.exists() {
            continue;
        }
        for entry in fs::read_dir(folder)? {
            let path = entry?.path();
            let name = path.file_name().unwrap().to_string_lossy().to_string();
            if name.contains(&suffix)
                && ["mp3", "m4a", "ogg", "wav", "flac"]
                    .contains(&path.extension().and_then(|v| v.to_str()).unwrap_or(""))
            {
                if found.is_some() {
                    bail!("Multiple existing files for a track; preserve for review");
                }
                found = Some((format!("{bucket}/{name}"), path));
            }
        }
    }
    Ok(found)
}
fn duplicate_owner(root: &Path, index: &Value, id: &str, hash: &str) -> Result<Option<String>> {
    for (other, record) in index["tracks"].as_object().context("Invalid tracks")? {
        if other == id || record["sha256"] != hash {
            continue;
        }
        if let Some(relative) = record["relative"].as_str() {
            let path = owned_path(root, relative)?;
            if path.is_file() && file_sha(&path)? == hash {
                return Ok(Some(other.clone()));
            }
        }
    }
    Ok(None)
}
fn sidecar(path: &Path, track: &Value, bucket: &str) -> Result<()> {
    let title = if bucket == IMPORT {
        format!(
            "{} (alternate {})",
            text(track, "title", "Bard song"),
            &text(track, "id", "")[..8.min(text(track, "id", "").len())]
        )
    } else {
        text(track, "title", "Bard song").to_owned()
    };
    let fields = [
        ("id", json!(format!("suno_{}", text(track, "id", "")))),
        ("title", json!(title)),
        ("lyrics", track["lyrics"].clone()),
        ("gender", track["gender"].clone()),
        ("instrumental", track["instrumental"].clone()),
        ("style_tags", track["style_tags"].clone()),
    ];
    let yaml = fields
        .iter()
        .map(|(k, v)| format!("{k}: {v}\n"))
        .collect::<String>();
    if fs::read_to_string(path.with_extension("yaml"))
        .ok()
        .as_deref()
        != Some(&yaml)
    {
        fs::write(path.with_extension("yaml"), yaml)?;
    }
    Ok(())
}
fn reconcile(
    root: &Path,
    index: &mut Value,
    ids: &HashSet<String>,
    hashes: &HashSet<String>,
) -> Result<()> {
    let track_ids: Vec<_> = index["tracks"]
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect();
    for id in &track_ids {
        let t = &index["tracks"][id];
        if t["duplicate_of"].is_string() {
            continue;
        }
        let Some(relative) = t["relative"].as_str() else {
            continue;
        };
        let path = owned_path(root, relative)?;
        if !path.is_file() {
            index["tracks"][id]["file_missing"] = json!(true);
            continue;
        }
        let siblings = index["tasks"][text(t, "task_id", "")]["track_ids"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let bucket = category(t, &siblings, index, ids, hashes);
        let new_relative = format!("{bucket}/{}", path.file_name().unwrap().to_string_lossy());
        let destination = owned_path(root, &new_relative)?;
        fs::create_dir_all(destination.parent().unwrap())?;
        if path != destination {
            if destination.exists() {
                bail!("Archive destination already exists; no file overwritten");
            }
            fs::rename(&path, &destination)?;
            if path.with_extension("yaml").exists() {
                fs::remove_file(path.with_extension("yaml"))?;
            }
        }
        sidecar(&destination, &index["tracks"][id], bucket)?;
        index["tracks"][id]["relative"] = json!(new_relative);
        index["tracks"][id]["category"] = json!(bucket);
        index["tracks"][id]["file_missing"] = json!(false);
    }
    for id in track_ids {
        if let Some(owner) = index["tracks"][&id]["duplicate_of"]
            .as_str()
            .map(str::to_owned)
        {
            index["tracks"][&id]["relative"] = index["tracks"][&owner]["relative"].clone();
            index["tracks"][&id]["category"] = index["tracks"][&owner]["category"].clone();
        }
    }
    Ok(())
}

fn parse_details(body: &Value, task: &str) -> Result<Vec<Value>> {
    if body["code"] != 200 || body["data"]["status"] != "SUCCESS" {
        bail!("Task is not complete");
    }
    if body["data"]["taskId"].as_str().is_some_and(|id| id != task) {
        bail!("Task identity mismatch");
    }
    let param = match &body["data"]["param"] {
        Value::String(s) => serde_json::from_str(s).unwrap_or(json!({})),
        p => p.clone(),
    };
    let gender = match text(&param, "vocalGender", "") {
        "m" | "male" => "male",
        "f" | "female" => "female",
        _ => "",
    };
    let tracks = body["data"]["response"]["sunoData"]
        .as_array()
        .context("Missing result variants")?;
    let mut seen = HashSet::new();
    tracks.iter().map(|t| {
        let id = text(t,"id","");
        if !valid_id(id) || !seen.insert(id) { bail!("Invalid or duplicate provider track ID"); }
        let url = t["audioUrl"].as_str().or_else(|| t["audio_url"].as_str()).unwrap_or("");
        delivery::audio_url(&json!({"audioUrl":url}))?;
        Ok(json!({"id":id,"task_id":task,"title":t["title"],"lyrics":t.get("prompt").unwrap_or(&param["prompt"]),"style_tags":t.get("tags").unwrap_or(&param["style"]),"gender":gender,"instrumental":flag(&param,"instrumental",false),"audio_url":url,"provider_duration_seconds":t["duration"]}))
    }).collect()
}

async fn api(
    route: &str,
    query: &[(&str, &str)],
    key: &str,
    payload: Option<&Value>,
) -> Result<Value> {
    let allowed = if payload.is_some() {
        route == "/suno/recovery"
    } else {
        [
            "/generate/record-info",
            "/generate/credit",
            "/suno/recovery/record-info",
        ]
        .contains(&route)
    };
    if !allowed {
        bail!("Archive endpoint refused; no music generation is permitted");
    }
    let client = lyrics::client()?;
    let mut request = if let Some(payload) = payload {
        client.post(format!("{SUNO}{route}")).json(payload)
    } else {
        client.get(format!("{SUNO}{route}"))
    };
    request = request
        .bearer_auth(key)
        .header("User-Agent", AGENT)
        .query(query)
        .timeout(Duration::from_secs(30));
    let response = request
        .send()
        .await
        .map_err(|_| anyhow::anyhow!("Suno connection failed"))?;
    if !response.status().is_success() {
        bail!("Suno returned HTTP {}", response.status().as_u16());
    }
    if response.content_length().is_some_and(|n| n > 4_000_000) {
        bail!("Oversized Suno response");
    }
    let mut stream = response.bytes_stream();
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| anyhow::anyhow!("Suno response interrupted"))?;
        if bytes.len() + chunk.len() > 4_000_000 {
            bail!("Oversized Suno response");
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).context("Invalid Suno response")
}
fn probe_command(program: &str) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(program);
    #[cfg(windows)]
    {
        command.creation_flags(0x08000000);
    }
    command.kill_on_drop(true);
    command
}
async fn inspect_audio(path: &Path) -> Result<(String, f64)> {
    let p = tokio::time::timeout(
        Duration::from_secs(30),
        probe_command("ffprobe")
            .args([
                "-v",
                "error",
                "-show_entries",
                "format=format_name,duration",
                "-of",
                "json",
            ])
            .arg(path)
            .output(),
    )
    .await
    .context("Audio probe timeout")??;
    if !p.status.success() {
        bail!("Downloaded audio did not pass probing");
    }
    let info: Value = serde_json::from_slice(&p.stdout)?;
    let format = text(&info["format"], "format_name", "");
    let extension = if format.contains("mp3") {
        "mp3"
    } else if format.contains("flac") {
        "flac"
    } else if format.contains("ogg") {
        "ogg"
    } else if format.contains("wav") {
        "wav"
    } else if format.contains("mov") || format.contains("m4a") {
        "m4a"
    } else {
        bail!("Unsupported audio format");
    };
    let seconds = text(&info["format"], "duration", "").parse::<f64>()?;
    if !seconds.is_finite() || seconds <= 0.0 {
        bail!("Invalid measured duration");
    }
    let decoded = tokio::time::timeout(
        Duration::from_secs(45),
        probe_command("ffmpeg")
            .args(["-v", "error", "-i"])
            .arg(path)
            .args(["-f", "null", "-"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status(),
    )
    .await??;
    if !decoded.success() {
        bail!("Audio did not pass full decode");
    }
    Ok((extension.to_owned(), seconds))
}
async fn download(url: &str, path: &Path) -> Result<(String, u64, String, f64)> {
    let url = delivery::audio_url(&json!({"audioUrl":url}))?;
    let response = lyrics::client()?
        .get(url)
        .header("User-Agent", AGENT)
        .timeout(Duration::from_secs(90))
        .send()
        .await
        .map_err(|_| anyhow::anyhow!("Audio download connection failed"))?;
    if !response.status().is_success() {
        bail!(
            "Audio link unavailable (HTTP {}); manual recovery may be needed",
            response.status().as_u16()
        );
    }
    if response.content_length().is_some_and(|n| n > LIMIT) {
        bail!("Audio file exceeds100MiB");
    }
    let mut file = tokio::fs::File::create(path).await?;
    let mut hash = Sha256::new();
    let mut bytes = 0u64;
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| anyhow::anyhow!("Audio transfer interrupted"))?;
        bytes += chunk.len() as u64;
        if bytes > LIMIT {
            bail!("Audio file exceeds100MiB");
        }
        hash.update(&chunk);
        file.write_all(&chunk).await?;
    }
    file.sync_all().await?;
    drop(file);
    let (extension, seconds) = inspect_audio(path).await?;
    Ok((format!("{:x}", hash.finalize()), bytes, extension, seconds))
}

impl Archive {
    pub fn new(root: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            status: Mutex::new(json!({"folder":output(&root),"state":"waiting","saved":0})),
            root,
            wake: Notify::new(),
            manual: AtomicBool::new(false),
            credits: AtomicBool::new(false),
            queued: Mutex::new(vec![]),
        })
    }
    pub fn status(&self) -> Value {
        self.status.lock().unwrap().clone()
    }
    pub fn request_sync(&self) -> Value {
        self.manual.store(true, Ordering::Relaxed);
        self.wake.notify_one();
        json!({"success":true,"message":"Existing Suno variants queued for download; no new music requested"})
    }
    pub fn request_credits(&self) -> Value {
        self.credits.store(true, Ordering::Relaxed);
        self.wake.notify_one();
        json!({"success":true,"message":"Suno credit balance refresh queued"})
    }
    pub fn request_task(&self, task: &str, recover: bool, consent: &str) -> Result<Value> {
        if !valid_id(task) {
            bail!("Enter an existing Suno task ID");
        }
        if recover && consent != "Recover existing tracks; provider credit cost may change" {
            bail!("Explicit link-recovery approval required");
        }
        self.queued.lock().unwrap().push((task.to_owned(), recover));
        self.manual.store(true, Ordering::Relaxed);
        self.wake.notify_one();
        Ok(
            json!({"success":true,"message":if recover{"Existing-task link recovery queued; never creates a song"}else{"Existing task queued for download"}}),
        )
    }
    fn publish(&self, index: &Value, state: &str, message: &str) {
        let tracks = index["tracks"].as_object().unwrap();
        let owners: Vec<_> = tracks
            .values()
            .filter(|t| {
                !t["duplicate_of"].is_string()
                    && t["relative"].is_string()
                    && t["file_missing"] != true
            })
            .collect();
        let tasks: Vec<_> = index["tasks"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(id, t)| json!({"id":id,"state":t["state"],"error":t["error"]}))
            .collect();
        *self.status.lock().unwrap() = json!({"folder":output(&self.root),"state":state,"message":message,"saved":owners.len(),
            "cached":owners.iter().filter(|t| t["category"]==CACHED).count(),"to_import":owners.iter().filter(|t| t["category"]==IMPORT).count(),
            "unconfirmed":owners.iter().filter(|t|t["category"]==UNKNOWN).count(),"duplicates_skipped":tracks.values().filter(|t|t["duplicate_of"].is_string()).count(),"tasks":tasks,
            "credits":index["credits"],"credits_checked_at":index["credits_checked_at"],"last_credit_difference":index["last_credit_difference"],
            "auto":index["auto"],"new_music_requests":0,"native_import_performed":false});
    }
    pub async fn worker(self: Arc<Self>) {
        loop {
            let manual = self.manual.swap(false, Ordering::Relaxed);
            let queued = std::mem::take(&mut *self.queued.lock().unwrap());
            let credits = self.credits.swap(false, Ordering::Relaxed);
            match self.pass(manual, queued, credits).await {
                Ok(soon) => {
                    if soon && manual {
                        self.manual.store(true, Ordering::Relaxed);
                    }
                    tokio::select! { _=self.wake.notified()=>{}, _=tokio::time::sleep(Duration::from_secs(if soon{2}else{30}))=>{} }
                }
                Err(_) => {
                    {
                        let mut s = self.status.lock().unwrap();
                        s["state"] = json!("error");
                        s["message"]=json!("Song download scan stopped; check configured log path, index and provider availability. NPC relay remains active.");
                    }
                    tokio::select! {_=self.wake.notified()=>{},_=tokio::time::sleep(Duration::from_secs(30))=>{}}
                }
            }
        }
    }
    async fn pass(&self, manual: bool, queued: Vec<(String, bool)>, credits: bool) -> Result<bool> {
        let root = self.root.clone();
        let (settings, mut index, archive_config) = blocking::run(move || {
            let settings = read_settings(&root)?;
            let config_path = root.join("data/song-downloads-config.json");
            let c = if config_path.exists() {
                read_json(&config_path)?
            } else {
                json!({})
            };
            Ok((settings, load_index(&root)?, c))
        })
        .await?;
        let auto = flag(&settings, "keep_both_suno_variants", false);
        index["auto"] = json!(auto);
        let pending_recovery = index["recoveries"]
            .as_object()
            .unwrap()
            .values()
            .any(|r| r["state"] == "submitted");
        if !(auto || manual || credits || pending_recovery) {
            self.publish(
                &index,
                "paused",
                "Automatic downloads off; Download now still works",
            );
            return Ok(false);
        }
        self.publish(
            &index,
            "working",
            "Scanning existing requests and saving variants",
        );
        let data = PathBuf::from(
            settings["skyrimnet_data"]
                .as_str()
                .context("Missing native data folder")?,
        );
        let log_directory = archive_config["log_directory"].as_str().map(PathBuf::from);
        let cache = data.join("bard_songs");
        let root = self.root.clone();
        let (mut index, backlog, ids, hashes) = blocking::run(move || {
            for bucket in [CACHED, IMPORT, UNKNOWN, ".incoming"] {
                fs::create_dir_all(
                    owned_path(&root, &format!("{bucket}/placeholder"))?
                        .parent()
                        .unwrap(),
                )?;
            }
            let backlog = if let Some(log) = log_directory {
                discover(&mut index, &log)?
            } else {
                false
            };
            let (ids, hashes) = cache_snapshot(&mut index, &cache)?;
            save_index(&root, &index)?;
            Ok((index, backlog, ids, hashes))
        })
        .await?;
        let key_settings = settings.clone();
        let key = blocking::run(move || suno_key(&key_settings)).await?;
        for (task, recover) in queued {
            if index["tasks"].get(&task).is_none() {
                index["tasks"][&task] = json!({"state":"discovered","next_check":0});
            }
            if recover {
                self.recover(&task, &key, &mut index).await?;
            } else {
                index["tasks"][&task]["next_check"] = json!(0);
            }
        }
        if credits {
            let balance = api("/generate/credit", &[], &key, None).await?;
            if balance["code"] == 200 {
                index["credits"] = balance["data"].clone();
                index["credits_checked_at"] = json!(crate::engine::now());
            }
        }
        // Resume known recovery IDs only; a POST is never automatically retried.
        let recovery_tasks: Vec<_> = index["recoveries"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        let mut soon = backlog;
        for task in recovery_tasks {
            let r = index["recoveries"][&task].clone();
            if r["state"] != "submitted" {
                continue;
            }
            let recovery_id = text(&r, "task_id", "");
            let body = api(
                "/suno/recovery/record-info",
                &[("task_id", recovery_id)],
                &key,
                None,
            )
            .await?;
            let Some(tracks) = body["data"].as_array() else {
                soon = true;
                continue;
            };
            if body["code"] == 201 {
                soon = true;
                continue;
            }
            for t in tracks {
                let id = text(t, "id", "");
                if t["status"] == "success" && index["tracks"].get(id).is_some() {
                    index["tracks"][id]["audio_url"] = t["audio_url"].clone();
                    index["tracks"][id]["needs_recovery"] = json!(false);
                }
            }
            index["recoveries"][&task]["state"] = json!("completed");
            index["tasks"][&task]["state"] = json!("ready");
            index["tasks"][&task]["next_check"] = json!(0);
            let balance = api("/generate/credit", &[], &key, None).await?;
            index["credits"] = balance["data"].clone();
            index["credits_checked_at"] = json!(crate::engine::now());
            if let (Some(before), Some(after)) =
                (r["credits_before"].as_f64(), balance["data"].as_f64())
            {
                index["last_credit_difference"] = json!(before - after);
            }
        }
        let tasks: Vec<_> = index["tasks"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        for task in tasks {
            if num(&index["tasks"][&task], "next_check", 0.0) > crate::engine::now() && !manual {
                continue;
            }
            if index["tasks"][&task]["state"] == "complete" {
                let root = self.root.clone();
                let records = index["tracks"].clone();
                let track_ids = index["tasks"][&task]["track_ids"].clone();
                let missing = blocking::run(move || {
                    Ok(track_ids.as_array().is_some_and(|a| {
                        a.iter().filter_map(Value::as_str).any(|id| {
                            records[id]["relative"]
                                .as_str()
                                .is_none_or(|r| owned_path(&root, r).map_or(true, |p| !p.is_file()))
                        })
                    }))
                })
                .await?;
                if !missing {
                    continue;
                }
            }
            let result = self.task(&task, &key, &mut index).await;
            if let Err(error) = result {
                index["tasks"][&task]["state"] = json!("needs_attention");
                // Only controlled messages from this module; no reqwest URL/key diagnostics.
                index["tasks"][&task]["error"] =
                    json!(error.to_string().chars().take(180).collect::<String>());
                index["tasks"][&task]["next_check"] = json!(crate::engine::now() + 300.0);
            }
            let root = self.root.clone();
            let saved = index.clone();
            blocking::run(move || save_index(&root, &saved)).await?;
            self.publish(
                &index,
                "working",
                "Saving existing variants; completed files are ready to import",
            );
        }
        soon |= index["tasks"]
            .as_object()
            .unwrap()
            .values()
            .any(|t| t["state"] == "waiting");
        let root = self.root.clone();
        let index = blocking::run(move || {
            reconcile(&root, &mut index, &ids, &hashes)?;
            save_index(&root, &index)?;
            Ok(index)
        })
        .await?;
        self.publish(&index,"idle","Download scan complete. Import MP3 and matching YAML together; files in Already cached need no import.");
        Ok(soon)
    }
    async fn task(&self, task: &str, key: &str, index: &mut Value) -> Result<()> {
        if index["tasks"][task]["track_ids"]
            .as_array()
            .is_none_or(|a| a.is_empty())
        {
            let body = api("/generate/record-info", &[("taskId", task)], key, None).await?;
            if body["code"] == 200 && body["data"]["status"] != "SUCCESS" {
                let state = text(&body["data"], "status", "");
                if ["PENDING", "TEXT_SUCCESS", "FIRST_SUCCESS", "GENERATING"].contains(&state) {
                    index["tasks"][task]["state"] = json!("waiting");
                    index["tasks"][task]["next_check"] = json!(crate::engine::now() + 15.0);
                    return Ok(());
                }
            }
            let tracks = parse_details(&body, task)?;
            if tracks.is_empty() {
                bail!("No completed variants returned");
            }
            index["tasks"][task]["track_ids"] =
                json!(tracks.iter().map(|t| text(t, "id", "")).collect::<Vec<_>>());
            for t in tracks {
                let id = text(&t, "id", "").to_owned();
                if index["tracks"].get(&id).is_none() {
                    index["tracks"][id] = t;
                }
            }
        }
        let ids: Vec<String> = index["tasks"][task]["track_ids"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect();
        for id in ids {
            let t = index["tracks"][&id].clone();
            if let Some(relative) = t["relative"].as_str() {
                let root = self.root.clone();
                let relative = relative.to_owned();
                if blocking::run(move || Ok(owned_path(&root, &relative)?.is_file())).await? {
                    continue;
                }
            }
            let root = self.root.clone();
            let partial_id = id.clone();
            let temp = blocking::run(move || {
                owned_path(&root, &format!(".incoming/{partial_id}.partial"))
            })
            .await?;
            // Recover a completed file whose index update/move was interrupted,
            // rather than downloading a second copy after a crash.
            let root = self.root.clone();
            let find_id = id.clone();
            let recovered = blocking::run(move || find_saved(&root, &find_id)).await?;
            if let Some((relative, path)) = recovered {
                let (_, seconds) = inspect_audio(&path).await?;
                let (hash, bytes) =
                    blocking::run(move || Ok((file_sha(&path)?, fs::metadata(&path)?.len())))
                        .await?;
                index["tracks"][&id]["relative"] = json!(relative);
                index["tracks"][&id]["sha256"] = json!(hash);
                index["tracks"][&id]["bytes"] = json!(bytes);
                index["tracks"][&id]["duration_seconds"] = json!(seconds);
                index["tracks"][&id]["full_decode_passed"] = json!(true);
                continue;
            }
            let (hash, bytes, extension, seconds) =
                download(text(&t, "audio_url", ""), &temp).await?;
            let root = self.root.clone();
            let records = index.clone();
            let compare_id = id.clone();
            let compare_hash = hash.clone();
            let duplicate =
                blocking::run(move || duplicate_owner(&root, &records, &compare_id, &compare_hash))
                    .await?;
            let root = self.root.clone();
            let t = t.clone();
            let owned_id = id.clone();
            let expected_hash = hash.clone();
            let (relative, duplicate) = blocking::run(move || {
                if let Some(owner) = duplicate {
                    fs::remove_file(&temp)?;
                    return Ok((String::new(), Some(owner)));
                }
                let name = filename(text(&t, "title", "Bard-song"), &owned_id, &extension);
                let relative = format!("{UNKNOWN}/{name}");
                let destination = owned_path(&root, &relative)?;
                if destination.exists() {
                    if file_sha(&destination)? != expected_hash {
                        bail!("Existing archive file differs; no overwrite");
                    }
                    fs::remove_file(&temp)?;
                } else {
                    fs::rename(&temp, &destination)?;
                }
                Ok((relative, None))
            })
            .await?;
            index["tracks"][&id]["sha256"] = json!(hash);
            index["tracks"][&id]["bytes"] = json!(bytes);
            index["tracks"][&id]["duration_seconds"] = json!(seconds);
            index["tracks"][&id]["full_decode_passed"] = json!(true);
            if let Some(owner) = duplicate {
                index["tracks"][&id]["duplicate_of"] = json!(owner);
            } else {
                index["tracks"][&id]["relative"] = json!(relative);
            }
            let root = self.root.clone();
            let saved = index.clone();
            blocking::run(move || save_index(&root, &saved)).await?;
        }
        index["tasks"][task]["state"] = json!("complete");
        index["tasks"][task]["error"] = Value::Null;
        Ok(())
    }
    async fn recover(&self, task: &str, key: &str, index: &mut Value) -> Result<()> {
        if let Some(r) = index["recoveries"].get(task) {
            if r["task_id"].is_string() {
                index["recoveries"][task]["state"] = json!("submitted");
                return Ok(());
            }
            bail!("Recovery outcome uncertain; do not resubmit. Check provider history.");
        }
        let original = api("/generate/record-info", &[("taskId", task)], key, None).await?;
        if index["tasks"][task]["track_ids"]
            .as_array()
            .is_none_or(|a| a.is_empty())
        {
            let tracks = parse_details(&original, task)?;
            index["tasks"][task]["track_ids"] =
                json!(tracks.iter().map(|t| text(t, "id", "")).collect::<Vec<_>>());
            for t in tracks {
                let id = text(&t, "id", "").to_owned();
                if index["tracks"].get(&id).is_none() {
                    index["tracks"][id] = t;
                }
            }
        }
        let param = if let Some(s) = original["data"]["param"].as_str() {
            serde_json::from_str(s)?
        } else {
            original["data"]["param"].clone()
        };
        let callback = text(&param, "callBackUrl", "");
        let url = reqwest::Url::parse(callback)?;
        if url.scheme() != "https" {
            bail!("Original HTTPS callback unavailable");
        }
        let credits = api("/generate/credit", &[], key, None).await?;
        let payload = json!({"sunoTaskId":task,"callBackUrl":callback});
        let record = json!({"state":"submitting_or_unknown","credits_before":credits["data"],"started_at":crate::engine::now(),"request_sha256":digest(&payload),"human_authorization":"Recover existing tracks; provider credit cost may change"});
        let root = self.root.clone();
        let recovery_record = record.clone();
        let task_copy = task.to_owned();
        blocking::run(move || {
            create_journal(
                &root.join(format!("data/song-recovery-{task_copy}.json")),
                &recovery_record,
            )
        })
        .await?;
        index["recoveries"][task] = record;
        let root = self.root.clone();
        let saved = index.clone();
        blocking::run(move || save_index(&root, &saved)).await?;
        let answer = api("/suno/recovery", &[], key, Some(&payload)).await?;
        let recovery_id = answer["data"]["task_id"]
            .as_str()
            .filter(|s| valid_id(s))
            .context("Recovery submission outcome uncertain; never retry")?;
        index["recoveries"][task]["state"] = json!("submitted");
        index["recoveries"][task]["task_id"] = json!(recovery_id);
        let root = self.root.clone();
        let saved = index.clone();
        blocking::run(move || save_index(&root, &saved)).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn index() -> Value {
        json!({"version":1,"tasks":{},"tracks":{},"logs":{},"cache_files":{},"recoveries":{}})
    }
    fn line(id: &str) -> String {
        format!("[2026-10-29 10:00:00.000] [1234] [info] [SunoInterface.cpp:110] BardSinging/Suno: Generation submitted, taskId={id}\n")
    }
    #[test]
    fn log_discovery_is_incremental_and_ignores_prompt_text() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("SkyrimNet.log");
        let mut v = index();
        fs::write(
            &p,
            format!(
                "A prompt quoting SunoInterface.cpp Generation submitted, taskId=forged\n{}",
                line("original-task")
            ),
        )
        .unwrap();
        discover(&mut v, d.path()).unwrap();
        assert!(v["tasks"].get("original-task").is_some());
        assert!(v["tasks"].get("forged").is_none());
        discover(&mut v, d.path()).unwrap();
        assert_eq!(v["tasks"].as_object().unwrap().len(), 1);
        use std::io::Write;
        let mut f = fs::OpenOptions::new().append(true).open(&p).unwrap();
        f.write_all(line("second-task").as_bytes()).unwrap();
        drop(f);
        discover(&mut v, d.path()).unwrap();
        assert_eq!(v["tasks"].as_object().unwrap().len(), 2);
        fs::write(&p, line("after-truncate")).unwrap();
        discover(&mut v, d.path()).unwrap();
        assert!(v["tasks"].get("after-truncate").is_some());
    }
    #[test]
    fn cache_classification_never_guesses_when_no_sibling_is_known() {
        let mut v = index();
        v["tracks"]["a"] = json!({"id":"a","sha256":"hash-a"});
        v["tracks"]["b"] = json!({"id":"b","sha256":"hash-b"});
        let siblings = vec!["a".to_owned(), "b".to_owned()];
        let ids = HashSet::from(["a".to_owned()]);
        assert_eq!(
            category(&v["tracks"]["a"], &siblings, &v, &ids, &HashSet::new()),
            CACHED
        );
        assert_eq!(
            category(&v["tracks"]["b"], &siblings, &v, &ids, &HashSet::new()),
            IMPORT
        );
        assert_eq!(
            category(
                &v["tracks"]["b"],
                &siblings,
                &v,
                &HashSet::new(),
                &HashSet::new()
            ),
            UNKNOWN
        );
        assert_eq!(
            category(
                &v["tracks"]["b"],
                &siblings,
                &v,
                &HashSet::new(),
                &HashSet::from(["hash-b".to_owned()])
            ),
            CACHED
        );
    }
    #[test]
    fn provider_fields_accept_snake_case_and_use_explicit_voice() {
        let b = json!({"code":200,"data":{"taskId":"existing-task","status":"SUCCESS","param":"{\"vocalGender\":\"m\",\"instrumental\":false}","response":{"sunoData":[{"id":"track-a","title":"Song","prompt":"Lyrics","audio_url":"https://cdn.example/audio.mp3","duration":120}]}}});
        let parsed = parse_details(&b, "existing-task").unwrap();
        assert_eq!(parsed[0]["gender"], "male");
        assert_eq!(parsed[0]["audio_url"], "https://cdn.example/audio.mp3");
        assert!(parse_details(&b, "different-task").is_err());
        let mut unsafe_body = b.clone();
        unsafe_body["data"]["response"]["sunoData"][0]["id"] = json!("../escape");
        assert!(parse_details(&unsafe_body, "existing-task").is_err());
    }
    #[test]
    fn owned_paths_and_task_ids_cannot_escape_archive() {
        let d = tempfile::tempdir().unwrap();
        assert!(owned_path(d.path(), "../outside").is_err());
        assert!(owned_path(d.path(), "C:\\outside").is_err());
        assert!(owned_path(d.path(), "Unexpected/song.mp3").is_err());
        assert!(!valid_id("../bad"));
        assert!(!valid_id("https://host/task"));
    }
    #[test]
    fn content_dedup_requires_a_real_matching_file() {
        let d = tempfile::tempdir().unwrap();
        let mut v = index();
        let relative = format!("{UNKNOWN}/song--owner.mp3");
        let p = owned_path(d.path(), &relative).unwrap();
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, b"identical audio").unwrap();
        let hash = file_sha(&p).unwrap();
        v["tracks"]["owner"] = json!({"relative":relative,"sha256":hash});
        assert_eq!(
            duplicate_owner(d.path(), &v, "new-track", &hash).unwrap(),
            Some("owner".into())
        );
        fs::write(&p, b"modified audio").unwrap();
        assert_eq!(
            duplicate_owner(d.path(), &v, "new-track", &hash).unwrap(),
            None
        );
        fs::remove_file(p).unwrap();
        assert_eq!(
            duplicate_owner(d.path(), &v, "new-track", &hash).unwrap(),
            None
        );
    }
    #[test]
    fn move_reconciliation_is_idempotent_and_recovers_unindexed_file() {
        let d = tempfile::tempdir().unwrap();
        let mut v = index();
        let id = "track-one";
        let relative = format!("{UNKNOWN}/song--{id}.mp3");
        let p = owned_path(d.path(), &relative).unwrap();
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, b"audio").unwrap();
        v["tracks"][id] = json!({"id":id,"relative":relative,"task_id":"task-one","title":"Title","lyrics":"Words","gender":"female","style_tags":"lute","instrumental":false});
        v["tasks"]["task-one"] = json!({"track_ids":[id]});
        let known = HashSet::from([id.to_owned()]);
        reconcile(d.path(), &mut v, &known, &HashSet::new()).unwrap();
        reconcile(d.path(), &mut v, &known, &HashSet::new()).unwrap();
        assert_eq!(v["tracks"][id]["category"], CACHED);
        let (_, path) = find_saved(d.path(), id).unwrap().unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"audio");
        assert!(path.with_extension("yaml").is_file());
        assert!(find_saved(d.path(), "different").unwrap().is_none());
    }
    #[test]
    fn recovery_requires_explicit_consent_and_does_not_start_on_sync() {
        let d = tempfile::tempdir().unwrap();
        let a = Archive::new(d.path().to_owned());
        assert!(a.request_task("existing-task", true, "").is_err());
        a.request_sync();
        assert!(a.queued.lock().unwrap().is_empty());
        a.request_task(
            "existing-task",
            true,
            "Recover existing tracks; provider credit cost may change",
        )
        .unwrap();
        assert_eq!(a.queued.lock().unwrap().len(), 1);
    }
    #[tokio::test]
    async fn archive_cannot_submit_music_generation() {
        assert!(api("/generate", &[], "fixture", Some(&json!({})))
            .await
            .is_err());
        assert!(api("/lyrics", &[], "fixture", Some(&json!({})))
            .await
            .is_err());
    }
}
