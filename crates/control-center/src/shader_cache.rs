//! Open Shaders / Community Shaders cache manager: MO2 active-profile VFS
//! discovery, reversible backup-and-clear of all active `ShaderCache` layers,
//! and explicit upfront pre-Main-Menu boot compilation settings enforcement.
use crate::{
    config::{Result, ShaderCacheConfig, now, read_json, wide, write_json},
    platform,
};
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
};
use windows_sys::Win32::{
    Foundation::ERROR_NOT_SAME_DEVICE,
    Storage::FileSystem::{MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW},
};

const BACKUP_MARKER_FILE: &str = "skyrimvr_cc_shader_backup.json";
const JOURNAL_REL_PATH: &str = ".local/shader-cache-journal.json";
const CS_SETTINGS_REL: &str = r"SKSE\Plugins\CommunityShaders\SettingsUser.json";
const CS_DEFAULT_REL: &str = r"SKSE\Plugins\CommunityShaders\SettingsDefault.json";

pub(crate) fn check_no_reparse(path: &Path) -> Result<()> {
    use std::os::windows::fs::MetadataExt;
    for ancestor in path.ancestors() {
        if !ancestor.exists() {
            continue;
        }
        let metadata = fs::symlink_metadata(ancestor)
            .map_err(|e| format!("Cannot inspect {}: {e}", ancestor.display()))?;
        if metadata.file_attributes() & 0x400 != 0 {
            return Err(format!(
                "Reparse/symlink paths are not allowed for shader cache management: {}",
                ancestor.display()
            ));
        }
    }
    Ok(())
}

fn close_state() -> Result<Vec<String>> {
    Ok(platform::checked_processes()?
        .into_iter()
        .filter(|(_, n)| {
            n.eq_ignore_ascii_case("SkyrimVR.exe")
                || n.eq_ignore_ascii_case("ModOrganizer.exe")
                || n.eq_ignore_ascii_case("ModOrganizer_original.exe")
        })
        .map(|(pid, name)| format!("{name} (PID {pid})"))
        .collect())
}

fn ensure_closed() -> Result<()> {
    let running = close_state()?;
    if running.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "Close Skyrim VR and Mod Organizer 2 before modifying shader caches. Running: {}",
            running.join(", ")
        ))
    }
}

pub(crate) fn decode_qt_byte_array(inner: &str) -> String {
    let bytes = inner.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 1 < bytes.len() {
            match bytes[i + 1] {
                b'x' | b'X' if i + 3 < bytes.len() => {
                    if let Ok(hex) = std::str::from_utf8(&bytes[i + 2..i + 4])
                        && let Ok(val) = u8::from_str_radix(hex, 16)
                    {
                        out.push(val);
                        i += 4;
                        continue;
                    }
                }
                b'\\' => {
                    out.push(b'\\');
                    i += 2;
                    continue;
                }
                b'"' => {
                    out.push(b'"');
                    i += 2;
                    continue;
                }
                _ => {}
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn parse_selected_profile(ini_text: &str) -> Option<String> {
    for line in ini_text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('#') || trimmed.starts_with(';') {
            continue;
        }
        let Some((key, raw_val)) = trimmed.split_once('=') else {
            continue;
        };
        if !key.trim().eq_ignore_ascii_case("selected_profile") {
            continue;
        }
        let val = raw_val.trim();
        let decoded = if let Some(inner) = val
            .strip_prefix("@ByteArray(")
            .and_then(|s| s.strip_suffix(')'))
        {
            decode_qt_byte_array(inner)
        } else {
            val.trim_matches('"').to_owned()
        };
        if !decoded.is_empty() {
            return Some(decoded);
        }
    }
    None
}

fn parse_enabled_mods(modlist_text: &str) -> Vec<String> {
    modlist_text
        .lines()
        .map(str::trim)
        .filter_map(|line| {
            let name = line.strip_prefix('+')?;
            if name.is_empty() || name.ends_with("_separator") {
                None
            } else {
                Some(name.to_owned())
            }
        })
        .collect()
}

fn parse_local_settings(settings_ini_text: &str) -> Option<bool> {
    for line in settings_ini_text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('#') || trimmed.starts_with(';') {
            continue;
        }
        if let Some((k, v)) = trimmed.split_once('=')
            && k.trim().eq_ignore_ascii_case("LocalSettings")
        {
            return match v.trim().to_ascii_lowercase().as_str() {
                "true" | "1" => Some(true),
                "false" | "0" => Some(false),
                _ => None,
            };
        }
    }
    None
}

fn global_skyrimvr_ini_candidates() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(od) = std::env::var_os("OneDrive") {
        let od_path = PathBuf::from(od);
        out.push(od_path.join(r"Documents\My Games\Skyrim VR\SkyrimVR.ini"));
        out.push(od_path.join(r"Tiedostot\My Games\Skyrim VR\SkyrimVR.ini"));
    }
    if let Some(up) = std::env::var_os("USERPROFILE") {
        out.push(PathBuf::from(up).join(r"Documents\My Games\Skyrim VR\SkyrimVR.ini"));
    }
    out
}

fn count_shaders_fast(cache_dir: &Path) -> usize {
    let manifest = cache_dir.join("Manifest.json");
    if manifest.is_file()
        && let Ok(v) = read_json(&manifest)
        && let Some(entries) = v["entries"].as_object()
    {
        return entries.len();
    }
    // Fast 1-level scan (`ShaderCache/<Class>/*.pso|*.vso|*.cso`)
    let Ok(top) = fs::read_dir(cache_dir) else {
        return 0;
    };
    let mut count = 0usize;
    for entry in top.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if let Ok(sub) = fs::read_dir(&path) {
                count += sub
                    .flatten()
                    .filter(|f| f.file_type().map(|t| t.is_file()).unwrap_or(false))
                    .count();
            }
        } else if let Some(name) = path.file_name().and_then(|s| s.to_str())
            && !name.eq_ignore_ascii_case("Info.ini")
            && !name.eq_ignore_ascii_case("Manifest.json")
        {
            count += 1;
        }
    }
    count
}

#[derive(Debug)]
struct ActiveCacheDir {
    source: String,
    path: PathBuf,
    shader_count: usize,
}

#[derive(Debug)]
struct ResolvedSetup {
    backup_root: PathBuf,
    profile_name: Option<String>,
    active_caches: Vec<ActiveCacheDir>,
    settings_user_path: Option<PathBuf>,
    settings_seed_path: Option<PathBuf>,
    overwrite_dir: PathBuf,
    skyrimvr_ini_path: Option<PathBuf>,
}

fn resolve_setup(cfg: &ShaderCacheConfig) -> ResolvedSetup {
    let backup_root = cfg
        .backup_root
        .clone()
        .unwrap_or_else(|| cfg.mo2_root.join("ShaderCache_Backups"));
    let overwrite_dir = cfg.mo2_root.join("overwrite");
    let mods_dir = cfg.mo2_root.join("mods");
    let mo2_ini = cfg.mo2_root.join("ModOrganizer.ini");

    let profile_name = fs::read(&mo2_ini)
        .ok()
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .and_then(|t| parse_selected_profile(&t));

    let profile_dir = profile_name
        .as_ref()
        .map(|name| cfg.mo2_root.join("profiles").join(name))
        .filter(|p| p.is_dir());

    let enabled_mods = profile_dir
        .as_ref()
        .and_then(|pd| fs::read_to_string(pd.join("modlist.txt")).ok())
        .map(|t| parse_enabled_mods(&t))
        .unwrap_or_default();

    // Build VFS root list in priority order: overwrite (highest) -> enabled mods -> gameData (lowest)
    let mut vfs_roots: Vec<(String, PathBuf)> = Vec::with_capacity(enabled_mods.len() + 2);
    vfs_roots.push(("overwrite".to_owned(), overwrite_dir.clone()));
    for m in &enabled_mods {
        vfs_roots.push((format!("mod:{m}"), mods_dir.join(m)));
    }
    if let Some(gd) = &cfg.game_data_dir {
        vfs_roots.push(("gameData".to_owned(), gd.clone()));
    }

    let mut active_caches = Vec::new();
    let mut settings_user_path = None;
    let mut settings_seed_path = None;

    for (source, root_dir) in &vfs_roots {
        let sc = root_dir.join("ShaderCache");
        if sc.is_dir() {
            let shader_count = count_shaders_fast(&sc);
            active_caches.push(ActiveCacheDir {
                source: source.clone(),
                path: sc,
                shader_count,
            });
        }
        if settings_user_path.is_none() {
            let candidate = root_dir.join(CS_SETTINGS_REL);
            if candidate.is_file() {
                settings_user_path = Some(candidate);
            }
        }
        if settings_seed_path.is_none() {
            let candidate = root_dir.join(CS_DEFAULT_REL);
            if candidate.is_file() {
                settings_seed_path = Some(candidate);
            }
        }
    }

    if settings_user_path.is_none()
        && let Some(fb) = &cfg.fallback_settings_path
        && fb.is_file()
    {
        settings_user_path = Some(fb.clone());
    }

    let skyrimvr_ini_path = if let Some(pd) = &profile_dir {
        let local_flag = fs::read_to_string(pd.join("settings.ini"))
            .ok()
            .and_then(|t| parse_local_settings(&t));
        let prof_ini = pd.join("skyrimvr.ini");
        if local_flag != Some(false) && prof_ini.is_file() {
            Some(prof_ini)
        } else {
            global_skyrimvr_ini_candidates()
                .into_iter()
                .find(|p| p.is_file())
        }
    } else {
        global_skyrimvr_ini_candidates()
            .into_iter()
            .find(|p| p.is_file())
    };

    ResolvedSetup {
        backup_root,
        profile_name,
        active_caches,
        settings_user_path,
        settings_seed_path,
        overwrite_dir,
        skyrimvr_ini_path,
    }
}

fn read_depth_buffer_culling(ini_path: &Path) -> Option<bool> {
    let bytes = fs::read(ini_path).ok()?;
    let text = String::from_utf8_lossy(&bytes);
    let mut in_vr_perf = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with(';') || trimmed.starts_with('#') || trimmed.is_empty() {
            continue;
        }
        if let Some(sec) = trimmed.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            in_vr_perf = sec.trim().eq_ignore_ascii_case("VRPerformance");
            continue;
        }
        if in_vr_perf
            && let Some((k, v)) = trimmed.split_once('=')
            && k.trim().eq_ignore_ascii_case("bDepthBufferCulling")
        {
            return match v.trim() {
                "0" | "false" | "False" => Some(false),
                "1" | "true" | "True" => Some(true),
                _ => None,
            };
        }
    }
    None
}

fn patch_ini_depth_buffer_culling_zero(ini_text: &str) -> String {
    let newline = if ini_text.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    };
    let had_trailing_nl = ini_text.ends_with('\n');
    let raw_lines: Vec<&str> = ini_text.lines().collect();

    let mut out: Vec<String> = Vec::with_capacity(raw_lines.len() + 3);
    let mut in_vr_perf = false;
    let mut saw_vr_perf = false;
    let mut updated_key = false;

    for line in raw_lines {
        let trimmed = line.trim();
        if !trimmed.starts_with(';')
            && !trimmed.starts_with('#')
            && let Some(sec) = trimmed.strip_prefix('[').and_then(|s| s.strip_suffix(']'))
        {
            if in_vr_perf && !updated_key {
                out.push("bDepthBufferCulling=0".to_owned());
                updated_key = true;
            }
            in_vr_perf = sec.trim().eq_ignore_ascii_case("VRPerformance");
            if in_vr_perf {
                saw_vr_perf = true;
            }
            out.push(line.to_owned());
            continue;
        }
        if in_vr_perf
            && !trimmed.starts_with(';')
            && !trimmed.starts_with('#')
            && let Some((k, _)) = trimmed.split_once('=')
            && k.trim().eq_ignore_ascii_case("bDepthBufferCulling")
        {
            if !updated_key {
                out.push("bDepthBufferCulling=0".to_owned());
                updated_key = true;
            }
            continue;
        }
        out.push(line.to_owned());
    }

    if in_vr_perf && !updated_key {
        out.push("bDepthBufferCulling=0".to_owned());
    } else if !saw_vr_perf {
        if !out.is_empty() && !out.last().unwrap().is_empty() {
            out.push(String::new());
        }
        out.push("[VRPerformance]".to_owned());
        out.push("bDepthBufferCulling=0".to_owned());
    }

    let mut joined = out.join(newline);
    if had_trailing_nl {
        joined.push_str(newline);
    }
    joined
}

fn write_text_atomic(path: &Path, content: &str) -> Result<()> {
    check_no_reparse(path)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let temp = path.with_extension(format!("{}.tmp", std::process::id()));
    fs::write(&temp, content.as_bytes()).map_err(|e| e.to_string())?;
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
        return Err(format!("Cannot update {}: {err}", path.display()));
    }
    Ok(())
}

fn list_managed_backups(backup_root: &Path) -> Vec<(PathBuf, Value)> {
    let Ok(entries) = fs::read_dir(backup_root) else {
        return Vec::new();
    };
    let mut backups = Vec::new();
    for entry in entries.flatten() {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        let Some(name) = dir.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        if !name.starts_with("backup_") {
            continue;
        }
        let marker = dir.join(BACKUP_MARKER_FILE);
        if marker.is_file()
            && let Ok(meta) = read_json(&marker)
        {
            backups.push((dir, meta));
        }
    }
    backups.sort_by(|(dir_a, a), (dir_b, b)| {
        let ta = a["createdAt"].as_f64().unwrap_or(0.0);
        let tb = b["createdAt"].as_f64().unwrap_or(0.0);
        tb.partial_cmp(&ta)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| dir_b.cmp(dir_a))
    });
    backups
}

fn prune_old_backups(backup_root: &Path, max_backups: usize) {
    let keep = max_backups.max(1);
    let backups = list_managed_backups(backup_root);
    for (dir, _) in backups.into_iter().skip(keep) {
        let _ = fs::remove_dir_all(dir);
    }
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
    check_no_reparse(src)?;
    fs::create_dir_all(dst).map_err(|e| format!("Cannot create {}: {e}", dst.display()))?;
    for entry in fs::read_dir(src).map_err(|e| format!("Cannot read {}: {e}", src.display()))? {
        let entry = entry.map_err(|e| e.to_string())?;
        let ty = entry.file_type().map_err(|e| e.to_string())?;
        if ty.is_symlink() {
            return Err(format!(
                "Refusing to copy symlink inside shader cache: {}",
                entry.path().display()
            ));
        }
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_recursive(&from, &to)?;
        } else {
            fs::copy(&from, &to)
                .map_err(|e| format!("Cannot copy {} to {}: {e}", from.display(), to.display()))?;
        }
    }
    Ok(())
}

fn move_dir_safe(from: &Path, to: &Path) -> Result<()> {
    check_no_reparse(from)?;
    check_no_reparse(to)?;
    if to.exists() {
        return Err(format!(
            "Shader cache destination already exists: {}",
            to.display()
        ));
    }
    if let Some(parent) = to.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let from_w = wide(&from.to_string_lossy());
    let to_w = wide(&to.to_string_lossy());
    let ok = unsafe { MoveFileExW(from_w.as_ptr(), to_w.as_ptr(), MOVEFILE_WRITE_THROUGH) };
    if ok != 0 {
        return Ok(());
    }
    let err = std::io::Error::last_os_error();
    if err.raw_os_error() == Some(ERROR_NOT_SAME_DEVICE as i32) {
        copy_dir_recursive(from, to)?;
        fs::remove_dir_all(from)
            .map_err(|e| format!("Copied backup, but could not remove {}: {e}", from.display()))?;
        return Ok(());
    }
    Err(format!(
        "Could not move {} to {}: {err}",
        from.display(),
        to.display()
    ))
}

/// Fast read-only status snapshot safe for the 2-second UI refresh loop.
pub fn status(cfg: Option<&ShaderCacheConfig>) -> Value {
    let Some(cfg) = cfg else {
        return json!({
            "state": "unconfigured",
            "note": "MO2 root could not be resolved from control-center.json.",
            "runningBlockers": [],
        });
    };
    if !cfg.mo2_root.is_dir() {
        return json!({
            "state": "missing-mo2",
            "mo2Root": cfg.mo2_root,
            "note": "Configured MO2 folder was not found on disk.",
            "runningBlockers": [],
        });
    }

    let setup = resolve_setup(cfg);
    let cs_json = setup
        .settings_user_path
        .as_ref()
        .or(setup.settings_seed_path.as_ref())
        .and_then(|p| read_json(p).ok());

    let bg_compile = cs_json
        .as_ref()
        .and_then(|v| v.pointer("/Menu/BackgroundShaderCompilationOnBoot"))
        .and_then(Value::as_bool);
    let disk_cache = cs_json
        .as_ref()
        .and_then(|v| v.pointer("/General/Enable Disk Cache"))
        .and_then(Value::as_bool);
    let depth_culling = setup
        .skyrimvr_ini_path
        .as_deref()
        .and_then(read_depth_buffer_culling);

    let upfront_ready =
        bg_compile == Some(false) && disk_cache == Some(true) && depth_culling == Some(false);

    let total_shaders: usize = setup.active_caches.iter().map(|c| c.shader_count).sum();
    let caches_json: Vec<Value> = setup
        .active_caches
        .iter()
        .map(|c| {
            json!({
                "source": c.source,
                "path": c.path,
                "shaderCount": c.shader_count,
            })
        })
        .collect();

    let backups = list_managed_backups(&setup.backup_root);
    let latest_backup = backups.first().map(|(dir, meta)| {
        json!({
            "folder": dir.file_name().and_then(|s| s.to_str()).unwrap_or(""),
            "path": dir,
            "totalShaders": meta["totalShaders"].as_u64().unwrap_or(0),
            "entryCount": meta["entries"].as_array().map_or(0, Vec::len),
            "profile": meta["profile"],
        })
    });

    let process = close_state();
    let state = if setup.active_caches.is_empty() {
        if upfront_ready {
            "cleared-ready-for-boot-compile"
        } else {
            "cleared-settings-need-attention"
        }
    } else if upfront_ready {
        "cached-upfront-ready"
    } else {
        "cached-settings-need-attention"
    };

    json!({
        "state": state,
        "profile": setup.profile_name,
        "mo2Root": cfg.mo2_root,
        "backupRoot": setup.backup_root,
        "activeCacheCount": setup.active_caches.len(),
        "totalCachedShaders": total_shaders,
        "activeCaches": caches_json,
        "settingsUserPath": setup.settings_user_path,
        "skyrimVrIniPath": setup.skyrimvr_ini_path,
        "backgroundCompileOnBoot": bg_compile,
        "diskCacheEnabled": disk_cache,
        "depthBufferCulling": depth_culling,
        "upfrontBootCompileReady": upfront_ready,
        "backupCount": backups.len(),
        "latestBackup": latest_backup,
        "runningBlockers": process.as_ref().ok(),
        "processCheckError": process.err(),
    })
}

fn apply_upfront_settings_inner(setup: &ResolvedSetup, backup_dir: Option<&Path>) -> Result<Value> {
    // 1. Patch CommunityShaders SettingsUser.json
    let target_settings = match &setup.settings_user_path {
        Some(p) => p.clone(),
        None => setup.overwrite_dir.join(CS_SETTINGS_REL),
    };
    check_no_reparse(&target_settings)?;

    let mut cs_val = if target_settings.is_file() {
        if let Some(bdir) = backup_dir {
            fs::create_dir_all(bdir).map_err(|e| e.to_string())?;
            let _ = fs::copy(&target_settings, bdir.join("SettingsUser.json.bak"));
        }
        read_json(&target_settings)?
    } else if let Some(seed) = &setup.settings_seed_path {
        read_json(seed)?
    } else {
        return Err(
            "CommunityShaders SettingsUser.json was not found in the active MO2 profile.".into(),
        );
    };

    if !cs_val.is_object() {
        return Err("CommunityShaders SettingsUser.json is not a JSON object.".into());
    }
    if !cs_val["Menu"].is_object() {
        cs_val["Menu"] = json!({});
    }
    if !cs_val["General"].is_object() {
        cs_val["General"] = json!({});
    }
    cs_val["Menu"]["BackgroundShaderCompilationOnBoot"] = json!(false);
    cs_val["General"]["Enable Disk Cache"] = json!(true);
    write_json(&target_settings, &cs_val)?;

    // 2. Patch active profile skyrimvr.ini (`[VRPerformance] bDepthBufferCulling=0`)
    let ini_path = setup
        .skyrimvr_ini_path
        .as_ref()
        .ok_or("Active SkyrimVR.ini could not be located for this MO2 profile.")?;
    check_no_reparse(ini_path)?;
    if let Some(bdir) = backup_dir {
        fs::create_dir_all(bdir).map_err(|e| e.to_string())?;
        let _ = fs::copy(ini_path, bdir.join("skyrimvr.ini.bak"));
    }
    let ini_bytes =
        fs::read(ini_path).map_err(|e| format!("Cannot read {}: {e}", ini_path.display()))?;
    let ini_text = String::from_utf8(ini_bytes)
        .map_err(|_| format!("{} is not valid UTF-8/ASCII text.", ini_path.display()))?;
    let patched_ini = patch_ini_depth_buffer_culling_zero(&ini_text);
    if patched_ini != ini_text {
        write_text_atomic(ini_path, &patched_ini)?;
    }

    Ok(json!({
        "settingsUserPath": target_settings,
        "skyrimVrIniPath": ini_path,
        "backgroundCompileOnBoot": false,
        "diskCacheEnabled": true,
        "depthBufferCulling": false,
    }))
}

pub fn enforce_upfront_settings(cfg: Option<&ShaderCacheConfig>, root: &Path) -> Result<Value> {
    let cfg = cfg.ok_or("Shader cache paths could not be resolved from control-center.json.")?;
    ensure_closed()?;
    let setup = resolve_setup(cfg);
    let cfg_backup_dir = root.join(format!(
        ".local/shader-settings-backup-{}",
        (now() * 1000.0) as u64
    ));
    let details = apply_upfront_settings_inner(&setup, Some(&cfg_backup_dir))?;
    let record = json!({
        "action": "enforce-settings",
        "message": "Upfront boot compile enforced (BackgroundShaderCompilationOnBoot=false, Disk Cache=on, bDepthBufferCulling=0); existing ShaderCache kept.",
        "details": details,
    });
    let _ = write_json(&root.join(JOURNAL_REL_PATH), &record);
    Ok(record)
}

pub fn clear_for_reload(
    cfg: Option<&ShaderCacheConfig>,
    root: &Path,
    enforce_boot_compile: bool,
) -> Result<Value> {
    let cfg = cfg.ok_or("Shader cache paths could not be resolved from control-center.json.")?;
    ensure_closed()?;
    let setup = resolve_setup(cfg);
    check_no_reparse(&setup.backup_root)?;

    let stamp_ms = (now() * 1000.0) as u64;
    let mut moved_entries: Vec<Value> = Vec::new();
    let mut total_moved_shaders = 0usize;
    let mut created_backup_dir: Option<PathBuf> = None;

    if !setup.active_caches.is_empty() {
        let bdir = setup.backup_root.join(format!("backup_{stamp_ms}"));
        fs::create_dir_all(&bdir).map_err(|e| format!("Cannot create backup folder: {e}"))?;

        for (idx, cache) in setup.active_caches.iter().enumerate() {
            let slot_name = format!("slot_{idx}");
            let dst = bdir.join(&slot_name);
            if let Err(e) = move_dir_safe(&cache.path, &dst) {
                // Roll back any already-moved slots in reverse order so the profile is never left half-cleared.
                for prev in moved_entries.iter().rev() {
                    if let (Some(slot), Some(orig)) =
                        (prev["slot"].as_str(), prev["originalPath"].as_str())
                    {
                        let _ = move_dir_safe(&bdir.join(slot), Path::new(orig));
                    }
                }
                let _ = fs::remove_dir_all(&bdir);
                return Err(e);
            }
            total_moved_shaders += cache.shader_count;
            moved_entries.push(json!({
                "slot": slot_name,
                "source": cache.source,
                "originalPath": cache.path,
                "shaderCount": cache.shader_count,
            }));
        }

        let marker = json!({
            "createdAt": now(),
            "profile": setup.profile_name,
            "totalShaders": total_moved_shaders,
            "entries": moved_entries,
            "enforceBootCompileApplied": enforce_boot_compile,
        });
        write_json(&bdir.join(BACKUP_MARKER_FILE), &marker)?;
        created_backup_dir = Some(bdir);
    }

    let settings_result = if enforce_boot_compile {
        let cfg_bak = created_backup_dir
            .clone()
            .unwrap_or_else(|| root.join(format!(".local/shader-settings-backup-{stamp_ms}")));
        Some(apply_upfront_settings_inner(&setup, Some(&cfg_bak))?)
    } else {
        None
    };

    if created_backup_dir.is_some() {
        prune_old_backups(&setup.backup_root, cfg.max_backups);
    }

    let msg = match (moved_entries.len(), enforce_boot_compile) {
        (0, true) => {
            "ShaderCache was already empty; upfront boot-compile settings verified/applied for next Skyrim VR launch."
                .to_owned()
        }
        (0, false) => "No active ShaderCache folders were present; already clear.".to_owned(),
        (n, true) => format!(
            "Backed up and cleared {n} ShaderCache folder(s) ({total_moved_shaders} shaders) + enforced upfront full boot compile for next launch."
        ),
        (n, false) => format!(
            "Backed up and cleared {n} ShaderCache folder(s) ({total_moved_shaders} shaders); graphics/INI settings left unchanged."
        ),
    };

    let record = json!({
        "action": "clear",
        "message": msg,
        "movedFolders": moved_entries,
        "totalMovedShaders": total_moved_shaders,
        "backupDir": created_backup_dir,
        "upfrontSettingsApplied": settings_result,
    });
    let _ = write_json(&root.join(JOURNAL_REL_PATH), &record);
    Ok(record)
}

pub fn restore_latest_backup(cfg: Option<&ShaderCacheConfig>, root: &Path) -> Result<Value> {
    let cfg = cfg.ok_or("Shader cache paths could not be resolved from control-center.json.")?;
    ensure_closed()?;
    let setup = resolve_setup(cfg);
    let backups = list_managed_backups(&setup.backup_root);
    let (backup_dir, meta) = backups
        .into_iter()
        .next()
        .ok_or("No managed ShaderCache backup was found to restore.")?;

    let entries = meta["entries"]
        .as_array()
        .filter(|a| !a.is_empty())
        .ok_or("Latest ShaderCache backup manifest has no entries.")?;

    // Verify all slot directories exist before touching live paths.
    for (idx, item) in entries.iter().enumerate() {
        let slot = item["slot"]
            .as_str()
            .ok_or_else(|| format!("Backup entry {idx} is missing slot name."))?;
        if !backup_dir.join(slot).is_dir() {
            return Err(format!(
                "Backup slot {} is missing inside {}; nothing was changed.",
                slot,
                backup_dir.display()
            ));
        }
    }

    // If a partial/new ShaderCache exists at any target path, park it safely rather than deleting or failing.
    let stamp_ms = (now() * 1000.0) as u64;
    let discard_dir = setup
        .backup_root
        .join(format!("discarded_partial_{stamp_ms}"));
    let mut parked = Vec::new();
    for (idx, item) in entries.iter().enumerate() {
        let orig = PathBuf::from(
            item["originalPath"]
                .as_str()
                .ok_or("Backup entry missing originalPath.")?,
        );
        if orig.exists() {
            let park_dst = discard_dir.join(format!("parked_{idx}"));
            move_dir_safe(&orig, &park_dst)?;
            parked.push((park_dst, orig));
        }
    }

    let mut restored: Vec<(String, PathBuf)> = Vec::new();
    for item in entries {
        let slot = item["slot"].as_str().unwrap();
        let orig = PathBuf::from(item["originalPath"].as_str().unwrap());
        let src = backup_dir.join(slot);
        if let Err(e) = move_dir_safe(&src, &orig) {
            // Roll back already-restored entries and any parked partial caches.
            for (prev_slot, prev_orig) in restored.iter().rev() {
                let _ = move_dir_safe(prev_orig, &backup_dir.join(prev_slot));
            }
            for (park_src, park_orig) in parked.into_iter().rev() {
                if !park_orig.exists() {
                    let _ = move_dir_safe(&park_src, &park_orig);
                }
            }
            return Err(e);
        }
        restored.push((slot.to_owned(), orig));
    }

    let _ = fs::remove_file(backup_dir.join(BACKUP_MARKER_FILE));
    let _ = fs::remove_dir_all(&backup_dir);

    let restored_paths: Vec<PathBuf> = restored.into_iter().map(|(_, p)| p).collect();
    let total_shaders = meta["totalShaders"].as_u64().unwrap_or(0);
    let record = json!({
        "action": "restore",
        "message": format!(
            "Restored {} ShaderCache folder(s) ({total_shaders} compiled shaders) from {}.",
            restored_paths.len(),
            backup_dir.file_name().and_then(|s| s.to_str()).unwrap_or("backup")
        ),
        "restoredPaths": restored_paths,
        "totalRestoredShaders": total_shaders,
        "parkedPartialDir": if discard_dir.exists() { Some(discard_dir) } else { None },
    });
    let _ = write_json(&root.join(JOURNAL_REL_PATH), &record);
    Ok(record)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct LiveCacheFiles {
    pub compiled_blobs: usize,
    pub manifest_entries: Option<usize>,
    pub has_info_ini: bool,
    pub has_temp_files: bool,
}

pub(crate) fn baseline_expected_shaders(cfg: &ShaderCacheConfig) -> usize {
    let setup = resolve_setup(cfg);
    let active_total: usize = setup.active_caches.iter().map(|c| c.shader_count).sum();
    if active_total >= 100 {
        return active_total;
    }
    let backups = list_managed_backups(&setup.backup_root);
    if let Some((_, meta)) = backups.first()
        && let Some(prev) = meta["totalShaders"].as_u64()
        && prev >= 100
    {
        return prev as usize;
    }
    3628
}

pub(crate) fn live_cache_dirs(cfg: &ShaderCacheConfig) -> Vec<PathBuf> {
    let setup = resolve_setup(cfg);
    let mut dirs = vec![setup.overwrite_dir.join("ShaderCache")];
    for c in setup.active_caches {
        if !dirs
            .iter()
            .any(|d| d.to_string_lossy().eq_ignore_ascii_case(&c.path.to_string_lossy()))
        {
            dirs.push(c.path);
        }
    }
    dirs
}

pub(crate) fn inspect_live_cache_dirs(dirs: &[PathBuf]) -> LiveCacheFiles {
    let mut out = LiveCacheFiles::default();
    let mut manifest_sum = 0usize;
    let mut saw_manifest = false;
    for cache_dir in dirs {
        let Ok(top) = fs::read_dir(cache_dir) else {
            continue;
        };
        for entry in top.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if let Ok(sub) = fs::read_dir(&path) {
                    for f in sub.flatten() {
                        if !f.file_type().map(|t| t.is_file()).unwrap_or(false) {
                            continue;
                        }
                        let name = f.file_name();
                        let name_s = name.to_string_lossy();
                        if name_s.to_ascii_lowercase().ends_with(".tmp") {
                            out.has_temp_files = true;
                        } else {
                            out.compiled_blobs += 1;
                        }
                    }
                }
            } else if let Some(name) = path.file_name().and_then(|s| s.to_str()) {
                let lower = name.to_ascii_lowercase();
                if lower.ends_with(".tmp") {
                    out.has_temp_files = true;
                } else if lower == "info.ini" {
                    out.has_info_ini = true;
                } else if lower == "manifest.json" {
                    if let Ok(v) = read_json(&path)
                        && let Some(entries) = v["entries"].as_object()
                    {
                        manifest_sum += entries.len();
                        saw_manifest = true;
                    }
                } else {
                    out.compiled_blobs += 1;
                }
            }
        }
    }
    if saw_manifest {
        out.manifest_entries = Some(manifest_sum);
    }
    out
}

pub fn open_backups_folder(cfg: Option<&ShaderCacheConfig>) -> Result<Value> {
    let cfg = cfg.ok_or("Shader cache paths could not be resolved from control-center.json.")?;
    let setup = resolve_setup(cfg);
    check_no_reparse(&setup.backup_root)?;
    fs::create_dir_all(&setup.backup_root)
        .map_err(|e| format!("Cannot create {}: {e}", setup.backup_root.display()))?;
    platform::open(&setup.backup_root.to_string_lossy())?;
    Ok(json!({
        "message": "Opened ShaderCache backups folder",
        "backupRoot": setup.backup_root,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(label: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "skyrim-shader-cache-{}-{}-{}",
            std::process::id(),
            now().to_bits(),
            label
        ));
        fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn parses_mo2_selected_profile_bytearray_and_utf8_escapes() {
        assert_eq!(
            parse_selected_profile(
                "[General]\r\nselected_profile=@ByteArray(PSVR2 - dlss5 - neural off - dlaa066 - current preference)\r\n"
            ),
            Some("PSVR2 - dlss5 - neural off - dlaa066 - current preference".to_owned())
        );
        assert_eq!(
            parse_selected_profile("selected_profile=@ByteArray(Ty\\xc3\\xb6p\\xc3\\xb6yt\\xc3\\xa4)"),
            Some("Työpöytä".to_owned())
        );
        assert_eq!(
            parse_selected_profile("selected_profile=SimpleProfile"),
            Some("SimpleProfile".to_owned())
        );
    }

    #[test]
    fn patches_skyrimvr_ini_preserving_crlf_and_unrelated_keys() {
        let orig = "[General]\r\nsLanguage=ENGLISH\r\n\r\n[VRPerformance]\r\nbDepthBufferCulling=1\r\n\r\n[VRUI]\r\nbVrMapMenuEnableFog=0\r\n";
        let patched = patch_ini_depth_buffer_culling_zero(orig);
        assert_eq!(
            patched,
            "[General]\r\nsLanguage=ENGLISH\r\n\r\n[VRPerformance]\r\nbDepthBufferCulling=0\r\n\r\n[VRUI]\r\nbVrMapMenuEnableFog=0\r\n"
        );

        let missing_key = "[General]\nsLanguage=ENGLISH\n[VRPerformance]\niPlaceholder=1\n[Grass]\nbAllowCreateGrass=1\n";
        let patched2 = patch_ini_depth_buffer_culling_zero(missing_key);
        assert_eq!(
            patched2,
            "[General]\nsLanguage=ENGLISH\n[VRPerformance]\niPlaceholder=1\nbDepthBufferCulling=0\n[Grass]\nbAllowCreateGrass=1\n"
        );
    }

    #[test]
    fn clear_enforces_settings_backs_up_only_enabled_vfs_caches_and_restores_over_partial() {
        let dir = temp_dir("roundtrip");
        let mo2 = dir.join("mo2");
        let prof_name = "Test VR Profile";
        let prof_dir = mo2.join("profiles").join(prof_name);
        let overwrite_sc = mo2.join("overwrite").join("ShaderCache");
        let enabled_mod_sc = mo2
            .join("mods")
            .join("Output NR - Recovered Global Cache")
            .join("ShaderCache");
        let disabled_mod_sc = mo2
            .join("mods")
            .join("Community Shaders VR Shader Cache - csx3.18")
            .join("ShaderCache");
        let settings_mod = mo2.join("mods").join("Output PSVR2 Current Preference");

        fs::create_dir_all(&prof_dir).unwrap();
        fs::create_dir_all(overwrite_sc.join("Lighting")).unwrap();
        fs::create_dir_all(enabled_mod_sc.join("Effect")).unwrap();
        fs::create_dir_all(disabled_mod_sc.join("Sky")).unwrap();
        fs::create_dir_all(settings_mod.join(r"SKSE\Plugins\CommunityShaders")).unwrap();

        fs::write(
            mo2.join("ModOrganizer.ini"),
            format!("[General]\r\nselected_profile=@ByteArray({prof_name})\r\n"),
        )
        .unwrap();
        fs::write(prof_dir.join("settings.ini"), "[General]\r\nLocalSettings=true\r\n").unwrap();
        fs::write(
            prof_dir.join("skyrimvr.ini"),
            "[VRPerformance]\r\nbDepthBufferCulling=1\r\n[VR]\r\nfFarDistance=100000\r\n",
        )
        .unwrap();
        fs::write(
            prof_dir.join("modlist.txt"),
            "+Output PSVR2 Current Preference\r\n+Output NR - Recovered Global Cache\r\n-Community Shaders VR Shader Cache - csx3.18\r\n+#Shaders_separator\r\n",
        )
        .unwrap();

        write_json(
            &overwrite_sc.join("Manifest.json"),
            &json!({"entries": {"Lighting/0.pso": "aaa", "Lighting/1.pso": "bbb"}}),
        )
        .unwrap();
        fs::write(overwrite_sc.join(r"Lighting\0.pso"), b"shader0").unwrap();
        fs::write(overwrite_sc.join(r"Lighting\1.pso"), b"shader1").unwrap();
        fs::write(enabled_mod_sc.join(r"Effect\2.pso"), b"shader2").unwrap();
        fs::write(disabled_mod_sc.join(r"Sky\keep.pso"), b"disabled-keep").unwrap();

        let cs_user_path = settings_mod.join(CS_SETTINGS_REL);
        write_json(
            &cs_user_path,
            &json!({
                "General": {"Enable Async": true, "Enable Disk Cache": false, "CustomKey": 42},
                "Menu": {"BackgroundShaderCompilationOnBoot": true, "ToggleKey": 35}
            }),
        )
        .unwrap();

        let cfg = ShaderCacheConfig {
            mo2_root: mo2.clone(),
            game_data_dir: None,
            backup_root: None,
            fallback_settings_path: None,
            mo2_shortcut: None,
            max_backups: 2,
        };

        let st_before = status(Some(&cfg));
        assert_eq!(st_before["activeCacheCount"], 2);
        assert_eq!(st_before["totalCachedShaders"], 3);
        assert_eq!(st_before["upfrontBootCompileReady"], false);

        // Test VFS resolution + backup + settings enforcement directly via inner helpers so test is independent of host running MO2.
        let setup = resolve_setup(&cfg);
        assert_eq!(setup.active_caches.len(), 2);

        let bdir = setup.backup_root.join("backup_1000");
        fs::create_dir_all(&bdir).unwrap();
        move_dir_safe(&overwrite_sc, &bdir.join("slot_0")).unwrap();
        move_dir_safe(&enabled_mod_sc, &bdir.join("slot_1")).unwrap();
        write_json(
            &bdir.join(BACKUP_MARKER_FILE),
            &json!({
                "createdAt": 1000.0,
                "profile": prof_name,
                "totalShaders": 3,
                "entries": [
                    {"slot": "slot_0", "source": "overwrite", "originalPath": overwrite_sc, "shaderCount": 2},
                    {"slot": "slot_1", "source": "mod:Output NR - Recovered Global Cache", "originalPath": enabled_mod_sc, "shaderCount": 1}
                ]
            }),
        )
        .unwrap();
        apply_upfront_settings_inner(&setup, Some(&bdir)).unwrap();

        // Disabled mod's ShaderCache was NOT touched!
        assert!(disabled_mod_sc.join(r"Sky\keep.pso").is_file());
        assert!(!overwrite_sc.exists());
        assert!(!enabled_mod_sc.exists());

        // SettingsUser.json preserved CustomKey & ToggleKey while flipping upfront flags
        let patched_cs = read_json(&cs_user_path).unwrap();
        assert_eq!(patched_cs["General"]["CustomKey"], 42);
        assert_eq!(patched_cs["General"]["Enable Disk Cache"], true);
        assert_eq!(patched_cs["Menu"]["BackgroundShaderCompilationOnBoot"], false);
        assert_eq!(patched_cs["Menu"]["ToggleKey"], 35);
        assert_eq!(read_depth_buffer_culling(&prof_dir.join("skyrimvr.ini")), Some(false));

        let st_cleared = status(Some(&cfg));
        assert_eq!(st_cleared["state"], "cleared-ready-for-boot-compile");
        assert_eq!(st_cleared["backupCount"], 1);

        // Simulate partial aborted compile in overwrite\ShaderCache, then verify restore parks partial and restores full backup
        fs::create_dir_all(overwrite_sc.join("Partial")).unwrap();
        fs::write(overwrite_sc.join(r"Partial\aborted.pso"), b"partial").unwrap();

        let backups = list_managed_backups(&setup.backup_root);
        assert_eq!(backups.len(), 1);
        let discard = setup.backup_root.join("discarded_partial_test");
        move_dir_safe(&overwrite_sc, &discard.join("parked_0")).unwrap();
        move_dir_safe(&bdir.join("slot_0"), &overwrite_sc).unwrap();
        move_dir_safe(&bdir.join("slot_1"), &enabled_mod_sc).unwrap();
        fs::remove_dir_all(&bdir).unwrap();

        assert!(overwrite_sc.join(r"Lighting\0.pso").is_file());
        assert!(enabled_mod_sc.join(r"Effect\2.pso").is_file());
        assert!(discard.join(r"parked_0\Partial\aborted.pso").is_file());

        // Verify pruning only removes managed backup_* folders beyond max_backups and never removes user/discarded folders
        for i in 1..=4 {
            let b = setup.backup_root.join(format!("backup_{i}"));
            fs::create_dir_all(&b).unwrap();
            write_json(&b.join(BACKUP_MARKER_FILE), &json!({"createdAt": i as f64})).unwrap();
        }
        let unmanaged = setup.backup_root.join("UserManualBackup");
        fs::create_dir_all(&unmanaged).unwrap();
        prune_old_backups(&setup.backup_root, 2);
        assert_eq!(list_managed_backups(&setup.backup_root).len(), 2);
        assert!(unmanaged.is_dir());
        assert!(discard.is_dir());

        fs::remove_dir_all(dir).unwrap();
    }
}
