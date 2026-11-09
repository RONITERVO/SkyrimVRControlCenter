//! Headless (no-headset) Open Shaders / Community Shaders pre-compiler.
//!
//! Temporarily enables SteamVR's built-in `driver_null` (1024x1024 headless HMD)
//! with a crash-safe byte-exact backup of `steamvr.vrsettings`, launches Skyrim VR
//! through Mod Organizer 2's SKSEVR shortcut so USVFS and RootBuilder run normally,
//! tracks real-time shader compilation progress across `CommunityShaders.log` and
//! `ShaderCache` disk blobs, and automatically closes Skyrim VR, waits for MO2
//! RootBuilder cleanup, stops headless SteamVR, and restores the user's exact
//! PSVR2 `steamvr.vrsettings`.
use crate::{
    config::{Config, Result, ShaderCacheConfig, now, read_json, wide, write_json},
    platform, runtime,
    shader_cache::{
        self, LiveCacheFiles, baseline_expected_shaders, check_no_reparse, decode_qt_byte_array,
        inspect_live_cache_dirs, live_cache_dirs,
    },
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File},
    io::{Read, Seek, SeekFrom},
    os::windows::process::CommandExt,
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant, SystemTime},
};
use windows_sys::Win32::{
    Foundation::{CloseHandle, HWND, LPARAM},
    Storage::FileSystem::{MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW},
    System::Threading::{
        OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
        QueryFullProcessImageNameW, TerminateProcess, WaitForSingleObject,
    },
    UI::WindowsAndMessaging::{EnumWindows, GetWindowThreadProcessId, PostMessageW, WM_CLOSE},
};

const PRECOMPILE_STATE_REL: &str = ".local/headless-precompile-state.json";
const PRECOMPILE_VRSETTINGS_BAK_REL: &str = ".local/steamvr.vrsettings.precompile.bak";
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const MAX_PRECOMPILE_DURATION: Duration = Duration::from_secs(90 * 60);
const MO2_SKYRIM_SPAWN_TIMEOUT: Duration = Duration::from_secs(210);
const CACHE_SETTLE_DURATION: Duration = Duration::from_secs(4);

const PREFLIGHT_BLOCKERS: &[&str] = &[
    "SkyrimVR.exe",
    "sksevr_loader.exe",
    "ModOrganizer.exe",
    "ModOrganizer_original.exe",
    "vrserver.exe",
    "vrmonitor.exe",
    "vrcompositor.exe",
    "PlayStationVR2App.exe",
];

const STEAMVR_DAEMONS: &[&str] = &[
    "vrmonitor.exe",
    "vrdashboard.exe",
    "vrcompositor.exe",
    "vrprismhost.exe",
    "vrserver.exe",
    "vrserverhelper.exe",
];

#[derive(Clone, Debug)]
struct Snapshot {
    active: bool,
    phase: String,
    compiled_shaders: usize,
    total_shaders: usize,
    percent: u8,
    permille: u16,
    elapsed_seconds: u64,
    eta_seconds: Option<u64>,
    failed_tasks: usize,
    current_class: Option<String>,
    shortcut: Option<String>,
    message: String,
}

impl Default for Snapshot {
    fn default() -> Self {
        Self {
            active: false,
            phase: "idle".to_owned(),
            compiled_shaders: 0,
            total_shaders: 0,
            percent: 0,
            permille: 0,
            elapsed_seconds: 0,
            eta_seconds: None,
            failed_tasks: 0,
            current_class: None,
            shortcut: None,
            message:
                "Headless pre-compile: idle — PSVR2 headset, PSVR2 App, and KAT can stay off"
                    .to_owned(),
        }
    }
}

impl Snapshot {
    fn to_json(&self) -> Value {
        json!({
            "active": self.active,
            "phase": self.phase,
            "compiledShaders": self.compiled_shaders,
            "totalShaders": self.total_shaders,
            "percent": self.percent,
            "permille": self.permille,
            "elapsedSeconds": self.elapsed_seconds,
            "etaSeconds": self.eta_seconds,
            "failedTasks": self.failed_tasks,
            "currentClass": self.current_class,
            "shortcut": self.shortcut,
            "message": self.message,
        })
    }
}

pub struct PrecompileManager {
    state: Arc<Mutex<Snapshot>>,
    active: Arc<AtomicBool>,
    cancel: Arc<AtomicBool>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl Default for PrecompileManager {
    fn default() -> Self {
        Self::new()
    }
}

impl PrecompileManager {
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(Snapshot::default())),
            active: Arc::new(AtomicBool::new(false)),
            cancel: Arc::new(AtomicBool::new(false)),
            worker: Mutex::new(None),
        }
    }

    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::SeqCst)
    }

    pub fn snapshot_json(&self) -> Value {
        self.state
            .lock()
            .map(|s| s.to_json())
            .unwrap_or_else(|_| Snapshot::default().to_json())
    }

    pub fn start(&self, config: &Config, root: &Path, clear_cache: bool) -> Result<Value> {
        if self.is_active() {
            return Err("Headless shader pre-compile is already running.".into());
        }
        // Reap any finished previous join handle.
        if let Ok(mut guard) = self.worker.lock()
            && let Some(prev) = guard.take()
        {
            let _ = prev.join();
        }

        // Recover any leftover crash backup before validating pre-flight state.
        let _ = recover_stale_headless_steamvr(root);

        let shader_cfg = config
            .effective_shader_cache()
            .ok_or("Shader cache MO2 paths could not be resolved from control-center.json.")?;
        if !shader_cfg.mo2_root.is_dir() {
            return Err(format!(
                "Configured MO2 directory does not exist: {}",
                shader_cfg.mo2_root.display()
            ));
        }

        ensure_preflight_closed()?;

        let mo2_exe = shader_cfg.mo2_root.join("ModOrganizer.exe");
        if !mo2_exe.is_file() {
            return Err(format!(
                "ModOrganizer.exe was not found at {}",
                mo2_exe.display()
            ));
        }
        let shortcut = resolve_mo2_shortcut(&shader_cfg)?;
        let (steamvr_settings, steamvr_runtime_dir) = discover_openvr_paths(Some(config))?;
        if !steamvr_settings.is_file() {
            return Err(format!(
                "SteamVR settings file was not found at {}",
                steamvr_settings.display()
            ));
        }
        check_no_reparse(&steamvr_settings)?;

        let baseline_total = baseline_expected_shaders(&shader_cfg);

        self.cancel.store(false, Ordering::SeqCst);
        self.active.store(true, Ordering::SeqCst);

        if let Ok(mut s) = self.state.lock() {
            *s = Snapshot {
                active: true,
                phase: "preparing".to_owned(),
                compiled_shaders: 0,
                total_shaders: baseline_total,
                percent: 1,
                permille: 10,
                elapsed_seconds: 0,
                eta_seconds: None,
                failed_tasks: 0,
                current_class: None,
                shortcut: Some(shortcut.clone()),
                message: "Preparing shader cache & headless SteamVR null driver…".to_owned(),
            };
        }

        let state_arc = self.state.clone();
        let active_arc = self.active.clone();
        let cancel_arc = self.cancel.clone();
        let root_buf = root.to_path_buf();

        let plan = PrecompilePlan {
            root: root_buf,
            shader_cfg,
            mo2_exe,
            shortcut: shortcut.clone(),
            steamvr_settings,
            steamvr_runtime_dir,
            clear_cache,
            baseline_total,
        };

        let handle = thread::Builder::new()
            .name("shader-precompile-worker".into())
            .spawn(move || {
                run_precompile_worker(plan, state_arc, active_arc, cancel_arc);
            })
            .map_err(|e| {
                self.active.store(false, Ordering::SeqCst);
                format!("Could not start shader pre-compile worker thread: {e}")
            })?;

        if let Ok(mut guard) = self.worker.lock() {
            *guard = Some(handle);
        }

        Ok(json!({
            "message": format!(
                "Headless shader pre-compile started via MO2 ({shortcut}); keep PSVR2 headset & KAT off."
            ),
            "shortcut": shortcut,
            "expectedShaders": baseline_total,
            "clearCache": clear_cache,
        }))
    }

    pub fn cancel(&self) -> Result<Value> {
        if !self.is_active() {
            return Err("No headless shader pre-compile is currently running.".into());
        }
        self.cancel.store(true, Ordering::SeqCst);
        if let Ok(mut s) = self.state.lock() {
            s.phase = "shutting-down".to_owned();
            s.message =
                "Cancelling pre-compile — closing Skyrim VR / SteamVR and restoring PSVR2 settings…"
                    .to_owned();
        }
        Ok(json!({
            "message": "Cancelling headless shader pre-compile and restoring PSVR2 SteamVR settings…"
        }))
    }

    pub fn cancel_and_wait(&self, timeout: Duration) {
        if !self.is_active() {
            return;
        }
        self.cancel.store(true, Ordering::SeqCst);
        let deadline = Instant::now() + timeout;
        while self.is_active() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(100));
        }
        if let Ok(mut guard) = self.worker.lock()
            && !self.is_active()
            && let Some(h) = guard.take()
        {
            let _ = h.join();
        }
    }
}

struct PrecompilePlan {
    root: PathBuf,
    shader_cfg: ShaderCacheConfig,
    mo2_exe: PathBuf,
    shortcut: String,
    steamvr_settings: PathBuf,
    steamvr_runtime_dir: Option<PathBuf>,
    clear_cache: bool,
    baseline_total: usize,
}

fn ensure_preflight_closed() -> Result<()> {
    let running: Vec<String> = platform::checked_processes()?
        .into_iter()
        .filter(|(_, n)| {
            PREFLIGHT_BLOCKERS
                .iter()
                .any(|b| n.eq_ignore_ascii_case(b))
        })
        .map(|(pid, name)| format!("{name} (PID {pid})"))
        .collect();
    if running.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "Close Skyrim VR, MO2, SteamVR, and PlayStation VR2 App before headless pre-compile. Running: {}",
            running.join(", ")
        ))
    }
}

pub fn parse_mo2_skse_shortcut(ini_text: &str) -> Option<String> {
    let mut in_section = false;
    let mut titles: std::collections::HashMap<usize, String> = std::collections::HashMap::new();
    let mut binaries: std::collections::HashMap<usize, String> = std::collections::HashMap::new();

    for line in ini_text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with(';') {
            continue;
        }
        if let Some(sec) = trimmed.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            in_section = sec.trim().eq_ignore_ascii_case("customExecutables");
            continue;
        }
        if !in_section {
            continue;
        }
        let Some((raw_k, raw_v)) = trimmed.split_once('=') else {
            continue;
        };
        let Some((idx_str, field)) = raw_k.trim().split_once('\\') else {
            continue;
        };
        let Ok(idx) = idx_str.trim().parse::<usize>() else {
            continue;
        };
        let val = raw_v.trim();
        let decoded = if let Some(inner) = val
            .strip_prefix("@ByteArray(")
            .and_then(|s| s.strip_suffix(')'))
        {
            decode_qt_byte_array(inner)
        } else {
            val.trim_matches('"').to_owned()
        };
        if field.eq_ignore_ascii_case("title") {
            titles.insert(idx, decoded);
        } else if field.eq_ignore_ascii_case("binary") {
            binaries.insert(idx, decoded);
        }
    }

    let mut indices: Vec<usize> = titles.keys().copied().collect();
    indices.sort_unstable();

    // Priority 1: sksevr_loader.exe entry preferring MGO / "Do Not Unlock"
    for &idx in &indices {
        let title = titles.get(&idx).map(String::as_str).unwrap_or("");
        let bin = binaries.get(&idx).map(String::as_str).unwrap_or("");
        let bin_lower = bin.to_ascii_lowercase();
        let title_lower = title.to_ascii_lowercase();
        if bin_lower.ends_with("sksevr_loader.exe")
            && (title_lower.contains("do not unlock") || title_lower.contains("launch mgo"))
            && !title.is_empty()
        {
            return Some(title.to_owned());
        }
    }
    // Priority 2: any sksevr_loader.exe entry
    for &idx in &indices {
        let title = titles.get(&idx).map(String::as_str).unwrap_or("");
        let bin = binaries.get(&idx).map(String::as_str).unwrap_or("");
        if bin.to_ascii_lowercase().ends_with("sksevr_loader.exe") && !title.is_empty() {
            return Some(title.to_owned());
        }
    }
    // Priority 3: title named SKSE / SKSEVR
    for &idx in &indices {
        let title = titles.get(&idx).map(String::as_str).unwrap_or("");
        if matches!(title.to_ascii_lowercase().as_str(), "skse" | "sksevr") {
            return Some(title.to_owned());
        }
    }
    None
}

fn resolve_mo2_shortcut(cfg: &ShaderCacheConfig) -> Result<String> {
    if let Some(custom) = cfg
        .mo2_shortcut
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        return Ok(custom.to_owned());
    }
    let ini_path = cfg.mo2_root.join("ModOrganizer.ini");
    let bytes = fs::read(&ini_path)
        .map_err(|e| format!("Cannot read {}: {e}", ini_path.display()))?;
    let text = String::from_utf8_lossy(&bytes);
    parse_mo2_skse_shortcut(&text).ok_or_else(|| {
        format!(
            "Could not find an sksevr_loader.exe executable entry in {}. Set shaderCache.mo2Shortcut in control-center.json.",
            ini_path.display()
        )
    })
}

fn discover_openvr_paths(config: Option<&Config>) -> Result<(PathBuf, Option<PathBuf>)> {
    let mut settings_candidate: Option<PathBuf> = None;
    let mut runtime_candidate: Option<PathBuf> = None;

    if let Some(local_app_data) = std::env::var_os("LOCALAPPDATA") {
        let vrpath = PathBuf::from(local_app_data).join(r"openvr\openvrpaths.vrpath");
        if vrpath.is_file()
            && let Ok(v) = read_json(&vrpath)
        {
            if let Some(cfg_dir) = v["config"]
                .as_array()
                .and_then(|a| a.first())
                .and_then(Value::as_str)
            {
                let p = PathBuf::from(cfg_dir).join("steamvr.vrsettings");
                if p.is_file() {
                    settings_candidate = Some(p);
                }
            }
            if let Some(rt_dir) = v["runtime"]
                .as_array()
                .and_then(|a| a.first())
                .and_then(Value::as_str)
            {
                let p = PathBuf::from(rt_dir);
                if p.is_dir() {
                    runtime_candidate = Some(p);
                }
            }
        }
    }

    if settings_candidate.is_none()
        && let Some(cfg) = config
        && let Ok(p) = runtime::settings_path(cfg)
        && p.is_file()
    {
        settings_candidate = Some(p);
    }

    if settings_candidate.is_none() {
        let default_cfg = PathBuf::from(r"C:\Program Files (x86)\Steam\config\steamvr.vrsettings");
        if default_cfg.is_file() {
            settings_candidate = Some(default_cfg);
        }
    }

    if runtime_candidate.is_none() {
        if let Some(sp) = &settings_candidate
            && let Some(steam_root) = sp.parent().and_then(Path::parent)
        {
            let rt = steam_root.join(r"steamapps\common\SteamVR");
            if rt.is_dir() {
                runtime_candidate = Some(rt);
            }
        }
        if runtime_candidate.is_none() {
            let default_rt =
                PathBuf::from(r"C:\Program Files (x86)\Steam\steamapps\common\SteamVR");
            if default_rt.is_dir() {
                runtime_candidate = Some(default_rt);
            }
        }
    }

    let settings = settings_candidate.ok_or(
        "Could not locate SteamVR config/steamvr.vrsettings via openvrpaths.vrpath or Steam installation.",
    )?;
    Ok((settings, runtime_candidate))
}

fn community_shaders_log_candidates() -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut push_unique = |p: PathBuf| {
        if !out
            .iter()
            .any(|existing: &PathBuf| existing.to_string_lossy().eq_ignore_ascii_case(&p.to_string_lossy()))
        {
            out.push(p);
        }
    };
    if let Some(od) = std::env::var_os("OneDrive") {
        let od_path = PathBuf::from(od);
        push_unique(od_path.join(r"Tiedostot\My Games\Skyrim VR\SKSE\CommunityShaders.log"));
        push_unique(od_path.join(r"Documents\My Games\Skyrim VR\SKSE\CommunityShaders.log"));
    }
    if let Some(up) = std::env::var_os("USERPROFILE") {
        push_unique(PathBuf::from(up).join(r"Documents\My Games\Skyrim VR\SKSE\CommunityShaders.log"));
    }
    out
}

pub fn patch_steamvr_for_headless(original_bytes: &[u8]) -> Result<Vec<u8>> {
    let text = String::from_utf8(original_bytes.to_vec())
        .map_err(|e| format!("steamvr.vrsettings is not UTF-8: {e}"))?;
    let mut v: Value = serde_json::from_str(text.trim_start_matches('\u{feff}'))
        .map_err(|e| format!("Cannot parse steamvr.vrsettings JSON: {e}"))?;
    if !v.is_object() {
        return Err("steamvr.vrsettings root is not a JSON object.".into());
    }

    if !v["driver_null"].is_object() {
        v["driver_null"] = json!({});
    }
    v["driver_null"]["enable"] = json!(true);
    v["driver_null"]["id"] = json!("Null Driver");
    v["driver_null"]["serialNumber"] = json!("Null Serial Number");
    v["driver_null"]["modelNumber"] = json!("Null Model Number");
    v["driver_null"]["loadPriority"] = json!(999);
    v["driver_null"]["windowX"] = json!(0);
    v["driver_null"]["windowY"] = json!(0);
    v["driver_null"]["windowWidth"] = json!(1280);
    v["driver_null"]["windowHeight"] = json!(720);
    v["driver_null"]["renderWidth"] = json!(1024);
    v["driver_null"]["renderHeight"] = json!(1024);
    v["driver_null"]["secondsFromVsyncToPhotons"] = json!(0.01111111);
    v["driver_null"]["displayFrequency"] = json!(90.0);

    if !v["steamvr"].is_object() {
        v["steamvr"] = json!({});
    }
    v["steamvr"]["forcedDriver"] = json!("null");
    v["steamvr"]["activateMultipleDrivers"] = json!(true);
    v["steamvr"]["requireHmd"] = json!(false);
    v["steamvr"]["enableHomeApp"] = json!(false);
    v["steamvr"]["showMirrorView"] = json!(false);
    v["steamvr"]["startMonitorFromAppLaunch"] = json!(false);

    serde_json::to_vec_pretty(&v).map_err(|e| e.to_string())
}

fn write_bytes_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    check_no_reparse(path)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let temp = path.with_extension(format!("{}.tmp", std::process::id()));
    fs::write(&temp, bytes).map_err(|e| format!("Cannot write {}: {e}", temp.display()))?;
    let from = wide(&temp.to_string_lossy());
    let to = wide(&path.to_string_lossy());
    let ok = unsafe {
        MoveFileExW(
            from.as_ptr(),
            to.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if ok == 0 {
        let err = std::io::Error::last_os_error();
        let _ = fs::remove_file(&temp);
        return Err(format!("Cannot replace {}: {err}", path.display()));
    }
    Ok(())
}

fn restore_steamvr_backup(
    settings_path: &Path,
    backup_path: &Path,
    state_path: &Path,
    expected_sha256: Option<&str>,
    note: &str,
) -> Result<()> {
    let orig_bytes = fs::read(backup_path)
        .map_err(|e| format!("Cannot read SteamVR backup {}: {e}", backup_path.display()))?;
    let actual_hash = format!("{:x}", Sha256::digest(&orig_bytes));
    if let Some(exp) = expected_sha256
        && !exp.is_empty()
        && !actual_hash.eq_ignore_ascii_case(exp)
    {
        return Err(format!(
            "SteamVR backup hash mismatch (expected {exp}, got {actual_hash}); leaving backup intact."
        ));
    }
    write_bytes_atomic(settings_path, &orig_bytes)?;
    let verify_bytes = fs::read(settings_path)
        .map_err(|e| format!("Cannot verify restored {}: {e}", settings_path.display()))?;
    if verify_bytes != orig_bytes {
        return Err("Restored steamvr.vrsettings bytes did not match original backup.".into());
    }
    let _ = fs::remove_file(backup_path);
    let _ = write_json(
        state_path,
        &json!({
            "active": false,
            "restoredAt": now(),
            "settingsPath": settings_path,
            "restoredSha256": actual_hash,
            "note": note,
        }),
    );
    Ok(())
}

/// Checks `.local/steamvr.vrsettings.precompile.bak` at Control Center startup
/// and restores the user's original PSVR2 `steamvr.vrsettings` if a previous
/// headless pre-compile was interrupted by a crash or power loss.
pub fn recover_stale_headless_steamvr(root: &Path) -> Option<String> {
    let bak_path = root.join(PRECOMPILE_VRSETTINGS_BAK_REL);
    let state_path = root.join(PRECOMPILE_STATE_REL);
    if !bak_path.is_file() {
        return None;
    }

    let state_val = read_json(&state_path).ok();
    let settings_path = state_val
        .as_ref()
        .and_then(|v| v["settingsPath"].as_str())
        .map(PathBuf::from)
        .or_else(|| discover_openvr_paths(None).ok().map(|(p, _)| p))?;
    let expected_sha = state_val
        .as_ref()
        .and_then(|v| v["originalSha256"].as_str())
        .map(str::to_owned);
    let runtime_dir = state_val
        .as_ref()
        .and_then(|v| v["steamvrRuntimeDir"].as_str())
        .map(PathBuf::from);

    let procs = platform::processes();
    let game_or_psvr2_running = procs.iter().any(|(_, n)| {
        n.eq_ignore_ascii_case("SkyrimVR.exe") || n.eq_ignore_ascii_case("PlayStationVR2App.exe")
    });
    if game_or_psvr2_running {
        return Some(
            "Interrupted headless SteamVR backup detected; close Skyrim VR / PlayStation VR2 App to auto-restore."
                .to_owned(),
        );
    }

    shutdown_headless_steamvr(runtime_dir.as_deref());
    match restore_steamvr_backup(
        &settings_path,
        &bak_path,
        &state_path,
        expected_sha.as_deref(),
        "Recovered original PSVR2 SteamVR settings after interrupted headless pre-compile",
    ) {
        Ok(()) => Some(
            "Recovered original PSVR2 SteamVR settings from interrupted headless pre-compile."
                .to_owned(),
        ),
        Err(e) => Some(format!("SteamVR recovery needs attention: {e}")),
    }
}

unsafe extern "system" fn enum_close_window(hwnd: HWND, ctx: LPARAM) -> i32 {
    let target_pid = unsafe { *(ctx as *const u32) };
    let mut owner = 0u32;
    unsafe {
        GetWindowThreadProcessId(hwnd, &mut owner);
        if owner == target_pid {
            PostMessageW(hwnd, WM_CLOSE, 0, 0);
        }
    }
    1
}

fn post_close_to_process_names(names: &[&str]) -> Vec<u32> {
    let mut pids = Vec::new();
    for (pid, proc_name) in platform::processes() {
        if names.iter().any(|n| proc_name.eq_ignore_ascii_case(n)) {
            unsafe {
                EnumWindows(Some(enum_close_window), &pid as *const u32 as isize);
            }
            pids.push(pid);
        }
    }
    pids
}

fn any_named_running(names: &[&str]) -> bool {
    platform::processes()
        .iter()
        .any(|(_, proc_name)| names.iter().any(|n| proc_name.eq_ignore_ascii_case(n)))
}

fn wait_until_gone(names: &[&str], timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if !any_named_running(names) {
            return true;
        }
        thread::sleep(Duration::from_millis(250));
    }
    !any_named_running(names)
}

fn terminate_verified_by_name(names: &[&str], path_ok: impl Fn(&Path) -> bool) {
    for (pid, proc_name) in platform::processes() {
        if !names.iter().any(|n| proc_name.eq_ignore_ascii_case(n)) {
            continue;
        }
        unsafe {
            let h = OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_TERMINATE | PROCESS_SYNCHRONIZE,
                0,
                pid,
            );
            if h.is_null() {
                continue;
            }
            let mut chars = vec![0u16; 32768];
            let mut n = chars.len() as u32;
            if QueryFullProcessImageNameW(h, 0, chars.as_mut_ptr(), &mut n) != 0 {
                let full_path = PathBuf::from(String::from_utf16_lossy(&chars[..n as usize]));
                if path_ok(&full_path) {
                    let _ = TerminateProcess(h, 0);
                    let _ = WaitForSingleObject(h, 4_000);
                }
            }
            CloseHandle(h);
        }
    }
}

fn shutdown_skyrim_and_wait_for_mo2(
    game_data_dir: Option<&Path>,
    update_msg: impl Fn(&str),
) {
    if any_named_running(&["SkyrimVR.exe"]) {
        update_msg("Closing Skyrim VR…");
        post_close_to_process_names(&["SkyrimVR.exe"]);
        if !wait_until_gone(&["SkyrimVR.exe"], Duration::from_secs(10)) {
            let expected_game_dir = game_data_dir.and_then(Path::parent);
            terminate_verified_by_name(&["SkyrimVR.exe"], |full_path| {
                let is_skyrim = full_path
                    .file_name()
                    .and_then(|s| s.to_str())
                    .is_some_and(|n| n.eq_ignore_ascii_case("SkyrimVR.exe"));
                if !is_skyrim {
                    return false;
                }
                if let Some(dir) = expected_game_dir {
                    full_path.parent().is_some_and(|p| {
                        p.to_string_lossy()
                            .eq_ignore_ascii_case(&dir.to_string_lossy())
                    })
                } else {
                    true
                }
            });
            let _ = wait_until_gone(&["SkyrimVR.exe"], Duration::from_secs(5));
        }
    }

    // Never force-terminate ModOrganizer.exe: wait for RootBuilder's onFinishedRun cleanup to complete cleanly.
    let mo2_names = ["ModOrganizer.exe", "ModOrganizer_original.exe"];
    if any_named_running(&mo2_names) {
        update_msg("Waiting for Mod Organizer 2 (RootBuilder cleanup) to finish…");
        if !wait_until_gone(&mo2_names, Duration::from_secs(25)) {
            post_close_to_process_names(&mo2_names);
            let _ = wait_until_gone(&mo2_names, Duration::from_secs(10));
        }
    }
}

fn shutdown_headless_steamvr(runtime_dir: Option<&Path>) {
    if !any_named_running(STEAMVR_DAEMONS) {
        return;
    }
    post_close_to_process_names(&["vrmonitor.exe", "vrdashboard.exe", "vrcompositor.exe", "vrserver.exe"]);
    if !wait_until_gone(STEAMVR_DAEMONS, Duration::from_secs(5)) {
        terminate_verified_by_name(STEAMVR_DAEMONS, |full_path| {
            let s = full_path.to_string_lossy().to_ascii_lowercase();
            if let Some(rt) = runtime_dir {
                s.starts_with(&rt.to_string_lossy().to_ascii_lowercase())
            } else {
                s.contains(r"\steamvr\bin\")
            }
        });
        let _ = wait_until_gone(STEAMVR_DAEMONS, Duration::from_secs(6));
    }
    // Brief settle pause after vrserver.exe exits so any shutdown flush finishes before we restore steamvr.vrsettings.
    thread::sleep(Duration::from_millis(450));
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CsLogTracker {
    pub hooked_load_shaders: bool,
    pub current_class: Option<String>,
    pub batches_started: usize,
    pub batches_completed: usize,
    pub in_flight: bool,
    pub queued_total: usize,
    pub completed_from_log: usize,
    pub failed_tasks: usize,
    pub boot_batch_done: bool,
    pub saw_saved_disk_cache_info: bool,
    pub disk_cache_held: bool,
}

impl CsLogTracker {
    pub fn observe_line(&mut self, line: &str) -> bool {
        let mut significant_activity = false;
        if line.contains("Hooking BSShader::LoadShaders") || line.contains("Using disk cache") {
            self.hooked_load_shaders = true;
        }
        if let Some((_, rest)) = line.split_once("BSShader::LoadShaders called on ")
            && let Some((cls, _)) = rest.split_once(" -")
        {
            let cls_trimmed = cls.trim();
            if !cls_trimmed.is_empty() {
                self.current_class = Some(cls_trimmed.to_owned());
                self.hooked_load_shaders = true;
            }
        }
        if let Some((_, rest)) = line.split_once("Shader compilation started (")
            && let Some((num_s, _)) = rest.split_once(" tasks queued)")
            && let Ok(n) = num_s.trim().parse::<usize>()
        {
            self.batches_started += 1;
            self.in_flight = true;
            if n >= 100 || self.queued_total == 0 {
                self.queued_total = self.queued_total.max(n);
            }
            significant_activity = true;
        }
        if let Some((_, rest)) = line.split_once("Shader compilation completed: ")
            && let Some((counts, tail)) = rest.split_once(" tasks (")
            && let Some((done_s, total_s)) = counts.split_once('/')
            && let (Ok(done), Ok(total)) =
                (done_s.trim().parse::<usize>(), total_s.trim().parse::<usize>())
        {
            let failed = tail
                .split_once(" failed)")
                .and_then(|(f_s, _)| f_s.trim().parse::<usize>().ok())
                .unwrap_or(0);
            self.batches_completed += 1;
            self.in_flight = self.batches_started > self.batches_completed;
            self.failed_tasks += failed;
            self.completed_from_log = self.completed_from_log.max(done);
            if total >= 100 {
                self.boot_batch_done = true;
                self.queued_total = self.queued_total.max(total);
            } else if self.queued_total == 0 {
                self.queued_total = total;
            }
            significant_activity = true;
        }
        if line.contains("Saved disk cache info") {
            self.saw_saved_disk_cache_info = true;
            significant_activity = true;
        }
        if line.contains("Disk cache HELD") {
            self.disk_cache_held = true;
        }
        significant_activity
    }

    pub fn is_ready_to_finish(&self, live: &LiveCacheFiles, clear_cache: bool) -> bool {
        if !self.saw_saved_disk_cache_info || self.in_flight {
            return false;
        }
        if clear_cache && self.batches_completed == 0 {
            return false;
        }
        live.has_info_ini
            && !live.has_temp_files
            && live.manifest_entries.is_some_and(|n| n > 0)
    }
}

pub fn compute_progress_numbers(
    compiled: usize,
    total: usize,
    compile_started_at: Option<Instant>,
) -> (u8, u16, Option<u64>) {
    let denom = total.max(compiled).max(1);
    let ratio = (compiled as f64 / denom as f64).clamp(0.0, 1.0);
    // Reserve 0..50 permille for startup and 970..1000 for final cache flush & clean shutdown.
    let permille = (50.0 + ratio * 920.0).round().clamp(50.0, 970.0) as u16;
    let percent = ((permille as f64) / 10.0).round().clamp(5.0, 97.0) as u8;
    let eta = compile_started_at.and_then(|t0| {
        let secs = t0.elapsed().as_secs_f64();
        if compiled >= 20 && secs >= 8.0 && denom > compiled {
            let rate = compiled as f64 / secs;
            if rate > 0.01 {
                Some(((denom - compiled) as f64 / rate).round() as u64)
            } else {
                None
            }
        } else {
            None
        }
    });
    (percent, permille, eta)
}

fn format_mm_ss(secs: u64) -> String {
    let mins = secs / 60;
    let rem = secs % 60;
    if mins >= 60 {
        format!("{}h {:02}m {:02}s", mins / 60, mins % 60, rem)
    } else {
        format!("{mins:02}:{rem:02}")
    }
}

struct FreshLogReader {
    candidates: Vec<PathBuf>,
    pre_launch_snapshots: Vec<(PathBuf, Option<(SystemTime, u64)>)>,
    launch_floor: SystemTime,
    active_path: Option<PathBuf>,
    offset: u64,
    partial_line: String,
}

impl FreshLogReader {
    fn new(candidates: Vec<PathBuf>) -> Self {
        let pre_launch_snapshots = candidates
            .iter()
            .map(|p| {
                let meta = fs::metadata(p)
                    .ok()
                    .and_then(|m| Some((m.modified().ok()?, m.len())));
                (p.clone(), meta)
            })
            .collect();
        let launch_floor = SystemTime::now()
            .checked_sub(Duration::from_secs(2))
            .unwrap_or(SystemTime::UNIX_EPOCH);
        Self {
            candidates,
            pre_launch_snapshots,
            launch_floor,
            active_path: None,
            offset: 0,
            partial_line: String::new(),
        }
    }

    fn poll_lines(&mut self) -> Vec<String> {
        if self.active_path.is_none() {
            for candidate in &self.candidates {
                let Ok(meta) = fs::metadata(candidate) else {
                    continue;
                };
                let Ok(mtime) = meta.modified() else {
                    continue;
                };
                if mtime < self.launch_floor {
                    continue;
                }
                let prev = self
                    .pre_launch_snapshots
                    .iter()
                    .find(|(p, _)| p == candidate)
                    .and_then(|(_, s)| *s);
                if prev == Some((mtime, meta.len())) {
                    continue;
                }
                self.active_path = Some(candidate.clone());
                self.offset = 0;
                self.partial_line.clear();
                break;
            }
        }

        let Some(path) = self.active_path.clone() else {
            return Vec::new();
        };
        let Ok(mut f) = File::open(&path) else {
            return Vec::new();
        };
        let Ok(meta) = f.metadata() else {
            return Vec::new();
        };
        if meta.len() < self.offset {
            // File was truncated/recreated on game startup.
            self.offset = 0;
            self.partial_line.clear();
        }
        if meta.len() == self.offset {
            return Vec::new();
        }
        if f.seek(SeekFrom::Start(self.offset)).is_err() {
            return Vec::new();
        }
        let mut buf = Vec::new();
        if f.read_to_end(&mut buf).is_err() || buf.is_empty() {
            return Vec::new();
        }
        self.offset += buf.len() as u64;
        self.partial_line
            .push_str(&String::from_utf8_lossy(&buf));

        let mut lines = Vec::new();
        while let Some(pos) = self.partial_line.find('\n') {
            let mut line: String = self.partial_line.drain(..=pos).collect();
            while line.ends_with('\n') || line.ends_with('\r') {
                line.pop();
            }
            lines.push(line);
        }
        lines
    }
}

enum WorkerOutcome {
    Completed { final_shaders: usize, failed: usize },
    Cancelled,
}

fn run_precompile_worker(
    plan: PrecompilePlan,
    state: Arc<Mutex<Snapshot>>,
    active: Arc<AtomicBool>,
    cancel: Arc<AtomicBool>,
) {
    let run_started = Instant::now();
    let bak_path = plan.root.join(PRECOMPILE_VRSETTINGS_BAK_REL);
    let state_path = plan.root.join(PRECOMPILE_STATE_REL);

    let set_state = |phase: &str,
                     compiled: usize,
                     total: usize,
                     percent: u8,
                     permille: u16,
                     eta: Option<u64>,
                     failed: usize,
                     cls: Option<String>,
                     msg: String| {
        if let Ok(mut s) = state.lock() {
            s.active = true;
            s.phase = phase.to_owned();
            s.compiled_shaders = compiled;
            s.total_shaders = total;
            s.percent = percent;
            s.permille = permille;
            s.elapsed_seconds = run_started.elapsed().as_secs();
            s.eta_seconds = eta;
            s.failed_tasks = failed;
            s.current_class = cls;
            s.message = msg;
        }
    };

    // Step 1: Prepare shader cache & upfront boot-compile settings BEFORE touching steamvr.vrsettings.
    let prep_result = if plan.clear_cache {
        shader_cache::clear_for_reload(Some(&plan.shader_cfg), &plan.root, true)
    } else {
        shader_cache::enforce_upfront_settings(Some(&plan.shader_cfg), &plan.root)
    };
    if let Err(e) = prep_result {
        if let Ok(mut s) = state.lock() {
            s.active = false;
            s.phase = "failed".to_owned();
            s.percent = 0;
            s.permille = 0;
            s.message = format!("Pre-compile stopped during preparation: {e}");
        }
        active.store(false, Ordering::SeqCst);
        return;
    }

    let mut log_reader = FreshLogReader::new(community_shaders_log_candidates());

    // Step 2: Back up steamvr.vrsettings byte-for-byte & enable headless driver_null.
    let orig_vrsettings = match fs::read(&plan.steamvr_settings) {
        Ok(b) => b,
        Err(e) => {
            if let Ok(mut s) = state.lock() {
                s.active = false;
                s.phase = "failed".to_owned();
                s.percent = 0;
                s.permille = 0;
                s.message = format!("Cannot read steamvr.vrsettings: {e}");
            }
            active.store(false, Ordering::SeqCst);
            return;
        }
    };
    let orig_sha256 = format!("{:x}", Sha256::digest(&orig_vrsettings));
    let patched_vrsettings = match patch_steamvr_for_headless(&orig_vrsettings) {
        Ok(b) => b,
        Err(e) => {
            if let Ok(mut s) = state.lock() {
                s.active = false;
                s.phase = "failed".to_owned();
                s.percent = 0;
                s.permille = 0;
                s.message = format!("Cannot patch steamvr.vrsettings: {e}");
            }
            active.store(false, Ordering::SeqCst);
            return;
        }
    };

    if let Err(e) = write_bytes_atomic(&bak_path, &orig_vrsettings).and_then(|()| {
        write_json(
            &state_path,
            &json!({
                "active": true,
                "startedAt": now(),
                "settingsPath": plan.steamvr_settings,
                "backupPath": bak_path,
                "steamvrRuntimeDir": plan.steamvr_runtime_dir,
                "originalSha256": orig_sha256,
            }),
        )
    }) {
        let _ = fs::remove_file(&bak_path);
        if let Ok(mut s) = state.lock() {
            s.active = false;
            s.phase = "failed".to_owned();
            s.percent = 0;
            s.permille = 0;
            s.message = format!("Cannot create SteamVR safety backup: {e}");
        }
        active.store(false, Ordering::SeqCst);
        return;
    }

    if let Err(e) = write_bytes_atomic(&plan.steamvr_settings, &patched_vrsettings) {
        let _ = restore_steamvr_backup(
            &plan.steamvr_settings,
            &bak_path,
            &state_path,
            Some(&orig_sha256),
            "Rolled back after patch write error",
        );
        if let Ok(mut s) = state.lock() {
            s.active = false;
            s.phase = "failed".to_owned();
            s.percent = 0;
            s.permille = 0;
            s.message = format!("Cannot activate headless SteamVR config: {e}");
        }
        active.store(false, Ordering::SeqCst);
        return;
    }

    // Everything from here onward MUST run teardown + restore_steamvr_backup!
    let outcome: Result<WorkerOutcome> = (|| {
        if cancel.load(Ordering::SeqCst) {
            return Ok(WorkerOutcome::Cancelled);
        }

        // Step 3: Start headless SteamVR (vrstartup.exe) so OpenXR / OCU connects without headset prompts.
        set_state(
            "starting-steamvr",
            0,
            plan.baseline_total,
            2,
            20,
            None,
            0,
            None,
            "Starting headless SteamVR (null driver, 1024x1024 — headset stays off)…".to_owned(),
        );
        if let Some(rt_dir) = &plan.steamvr_runtime_dir {
            let vrstartup = rt_dir.join(r"bin\win64\vrstartup.exe");
            if vrstartup.is_file() {
                let _ = Command::new(&vrstartup)
                    .current_dir(rt_dir)
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .creation_flags(CREATE_NO_WINDOW)
                    .spawn();
                let wait_start = Instant::now();
                while wait_start.elapsed() < Duration::from_secs(12) {
                    if cancel.load(Ordering::SeqCst) {
                        return Ok(WorkerOutcome::Cancelled);
                    }
                    if platform::is_running("vrserver.exe") {
                        thread::sleep(Duration::from_millis(1200));
                        break;
                    }
                    thread::sleep(Duration::from_millis(250));
                }
            }
        }

        if cancel.load(Ordering::SeqCst) {
            return Ok(WorkerOutcome::Cancelled);
        }

        // Step 4: Launch ModOrganizer.exe with moshortcut://:<shortcut>.
        set_state(
            "launching-mo2",
            0,
            plan.baseline_total,
            3,
            30,
            None,
            0,
            None,
            format!(
                "Launching Skyrim VR via Mod Organizer 2 ({})…",
                plan.shortcut
            ),
        );
        Command::new(&plan.mo2_exe)
            .arg(format!("moshortcut://:{}", plan.shortcut))
            .current_dir(&plan.shader_cfg.mo2_root)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|e| format!("Failed to start ModOrganizer.exe: {e}"))?;

        // Wait for SkyrimVR.exe to spawn (MO2 RootBuilder + USVFS + SKSEVR).
        let spawn_wait = Instant::now();
        let mut saw_mo2 = false;
        let mut skyrim_spawned = false;
        while spawn_wait.elapsed() < MO2_SKYRIM_SPAWN_TIMEOUT {
            if cancel.load(Ordering::SeqCst) {
                return Ok(WorkerOutcome::Cancelled);
            }
            let procs = platform::processes();
            if procs.iter().any(|(_, n)| n.eq_ignore_ascii_case("SkyrimVR.exe")) {
                skyrim_spawned = true;
                break;
            }
            let mo2_alive = procs.iter().any(|(_, n)| {
                n.eq_ignore_ascii_case("ModOrganizer.exe")
                    || n.eq_ignore_ascii_case("ModOrganizer_original.exe")
                    || n.eq_ignore_ascii_case("sksevr_loader.exe")
            });
            if mo2_alive {
                saw_mo2 = true;
            } else if saw_mo2 && spawn_wait.elapsed() >= Duration::from_secs(6) {
                return Err(
                    "Mod Organizer 2 exited before SkyrimVR.exe launched. Check MO2 shortcut or USVFS."
                        .into(),
                );
            }
            set_state(
                "launching-mo2",
                0,
                plan.baseline_total,
                4,
                40,
                None,
                0,
                None,
                format!(
                    "MO2 preparing RootBuilder & USVFS for '{}' ({} elapsed)…",
                    plan.shortcut,
                    format_mm_ss(run_started.elapsed().as_secs())
                ),
            );
            thread::sleep(Duration::from_millis(500));
        }
        if !skyrim_spawned {
            return Err("Timed out waiting for SkyrimVR.exe to start via Mod Organizer 2.".into());
        }

        // Step 5: Monitor CommunityShaders.log + live ShaderCache blobs until compilation completes and saves.
        let cache_dirs = live_cache_dirs(&plan.shader_cfg);
        let mut tracker = CsLogTracker::default();
        let mut compile_started_at: Option<Instant> = None;
        let mut last_activity = Instant::now();
        let mut prev_blobs = 0usize;

        while run_started.elapsed() < MAX_PRECOMPILE_DURATION {
            if cancel.load(Ordering::SeqCst) {
                return Ok(WorkerOutcome::Cancelled);
            }

            for line in log_reader.poll_lines() {
                if tracker.observe_line(&line) {
                    last_activity = Instant::now();
                }
            }

            let live = inspect_live_cache_dirs(&cache_dirs);
            if live.compiled_blobs != prev_blobs {
                prev_blobs = live.compiled_blobs;
                last_activity = Instant::now();
            }

            if compile_started_at.is_none()
                && (live.compiled_blobs > 0 || tracker.batches_started > 0)
            {
                compile_started_at = Some(Instant::now());
            }

            let mut compiled = live
                .compiled_blobs
                .max(live.manifest_entries.unwrap_or(0));
            if tracker.boot_batch_done {
                compiled = compiled.max(tracker.completed_from_log);
            }
            let total = if tracker.queued_total >= 100 {
                tracker.queued_total.max(compiled)
            } else {
                plan.baseline_total.max(compiled).max(1)
            };

            if tracker.is_ready_to_finish(&live, plan.clear_cache) {
                if last_activity.elapsed() >= CACHE_SETTLE_DURATION {
                    let final_shaders = live.manifest_entries.unwrap_or(compiled);
                    return Ok(WorkerOutcome::Completed {
                        final_shaders,
                        failed: tracker.failed_tasks,
                    });
                }
                set_state(
                    "saving-cache",
                    compiled,
                    total,
                    98,
                    985,
                    Some(0),
                    tracker.failed_tasks,
                    tracker.current_class.clone(),
                    format!(
                        "Disk cache saved ({compiled} / {total} shaders) — verifying final file flush…"
                    ),
                );
            } else if compile_started_at.is_some() {
                let (pct, permille, eta) =
                    compute_progress_numbers(compiled, total, compile_started_at);
                let eta_str = eta
                    .map(|s| format!(" · ETA ~{}", format_mm_ss(s)))
                    .unwrap_or_default();
                let class_str = tracker
                    .current_class
                    .as_deref()
                    .map(|c| format!(" · {c}"))
                    .unwrap_or_default();
                let held_warn = if tracker.disk_cache_held {
                    " [Disk cache HELD]"
                } else {
                    ""
                };
                set_state(
                    "compiling",
                    compiled,
                    total,
                    pct,
                    permille,
                    eta,
                    tracker.failed_tasks,
                    tracker.current_class.clone(),
                    format!(
                        "Compiling shaders: {compiled} / {total} ({pct}%) · Elapsed {}{eta_str}{class_str}{held_warn}",
                        format_mm_ss(run_started.elapsed().as_secs())
                    ),
                );
            } else {
                let sub = if tracker.hooked_load_shaders {
                    let cls = tracker
                        .current_class
                        .as_deref()
                        .map(|c| format!(" ({c})"))
                        .unwrap_or_default();
                    format!("Skyrim VR loading shader packages{cls}…")
                } else {
                    "Skyrim VR initializing SKSE & plugins…".to_owned()
                };
                set_state(
                    "loading-game",
                    0,
                    total,
                    5,
                    50,
                    None,
                    0,
                    tracker.current_class.clone(),
                    format!("{sub} · Elapsed {}", format_mm_ss(run_started.elapsed().as_secs())),
                );
            }

            if !platform::is_running("SkyrimVR.exe") {
                // Grace re-check in case SkyrimVR.exe exited right after flushing Manifest.json.
                thread::sleep(Duration::from_millis(1200));
                for line in log_reader.poll_lines() {
                    tracker.observe_line(&line);
                }
                let final_live = inspect_live_cache_dirs(&cache_dirs);
                if tracker.is_ready_to_finish(&final_live, plan.clear_cache) {
                    return Ok(WorkerOutcome::Completed {
                        final_shaders: final_live.manifest_entries.unwrap_or(final_live.compiled_blobs),
                        failed: tracker.failed_tasks,
                    });
                }
                return Err(format!(
                    "SkyrimVR.exe exited unexpectedly before shader cache save completed ({} blobs on disk).",
                    final_live.compiled_blobs
                ));
            }

            thread::sleep(Duration::from_millis(500));
        }

        Err("Headless shader pre-compile exceeded the 90-minute safety timeout.".into())
    })();

    // Step 6: Unconditional Teardown & Byte-Exact SteamVR Restoration.
    let completed_ok = matches!(outcome, Ok(WorkerOutcome::Completed { .. }));
    shutdown_skyrim_and_wait_for_mo2(plan.shader_cfg.game_data_dir.as_deref(), |msg| {
        if let Ok(mut s) = state.lock() {
            s.phase = "shutting-down".to_owned();
            if completed_ok {
                s.percent = 99;
                s.permille = 990;
            }
            s.elapsed_seconds = run_started.elapsed().as_secs();
            s.eta_seconds = None;
            s.message = msg.to_owned();
        }
    });

    if let Ok(mut s) = state.lock() {
        s.phase = "shutting-down".to_owned();
        s.message = "Stopping headless SteamVR and restoring PSVR2 SteamVR settings…".to_owned();
    }
    shutdown_headless_steamvr(plan.steamvr_runtime_dir.as_deref());

    let restore_res = restore_steamvr_backup(
        &plan.steamvr_settings,
        &bak_path,
        &state_path,
        Some(&orig_sha256),
        "Restored original PSVR2 SteamVR settings after headless pre-compile",
    );

    let total_elapsed = run_started.elapsed().as_secs();
    if let Ok(mut s) = state.lock() {
        s.active = false;
        s.elapsed_seconds = total_elapsed;
        s.eta_seconds = None;
        s.current_class = None;

        let restore_suffix = match &restore_res {
            Ok(()) => "PSVR2 SteamVR settings restored.",
            Err(re) => {
                s.phase = "failed".to_owned();
                s.message = format!("Pre-compile finished, but SteamVR restore reported: {re}");
                active.store(false, Ordering::SeqCst);
                return;
            }
        };

        match outcome {
            Ok(WorkerOutcome::Completed {
                final_shaders,
                failed,
            }) => {
                s.phase = "completed".to_owned();
                s.compiled_shaders = final_shaders;
                s.total_shaders = final_shaders;
                s.percent = 100;
                s.permille = 1000;
                s.failed_tasks = failed;
                let fail_note = if failed > 0 {
                    format!(" ({failed} failed)")
                } else {
                    String::new()
                };
                s.message = format!(
                    "Completed: {final_shaders} shaders compiled & saved in {}{fail_note} — {restore_suffix}",
                    format_mm_ss(total_elapsed)
                );
            }
            Ok(WorkerOutcome::Cancelled) => {
                s.phase = "cancelled".to_owned();
                s.percent = 0;
                s.permille = 0;
                s.message = format!(
                    "Pre-compile cancelled after {} — Skyrim VR closed & {restore_suffix}",
                    format_mm_ss(total_elapsed)
                );
            }
            Err(e) => {
                s.phase = "failed".to_owned();
                s.message = format!("Pre-compile stopped: {e} — {restore_suffix}");
            }
        }
    }

    active.store(false, Ordering::SeqCst);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(label: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "skyrim-precompile-{}-{}-{}",
            std::process::id(),
            now().to_bits(),
            label
        ));
        fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn parses_mo2_custom_executables_preferring_launch_mgo_do_not_unlock() {
        let ini = "\
[General]\r\n\
selected_profile=@ByteArray(PSVR2 - dlss5)\r\n\
\r\n\
[customExecutables]\r\n\
size=3\r\n\
1\\arguments=\r\n\
1\\binary=D:/SkyrimVR MGO/mods/BodySlide/BodySlide.exe\r\n\
1\\title=BodySlide\r\n\
2\\arguments=\r\n\
2\\binary=D:/SkyrimVR MGO/mods/Skyrim Script Extender for VR (SKSEVR)/root/sksevr_loader.exe\r\n\
2\\title=SKSEFallback\r\n\
3\\arguments=\r\n\
3\\binary=D:/SkyrimVR MGO/mods/Skyrim Script Extender for VR (SKSEVR)/root/sksevr_loader.exe\r\n\
3\\title=@ByteArray(Launch MGO - Do Not Unlock)\r\n";

        assert_eq!(
            parse_mo2_skse_shortcut(ini),
            Some("Launch MGO - Do Not Unlock".to_owned())
        );

        let fallback_ini = "\
[customExecutables]\n\
1\\binary=C:/SKSE/sksevr_loader.exe\n\
1\\title=Custom SKSEVR Launcher\n";
        assert_eq!(
            parse_mo2_skse_shortcut(fallback_ini),
            Some("Custom SKSEVR Launcher".to_owned())
        );
    }

    #[test]
    fn patches_steamvr_for_headless_null_driver_and_restores_exact_bytes() {
        let dir = temp_dir("vrsettings");
        let settings_path = dir.join("steamvr.vrsettings");
        let bak_path = dir.join("steamvr.vrsettings.precompile.bak");
        let state_path = dir.join("headless-precompile-state.json");

        let original = b"{\r\n   \"LastKnown\" : {\r\n      \"ActualHMDDriver\" : \"playstation_vr2\"\r\n   },\r\n   \"playstation_vr2_ex\" : {\r\n      \"useEnhancedHaptics\" : true,\r\n      \"useToolkitSync\" : true\r\n   },\r\n   \"steamvr\" : {\r\n      \"motionSmoothing\" : false,\r\n      \"supersampleScale\" : 1\r\n   }\r\n}\r\n";
        fs::write(&settings_path, original).unwrap();
        let orig_sha = format!("{:x}", Sha256::digest(original));

        write_bytes_atomic(&bak_path, original).unwrap();
        let patched = patch_steamvr_for_headless(original).unwrap();
        write_bytes_atomic(&settings_path, &patched).unwrap();

        let patched_json = read_json(&settings_path).unwrap();
        assert_eq!(patched_json["driver_null"]["enable"], true);
        assert_eq!(patched_json["driver_null"]["renderWidth"], 1024);
        assert_eq!(patched_json["driver_null"]["renderHeight"], 1024);
        assert_eq!(patched_json["driver_null"]["loadPriority"], 999);
        assert_eq!(patched_json["steamvr"]["forcedDriver"], "null");
        assert_eq!(patched_json["steamvr"]["activateMultipleDrivers"], true);
        assert_eq!(patched_json["steamvr"]["motionSmoothing"], false);
        assert_eq!(patched_json["playstation_vr2_ex"]["useEnhancedHaptics"], true);

        // Simulate SteamVR writing dirty state on exit, then verify restore brings back byte-identical original.
        fs::write(&settings_path, b"{\"steamvr\":{\"forcedDriver\":\"null\"},\"dirty\":true}").unwrap();
        restore_steamvr_backup(
            &settings_path,
            &bak_path,
            &state_path,
            Some(&orig_sha),
            "unit-test restore",
        )
        .unwrap();

        assert_eq!(fs::read(&settings_path).unwrap(), original);
        assert!(!bak_path.exists());
        let state_json = read_json(&state_path).unwrap();
        assert_eq!(state_json["active"], false);

        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn tracks_community_shaders_log_lifecycle_and_completion_gates() {
        let mut tracker = CsLogTracker::default();
        let empty_live = LiveCacheFiles::default();

        tracker.observe_line("[18:45:54.686] [4636 ] [I] Hooking BSShader::LoadShaders");
        tracker.observe_line("[18:47:26.530] [4636 ] [I] BSShader::LoadShaders called on Lighting - ps count 9780");
        assert!(tracker.hooked_load_shaders);
        assert_eq!(tracker.current_class.as_deref(), Some("Lighting"));
        assert!(!tracker.is_ready_to_finish(&empty_live, true));

        assert!(tracker.observe_line("[18:47:33.000] [4636 ] [I] Shader compilation started (3628 tasks queued)"));
        assert!(tracker.in_flight);
        assert_eq!(tracker.queued_total, 3628);

        let mid_live = LiveCacheFiles {
            compiled_blobs: 1800,
            manifest_entries: None,
            has_info_ini: true,
            has_temp_files: false,
        };
        // Even if Saved disk cache info logged early while compilation is still in flight, must NOT finish!
        tracker.observe_line("[18:48:38.068] [19908] [I] Saved disk cache info (plugin version: 2-10-1-0)");
        assert!(!tracker.is_ready_to_finish(&mid_live, true));

        assert!(tracker.observe_line("[19:09:12.100] [19908] [I] Shader compilation completed: 3628/3628 tasks (0 failed) in 00:21:39"));
        assert!(!tracker.in_flight);
        assert!(tracker.boot_batch_done);
        assert_eq!(tracker.completed_from_log, 3628);

        // Still not ready if Manifest.json has a .tmp write in progress
        let flushing_live = LiveCacheFiles {
            compiled_blobs: 3628,
            manifest_entries: Some(3628),
            has_info_ini: true,
            has_temp_files: true,
        };
        assert!(!tracker.is_ready_to_finish(&flushing_live, true));

        let flushed_live = LiveCacheFiles {
            compiled_blobs: 3628,
            manifest_entries: Some(3628),
            has_info_ini: true,
            has_temp_files: false,
        };
        assert!(tracker.is_ready_to_finish(&flushed_live, true));
    }

    #[test]
    fn fresh_log_reader_ignores_pre_launch_untouched_log_and_reads_new_session() {
        let dir = temp_dir("logreader");
        let log_path = dir.join("CommunityShaders.log");
        fs::write(
            &log_path,
            "[18:48:38.068] [19908] [I] Saved disk cache info (plugin version: 2-10-1-0)\r\n",
        )
        .unwrap();

        let mut reader = FreshLogReader::new(vec![log_path.clone()]);
        // Untouched pre-launch log must NOT be read!
        assert!(reader.poll_lines().is_empty());

        // Simulate SkyrimVR.exe truncating/rewriting CommunityShaders.log for the new run.
        fs::write(
            &log_path,
            "[12:50:01.100] [1111 ] [I] Hooking BSShader::LoadShaders\r\n[12:50:05.200] [1111 ] [I] Shader compilation started (3628 tasks queued)\r\n",
        )
        .unwrap();
        let lines = reader.poll_lines();
        assert_eq!(lines.len(), 2);
        assert!(lines[1].contains("3628 tasks queued"));

        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn computes_bounded_progress_percentage_and_permille() {
        let (pct0, pm0, eta0) = compute_progress_numbers(0, 3628, None);
        assert_eq!(pct0, 5);
        assert_eq!(pm0, 50);
        assert_eq!(eta0, None);

        let (pct_half, pm_half, _) = compute_progress_numbers(1814, 3628, None);
        assert_eq!(pm_half, 510);
        assert_eq!(pct_half, 51);

        let (pct_full, pm_full, _) = compute_progress_numbers(3628, 3628, None);
        assert_eq!(pm_full, 970);
        assert_eq!(pct_full, 97);
    }
}

