//! Reversible NR-only file guard. No graphics configuration or cache mutations.
use crate::config::{NrGuardConfig, Result, read_json, wide, write_json};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File},
    io::Read,
    path::{Path, PathBuf},
};
use windows_sys::Win32::Storage::FileSystem::{MOVEFILE_WRITE_THROUGH, MoveFileExW};

const EXPECTED_DLL_NAME: &str = "nvngx_dlssnr.dll";
const EXPECTED_BYTES: u64 = 165_840_496;
const EXPECTED_SHA256: &str = "e16bcf15e16e13f527491cdf7845b2fe6521a738d8f7c9c721866a8496e1fc8e";
const SCOPE: &str = "Shared shader mod: NR is unavailable in every profile using it until restored. DLSS/DLAA and all settings are unchanged.";

fn validated_paths(cfg: &NrGuardConfig) -> Result<(PathBuf, PathBuf)> {
    let name = cfg
        .dll_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");
    if name != EXPECTED_DLL_NAME {
        return Err("NR guard dllPath must target nvngx_dlssnr.dll exactly.".into());
    }
    let hidden = cfg.dll_path.with_file_name("nvngx_dlssnr.dll.mohidden");
    Ok((cfg.dll_path.clone(), hidden))
}

fn nr_checkbox(cfg: &NrGuardConfig) -> Option<bool> {
    read_json(&cfg.community_shaders_settings)
        .ok()?
        .pointer("/Upscaling/foveatedRender/neuralRenderingEnabled")?
        .as_bool()
}

fn close_state() -> Result<Vec<String>> {
    Ok(crate::platform::checked_processes()?
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
            "Close Skyrim and Mod Organizer before changing the NR guard. Running: {}",
            running.join(", ")
        ))
    }
}

pub fn status(cfg: Option<&NrGuardConfig>) -> Value {
    let Some(cfg) = cfg else {
        return json!({
            "state": "unconfigured",
            "scope": SCOPE,
            "nrCheckboxEnabled": Value::Null,
            "requiresClosedGameAndMo2": true,
            "runningBlockers": [],
            "processCheckError": Value::Null,
            "hashChecked": false,
            "note": "NR guard paths are not configured in control-center.json."
        });
    };
    let (source, hidden) = match validated_paths(cfg) {
        Ok(pair) => pair,
        Err(e) => {
            return json!({
                "state": "invalid-config",
                "scope": SCOPE,
                "nrCheckboxEnabled": Value::Null,
                "requiresClosedGameAndMo2": true,
                "runningBlockers": [],
                "processCheckError": e,
                "hashChecked": false,
                "note": "Check nrGuard.dllPath in control-center.json."
            });
        }
    };
    let original = source.exists();
    let blocked = hidden.exists();
    let process = close_state();
    json!({
        "state": match (original, blocked) {
            (true, false) => "available",
            (false, true) => "blocked",
            (true, true) => "conflict",
            (false, false) => "missing",
        },
        "source": source,
        "hidden": hidden,
        "scope": SCOPE,
        "nrCheckboxEnabled": nr_checkbox(cfg),
        "requiresClosedGameAndMo2": true,
        "runningBlockers": process.as_ref().ok(),
        "processCheckError": process.err(),
        "hashChecked": false,
        "note": "Read-only status checks names only; full exact hash is checked before every change. A running game's existing allocations are not unloaded."
    })
}

fn verify_known_runtime(path: &Path, expected_bytes: u64, expected_hash: &str) -> Result<()> {
    // Reject junction/symlink ancestors rather than moving a substituted target.
    use std::os::windows::fs::MetadataExt;
    for ancestor in path.ancestors() {
        let metadata = fs::symlink_metadata(ancestor)
            .map_err(|e| format!("Cannot inspect {}: {e}", ancestor.display()))?;
        if metadata.file_attributes() & 0x400 != 0 {
            return Err(format!(
                "Reparse paths are not allowed for the NR guard: {}",
                ancestor.display()
            ));
        }
    }
    let mut file = File::open(path).map_err(|e| format!("Cannot read NR runtime: {e}"))?;
    let meta = file.metadata().map_err(|e| e.to_string())?;
    if !meta.is_file() || meta.len() != expected_bytes {
        return Err(
            "NR runtime size changed. Nothing was renamed; review the updated mod first.".into(),
        );
    }
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 65536];
    loop {
        let n = file.read(&mut buffer).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
    }
    let actual = format!("{:x}", hash.finalize());
    if actual != expected_hash.to_ascii_lowercase() {
        return Err(
            "NR runtime hash changed. Nothing was renamed; review the updated mod first.".into(),
        );
    }
    Ok(())
}

fn alternate_runtimes(cfg: &NrGuardConfig, source: &Path) -> Result<Vec<PathBuf>> {
    let mut roots = vec![cfg.game_data_dir.clone(), cfg.mo2_overwrite_dir.clone()];
    for entry in fs::read_dir(&cfg.mo2_mods_dir)
        .map_err(|e| format!("Cannot inspect MO2 mod roots: {e}"))?
    {
        let entry = entry.map_err(|e| e.to_string())?;
        if entry.file_type().map_err(|e| e.to_string())?.is_dir() {
            roots.push(entry.path());
        }
    }
    let mut found = Vec::new();
    for root in roots {
        for relative in [
            r"Shaders\Upscaling\Streamline\nvngx_dlssnr.dll",
            r"Shaders\Upscaling\StreamlineDX12\nvngx_dlssnr.dll",
        ] {
            let p = root.join(relative);
            if p != source && p.exists() {
                found.push(p);
            }
        }
    }
    Ok(found)
}

fn rename_no_overwrite(from: &Path, to: &Path) -> Result<()> {
    if to.exists() {
        return Err("NR destination already exists; nothing was overwritten.".into());
    }
    let from_w = wide(&from.to_string_lossy());
    let to_w = wide(&to.to_string_lossy());
    // No MOVEFILE_REPLACE_EXISTING: the OS also rejects a racing destination.
    let result = unsafe { MoveFileExW(from_w.as_ptr(), to_w.as_ptr(), MOVEFILE_WRITE_THROUGH) };
    if result == 0 {
        return Err(format!(
            "NR file was not renamed: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
}

fn journal(root: &Path, value: &Value) -> Result<()> {
    write_json(&root.join(".local/nr-guard-journal.json"), value)
}

fn change(cfg: &NrGuardConfig, root: &Path, block: bool) -> Result<Value> {
    let (source, hidden) = validated_paths(cfg)?;
    ensure_closed()?;
    let (from, to) = if block {
        (source.clone(), hidden)
    } else {
        (hidden, source.clone())
    };
    if !from.is_file() {
        return Err("Expected NR file is missing, or the guard is already in the requested state. Nothing was changed.".into());
    }
    if to.exists() {
        return Err("Both NR filenames exist; nothing was overwritten.".into());
    }
    if block {
        if nr_checkbox(cfg) != Some(false) {
            return Err("NR must already be unticked in the current-preference settings. The guard never changes graphics settings.".into());
        }
        let others = alternate_runtimes(cfg, &source)?;
        if !others.is_empty() {
            return Err(format!(
                "Another NR runtime is present; review it before blocking: {}",
                others
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
    }
    verify_known_runtime(&from, EXPECTED_BYTES, EXPECTED_SHA256)?;
    journal(
        root,
        &json!({"state":"prepared", "action":if block {"block"} else {"restore"},"from":from,"to":to,"sha256":EXPECTED_SHA256,"bytes":EXPECTED_BYTES}),
    )?;
    ensure_closed()?;
    rename_no_overwrite(&from, &to)?;
    let record = json!({"state":"done","action":if block {"block"} else {"restore"},"from":from,"to":to,"sha256":EXPECTED_SHA256,"bytes":EXPECTED_BYTES,"scope":SCOPE,"settingsChanged":false,"filesDeleted":false});
    // The rename is successful even if later journaling fails. Report both facts.
    if let Err(e) = journal(root, &record) {
        return Ok(json!({"result":record,"journalWarning":e}));
    }
    Ok(record)
}

pub fn disable(cfg: Option<&NrGuardConfig>, root: &Path) -> Result<Value> {
    let cfg = cfg.ok_or("NR guard paths are not configured in control-center.json.")?;
    change(cfg, root, true)
}

pub fn restore(cfg: Option<&NrGuardConfig>, root: &Path) -> Result<Value> {
    let cfg = cfg.ok_or("NR guard paths are not configured in control-center.json.")?;
    change(cfg, root, false)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn temp_dir(label: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "skyrim-nr-{}-{}-{}",
            std::process::id(),
            crate::config::now().to_bits(),
            label
        ));
        fs::create_dir_all(&p).unwrap();
        p
    }
    #[test]
    fn exact_hash_and_size_required() {
        let dir = temp_dir("hash");
        let file = dir.join("fake.dll");
        fs::write(&file, b"nr test").unwrap();
        let hash = format!("{:x}", Sha256::digest(b"nr test"));
        assert!(verify_known_runtime(&file, 7, &hash).is_ok());
        assert!(verify_known_runtime(&file, 8, &hash).is_err());
        assert!(verify_known_runtime(&file, 7, &"0".repeat(64)).is_err());
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn rename_never_overwrites_and_is_reversible() {
        let dir = temp_dir("rename");
        let from = dir.join("fake.dll");
        let to = dir.join("fake.dll.mohidden");
        fs::write(&from, b"original").unwrap();
        fs::write(&to, b"keep").unwrap();
        assert!(rename_no_overwrite(&from, &to).is_err());
        assert_eq!(fs::read(&to).unwrap(), b"keep");
        fs::remove_file(&to).unwrap();
        rename_no_overwrite(&from, &to).unwrap();
        assert!(!from.exists());
        rename_no_overwrite(&to, &from).unwrap();
        assert_eq!(fs::read(&from).unwrap(), b"original");
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn unconfigured_status_and_strict_dll_name_enforced() {
        assert_eq!(status(None)["state"], "unconfigured");
        let bad = NrGuardConfig {
            dll_path: PathBuf::from(r"C:\mods\other.dll"),
            community_shaders_settings: PathBuf::from(r"C:\mods\SettingsUser.json"),
            mo2_mods_dir: PathBuf::from(r"C:\mods"),
            mo2_overwrite_dir: PathBuf::from(r"C:\overwrite"),
            game_data_dir: PathBuf::from(r"C:\SkyrimVR\Data"),
        };
        assert!(validated_paths(&bad).is_err());
        assert_eq!(status(Some(&bad))["state"], "invalid-config");
    }
}
