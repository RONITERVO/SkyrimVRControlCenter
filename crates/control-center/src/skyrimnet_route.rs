//! Reversible SkyrimNet `OpenRouter.yaml` route switcher for the local Antigravity relay.
use crate::{
    config::{Config, Result, write_bytes_atomic, write_json},
    platform,
    shader_cache::check_no_reparse,
};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

const BACKUP_SUFFIX: &str = "-before-antigravity";
const RESTORE_BACKUP_SUFFIX: &str = "-before-openrouter-restore";
const JOURNAL_REL_PATH: &str = ".local/antigravity-route-journal.json";
const LOCAL_ID: &str = "local_bard_composer";
const OPENROUTER_ID: &str = "openrouter";
const RELAY_ENDPOINTS: &[&str] = &["127.0.0.1:8097", "localhost:8097"];
/// Bard slots that were already local before any Antigravity switch (used only when no backup exists).
const DEFAULT_BARD_LOCAL_SLOTS: usize = 2;

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
                LOCAL_ID => counts.local_bard_composer += 1,
                OPENROUTER_ID => counts.openrouter += 1,
                "openrouter_decisions" => counts.openrouter_decisions += 1,
                _ => counts.other += 1,
            }
        }
    }
    counts
}

/// Rewrites `provider_id` values on the given (0-based) lines from `from` to `to`, leaving
/// every other byte (comments, formatting, line endings) intact.
fn rewrite_provider_lines(
    yaml_text: &str,
    only_lines: Option<&HashSet<usize>>,
    from: &str,
    to: &str,
) -> (String, usize) {
    let mut out = String::with_capacity(yaml_text.len() + 256);
    let mut changed = 0usize;
    for (idx, chunk) in yaml_text.split_inclusive('\n').enumerate() {
        let (body, eol) = if let Some(s) = chunk.strip_suffix("\r\n") {
            (s, "\r\n")
        } else if let Some(s) = chunk.strip_suffix('\n') {
            (s, "\n")
        } else {
            (chunk, "")
        };
        let selected = only_lines.is_none_or(|set| set.contains(&idx));
        match parse_provider_line(body) {
            Some((prefix, id, tail)) if selected && id == from => {
                out.push_str(prefix);
                out.push_str(to);
                out.push_str(tail);
                out.push_str(eol);
                changed += 1;
            }
            _ => out.push_str(chunk),
        }
    }
    (out, changed)
}

/// Rewrites exact `- provider_id: openrouter` entries to `- provider_id: local_bard_composer`,
/// leaving `openrouter_decisions`, comments, formatting, and line endings intact.
pub fn rewrite_openrouter_to_local(yaml_text: &str) -> (String, usize) {
    rewrite_provider_lines(yaml_text, None, OPENROUTER_ID, LOCAL_ID)
}

/// A `- provider_id:` entry and its structural location in the YAML document.
#[derive(Clone, Debug, Eq, PartialEq)]
struct ProviderSlot {
    line: usize,
    parent: String,
    /// Ancestor mapping keys plus the ordinal among provider entries under them,
    /// e.g. `openrouter/variants/AgentDefault/model_params#0`.
    key: String,
    id: String,
    /// Normalized non-`provider_id` fields belonging to this YAML sequence item.
    signature: Vec<String>,
}

fn mapping_key(content: &str) -> Option<String> {
    let colon = content.find(':')?;
    let key = content[..colon].trim();
    let rest = &content[colon + 1..];
    if !(rest.is_empty() || rest.starts_with(char::is_whitespace)) {
        return None;
    }
    if key.is_empty() || key.starts_with('#') || key.chars().any(char::is_whitespace) {
        return None;
    }
    Some(key.trim_matches(|c| c == '"' || c == '\'').to_owned())
}

fn normalize_item_field(rel_indent: usize, trimmed: &str) -> String {
    if let Some((k, v)) = trimmed.split_once(':') {
        let key = k.trim();
        let val = v.trim();
        if val.is_empty() {
            format!("{rel_indent}:{key}:")
        } else {
            format!("{rel_indent}:{key}: {val}")
        }
    } else {
        format!("{rel_indent}:{trimmed}")
    }
}

/// Locates every provider slot by indentation-derived path and sibling item fields, so
/// moved slots can be matched or ambiguous list edits detected across YAML versions.
fn provider_slots(yaml_text: &str) -> Vec<ProviderSlot> {
    let mut stack: Vec<(usize, String)> = Vec::new();
    let mut ordinals: HashMap<String, usize> = HashMap::new();
    let mut slots: Vec<ProviderSlot> = Vec::new();
    let mut active_item_indent: Option<usize> = None;

    for (idx, raw) in yaml_text.lines().enumerate() {
        let body = raw.trim_end();
        let trimmed = body.trim_start();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let indent = body.len() - trimmed.len();
        if let Some(item_indent) = active_item_indent {
            if indent > item_indent {
                if let Some(last) = slots.last_mut() {
                    last.signature
                        .push(normalize_item_field(indent - item_indent, trimmed));
                }
            } else {
                active_item_indent = None;
            }
        }
        while stack.last().is_some_and(|(i, _)| *i >= indent) {
            stack.pop();
        }
        let (content_indent, content) = match trimmed.strip_prefix('-') {
            Some(rest) if rest.is_empty() || rest.starts_with(char::is_whitespace) => {
                let after = rest.trim_start();
                (indent + 1 + (rest.len() - after.len()), after)
            }
            _ => (indent, trimmed),
        };
        if let Some((_, id, _)) = parse_provider_line(body) {
            let parent = stack
                .iter()
                .map(|(_, k)| k.as_str())
                .collect::<Vec<_>>()
                .join("/");
            let ordinal = ordinals.entry(parent.clone()).or_insert(0);
            slots.push(ProviderSlot {
                line: idx,
                parent: parent.clone(),
                key: format!("{parent}#{ordinal}"),
                id: id.to_owned(),
                signature: Vec::new(),
            });
            *ordinal += 1;
            active_item_indent = Some(indent);
        }
        if let Some(key) = mapping_key(content) {
            stack.push((content_indent, key));
        }
    }
    slots
}

/// Result of reversing an Antigravity switch against a pre-switch backup.
#[derive(Debug, Eq, PartialEq)]
pub struct RestorePlan {
    pub text: String,
    pub reverted: usize,
    /// `local_bard_composer` slots that do not exist in the backup (left untouched).
    pub unmatched_local: usize,
    /// Parent paths where `model_params` entries changed ambiguously and were left untouched.
    pub ambiguous_groups: Vec<String>,
}

fn plan_parent_group(
    b_slots: &[&ProviderSlot],
    l_slots: &[&ProviderSlot],
    targets: &mut HashSet<usize>,
    unmatched_local: &mut usize,
) -> bool {
    if b_slots.is_empty() {
        *unmatched_local += l_slots.iter().filter(|s| s.id == LOCAL_ID).count();
        return false;
    }

    if l_slots.len() == b_slots.len()
        && b_slots
            .iter()
            .zip(l_slots.iter())
            .all(|(b, l)| b.signature == l.signature)
    {
        for (b, l) in b_slots.iter().zip(l_slots.iter()) {
            if l.id == LOCAL_ID && b.id == OPENROUTER_ID {
                targets.insert(l.line);
            }
        }
        return false;
    }

    if b_slots.iter().all(|b| b.id != OPENROUTER_ID) {
        let b_local = b_slots.iter().filter(|b| b.id == LOCAL_ID).count();
        let l_local = l_slots.iter().filter(|l| l.id == LOCAL_ID).count();
        *unmatched_local += l_local.saturating_sub(b_local);
        return false;
    }

    // List length or item signatures changed under this parent; group by item fields.
    let mut b_by_sig: HashMap<&[String], Vec<&ProviderSlot>> = HashMap::new();
    let mut l_by_sig: HashMap<&[String], Vec<&ProviderSlot>> = HashMap::new();
    let mut signatures: Vec<&[String]> = Vec::new();
    for b in b_slots {
        let sig = b.signature.as_slice();
        if !b_by_sig.contains_key(sig) {
            signatures.push(sig);
        }
        b_by_sig.entry(sig).or_default().push(b);
    }
    for l in l_slots {
        let sig = l.signature.as_slice();
        if !b_by_sig.contains_key(sig) && !l_by_sig.contains_key(sig) {
            signatures.push(sig);
        }
        l_by_sig.entry(sig).or_default().push(l);
    }

    let mut candidate_targets = Vec::new();
    let mut added_unbacked_local = 0usize;
    let mut missing_backup_open = 0usize;
    let mut ambiguous = false;

    for sig in signatures {
        let bb = b_by_sig.get(sig).map(Vec::as_slice).unwrap_or(&[]);
        let lb = l_by_sig.get(sig).map(Vec::as_slice).unwrap_or(&[]);
        let b_open = bb.iter().filter(|b| b.id == OPENROUTER_ID).count();
        let b_other = bb.len() - b_open;
        let l_local: Vec<usize> = lb
            .iter()
            .filter(|l| l.id == LOCAL_ID)
            .map(|l| l.line)
            .collect();
        let l_other = lb.len() - l_local.len();

        if bb.is_empty() {
            added_unbacked_local += l_local.len();
        } else if b_open == 0 {
            let b_local = bb.iter().filter(|b| b.id == LOCAL_ID).count();
            added_unbacked_local += l_local.len().saturating_sub(b_local);
        } else if b_other == 0 && l_other == 0 && l_local.len() <= b_open {
            if l_local.len() < b_open {
                missing_backup_open += b_open - l_local.len();
            }
            candidate_targets.extend(l_local);
        } else if lb.len() == bb.len()
            && bb
                .iter()
                .zip(lb.iter())
                .all(|(b, l)| b.id == l.id || (b.id == OPENROUTER_ID && l.id == LOCAL_ID))
        {
            for (b, l) in bb.iter().zip(lb.iter()) {
                if b.id == OPENROUTER_ID && l.id == LOCAL_ID {
                    candidate_targets.push(l.line);
                }
            }
        } else {
            ambiguous = true;
            break;
        }
    }

    if ambiguous || (missing_backup_open > 0 && added_unbacked_local > 0) {
        return true;
    }

    targets.extend(candidate_targets);
    *unmatched_local += added_unbacked_local;
    false
}

/// Reverts only the slots that were `openrouter` in the backup and are
/// `local_bard_composer` now. Distinguishes moved entries by sibling item fields and
/// refuses to rewrite groups whose list changes are ambiguous.
pub fn plan_restore(live: &str, backup: &str) -> RestorePlan {
    let backup_slots = provider_slots(backup);
    let live_slots = provider_slots(live);

    let mut parent_order: Vec<String> = Vec::new();
    let mut b_by_parent: HashMap<&str, Vec<&ProviderSlot>> = HashMap::new();
    let mut l_by_parent: HashMap<&str, Vec<&ProviderSlot>> = HashMap::new();
    for b in &backup_slots {
        if !b_by_parent.contains_key(b.parent.as_str()) {
            parent_order.push(b.parent.clone());
        }
        b_by_parent.entry(b.parent.as_str()).or_default().push(b);
    }
    for l in &live_slots {
        if !b_by_parent.contains_key(l.parent.as_str())
            && !l_by_parent.contains_key(l.parent.as_str())
        {
            parent_order.push(l.parent.clone());
        }
        l_by_parent.entry(l.parent.as_str()).or_default().push(l);
    }

    let mut targets = HashSet::new();
    let mut unmatched_local = 0usize;
    let mut ambiguous_groups = Vec::new();

    for parent in parent_order {
        let bs = b_by_parent.get(parent.as_str()).map(Vec::as_slice).unwrap_or(&[]);
        let ls = l_by_parent.get(parent.as_str()).map(Vec::as_slice).unwrap_or(&[]);
        if plan_parent_group(bs, ls, &mut targets, &mut unmatched_local) {
            ambiguous_groups.push(parent);
        }
    }

    let (text, reverted) = rewrite_provider_lines(live, Some(&targets), LOCAL_ID, OPENROUTER_ID);
    RestorePlan {
        text,
        reverted,
        unmatched_local,
        ambiguous_groups,
    }
}

/// Reads only the `endpoint:` of the `local_bard_composer` provider in `Providers.yaml`.
/// Credential fields are never read into the result.
fn local_provider_endpoint(providers_text: &str) -> Option<String> {
    let mut block_indent: Option<usize> = None;
    for raw in providers_text.lines() {
        let body = raw.trim_end();
        let trimmed = body.trim_start();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let indent = body.len() - trimmed.len();
        if let Some(bi) = block_indent {
            if indent <= bi {
                block_indent = None;
            } else if let Some(v) = trimmed.strip_prefix("endpoint:") {
                return Some(v.trim().trim_matches(|c| c == '"' || c == '\'').to_owned());
            } else {
                continue;
            }
        }
        if let Some(rest) = trimmed.strip_prefix('-') {
            let item = rest.trim_start();
            if let Some(id) = item.strip_prefix("id:")
                && id.trim().trim_matches(|c| c == '"' || c == '\'') == LOCAL_ID
            {
                block_indent = Some(indent);
            }
        }
    }
    None
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

fn new_backup_dir(root: &Path, suffix: &str) -> Result<PathBuf> {
    let ts = utc_timestamp_label();
    let mut dir = root.join("backups").join(format!("{ts}{suffix}"));
    if dir.exists() {
        dir = root
            .join("backups")
            .join(format!("{ts}-{}{suffix}", std::process::id()));
    }
    fs::create_dir_all(&dir).map_err(|e| format!("Cannot create backup folder: {e}"))?;
    Ok(dir)
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
fn find_valid_restore_backup(root: &Path) -> Option<(PathBuf, RouteCounts, String)> {
    for dir in list_antigravity_backups(root) {
        let yaml = dir.join("OpenRouter.yaml");
        if !yaml.is_file() {
            continue;
        }
        if let Ok(text) = fs::read_to_string(&yaml) {
            let counts = count_routes(&text);
            if counts.openrouter > 0 {
                return Some((dir, counts, text));
            }
        }
    }
    None
}

fn write_journal(root: &Path, entry: &Value) {
    let _ = write_json(&root.join(JOURNAL_REL_PATH), entry);
}

/// Returns read-only SkyrimNet `OpenRouter.yaml` route status and restore readiness.
///
/// `state` describes the slots in `OpenRouter.yaml`; `effective` additionally checks that
/// `local_bard_composer` points at the relay and that the relay itself uses Antigravity.
pub fn status(cfg: &Config, root: &Path) -> Value {
    let Some(config_dir) = cfg.effective_skyrimnet_config_dir() else {
        return json!({
            "state": "unconfigured",
            "effective": "unconfigured",
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
        .and_then(|(p, _, _)| p.file_name())
        .map(|n| n.to_string_lossy().into_owned());

    if !yaml_path.is_file() {
        return json!({
            "state": "missing",
            "effective": "missing",
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
            "effective": "unreadable",
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
    let baseline_local = backup
        .as_ref()
        .map(|(_, c, _)| c.local_bard_composer)
        .unwrap_or(DEFAULT_BARD_LOCAL_SLOTS);
    let state = if counts.openrouter == 0 && counts.local_bard_composer > 0 {
        "antigravity"
    } else if counts.openrouter > 0 && counts.local_bard_composer <= baseline_local {
        "openrouter"
    } else if counts.openrouter > 0 {
        "partial"
    } else {
        "unknown"
    };

    let local_endpoint = fs::read_to_string(config_dir.join("Providers.yaml"))
        .ok()
        .and_then(|t| local_provider_endpoint(&t));
    let endpoint_on_relay = local_endpoint
        .as_deref()
        .is_some_and(|e| RELAY_ENDPOINTS.iter().any(|r| e.contains(r)));
    let relay_backend = cfg
        .music_settings()
        .ok()
        .and_then(|s| s["llm_backend"].as_str().map(str::to_owned))
        .unwrap_or_else(|| "openrouter".to_owned());
    let effective = match state {
        "antigravity" | "partial" if !endpoint_on_relay => "local_endpoint_not_relay",
        "antigravity" if relay_backend != "antigravity" => "relay_openrouter",
        other => other,
    };

    json!({
        "state": state,
        "effective": effective,
        "configDir": config_dir,
        "openRouterYaml": yaml_path,
        "localBardComposerSlots": counts.local_bard_composer,
        "openRouterSlots": counts.openrouter,
        "decisionsSlots": counts.openrouter_decisions,
        "otherSlots": counts.other,
        "localProviderEndpoint": local_endpoint,
        "localProviderOnRelay": endpoint_on_relay,
        "relayBackend": relay_backend,
        "hasBackup": backup.is_some(),
        "latestBackup": backup_folder,
        "runningBlockers": process.as_ref().ok(),
        "processCheckError": process.err(),
    })
}

/// Switches SkyrimNet `OpenRouter.yaml` slots (`provider_id: openrouter`) to `local_bard_composer`
/// after backing up the current file and verifying Skyrim VR and MO2 are closed.
///
/// Only `OpenRouter.yaml` is backed up: `Providers.yaml` holds provider credentials and is never
/// modified, so it is not copied.
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

    let backup_dir = new_backup_dir(root, BACKUP_SUFFIX)?;
    fs::copy(&yaml_path, backup_dir.join("OpenRouter.yaml"))
        .map_err(|e| format!("Failed to back up OpenRouter.yaml: {e}"))?;

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
        "message": format!("Switched {changed} SkyrimNet route(s) to Antigravity (local_bard_composer); OpenRouter.yaml backed up and decisions preserved.")
    }))
}

/// Reverses the Antigravity switch while Skyrim VR and MO2 are closed.
///
/// Only slots that were `openrouter` in the newest valid pre-Antigravity backup and are
/// `local_bard_composer` now are changed back; models, timeouts, new slots and every other
/// edit made since the switch are kept. The live file is backed up before it is rewritten.
pub fn restore_openrouter(cfg: &Config, root: &Path) -> Result<Value> {
    let config_dir = cfg
        .effective_skyrimnet_config_dir()
        .ok_or("SkyrimNet config directory is not configured")?;
    let yaml_path = config_dir.join("OpenRouter.yaml");

    ensure_closed()?;
    check_no_reparse(&yaml_path)?;

    let (backup_dir, _, backup_text) = find_valid_restore_backup(root)
        .ok_or("No valid pre-Antigravity OpenRouter.yaml backup found in backups/*-before-antigravity")?;
    let live_text = fs::read_to_string(&yaml_path)
        .map_err(|e| format!("Cannot read {}: {e}", yaml_path.display()))?;
    let plan = plan_restore(&live_text, &backup_text);
    let backup_name = backup_dir
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();

    if !plan.ambiguous_groups.is_empty() {
        return Err(format!(
            "Refused to restore OpenRouter.yaml because provider list entries changed ambiguously under: {}. Reconcile those slots or restore manually from {backup_name}.",
            plan.ambiguous_groups.join(", "),
        ));
    }

    if plan.reverted == 0 {
        return Ok(json!({
            "changed": 0,
            "state": "openrouter",
            "restoredFrom": backup_dir,
            "message": format!(
                "No switched slots to revert (compared with {backup_name}); OpenRouter.yaml was not modified."
            )
        }));
    }

    let safety_dir = new_backup_dir(root, RESTORE_BACKUP_SUFFIX)?;
    fs::copy(&yaml_path, safety_dir.join("OpenRouter.yaml"))
        .map_err(|e| format!("Failed to back up current OpenRouter.yaml: {e}"))?;

    ensure_closed()?;
    write_bytes_atomic(&yaml_path, plan.text.as_bytes())?;
    let after = count_routes(&plan.text);

    let record = json!({
        "action": "restore_openrouter",
        "comparedWith": backup_dir,
        "currentBackupDir": safety_dir,
        "revertedSlots": plan.reverted,
        "unmatchedLocalSlots": plan.unmatched_local,
        "openRouterSlots": after.openrouter,
        "localBardComposerSlots": after.local_bard_composer,
        "decisionsSlots": after.openrouter_decisions,
    });
    write_journal(root, &record);

    let unmatched_note = if plan.unmatched_local > 0 {
        format!(
            " {} local_bard_composer slot(s) not present in the backup were left unchanged.",
            plan.unmatched_local
        )
    } else {
        String::new()
    };
    Ok(json!({
        "changed": plan.reverted,
        "state": "openrouter",
        "restoredFrom": backup_dir,
        "backupDir": safety_dir,
        "message": format!(
            "Reverted {} slot(s) to openrouter using {backup_name}; other edits kept and the previous file was backed up.{unmatched_note}",
            plan.reverted
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

    fn test_config(root: &Path, skyrimnet_cfg: &Path) -> Config {
        Config {
            music_root: root.join("runtime/bard"),
            feedback_root: root.join("runtime/psvr2"),
            agent_port: 19464,
            close_kat_overlays: false,
            dependencies: vec![],
            nr_guard: None,
            shader_cache: None,
            skyrimnet_config_dir: Some(skyrimnet_cfg.to_path_buf()),
        }
    }

    const ORIGINAL: &str = "openrouter:\n  model_name: a/b\n  model_params:\n    - provider_id: local_bard_composer\n      temperature: 1\n    - provider_id: local_bard_composer\n      temperature: 0.7\n  variants:\n    AgentDefault:\n      model_name: anthropic/claude-haiku-4.5\n      model_params:\n        - provider_id: openrouter\n          temperature: 0.7\n    Decisions:\n      model_params:\n        - provider_id: openrouter_decisions\n    Combat:\n      model_params:\n        - provider_id: openrouter\n        - provider_id: openrouter\n";

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
    fn provider_slots_are_keyed_by_yaml_location() {
        let keys: Vec<String> = provider_slots(ORIGINAL).into_iter().map(|s| s.key).collect();
        assert_eq!(
            keys,
            vec![
                "openrouter/model_params#0",
                "openrouter/model_params#1",
                "openrouter/variants/AgentDefault/model_params#0",
                "openrouter/variants/Decisions/model_params#0",
                "openrouter/variants/Combat/model_params#0",
                "openrouter/variants/Combat/model_params#1",
            ]
        );
    }

    #[test]
    fn restore_plan_reverts_only_switched_slots_and_keeps_later_edits() {
        let (switched, _) = rewrite_openrouter_to_local(ORIGINAL);
        // Edits made after the switch: a model change, plus a new slot added by a SkyrimNet update.
        let edited = switched
            .replace("anthropic/claude-haiku-4.5", "google/gemini-3-flash")
            .replace(
                "    Combat:\n",
                "    OmniSight:\n      model_params:\n        - provider_id: local_bard_composer\n    Combat:\n",
            );
        let plan = plan_restore(&edited, ORIGINAL);
        assert_eq!(plan.reverted, 3);
        assert_eq!(plan.unmatched_local, 1);
        assert!(plan.text.contains("google/gemini-3-flash"), "model edit must survive");
        assert!(plan.text.contains("    OmniSight:\n      model_params:\n        - provider_id: local_bard_composer\n"));
        let counts = count_routes(&plan.text);
        assert_eq!(counts.openrouter, 3);
        assert_eq!(counts.local_bard_composer, 3); // two bard slots + new unmatched slot
        assert_eq!(counts.openrouter_decisions, 1);

        // Applying the plan again is a no-op.
        assert_eq!(plan_restore(&plan.text, ORIGINAL).reverted, 0);
    }

    #[test]
    fn restore_plan_preserves_crlf() {
        let original = ORIGINAL.replace('\n', "\r\n");
        let (switched, _) = rewrite_openrouter_to_local(&original);
        let plan = plan_restore(&switched, &original);
        assert_eq!(plan.text, original);
    }

    #[test]
    fn local_provider_endpoint_reads_only_the_local_block() {
        let providers = "providers:\n  - id: openrouter\n    endpoint: https://openrouter.ai/api/v1/chat/completions\n    api_key: secret-one\n  - id: local_bard_composer\n    name: Local relay\n    api_key: secret-two\n    endpoint: http://127.0.0.1:8097/v1/chat/completions\n  - id: chutes\n    endpoint: https://llm.chutes.ai/v1/chat/completions\n";
        assert_eq!(
            local_provider_endpoint(providers).as_deref(),
            Some("http://127.0.0.1:8097/v1/chat/completions")
        );
        let missing = "providers:\n  - id: local_bard_composer\n    name: x\n  - id: other\n    endpoint: https://example.com\n";
        assert_eq!(local_provider_endpoint(missing), None);
    }

    #[test]
    fn switch_and_restore_skip_poisoned_backups_and_preserve_edits() {
        let root = temp_dir("switch-restore");
        let skyrimnet_cfg = root.join("skyrimnet/config");
        fs::create_dir_all(&skyrimnet_cfg).unwrap();

        fs::write(skyrimnet_cfg.join("OpenRouter.yaml"), ORIGINAL).unwrap();
        fs::write(skyrimnet_cfg.join("Providers.yaml"), "providers:\n  - id: local_bard_composer\n    endpoint: http://127.0.0.1:8097/v1/chat/completions\n").unwrap();
        let cfg = test_config(&root, &skyrimnet_cfg);

        // First switch converts 3 openrouter slots and backs up OpenRouter.yaml only.
        let res1 = switch_to_antigravity(&cfg, &root).unwrap();
        assert_eq!(res1["changed"], 3);
        let backup_dir = PathBuf::from(res1["backupDir"].as_str().unwrap());
        assert!(backup_dir.join("OpenRouter.yaml").is_file());
        assert!(!backup_dir.join("Providers.yaml").exists(), "credentials file must not be copied");
        let st = status(&cfg, &root);
        assert_eq!(st["state"], "antigravity");
        // No relay settings -> relay backend defaults to openrouter, so it is not effectively Antigravity.
        assert_eq!(st["effective"], "relay_openrouter");
        assert_eq!(st["localProviderOnRelay"], true);

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

        // A model edit made while switched must survive the restore.
        let live = fs::read_to_string(skyrimnet_cfg.join("OpenRouter.yaml")).unwrap();
        let edited = live.replace("model_name: a/b", "model_name: c/d");
        fs::write(skyrimnet_cfg.join("OpenRouter.yaml"), &edited).unwrap();

        let restored = restore_openrouter(&cfg, &root).unwrap();
        assert_eq!(restored["state"], "openrouter");
        assert_eq!(restored["changed"], 3);
        assert_eq!(
            fs::read_to_string(skyrimnet_cfg.join("OpenRouter.yaml")).unwrap(),
            ORIGINAL.replace("model_name: a/b", "model_name: c/d")
        );
        // The pre-restore file was backed up.
        let safety = PathBuf::from(restored["backupDir"].as_str().unwrap());
        assert_eq!(fs::read_to_string(safety.join("OpenRouter.yaml")).unwrap(), edited);

        // A second restore changes nothing and writes nothing.
        let again = restore_openrouter(&cfg, &root).unwrap();
        assert_eq!(again["changed"], 0);
        assert!(again.get("backupDir").is_none());

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn restore_matches_moved_distinct_entries_when_local_inserted_or_removed() {
        let backup = "openrouter:\n  variants:\n    Combat:\n      model_params:\n        - provider_id: openrouter\n          temperature: 1\n        - provider_id: local_bard_composer\n          temperature: 0.7\n";
        let (switched, _) = rewrite_openrouter_to_local(backup);
        // After switching, user prepends a new intentional local entry with distinct parameters.
        let inserted = switched.replace(
            "      model_params:\n",
            "      model_params:\n        - provider_id: local_bard_composer\n          temperature: 0.5\n",
        );
        let plan_ins = plan_restore(&inserted, backup);
        assert!(plan_ins.ambiguous_groups.is_empty());
        assert_eq!(plan_ins.reverted, 1);
        assert_eq!(plan_ins.unmatched_local, 1);
        let ins_slots: Vec<String> = provider_slots(&plan_ins.text)
            .into_iter()
            .map(|s| s.id)
            .collect();
        assert_eq!(ins_slots, vec![LOCAL_ID, OPENROUTER_ID, LOCAL_ID]);

        // Removing the first (switched) entry must not mistake the remaining intentional local entry for it.
        let removed_first = "openrouter:\n  variants:\n    Combat:\n      model_params:\n        - provider_id: local_bard_composer\n          temperature: 0.7\n";
        let plan_rem = plan_restore(removed_first, backup);
        assert!(plan_rem.ambiguous_groups.is_empty());
        assert_eq!(plan_rem.reverted, 0);
        assert_eq!(plan_rem.text, removed_first);
    }

    #[test]
    fn restore_refuses_ambiguous_insertion_or_mixed_deletion_without_rewriting_file() {
        // Indistinguishable items (no sibling fields): inserting a new local entry before switched Combat entries.
        let (switched, _) = rewrite_openrouter_to_local(ORIGINAL);
        let inserted_ambiguous = switched.replace(
            "    Combat:\n      model_params:\n",
            "    Combat:\n      model_params:\n        - provider_id: local_bard_composer\n",
        );
        let plan = plan_restore(&inserted_ambiguous, ORIGINAL);
        assert_eq!(
            plan.ambiguous_groups,
            vec!["openrouter/variants/Combat/model_params"]
        );
        // AgentDefault is unambiguous, while Combat was refused and left untouched.
        assert_eq!(plan.reverted, 1);
        assert!(plan.text.contains("    Combat:\n      model_params:\n        - provider_id: local_bard_composer\n        - provider_id: local_bard_composer\n        - provider_id: local_bard_composer\n"));

        // restore_openrouter must refuse the whole operation and leave the live file untouched.
        let root = temp_dir("ambiguous-refuse");
        let skyrimnet_cfg = root.join("skyrimnet/config");
        fs::create_dir_all(&skyrimnet_cfg).unwrap();
        let before_dir = root.join("backups/20261111-000000-before-antigravity");
        fs::create_dir_all(&before_dir).unwrap();
        fs::write(before_dir.join("OpenRouter.yaml"), ORIGINAL).unwrap();
        fs::write(skyrimnet_cfg.join("OpenRouter.yaml"), &inserted_ambiguous).unwrap();
        fs::write(skyrimnet_cfg.join("Providers.yaml"), "providers: []\n").unwrap();
        let cfg = test_config(&root, &skyrimnet_cfg);

        let err = restore_openrouter(&cfg, &root).unwrap_err();
        assert!(err.contains("openrouter/variants/Combat/model_params"), "{err}");
        assert_eq!(
            fs::read_to_string(skyrimnet_cfg.join("OpenRouter.yaml")).unwrap(),
            inserted_ambiguous
        );

        // Mixed identical fields in backup + deletion is also flagged ambiguous.
        let mixed_backup = "openrouter:\n  variants:\n    Combat:\n      model_params:\n        - provider_id: local_bard_composer\n        - provider_id: openrouter\n";
        let mixed_deleted = "openrouter:\n  variants:\n    Combat:\n      model_params:\n        - provider_id: local_bard_composer\n";
        let plan_mixed = plan_restore(mixed_deleted, mixed_backup);
        assert_eq!(
            plan_mixed.ambiguous_groups,
            vec!["openrouter/variants/Combat/model_params"]
        );
        assert_eq!(plan_mixed.reverted, 0);

        fs::remove_dir_all(root).unwrap();
    }
}
