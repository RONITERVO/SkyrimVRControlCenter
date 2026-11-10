use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

pub type Result<T> = std::result::Result<T, String>;
pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}
pub fn read_json(path: &Path) -> Result<Value> {
    serde_json::from_slice(
        &fs::read(path).map_err(|e| format!("Cannot read {}: {e}", path.display()))?,
    )
    .map_err(|e| e.to_string())
}
pub fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}
pub fn write_bytes_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    fs::create_dir_all(path.parent().ok_or("Missing parent")?).map_err(|e| e.to_string())?;
    let temp = path.with_extension(format!("{}.tmp", std::process::id()));
    fs::write(&temp, bytes).map_err(|e| e.to_string())?;
    let from = wide(&temp.to_string_lossy());
    let to = wide(&path.to_string_lossy());
    let ok = unsafe {
        windows_sys::Win32::Storage::FileSystem::MoveFileExW(
            from.as_ptr(),
            to.as_ptr(),
            windows_sys::Win32::Storage::FileSystem::MOVEFILE_REPLACE_EXISTING
                | windows_sys::Win32::Storage::FileSystem::MOVEFILE_WRITE_THROUGH,
        )
    };
    if ok == 0 {
        let _ = fs::remove_file(&temp);
        return Err(std::io::Error::last_os_error().to_string());
    }
    Ok(())
}
pub fn write_json(path: &Path, value: &Value) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(value).map_err(|e| e.to_string())?;
    write_bytes_atomic(path, &bytes)
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NrGuardConfig {
    pub dll_path: PathBuf,
    pub community_shaders_settings: PathBuf,
    pub mo2_mods_dir: PathBuf,
    pub mo2_overwrite_dir: PathBuf,
    pub game_data_dir: PathBuf,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ShaderCacheConfig {
    pub mo2_root: PathBuf,
    #[serde(default)]
    pub game_data_dir: Option<PathBuf>,
    #[serde(default)]
    pub backup_root: Option<PathBuf>,
    #[serde(default)]
    pub fallback_settings_path: Option<PathBuf>,
    #[serde(default)]
    pub mo2_shortcut: Option<String>,
    #[serde(default = "default_max_shader_backups")]
    pub max_backups: usize,
}
fn default_max_shader_backups() -> usize {
    3
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Config {
    #[serde(default = "default_music_root")]
    pub music_root: PathBuf,
    #[serde(default = "default_feedback_root")]
    pub feedback_root: PathBuf,
    pub agent_port: u16,
    #[serde(default = "default_true")]
    pub close_kat_overlays: bool,
    pub dependencies: Vec<Dependency>,
    #[serde(default)]
    pub nr_guard: Option<NrGuardConfig>,
    #[serde(default)]
    pub shader_cache: Option<ShaderCacheConfig>,
    #[serde(default)]
    pub skyrimnet_config_dir: Option<PathBuf>,
}
fn default_music_root() -> PathBuf {
    PathBuf::from("runtime/bard")
}
fn default_feedback_root() -> PathBuf {
    PathBuf::from("runtime/psvr2")
}
fn default_true() -> bool {
    true
}
pub fn resolve_path(base: &Path, path: &Path) -> PathBuf {
    if path.is_relative() {
        base.join(path)
    } else {
        path.to_path_buf()
    }
}
pub fn feedback_install_path(
    feedback_root: &Path,
    key: &str,
    missing_msg: &str,
) -> Result<PathBuf> {
    let i = read_json(&feedback_root.join("installation.json"))?;
    let raw = i[key].as_str().ok_or(missing_msg)?;
    Ok(resolve_path(feedback_root, Path::new(raw)))
}
#[derive(Clone, Deserialize, Serialize)]
pub struct Dependency {
    pub id: String,
    pub label: String,
    pub target: String,
    pub process: String,
    #[serde(default)]
    pub session: bool,
}
impl Config {
    pub fn load(root: &Path) -> Result<Self> {
        let v = read_json(&root.join("control-center.json"))?;
        let mut cfg: Self = serde_json::from_value(v).map_err(|e| e.to_string())?;
        cfg.music_root = resolve_path(root, &cfg.music_root);
        cfg.feedback_root = resolve_path(root, &cfg.feedback_root);
        if let Some(nr) = cfg.nr_guard.as_mut() {
            nr.dll_path = resolve_path(root, &nr.dll_path);
            nr.community_shaders_settings = resolve_path(root, &nr.community_shaders_settings);
            nr.mo2_mods_dir = resolve_path(root, &nr.mo2_mods_dir);
            nr.mo2_overwrite_dir = resolve_path(root, &nr.mo2_overwrite_dir);
            nr.game_data_dir = resolve_path(root, &nr.game_data_dir);
        }
        if let Some(sc) = cfg.shader_cache.as_mut() {
            sc.mo2_root = resolve_path(root, &sc.mo2_root);
            if let Some(gd) = sc.game_data_dir.as_mut() {
                *gd = resolve_path(root, gd);
            }
            if let Some(br) = sc.backup_root.as_mut() {
                *br = resolve_path(root, br);
            }
            if let Some(fp) = sc.fallback_settings_path.as_mut() {
                *fp = resolve_path(root, fp);
            }
        }
        if let Some(sn) = cfg.skyrimnet_config_dir.as_mut() {
            *sn = resolve_path(root, sn);
        }
        Ok(cfg)
    }
    /// Resolves the SkyrimNet `config` directory (containing `OpenRouter.yaml`).
    pub fn effective_skyrimnet_config_dir(&self) -> Option<PathBuf> {
        if let Some(dir) = &self.skyrimnet_config_dir {
            return Some(dir.clone());
        }
        if let Ok(settings) = self.music_settings() {
            if let Some(cred) = settings["openrouter_credentials_file"]
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                let p = resolve_path(&self.music_root, Path::new(cred));
                if let Some(parent) = p.parent() {
                    return Some(parent.to_path_buf());
                }
            }
            if let Some(data) = settings["skyrimnet_data"]
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                let p = resolve_path(&self.music_root, Path::new(data));
                if let Some(parent) = p.parent() {
                    return Some(parent.join("config"));
                }
            }
        }
        if let Some(nr) = &self.nr_guard {
            return Some(nr.mo2_overwrite_dir.join(r"SKSE\Plugins\SkyrimNet\config"));
        }
        self.effective_shader_cache()
            .map(|sc| sc.mo2_root.join(r"overwrite\SKSE\Plugins\SkyrimNet\config"))
    }
    pub fn effective_shader_cache(&self) -> Option<ShaderCacheConfig> {
        if let Some(mut sc) = self.shader_cache.clone() {
            if sc.game_data_dir.is_none() {
                sc.game_data_dir = self.nr_guard.as_ref().map(|nr| nr.game_data_dir.clone());
            }
            if sc.fallback_settings_path.is_none() {
                sc.fallback_settings_path = self
                    .nr_guard
                    .as_ref()
                    .map(|nr| nr.community_shaders_settings.clone());
            }
            return Some(sc);
        }
        if let Some(nr) = &self.nr_guard
            && let Some(mo2_root) = nr
                .mo2_overwrite_dir
                .parent()
                .or_else(|| nr.mo2_mods_dir.parent())
        {
            return Some(ShaderCacheConfig {
                mo2_root: mo2_root.to_path_buf(),
                game_data_dir: Some(nr.game_data_dir.clone()),
                backup_root: None,
                fallback_settings_path: Some(nr.community_shaders_settings.clone()),
                mo2_shortcut: None,
                max_backups: default_max_shader_backups(),
            });
        }
        let mo2_dep = self.dependencies.iter().find(|d| {
            d.id.eq_ignore_ascii_case("mo2") || d.process.eq_ignore_ascii_case("ModOrganizer.exe")
        })?;
        let mo2_root = Path::new(&mo2_dep.target).parent()?.to_path_buf();
        Some(ShaderCacheConfig {
            mo2_root,
            game_data_dir: None,
            backup_root: None,
            fallback_settings_path: None,
            mo2_shortcut: None,
            max_backups: default_max_shader_backups(),
        })
    }
    pub fn music_settings(&self) -> Result<Value> {
        read_json(&self.music_root.join("settings.json"))
    }
    pub fn feedback_settings(&self) -> Result<crate::feedback::Settings> {
        let path = self.feedback_root.join(".local/settings.json");
        if !path.exists() {
            return Ok(crate::feedback::Settings::default());
        }
        let s: crate::feedback::Settings =
            serde_json::from_value(read_json(&path)?).map_err(|e| e.to_string())?;
        s.validate()?;
        Ok(s)
    }
}
pub const MUSIC_KEYS: &[&str] = &[
    "selected_model",
    "reasoning_effort",
    "include_history",
    "voice_mode",
    "suno_duration_seconds",
    "interval_real_gameplay_seconds",
    "native_real_interval_override",
    "keep_both_suno_variants",
    "llm_backend",
    "lyrics_provider",
    "antigravity",
];
pub fn public_music(v: &Value) -> Value {
    let mut result = json!({});
    for key in MUSIC_KEYS.iter().chain(
        [
            "model_profiles",
            "native_integration_enabled",
            "native_spending_mode",
        ]
        .iter(),
    ) {
        result[*key] = v[*key].clone();
    }
    result["ongoing_authorized"] = json!(
        v["native_recurring_suno_authorization"]
            .as_str()
            .is_some_and(|s| !s.is_empty())
    );
    result
}
fn normalize_optional_path(value: &mut Value, name: &str) -> Result<()> {
    match value {
        Value::Null => Ok(()),
        Value::String(s) => {
            let trimmed = s.trim().to_owned();
            if trimmed.is_empty() {
                *value = Value::Null;
            } else {
                *value = Value::String(trimmed);
            }
            Ok(())
        }
        _ => Err(format!("antigravity.{name} must be a string or null")),
    }
}
pub fn patch_music(current: &Value, patch: &Value) -> Result<Value> {
    let fields = patch
        .as_object()
        .ok_or("Settings patch must be an object")?;
    let mut v = current.clone();
    for (k, value) in fields {
        if !MUSIC_KEYS.contains(&k.as_str()) && k != "model_profiles" {
            return Err(format!("Cannot edit {k} through song preferences"));
        }
        if k == "antigravity"
            && let (Some(existing), Some(incoming)) = (v["antigravity"].as_object_mut(), value.as_object())
        {
            for (ak, av) in incoming {
                existing.insert(ak.clone(), av.clone());
            }
        } else {
            v[k] = value.clone();
        }
    }
    let model = v["selected_model"]
        .as_str()
        .ok_or("Model must be a string")?
        .to_owned();
    if model.len() > 200
        || !model.contains('/')
        || !model
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_.-/:".contains(c))
        || model.starts_with('/')
        || model.ends_with('/')
    {
        return Err("Enter a model ID in author/model form".into());
    }
    let mut ids = std::collections::HashSet::new();
    let profiles = v["model_profiles"]
        .as_array()
        .ok_or("Saved models must be a list")?;
    for p in profiles {
        let id = p["id"].as_str().ok_or("Model ID missing")?;
        if !ids.insert(id.to_owned()) {
            return Err("Model IDs must be unique".into());
        }
        if p["enabled"].as_bool().is_none() {
            return Err("Model enabled must be boolean".into());
        }
        if id == model && p["enabled"] != true {
            return Err("Selected song model is disabled; enable it first".into());
        }
    }
    for key in ["llm_backend", "lyrics_provider"] {
        if !v[key].is_null() {
            let val = v[key]
                .as_str()
                .ok_or_else(|| format!("{key} must be antigravity or openrouter"))?;
            if val != "antigravity" && val != "openrouter" {
                return Err(format!("{key} must be antigravity or openrouter"));
            }
        }
    }
    if !v["antigravity"].is_null() {
        let ag = v["antigravity"]
            .as_object_mut()
            .ok_or("antigravity settings must be an object")?;
        if let Some(m_val) = ag.get_mut("model")
            && !m_val.is_null()
        {
            let m = m_val
                .as_str()
                .map(str::trim)
                .ok_or("Antigravity model must be a string")?
                .to_owned();
            if !crate::antigravity_cli::is_valid_model_id(&m) {
                return Err("Enter a valid Antigravity model ID (no slashes or spaces)".into());
            }
            *m_val = Value::String(m);
        }
        if let Some(e_val) = ag.get_mut("effort")
            && !e_val.is_null()
        {
            let e = e_val
                .as_str()
                .map(|s| s.trim().to_ascii_lowercase())
                .ok_or("Antigravity effort must be low, medium, or high")?;
            let normalized = match e.as_str() {
                "low" | "medium" | "high" => e,
                "med" => "medium".to_owned(),
                _ => return Err("Antigravity effort must be low, medium, or high".into()),
            };
            *e_val = Value::String(normalized);
        }
        if let Some(agy_path) = ag.get_mut("agy_path") {
            normalize_optional_path(agy_path, "agy_path")?;
        }
        if let Some(sandbox_dir) = ag.get_mut("sandbox_dir") {
            normalize_optional_path(sandbox_dir, "sandbox_dir")?;
        }
    }
    if !["off", "minimal", "low", "medium", "high", "xhigh", "max"]
        .contains(&v["reasoning_effort"].as_str().unwrap_or(""))
    {
        return Err("Unsupported thinking effort".into());
    }
    if !["auto", "male", "female"].contains(&v["voice_mode"].as_str().unwrap_or("")) {
        return Err("Voice must be auto, male or female".into());
    }
    for k in [
        "include_history",
        "native_real_interval_override",
        "keep_both_suno_variants",
    ] {
        if v[k].as_bool().is_none() {
            return Err(format!("{k} must be boolean"));
        }
    }
    if !v["suno_duration_seconds"].is_null()
        && !v["suno_duration_seconds"]
            .as_u64()
            .is_some_and(|d| (120..=179).contains(&d))
    {
        return Err("Suno duration must be blank or 120–179 seconds".into());
    }
    if !v["interval_real_gameplay_seconds"]
        .as_u64()
        .is_some_and(|d| (60..=86400).contains(&d))
    {
        return Err("Gameplay interval must be 1–1440 minutes".into());
    }
    if v["voice_mode"] == "male" {
        v["vocal_gender"] = json!("m")
    }
    if v["voice_mode"] == "female" {
        v["vocal_gender"] = json!("f")
    }
    if !ids.contains(&model) {
        v["model_profiles"]
            .as_array_mut()
            .unwrap()
            .push(json!({"id":model,"label":model,"enabled":true}));
    }
    Ok(v)
}
#[cfg(test)]
mod tests {
    use super::*;
    fn settings() -> Value {
        json!({"selected_model":"z-ai/glm-5.3-flash","reasoning_effort":"high","include_history":true,"voice_mode":"auto","suno_duration_seconds":null,"interval_real_gameplay_seconds":480,"native_real_interval_override":false,"keep_both_suno_variants":true,"model_profiles":[{"id":"z-ai/glm-5.3-flash","enabled":true},{"id":"deepseek/disabled","enabled":false}],"authorization":"preserve","provider_path":"preserve"})
    }
    #[test]
    fn preserve_unknown_fields() {
        let v = patch_music(&settings(), &json!({"voice_mode":"male"})).unwrap();
        assert_eq!(v["authorization"], "preserve");
        assert_eq!(v["provider_path"], "preserve");
        assert_eq!(v["vocal_gender"], "m");
    }
    #[test]
    fn reject_disabled_or_invalid() {
        for p in [
            json!({"selected_model":"deepseek/disabled"}),
            json!({"suno_duration_seconds":180}),
            json!({"include_history":"true"}),
            json!({"native_spending_mode":"native_automatic"}),
            json!({"llm_backend":"other"}),
            json!({"lyrics_provider":"other"}),
            json!({"antigravity":{"model":"invalid/slash"}}),
            json!({"antigravity":{"effort":"turbo"}}),
        ] {
            assert!(patch_music(&settings(), &p).is_err());
        }
    }
    #[test]
    fn antigravity_patch_merges_and_normalizes_blank_paths() {
        let mut base = settings();
        base["antigravity"] = json!({"model":"gemini-3.8-flash-low","effort":"low","extra":"keep"});
        let updated = patch_music(
            &base,
            &json!({
                "llm_backend": "antigravity",
                "lyrics_provider": "antigravity",
                "antigravity": {
                    "model": "gemini-4-argon-high",
                    "effort": "high",
                    "agy_path": "  ",
                    "sandbox_dir": ""
                }
            }),
        )
        .unwrap();
        assert_eq!(updated["llm_backend"], "antigravity");
        assert_eq!(updated["lyrics_provider"], "antigravity");
        assert_eq!(updated["antigravity"]["model"], "gemini-4-argon-high");
        assert_eq!(updated["antigravity"]["effort"], "high");
        assert_eq!(updated["antigravity"]["extra"], "keep");
        assert!(updated["antigravity"]["agy_path"].is_null());
        assert!(updated["antigravity"]["sandbox_dir"].is_null());
    }
    #[test]
    fn custom_model_has_no_inherited_price_cap() {
        let v = patch_music(&settings(), &json!({"selected_model":"test/model"})).unwrap();
        assert!(v["model_profiles"][2]["max_price"].is_null());
    }
    #[test]
    fn load_resolves_relative_roots_and_defaults() {
        let dir = std::env::temp_dir().join(format!(
            "skyrim-cc-cfg-{}-{}",
            std::process::id(),
            now().to_bits()
        ));
        fs::create_dir_all(&dir).unwrap();
        write_json(
            &dir.join("control-center.json"),
            &json!({"agentPort":19464,"dependencies":[]}),
        )
        .unwrap();
        let cfg = Config::load(&dir).unwrap();
        assert_eq!(cfg.music_root, dir.join("runtime/bard"));
        assert_eq!(cfg.feedback_root, dir.join("runtime/psvr2"));
        fs::remove_dir_all(dir).unwrap();
    }
}
