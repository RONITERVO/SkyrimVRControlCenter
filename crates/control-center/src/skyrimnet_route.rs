//! Reversible SkyrimNet `OpenRouter.yaml` route switcher for the local Antigravity relay.
use crate::{
    config::{Config, Result, write_bytes_atomic, write_json},
    platform,
    shader_cache::check_no_reparse,
};
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

const BACKUP_SUFFIX: &str = "-before-antigravity";
const JOURNAL_REL_PATH: &str = ".local/antigravity-route-journal.json";

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RouteCounts {
    pub local_bard_composer: usize,
    pub openrouter: usize,
    pub openrouter_decisions: usize,
    pub other: usize,
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
            "Close Skyrim VR and Mod Organizer 2 before changing SkyrimNet routes. Running: {}",
            running.join(", ")
        ))
    }
}

/// Parses a YAML list item line of the form `<indent>- provider_id: <id><trailing_ws>`.
/// Returns `(prefix_through_spaces_after_colon, id, trailing_ws_excluding_eol)` if matched.
fn parse_provider_line(line_no_eol: &str) -> Option<(&str, &str, &str)> {
    let trimmed_start = line_no_eol.trim_start();
    let after_dash = trimmed_start.strip_prefix('-')?;
    if !after_dash.starts_with(char::is_whitespace) {
        return None;
    }
    let after_dash_ws = after_dash.trim_start();
    let after_key = after_dash_ws.strip_prefix("provider_id")?;
    let after_key_ws = after_key.trim_start();
    let after_colon = after_key_ws.strip_prefix(':')?;
    let value_and_tail = after_colon.trim_start();
    let prefix_len = line_no_eol.len() - value_and_tail.len();
    let id = value_and_tail.trim_end();
    if id.is_empty() || id.starts_with('#') || id.chars().any(char::is_whitespace) {
        return None;
    }
    let tail_len = value_and_tail.len() - id.len();
    Some((
        &line_no_eol[..prefix_len],
        id,
        &value_and_tail[id.len()..id.len() + tail_len],
    ))
}

/// Counts `provider_id` entries in `OpenRouter.yaml` text.
pub fn count_routes(yaml_text: &str) -> RouteCounts {
    let mut counts = RouteCounts::default();
    for raw_line in yaml_text.lines() {
        if let Some((_, id, _)) = parse_provider_line(raw_line) {
            match id {
                "local_bard_composer" => counts.local_bard_composer += 1,
                "openrouter" => counts.openrouter += 1,
                "openrouter_decisions" => counts.openrouter_decisions += 1,
                _ => counts.other += 1,
            }
        }
    }
    counts
}

/// Rewrites exact `- provider_id: openrouter` entries to `- provider_id: local_bard_composer`,
/// leaving `openrouter_decisions`, comments, formatting, and line endings intact.
pub fn rewrite_openrouter_to_local(yaml_text: &str) -> (String, usize) {
    let mut out = String::with_capacity(yaml_text.len() + 256);
    let mut changed = 0usize;
    for chunk in yaml_text.split_inclusive('\n') {
        let (body, eol) = if let Some(s) = chunk.strip_suffix("\r\n") {
            (s, "\r\n")
        } else if let Some(s) = chunk.strip_suffix('\n') {
            (s, "\n")
        } else {
            (chunk, "")
        };
        if let Some((prefix, "openrouter", tail)) = parse_provider_line(body) {
            out.push_str(prefix);
            out.push_str("local_bard_composer");
            out.push_str(tail);
            out.push_str(eol);
            changed += 1;
        } else {
            out.push_str(chunk);
        }
    }
    (out, changed)
}

fn utc_timestamp_label() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let hour = rem / 3600;
    let minute = (rem % 3600) / 60;
    let second = rem % 60;
    // Howard Hinnant civil-from-days algorithm
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = (yoe as i64) + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = y + if m <= 2 { 1 } else { 0 };
    format!("{year:04}{m:02}{d:02}-{hour:02}{minute:02}{second:02}")
}

fn list_antigravity_backups(root: &Path) -> Vec<PathBuf> {
    let backups_dir = root.join("backups");
    let Ok(entries) = fs::read_dir(&backups_dir) else {
        return Vec::new();
    };
    let mut dirs: Vec<(String, PathBuf)> = entries
        .flatten()
        .filter_map(|e| {
            let p = e.path();
            if !p.is_dir() {
                return None;
            }
            let name = p.file_name()?.to_str()?.to_owned();
            if name.ends_with(BACKUP_SUFFIX) {
                Some((name, p))
            } else {
                None
            }
        })
        .collect();
    dirs.sort_by(|a, b| b.0.cmp(&a.0));
    dirs.into_iter().map(|(_, p)| p).collect()
}

/// Finds the newest `*-before-antigravity` backup whose `OpenRouter.yaml` actually contains
/// `provider_id: openrouter` entries (skipping any accidental post-switch backup).
fn find_valid_restore_backup(root: &Path) -> Option<(PathBuf, RouteCounts)> {
    for dir in list_antigravity_backups(root) {
        let yaml = dir.join("OpenRouter.yaml");
        if !yaml.is_file() {
            continue;
        }
        if let Ok(text) = fs::read_to_string(&yaml) {
            let counts = count_routes(&text);
            if counts.openrouter > 0 {
                return Some((dir, counts));
            }
        }
    }
    None
}

fn write_journal(root: &Path, entry: &Value) {
    let _ = write_json(&root.join(JOURNAL_REL_PATH), entry);
}

/// Returns read-only SkyrimNet `OpenRouter.yaml` route status and restore readiness.
pub fn status(cfg: &Config, root: &Path) -> Value {
    let Some(config_dir) = cfg.effective_skyrimnet_config_dir() else {
        return json!({
            "state": "unconfigured",
            "configDir": Value::Null,
            "openRouterYaml": Value::Null,
            "runningBlockers": [],
            "processCheckError": Value::Null,
            "hasBackup": false,
            "note": "SkyrimNet config directory could not be resolved from settings or control-center.json."
        });
    };
    let yaml_path = config_dir.join("OpenRouter.yaml");
    let process = close_state();
    let backup = find_valid_restore_backup(root);
    let backup_folder = backup
        .as_ref()
        .and_then(|(p, _)| p.file_name())
        .map(|n| n.to_string_lossy().into_owned());

    if !yaml_path.is_file() {
        return json!({
            "state": "missing",
            "configDir": config_dir,
            "openRouterYaml": yaml_path,
            "runningBlockers": process.as_ref().ok(),
            "processCheckError": process.err(),
            "hasBackup": backup.is_some(),
            "latestBackup": backup_folder,
            "note": "OpenRouter.yaml was not found in the resolved SkyrimNet config folder."
        });
    }

    let Ok(text) = fs::read_to_string(&yaml_path) else {
        return json!({
            "state": "unreadable",
            "configDir": config_dir,
            "openRouterYaml": yaml_path,
            "runningBlockers": process.as_ref().ok(),
            "processCheckError": process.err(),
            "hasBackup": backup.is_some(),
            "latestBackup": backup_folder,
            "note": "Cannot read SkyrimNet OpenRouter.yaml."
        });
    };

    let counts = count_routes(&text);
    let state = if counts.openrouter == 0 && counts.local_bard_composer > 0 {
        "antigravity"
    } else if counts.openrouter > 0 && counts.local_bard_composer <= 2 {
        "openrouter"
    } else if counts.openrouter > 0 {
        "partial"
    } else {
        "unknown"
    };

    json!({
        "state": state,
        "configDir": config_dir,
        "openRouterYaml": yaml_path,
        "localBardComposerSlots": counts.local_bard_composer,
        "openRouterSlots": counts.openrouter,
        "decisionsSlots": counts.openrouter_decisions,
        "otherSlots": counts.other,
        "hasBackup": backup.is_some(),
        "latestBackup": backup_folder,
        "runningBlockers": process.as_ref().ok(),
        "processCheckError": process.err(),
    })
}

/// Switches SkyrimNet `OpenRouter.yaml` slots (`provider_id: openrouter`) to `local_bard_composer`
/// after backing up the current files and verifying Skyrim VR and MO2 are closed.
pub fn switch_to_antigravity(cfg: &Config, root: &Path) -> Result<Value> {
    let config_dir = cfg
        .effective_skyrimnet_config_dir()
        .ok_or("SkyrimNet config directory is not configured")?;
    let yaml_path = config_dir.join("OpenRouter.yaml");
    if !yaml_path.is_file() {
        return Err(format!(
            "SkyrimNet OpenRouter.yaml not found at {}",
            yaml_path.display()
        ));
    }

    ensure_closed()?;
    check_no_reparse(&yaml_path)?;

    let text = fs::read_to_string(&yaml_path)
        .map_err(|e| format!("Cannot read {}: {e}", yaml_path.display()))?;
    let before = count_routes(&text);
    if before.openrouter == 0 {
        if before.local_bard_composer > 0 {
            return Ok(json!({
                "changed": 0,
                "state": "antigravity",
                "message": "SkyrimNet OpenRouter.yaml is already routed to Antigravity (local_bard_composer); existing backups preserved."
            }));
        }
        return Err("No `provider_id: openrouter` entries found in OpenRouter.yaml; nothing was modified.".into());
    }

    let ts = utc_timestamp_label();
    let mut backup_dir = root.join("backups").join(format!("{ts}{BACKUP_SUFFIX}"));
    if backup_dir.exists() {
        backup_dir = root
            .join("backups")
            .join(format!("{ts}-{}{BACKUP_SUFFIX}", std::process::id()));
    }
    fs::create_dir_all(&backup_dir).map_err(|e| format!("Cannot create backup folder: {e}"))?;
    fs::copy(&yaml_path, backup_dir.join("OpenRouter.yaml"))
        .map_err(|e| format!("Failed to back up OpenRouter.yaml: {e}"))?;
    let providers_yaml = config_dir.join("Providers.yaml");
    if providers_yaml.is_file() {
        let _ = fs::copy(&providers_yaml, backup_dir.join("Providers.yaml"));
    }

    let (updated, changed) = rewrite_openrouter_to_local(&text);
    ensure_closed()?;
    write_bytes_atomic(&yaml_path, updated.as_bytes())?;
    let after = count_routes(&updated);

    let record = json!({
        "action": "switch_to_antigravity",
        "backupDir": backup_dir,
        "changedSlots": changed,
        "localBardComposerSlots": after.local_bard_composer,
        "openRouterSlots": after.openrouter,
        "decisionsSlots": after.openrouter_decisions,
    });
    write_journal(root, &record);

    Ok(json!({
        "changed": changed,
        "state": "antigravity",
        "backupDir": backup_dir,
        "message": format!("Switched {changed} SkyrimNet route(s) to Antigravity (local_bard_composer); backup saved and decisions preserved.")
    }))
}

/// Restores `OpenRouter.yaml` from the newest valid pre-Antigravity backup while Skyrim VR and MO2 are closed.
pub fn restore_openrouter(cfg: &Config, root: &Path) -> Result<Value> {
    let config_dir = cfg
        .effective_skyrimnet_config_dir()
        .ok_or("SkyrimNet config directory is not configured")?;
    let yaml_path = config_dir.join("OpenRouter.yaml");

    ensure_closed()?;
    check_no_reparse(&yaml_path)?;

    let (backup_dir, backup_counts) = find_valid_restore_backup(root)
        .ok_or("No valid pre-Antigravity OpenRouter.yaml backup found in backups/*-before-antigravity")?;
    let source_yaml = backup_dir.join("OpenRouter.yaml");
    let backup_bytes = fs::read(&source_yaml)
        .map_err(|e| format!("Cannot read backup {}: {e}", source_yaml.display()))?;

    ensure_closed()?;
    write_bytes_atomic(&yaml_path, &backup_bytes)?;

    let record = json!({
        "action": "restore_openrouter",
        "restoredFrom": backup_dir,
        "openRouterSlots": backup_counts.openrouter,
        "localBardComposerSlots": backup_counts.local_bard_composer,
        "decisionsSlots": backup_counts.openrouter_decisions,
    });
    write_journal(root, &record);

    Ok(json!({
        "state": "openrouter",
        "restoredFrom": backup_dir,
        "message": format!(
            "Restored SkyrimNet OpenRouter.yaml ({} openrouter, {} local_bard_composer) from {}.",
            backup_counts.openrouter,
            backup_counts.local_bard_composer,
            backup_dir.file_name().unwrap_or_default().to_string_lossy()
        )
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(label: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "skyrim-route-{}-{}-{}",
            std::process::id(),
            crate::config::now().to_bits(),
            label
        ));
        fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn rewrites_only_exact_openrouter_slots_preserving_decisions_and_crlf() {
        let input = "slots:\r\n  - name: dialogue\r\n    - provider_id: openrouter\r\n  - name: bard\r\n    - provider_id: local_bard_composer\r\n  - name: decisions\r\n    - provider_id: openrouter_decisions\r\n";
        let (out, changed) = rewrite_openrouter_to_local(input);
        assert_eq!(changed, 1);
        assert!(out.contains("    - provider_id: local_bard_composer\r\n"));
        assert!(out.contains("    - provider_id: openrouter_decisions\r\n"));
        let counts = count_routes(&out);
        assert_eq!(counts.openrouter, 0);
        assert_eq!(counts.local_bard_composer, 2);
        assert_eq!(counts.openrouter_decisions, 1);
    }

    #[test]
    fn switch_and_restore_skip_poisoned_backups_and_preserve_originals() {
        let root = temp_dir("switch-restore");
        let skyrimnet_cfg = root.join("skyrimnet/config");
        fs::create_dir_all(&skyrimnet_cfg).unwrap();

        let original_yaml = "variants:\n  - provider_id: openrouter\n  - provider_id: openrouter\n  - provider_id: local_bard_composer\n  - provider_id: openrouter_decisions\n";
        fs::write(skyrimnet_cfg.join("OpenRouter.yaml"), original_yaml).unwrap();
        fs::write(skyrimnet_cfg.join("Providers.yaml"), "providers: []\n").unwrap();

        let cfg = Config {
            music_root: root.join("runtime/bard"),
            feedback_root: root.join("runtime/psvr2"),
            agent_port: 19464,
            close_kat_overlays: false,
            dependencies: vec![],
            nr_guard: None,
            shader_cache: None,
            skyrimnet_config_dir: Some(skyrimnet_cfg.clone()),
        };

        // First switch converts 2 openrouter slots and creates backup
        let res1 = switch_to_antigravity(&cfg, &root).unwrap();
        assert_eq!(res1["changed"], 2);
        assert_eq!(status(&cfg, &root)["state"], "antigravity");

        // Second switch is idempotent and creates NO extra backup
        let backup_count_before = list_antigravity_backups(&root).len();
        let res2 = switch_to_antigravity(&cfg, &root).unwrap();
        assert_eq!(res2["changed"], 0);
        assert_eq!(list_antigravity_backups(&root).len(), backup_count_before);

        // Even if a newer poisoned backup exists (already switched), restore skips it and uses the clean one
        let poisoned = root.join("backups/99999999-999999-before-antigravity");
        fs::create_dir_all(&poisoned).unwrap();
        fs::write(
            poisoned.join("OpenRouter.yaml"),
            fs::read_to_string(skyrimnet_cfg.join("OpenRouter.yaml")).unwrap(),
        )
        .unwrap();

        let restored = restore_openrouter(&cfg, &root).unwrap();
        assert_eq!(restored["state"], "openrouter");
        assert_eq!(
            fs::read_to_string(skyrimnet_cfg.join("OpenRouter.yaml")).unwrap(),
            original_yaml
        );

        fs::remove_dir_all(root).unwrap();
    }
}

