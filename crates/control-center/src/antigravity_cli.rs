//! Antigravity CLI discovery and non-generative model catalog lookup.
use crate::config::Result;
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    io::Read,
    os::windows::process::CommandExt,
    path::PathBuf,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

const CREATE_NO_WINDOW: u32 = 0x08000000;

pub const DEFAULT_ANTIGRAVITY_MODEL: &str = "gemini-3.8-flash-low";
pub const DEFAULT_ANTIGRAVITY_EFFORT: &str = "low";

pub const DEFAULT_MODELS: &[(&str, &str)] = &[
    ("gemini-3.8-flash-low", "Gemini 3.8 Flash (Low)"),
    ("gemini-3.8-flash-medium", "Gemini 3.8 Flash (Medium)"),
    ("gemini-3.8-flash-high", "Gemini 3.8 Flash (High)"),
    ("gemini-4-argon-low", "Gemini 4 Argon (Low)"),
    ("gemini-4-argon-medium", "Gemini 4 Argon (Medium)"),
    ("gemini-4-argon-high", "Gemini 4 Argon (High)"),
    ("claude-sonnet-5-5-low", "Claude Sonnet 5.5 (Low)"),
    ("claude-sonnet-5-5-medium", "Claude Sonnet 5.5 (Medium)"),
    ("claude-sonnet-5-5-high", "Claude Sonnet 5.5 (High)"),
    ("claude-opus-5-5-low", "Claude Opus 5.5 (Low)"),
    ("claude-opus-5-5-medium", "Claude Opus 5.5 (Medium)"),
    ("claude-opus-5-5-high", "Claude Opus 5.5 (High)"),
    ("gpt-oss-120b-medium", "GPT-OSS 120B (Medium)"),
];

/// Normalizes an effort string into one of Antigravity's supported tiers.
pub fn normalize_effort(raw: &str) -> &'static str {
    match raw.trim().to_ascii_lowercase().as_str() {
        "low" | "minimal" | "off" | "none" => "low",
        "medium" | "med" => "medium",
        "high" | "xhigh" | "max" => "high",
        _ => "low",
    }
}

/// Applies an effort tier suffix to a model ID (preserving fixed-tier models like gpt-oss-120b-medium).
pub fn apply_effort_to_model(model: &str, effort: &str) -> String {
    let trimmed = model.trim();
    if trimmed.is_empty() {
        return format!("gemini-3.8-flash-{}", normalize_effort(effort));
    }
    let base = trimmed
        .strip_suffix("-low")
        .or_else(|| trimmed.strip_suffix("-medium"))
        .or_else(|| trimmed.strip_suffix("-high"))
        .unwrap_or(trimmed);
    if base == "gpt-oss-120b" {
        return "gpt-oss-120b-medium".to_owned();
    }
    format!("{base}-{}", normalize_effort(effort))
}

/// Extracts the effort tier suffix if present on an Antigravity model ID.
pub fn effort_from_model(model: &str) -> Option<&'static str> {
    let trimmed = model.trim();
    if trimmed.ends_with("-low") {
        Some("low")
    } else if trimmed.ends_with("-medium") {
        Some("medium")
    } else if trimmed.ends_with("-high") {
        Some("high")
    } else {
        None
    }
}

/// Checks whether a string is a valid Antigravity model ID (no slashes or whitespace).
pub fn is_valid_model_id(id: &str) -> bool {
    let id = id.trim();
    !id.is_empty()
        && id.len() <= 120
        && !id.contains('/')
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_.-".contains(c))
}

/// Discovers `agy.exe` matching `bard-engine`'s resolution order.
pub fn find_agy_path(configured: Option<&str>) -> Result<PathBuf> {
    if let Some(raw) = configured.map(str::trim).filter(|s| !s.is_empty()) {
        let p = PathBuf::from(raw);
        if p.is_file() {
            return Ok(p);
        }
        return Err(format!("Configured agy.exe path does not exist: {raw}"));
    }
    if let Ok(local_app_data) = std::env::var("LOCALAPPDATA") {
        let p = PathBuf::from(local_app_data)
            .join("agy")
            .join("bin")
            .join("agy.exe");
        if p.is_file() {
            return Ok(p);
        }
    }
    if let Ok(user_profile) = std::env::var("USERPROFILE") {
        let p = PathBuf::from(user_profile)
            .join(".gemini")
            .join("bin")
            .join("agy.exe");
        if p.is_file() {
            return Ok(p);
        }
    }
    if let Ok(path_var) = std::env::var("PATH") {
        for dir in std::env::split_paths(&path_var) {
            for candidate in [dir.join("agy.exe"), dir.join("agy")] {
                if candidate.is_file() {
                    return Ok(candidate);
                }
            }
        }
    }
    Err("Antigravity CLI (agy.exe) not found in default locations or PATH".into())
}

/// Parses stdout from `agy models` into deduplicated `(id, label)` pairs.
pub fn parse_models_output(stdout: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for line in stdout.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with("Fetching") {
            continue;
        }
        let (id, label) = if let Some((left, right)) = trimmed.split_once('\t') {
            (left.trim(), right.trim())
        } else {
            continue;
        };
        if !is_valid_model_id(id) || !seen.insert(id.to_owned()) {
            continue;
        }
        let label = if label.is_empty() { id } else { label };
        out.push((id.to_owned(), label.to_owned()));
    }
    out
}

/// Queries `agy models` (zero LLM tokens/credits) to refresh the available model catalog.
pub fn discover_models(configured: Option<&str>) -> Result<Vec<(String, String)>> {
    let bin = find_agy_path(configured)?;
    let mut child = Command::new(&bin)
        .arg("models")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map_err(|e| format!("Failed to start {}: {e}", bin.display()))?;

    let mut out_pipe = child.stdout.take();
    let mut err_pipe = child.stderr.take();
    let out_reader = thread::spawn(move || {
        let mut buf = String::new();
        if let Some(mut r) = out_pipe.take() {
            let _ = r.read_to_string(&mut buf);
        }
        buf
    });
    let err_reader = thread::spawn(move || {
        let mut buf = String::new();
        if let Some(mut r) = err_pipe.take() {
            let _ = r.read_to_string(&mut buf);
        }
        buf
    });

    let deadline = Instant::now() + Duration::from_secs(5);
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st,
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(50)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("Timed out querying `agy models`".into());
            }
            Err(e) => return Err(format!("Failed waiting for agy models: {e}")),
        }
    };

    let stdout = out_reader.join().unwrap_or_default();
    let stderr = err_reader.join().unwrap_or_default();
    if !status.success() {
        let detail = stderr.trim();
        return Err(if detail.is_empty() {
            format!("`agy models` exited with status {status}")
        } else {
            format!("`agy models` failed: {detail}")
        });
    }

    let models = parse_models_output(&stdout);
    if models.is_empty() {
        return Err("`agy models` returned no parseable model IDs".into());
    }
    Ok(models)
}

/// Converts a list of `(id, label)` pairs into JSON objects for status snapshots.
pub fn models_to_json(models: &[(String, String)]) -> Value {
    Value::Array(
        models
            .iter()
            .map(|(id, label)| json!({"id": id, "label": label}))
            .collect(),
    )
}

/// Default offline model list as JSON.
pub fn default_models_json() -> Value {
    let owned: Vec<(String, String)> = DEFAULT_MODELS
        .iter()
        .map(|(id, label)| ((*id).to_owned(), (*label).to_owned()))
        .collect();
    models_to_json(&owned)
}

/// Non-blocking status check for `agy.exe` presence using current bard settings.
pub fn cli_status(settings: &Value) -> Value {
    let configured = settings["antigravity"]["agy_path"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    match find_agy_path(configured) {
        Ok(path) => json!({"found": true, "path": path}),
        Err(err) => json!({"found": false, "error": err}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_agy_models_output_and_deduplicates() {
        let raw = "Fetching available models...\n\
                   gemini-3.8-flash-low\tGemini 3.8 Flash (Low)\n\
                   gemini-4-argon-high\tGemini 4 Argon (High)\n\
                   gemini-3.8-flash-low\tDuplicate\n\
                   bad/model\tIgnored\n";
        let parsed = parse_models_output(raw);
        assert_eq!(
            parsed,
            vec![
                (
                    "gemini-3.8-flash-low".to_owned(),
                    "Gemini 3.8 Flash (Low)".to_owned()
                ),
                (
                    "gemini-4-argon-high".to_owned(),
                    "Gemini 4 Argon (High)".to_owned()
                ),
            ]
        );
    }

    #[test]
    fn effort_sync_handles_tiered_and_fixed_models() {
        assert_eq!(
            apply_effort_to_model("gemini-3.8-flash-low", "high"),
            "gemini-3.8-flash-high"
        );
        assert_eq!(
            apply_effort_to_model("gemini-4-argon", "medium"),
            "gemini-4-argon-medium"
        );
        assert_eq!(
            apply_effort_to_model("gpt-oss-120b-medium", "low"),
            "gpt-oss-120b-medium"
        );
        assert_eq!(effort_from_model("claude-opus-5-5-high"), Some("high"));
        assert_eq!(effort_from_model("gemini-4-argon"), None);
    }
}

