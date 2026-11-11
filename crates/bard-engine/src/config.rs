use anyhow::{bail, Context, Result};
use regex::Regex;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

pub const OPENROUTER: &str = "https://openrouter.ai/api/v1/chat/completions";
pub const SUNO: &str = "https://api.sunoapi.org/api/v1";
pub const MARKER: &str = "SKYRIM_BARD_COMPOSER_NATIVE_CONTEXT_V1";

pub fn read_json(path: &Path) -> Result<Value> {
    let bytes = fs::read(path).context("Could not read JSON file")?;
    let bytes = bytes.strip_prefix(&[239, 187, 191]).unwrap_or(&bytes);
    serde_json::from_slice(bytes).context("Invalid JSON file")
}
pub fn save_json(path: &Path, value: &Value) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temp = path.with_extension(format!(
        "{}.tmp",
        path.extension().unwrap_or_default().to_string_lossy()
    ));
    let mut file = fs::File::create(&temp)?;
    file.write_all(&serde_json::to_vec_pretty(value)?)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    drop(file);
    fs::rename(temp, path).context("Could not atomically replace JSON file")
}
pub fn create_journal(path: &Path, value: &Value) -> Result<()> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .context("A prior journal exists; do not retry an uncertain request")?;
    file.write_all(&serde_json::to_vec(value)?)?;
    file.sync_all()?;
    Ok(())
}
pub fn text<'a>(v: &'a Value, key: &str, default: &'a str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or(default)
}
pub fn flag(v: &Value, key: &str, default: bool) -> bool {
    v.get(key).and_then(Value::as_bool).unwrap_or(default)
}
pub fn num(v: &Value, key: &str, default: f64) -> f64 {
    v.get(key).and_then(Value::as_f64).unwrap_or(default)
}
pub fn read_settings(root: &Path) -> Result<Value> {
    let v = read_json(&root.join("settings.json"))?;
    if !v.is_object() {
        bail!("Settings must be a JSON object");
    }
    Ok(v)
}
pub fn load(root: &Path) -> Result<Value> {
    let v = read_settings(root)?;
    validate(&v)?;
    Ok(v)
}
pub fn model_profile(s: &Value) -> Result<Value> {
    let id = text(s, "selected_model", "");
    if id.len() > 200 || !Regex::new(r"^[A-Za-z0-9_.-]+/[A-Za-z0-9_.:/-]+$")?.is_match(id) {
        bail!("Invalid selected OpenRouter model ID");
    }
    let mut seen = std::collections::HashSet::new();
    let mut selected = None;
    if let Some(profiles) = s["model_profiles"].as_array() {
        for p in profiles {
            let profile_id = p["id"].as_str().context("Profile ID must be a string")?;
            if !seen.insert(profile_id) {
                bail!("Model IDs must be unique");
            }
            if profile_id == id {
                if p["enabled"] != true {
                    bail!("Selected song model is disabled");
                }
                selected = Some(p.clone());
            }
        }
    }
    Ok(selected.unwrap_or_else(|| json!({"id":id,"label":id,"enabled":true})))
}
pub fn validate(s: &Value) -> Result<()> {
    let provider = text(s, "lyrics_provider", "openrouter");
    if provider != "openrouter" && provider != "antigravity" {
        bail!("Rust engine currently supports the selected OpenRouter or Antigravity workflows only");
    }
    let backend = text(s, "llm_backend", "openrouter");
    if backend != "openrouter" && backend != "antigravity" {
        bail!("llm_backend must be openrouter or antigravity");
    }
    if provider == "openrouter" {
        model_profile(s)?;
    }
    if !["off", "minimal", "low", "medium", "high", "xhigh", "max"].contains(&text(
        s,
        "reasoning_effort",
        "high",
    )) {
        bail!("Unsupported thinking setting");
    }
    if !s
        .get("max_output_tokens")
        .unwrap_or(&json!(30000))
        .as_u64()
        .is_some_and(|n| (1024..=128000).contains(&n))
    {
        bail!("Output tokens must be 1024 to 128000");
    }
    if !s["lyrics_temperature"].is_null()
        && !s["lyrics_temperature"]
            .as_f64()
            .is_some_and(|n| (0.0..=2.0).contains(&n))
    {
        bail!("Temperature must be blank or 0 to 2");
    }
    for k in [
        "include_history",
        "native_integration_enabled",
        "native_real_interval_override",
        "keep_both_suno_variants",
    ] {
        if s.get(k).is_some_and(|v| !v.is_boolean()) {
            bail!("Boolean setting has invalid type");
        }
    }
    if !s["suno_duration_seconds"].is_null()
        && !s["suno_duration_seconds"]
            .as_u64()
            .is_some_and(|n| (120..=179).contains(&n))
    {
        bail!("Suno duration must be blank or 120 to 179 seconds");
    }
    if !["auto", "male", "female"].contains(&text(s, "voice_mode", "auto")) {
        bail!("Invalid voice mode");
    }
    if !["m", "f"].contains(&text(s, "vocal_gender", "f")) {
        bail!("Invalid explicit voice");
    }
    if !["one_song", "native_automatic"].contains(&text(s, "native_spending_mode", "one_song")) {
        bail!("Invalid native spending mode");
    }
    if text(s, "native_spending_mode", "") == "native_automatic"
        && !truthy(&s["native_recurring_suno_authorization"])
    {
        bail!("Ongoing native generation needs recorded human authorization");
    }
    let first = num(s, "first_response_timeout_seconds", 30.0);
    let total = num(s, "completion_timeout_seconds", 300.0);
    if !(1.0..=30.0).contains(&first) || total < first || total > 300.0 {
        bail!("Rust response bounds are at most 30 seconds first content and 300 seconds total");
    }
    Ok(())
}
pub fn truthy(v: &Value) -> bool {
    !v.is_null() && v != false && v != "" && v != 0
}
pub fn sha(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
// Matches Python json.dumps(sort_keys=True) for compatibility with existing journals.
pub fn python_json(v: &Value) -> String {
    match v {
        Value::Object(map) => {
            let mut keys: Vec<_> = map.keys().collect();
            keys.sort();
            format!(
                "{{{}}}",
                keys.into_iter()
                    .map(|k| format!("{}: {}", python_json(&json!(k)), python_json(&map[k])))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        }
        Value::Array(a) => format!(
            "[{}]",
            a.iter().map(python_json).collect::<Vec<_>>().join(", ")
        ),
        Value::String(s) => {
            let encoded = serde_json::to_string(s).unwrap();
            let mut out = String::new();
            for c in encoded.chars() {
                if c as u32 >= 127 {
                    let mut units = [0; 2];
                    for unit in c.encode_utf16(&mut units) {
                        out.push_str(&format!("\\u{unit:04x}"));
                    }
                } else {
                    out.push(c);
                }
            }
            out
        }
        Value::Number(n) if n.is_f64() => python_float(&n.to_string()),
        _ => v.to_string(),
    }
}
fn python_float(raw: &str) -> String {
    // Python and Rust use the same shortest-roundtrip digits but differ in
    // scientific notation thresholds and exponent zero padding.
    let (sign, unsigned) = raw.strip_prefix('-').map_or(("", raw), |s| ("-", s));
    let (mantissa, power) = unsigned
        .split_once('e')
        .or_else(|| unsigned.split_once('E'))
        .map_or((unsigned, 0), |(m, e)| (m, e.parse::<i32>().unwrap()));
    let integer_digits = mantissa.find('.').unwrap_or(mantissa.len()) as i32;
    let digits = mantissa.chars().filter(|c| *c != '.').collect::<String>();
    let Some(first) = digits.find(|c| c != '0') else {
        return format!("{sign}0.0");
    };
    let exponent = integer_digits - first as i32 - 1 + power;
    let digits = digits[first..].trim_end_matches('0');
    if !(-4..16).contains(&exponent) {
        let fraction = if digits.len() > 1 {
            format!(".{}", &digits[1..])
        } else {
            String::new()
        };
        return format!(
            "{sign}{}{fraction}e{}{:02}",
            &digits[..1],
            if exponent < 0 { "-" } else { "+" },
            exponent.abs()
        );
    }
    let decimal = exponent + 1;
    if decimal <= 0 {
        return format!("{sign}0.{}{}", "0".repeat((-decimal) as usize), digits);
    }
    if decimal as usize >= digits.len() {
        return format!(
            "{sign}{}{}.0",
            digits,
            "0".repeat(decimal as usize - digits.len())
        );
    }
    format!(
        "{sign}{}.{}",
        &digits[..decimal as usize],
        &digits[decimal as usize..]
    )
}
pub fn digest(v: &Value) -> String {
    sha(python_json(v).as_bytes())
}
fn yaml_scalar(value: &str) -> Result<String> {
    let value = value.trim();
    if value.starts_with('"') {
        serde_json::from_str(value).context("Invalid credential scalar")
    } else if value.starts_with('\'') && value.ends_with('\'') {
        Ok(value[1..value.len() - 1].replace("''", "'"))
    } else {
        Ok(value.to_owned())
    }
}
pub fn openrouter_key(s: &Value) -> Result<String> {
    let path = s["openrouter_credentials_file"]
        .as_str()
        .context("Missing provider credential file path")?;
    let source = fs::read_to_string(path).context("Cannot read existing provider credentials")?;
    let re = Regex::new(r"(?m)^  - id:\s*openrouter\s*$")?;
    if let Some(m) = re.find(&source) {
        let block = &source[m.end()..];
        let block = block.split("\n  - id:").next().unwrap_or(block);
        if let Some(m) = Regex::new(r"(?m)^    api_key:[ \t]*([^\r\n]*)")?.captures(block) {
            let key = yaml_scalar(&m[1])?;
            if key.starts_with("sk-or-") {
                return Ok(key);
            }
        }
    }
    bail!("OpenRouter key missing from existing Providers.yaml")
}
pub fn suno_key(s: &Value) -> Result<String> {
    let data = PathBuf::from(
        s["skyrimnet_data"]
            .as_str()
            .context("Missing SkyrimNet data path")?,
    );
    let source = fs::read_to_string(
        data.parent()
            .context("Invalid runtime path")?
            .join("config/BardSinging.yaml"),
    )
    .context("Cannot read existing Suno credentials")?;
    let start = Regex::new(r"(?m)^  suno:[ \t]*\r?$")?
        .find(&source)
        .context("Missing Suno configuration")?
        .end();
    let block = &source[start..];
    let end = Regex::new(r"(?m)^  [a-zA-Z_]+:")?
        .find(block)
        .map_or(block.len(), |m| m.start());
    let block = &block[..end];
    let value = |key: &str| -> Result<String> {
        let r = Regex::new(&format!(r"(?m)^    {key}:[ \t]*([^\r\n]*)"))?;
        let c = r.captures(block).context("Missing Suno scalar")?;
        yaml_scalar(&c[1])
    };
    if value("base_url")?.trim_end_matches('/') != SUNO {
        bail!("Suno host must be the canonical HTTPS endpoint");
    }
    let key = value("api_key")?;
    if key.is_empty() {
        bail!("Suno key is empty");
    }
    Ok(key)
}
pub fn token(root: &Path) -> Result<String> {
    let v = fs::read_to_string(root.join("data/native-adapter-token.txt"))
        .context("Existing adapter token is required; configure the native route first")?;
    let v = v.trim();
    if v.len() < 16 {
        bail!("Invalid adapter token");
    }
    Ok(v.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn python_digest_compatibility() {
        assert_eq!(
            python_json(&json!({"b":"ä🎵","a":[1,true,null]})),
            r#"{"a": [1, true, null], "b": "\u00e4\ud83c\udfb5"}"#
        );
    }
    #[test]
    fn python_digest_escapes_ascii_delete() {
        assert_eq!(python_json(&json!("\u{7f}")), "\"\\u007f\"");
    }
    #[test]
    fn python_digest_float_exponents() {
        for (value, expected) in [
            (1.0, "1.0"),
            (0.00001, "1e-05"),
            (0.000001, "1e-06"),
            (0.0001, "0.0001"),
            (1e16, "1e+16"),
            (1e15, "1000000000000000.0"),
            (-0.0, "-0.0"),
            (1.234e-7, "1.234e-07"),
        ] {
            assert_eq!(python_json(&json!(value)), expected);
        }
    }
    #[test]
    fn disabled_model_never_substituted() {
        assert!(model_profile(&json!({"selected_model":"z-ai/glm-5.3-flash","model_profiles":[{"id":"z-ai/glm-5.3-flash","enabled":false}]})).is_err());
    }
    #[test]
    fn scalar_preserves_quoted_secrets_without_yaml_execution() {
        assert_eq!(yaml_scalar("'ab''cd'").unwrap(), "ab'cd");
    }
}
