use crate::{
    config::{self, Config, Result},
    platform,
};
use serde_json::{Value, json};
use std::{fs, path::PathBuf};
pub(crate) fn settings_path(c: &Config) -> Result<PathBuf> {
    let path = config::feedback_install_path(
        &c.feedback_root,
        "driverDir",
        "Installed driver directory missing",
    )?;
    Ok(path
        .ancestors()
        .nth(6)
        .ok_or("Unexpected Steam installation path")?
        .join("config/steamvr.vrsettings"))
}
fn read(c: &Config) -> Result<Value> {
    let bytes = fs::read(settings_path(c)?).map_err(|e| e.to_string())?;
    let text = String::from_utf8(bytes).map_err(|e| e.to_string())?;
    serde_json::from_str(text.trim_start_matches('\u{feff}')).map_err(|e| e.to_string())
}
pub fn options(c: &Config) -> Result<Value> {
    let v = read(c)?;
    Ok(
        json!({"useToolkitSync":v["playstation_vr2_ex"]["useToolkitSync"].as_bool().unwrap_or(true),"useEnhancedHaptics":v["playstation_vr2_ex"]["useEnhancedHaptics"].as_bool().unwrap_or(true),"roomViewCamera":v["camera"]["enableCamera"].as_bool().unwrap_or(false)}),
    )
}
pub fn apply(c: &Config, values: &Value) -> Result<Value> {
    let fields = values
        .as_object()
        .ok_or("Driver options must be an object")?;
    if fields.len() != 3
        || ["useToolkitSync", "useEnhancedHaptics", "roomViewCamera"]
            .iter()
            .any(|k| values[*k].as_bool().is_none())
    {
        return Err("Invalid driver options".into());
    }
    let running = platform::checked_processes()?;
    if running.iter().any(|(_, n)| {
        [
            "vrserver.exe",
            "vrmonitor.exe",
            "vrcompositor.exe",
            "SkyrimVR.exe",
            "PlayStationVR2App.exe",
        ]
        .iter()
        .any(|name| n.eq_ignore_ascii_case(name))
    }) {
        return Err(
            "Close Skyrim, SteamVR and the PlayStation VR2 App before applying driver options"
                .into(),
        );
    }
    let path = settings_path(c)?;
    let mut v = read(c)?;
    let folder = c.feedback_root.join(".local/runtime-backups");
    fs::create_dir_all(&folder).map_err(|e| e.to_string())?;
    fs::copy(
        &path,
        folder.join(format!(
            "{}-rust.vrsettings",
            (config::now() * 1000.) as u64
        )),
    )
    .map_err(|e| e.to_string())?;
    for key in ["useToolkitSync", "useEnhancedHaptics"] {
        v["playstation_vr2_ex"][key] = values[key].clone();
    }
    v["camera"]["enableCamera"] = values["roomViewCamera"].clone();
    config::write_json(&path, &v)?;
    Ok(
        json!({"message":"Three driver options saved with a backup; they apply after SteamVR restarts"}),
    )
}
