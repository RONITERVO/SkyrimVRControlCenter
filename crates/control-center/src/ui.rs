#![allow(clippy::too_many_arguments)] // Native Win32 control geometry mirrors CreateWindowEx.
use crate::{
    antigravity_cli,
    config::{Config, Result, wide},
    service::Service,
};
use serde_json::{Value, json};
use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    path::PathBuf,
    ptr::{null, null_mut},
};
use windows_sys::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, WPARAM},
    Graphics::Gdi::{
        CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS, COLOR_WINDOW, CreateFontW, DEFAULT_CHARSET,
        DEFAULT_PITCH, DeleteObject, FF_DONTCARE, FW_NORMAL, HFONT, OUT_DEFAULT_PRECIS,
    },
    System::LibraryLoader::GetModuleHandleW,
    UI::{
        Controls::{
            ICC_PROGRESS_CLASS, INITCOMMONCONTROLSEX, InitCommonControlsEx, PBM_SETPOS,
            PBM_SETRANGE32, PBS_SMOOTH,
        },
        Input::KeyboardAndMouse::EnableWindow,
        WindowsAndMessaging::*,
    },
};
const MUSIC: u16 = 10;
const ANTIGRAVITY: u16 = 17;
const FEEDBACK: u16 = 11;
const STARTUP: u16 = 12;
const DRIVER: u16 = 13;
const NR: u16 = 14;
const DOWNLOADS: u16 = 15;
const SHADERS: u16 = 16;

const PAGE_MUSIC: usize = 0;
const PAGE_ANTIGRAVITY: usize = 1;
const PAGE_FEEDBACK: usize = 2;
const PAGE_STARTUP: usize = 3;
const PAGE_DRIVER: usize = 4;
const PAGE_NR: usize = 5;
const PAGE_DOWNLOADS: usize = 6;
const PAGE_SHADERS: usize = 7;

pub struct App {
    hwnd: HWND,
    font: HFONT,
    controls: HashMap<u16, HWND>,
    pages: [Vec<HWND>; 8],
    page: usize,
    service: Service,
    config: Config,
    profiles: Vec<Value>,
    smoke: bool,
    last_progress_permille: Cell<u16>,
    last_precompile_active: Cell<Option<bool>>,
    last_models_key: RefCell<String>,
}
impl App {
    unsafe fn add(
        &mut self,
        page: Option<usize>,
        id: u16,
        class: &str,
        text: &str,
        style: u32,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
    ) -> HWND {
        let cls = wide(class);
        let txt = wide(text);
        let child = unsafe {
            CreateWindowExW(
                0,
                cls.as_ptr(),
                txt.as_ptr(),
                WS_CHILD | WS_VISIBLE | style,
                x,
                y,
                w,
                h,
                self.hwnd,
                id as usize as _,
                GetModuleHandleW(null()),
                null(),
            )
        };
        unsafe {
            SendMessageW(child, WM_SETFONT, self.font as usize, 1);
        }
        self.controls.insert(id, child);
        if let Some(p) = page {
            self.pages[p].push(child);
        }
        child
    }
    unsafe fn label(&mut self, p: usize, id: u16, text: &str, x: i32, y: i32, w: i32, h: i32) {
        unsafe {
            self.add(Some(p), id, "STATIC", text, 0, x, y, w, h);
        }
    }
    unsafe fn button(&mut self, p: usize, id: u16, text: &str, x: i32, y: i32, w: i32) {
        unsafe {
            self.add(
                Some(p),
                id,
                "BUTTON",
                text,
                WS_TABSTOP | BS_PUSHBUTTON as u32,
                x,
                y,
                w,
                34,
            );
        }
    }
    unsafe fn check(
        &mut self,
        p: usize,
        id: u16,
        text: &str,
        checked: bool,
        x: i32,
        y: i32,
        w: i32,
    ) {
        let h = unsafe {
            self.add(
                Some(p),
                id,
                "BUTTON",
                text,
                WS_TABSTOP | BS_AUTOCHECKBOX as u32,
                x,
                y,
                w,
                28,
            )
        };
        unsafe {
            SendMessageW(h, BM_SETCHECK, if checked { 1 } else { 0 }, 0);
        }
    }
    unsafe fn edit(&mut self, p: usize, id: u16, text: &str, x: i32, y: i32, w: i32) {
        unsafe {
            self.add(
                Some(p),
                id,
                "EDIT",
                text,
                WS_TABSTOP | WS_BORDER | ES_AUTOHSCROLL as u32,
                x,
                y,
                w,
                28,
            );
        }
    }
    unsafe fn combo(
        &mut self,
        p: usize,
        id: u16,
        options: &[&str],
        selected: &str,
        x: i32,
        y: i32,
        w: i32,
        editable: bool,
    ) {
        let h = unsafe {
            self.add(
                Some(p),
                id,
                "COMBOBOX",
                "",
                WS_TABSTOP
                    | WS_VSCROLL
                    | if editable {
                        CBS_DROPDOWN as u32
                    } else {
                        CBS_DROPDOWNLIST as u32
                    },
                x,
                y,
                w,
                270,
            )
        };
        for (i, s) in options.iter().enumerate() {
            let text = wide(s);
            unsafe {
                SendMessageW(h, CB_ADDSTRING, 0, text.as_ptr() as isize);
                if *s == selected {
                    SendMessageW(h, CB_SETCURSEL, i, 0);
                }
            }
        }
        if editable {
            let txt = wide(selected);
            unsafe {
                SetWindowTextW(h, txt.as_ptr());
            }
        }
    }
    unsafe fn build(&mut self) -> Result<()> {
        let settings = self.config.music_settings()?;
        let trigger = self.config.feedback_settings()?;
        unsafe {
            self.add(
                None,
                1,
                "STATIC",
                "Skyrim VR Control Center",
                0,
                26,
                20,
                1080,
                30,
            );
            self.add(
                None,
                2,
                "STATIC",
                if self.service.status.lock().unwrap()["preview"] == true {
                    "PREVIEW — read-only; native services and hardware are inactive"
                } else {
                    "Native controls · music relay stays available for NPC dialogue"
                },
                0,
                26,
                55,
                1080,
                24,
            );
            self.add(
                None,
                3,
                "STATIC",
                "Checking local music & LLM relay…",
                0,
                26,
                83,
                1080,
                24,
            );
            self.add(
                None,
                4,
                "STATIC",
                "Checking PSVR2 feedback…",
                0,
                26,
                110,
                1080,
                24,
            );
            for (i, (id, title)) in [
                (MUSIC, "Songs / models"),
                (ANTIGRAVITY, "Antigravity"),
                (FEEDBACK, "PSVR2 feedback"),
                (STARTUP, "Start & setup"),
                (DRIVER, "Driver features"),
                (NR, "NR guard"),
                (DOWNLOADS, "Song downloads"),
                (SHADERS, "Shader cache"),
            ]
            .into_iter()
            .enumerate()
            {
                self.add(
                    None,
                    id,
                    "BUTTON",
                    title,
                    WS_TABSTOP,
                    26 + i as i32 * 136,
                    146,
                    128,
                    36,
                );
            }
            self.add(None, 5, "STATIC", "", 0, 26, 781, 1080, 58);

            // Page 0: Songs / models
            self.label(PAGE_MUSIC, 100, "Next song preferences", 26, 204, 340, 28);
            self.label(PAGE_MUSIC, 107, "Bard lyrics provider", 490, 204, 185, 24);
            self.combo(
                PAGE_MUSIC,
                118,
                &["antigravity", "openrouter"],
                settings["lyrics_provider"].as_str().unwrap_or("openrouter"),
                680,
                200,
                180,
                false,
            );
            self.label(
                PAGE_MUSIC,
                101,
                "Active OpenRouter model (editable author/model ID — used when OpenRouter is selected)",
                26,
                240,
                820,
                24,
            );
            let ids: Vec<String> = settings["model_profiles"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|p| p["enabled"] == true)
                .filter_map(|p| p["id"].as_str().map(str::to_owned))
                .collect();
            let refs: Vec<&str> = ids.iter().map(String::as_str).collect();
            self.combo(
                PAGE_MUSIC,
                110,
                &refs,
                settings["selected_model"].as_str().unwrap_or(""),
                26,
                268,
                620,
                true,
            );
            self.profiles = settings["model_profiles"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            for (i, p) in self.profiles.clone().iter().take(4).enumerate() {
                self.check(
                    PAGE_MUSIC,
                    120 + i as u16,
                    p["label"]
                        .as_str()
                        .or_else(|| p["id"].as_str())
                        .unwrap_or("Model"),
                    p["enabled"] == true,
                    26 + (i % 2) as i32 * 460,
                    310 + (i / 2) as i32 * 30,
                    450,
                );
            }
            self.label(PAGE_MUSIC, 102, "Thinking effort", 26, 393, 180, 24);
            self.combo(
                PAGE_MUSIC,
                111,
                &["off", "minimal", "low", "medium", "high", "xhigh", "max"],
                settings["reasoning_effort"].as_str().unwrap_or("high"),
                210,
                389,
                150,
                false,
            );
            self.label(PAGE_MUSIC, 103, "Lyric / draft voice", 490, 393, 150, 24);
            self.combo(
                PAGE_MUSIC,
                112,
                &["auto", "male", "female"],
                settings["voice_mode"].as_str().unwrap_or("auto"),
                650,
                389,
                210,
                false,
            );
            self.check(
                PAGE_MUSIC,
                113,
                "Use full cleaned songwriting history",
                settings["include_history"] == true,
                26,
                430,
                435,
            );
            self.label(
                PAGE_MUSIC,
                114,
                "Both variants: see Song downloads tab",
                490,
                430,
                455,
                24,
            );
            self.label(
                PAGE_MUSIC,
                104,
                "Standalone Suno seconds (blank = auto)",
                26,
                476,
                360,
                24,
            );
            self.edit(
                PAGE_MUSIC,
                115,
                &settings["suno_duration_seconds"]
                    .as_u64()
                    .map(|v| v.to_string())
                    .unwrap_or_default(),
                390,
                472,
                90,
            );
            self.label(
                PAGE_MUSIC,
                105,
                "Minimum active-play interval (minutes)",
                490,
                476,
                320,
                24,
            );
            self.edit(
                PAGE_MUSIC,
                116,
                &format!(
                    "{}",
                    settings["interval_real_gameplay_seconds"]
                        .as_u64()
                        .unwrap_or(480)
                        / 60
                ),
                830,
                472,
                95,
            );
            self.check(
                PAGE_MUSIC,
                117,
                "Enable optional real-play interval override; otherwise use native timing",
                settings["native_real_interval_override"] == true,
                26,
                512,
                880,
            );
            self.button(PAGE_MUSIC, 180, "Save song preferences", 26, 555, 235);
            self.button(PAGE_MUSIC, 181, "Open SkyrimNet dashboard", 275, 555, 250);
            self.button(PAGE_MUSIC, 182, "Pause new custom lyrics", 26, 607, 235);
            self.button(PAGE_MUSIC, 183, "Arm one song (~12 credits)", 275, 607, 250);
            self.button(PAGE_MUSIC, 184, "Resume authorized ongoing", 540, 607, 270);
            self.label(PAGE_MUSIC,106,"Auto uses the game's singer. Manual voice affects lyric context and standalone delivery.\r\nNative Suno keeps the game's duration and final voice; seconds apply to standalone review.\r\nPause gates new custom lyrics only; submitted music and native fallback may continue.\r\nSong downloads saves both variants; importing extra songs into SkyrimNet remains manual.\r\nWhen Bard lyrics provider is set to Antigravity, model & effort come from the Antigravity tab.",26,652,1070,125);

            // Page 1: Antigravity LLM & SkyrimNet routing
            self.label(
                PAGE_ANTIGRAVITY,
                1300,
                "Antigravity local LLM relay (zero-credit NPC dialogue, combat, vision & optional Bard lyrics)",
                26,
                204,
                1070,
                26,
            );
            self.add(
                Some(PAGE_ANTIGRAVITY),
                1301,
                "EDIT",
                "Checking Antigravity CLI, ACP worker, and SkyrimNet routes…",
                WS_BORDER | WS_VSCROLL | ES_MULTILINE as u32 | ES_READONLY as u32,
                26,
                234,
                1080,
                138,
            );
            self.label(
                PAGE_ANTIGRAVITY,
                1302,
                "SkyrimNet / NPC relay backend",
                26,
                390,
                230,
                24,
            );
            self.combo(
                PAGE_ANTIGRAVITY,
                1310,
                &["antigravity", "openrouter"],
                settings["llm_backend"].as_str().unwrap_or("openrouter"),
                260,
                386,
                190,
                false,
            );
            self.label(
                PAGE_ANTIGRAVITY,
                1303,
                "Bard lyrics provider",
                490,
                390,
                185,
                24,
            );
            self.combo(
                PAGE_ANTIGRAVITY,
                1311,
                &["antigravity", "openrouter"],
                settings["lyrics_provider"].as_str().unwrap_or("openrouter"),
                680,
                386,
                190,
                false,
            );
            let ag_model = settings["antigravity"]["model"]
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or(antigravity_cli::DEFAULT_ANTIGRAVITY_MODEL);
            let ag_effort_raw = settings["antigravity"]["effort"]
                .as_str()
                .or_else(|| settings["reasoning_effort"].as_str())
                .unwrap_or(antigravity_cli::DEFAULT_ANTIGRAVITY_EFFORT);
            let ag_effort = antigravity_cli::normalize_effort(ag_effort_raw);
            let mut ag_models: Vec<String> = antigravity_cli::DEFAULT_MODELS
                .iter()
                .map(|(id, _)| (*id).to_owned())
                .collect();
            if !ag_models.iter().any(|m| m == ag_model) {
                ag_models.insert(0, ag_model.to_owned());
            }
            let ag_model_refs: Vec<&str> = ag_models.iter().map(String::as_str).collect();
            self.label(
                PAGE_ANTIGRAVITY,
                1304,
                "Antigravity model ID",
                26,
                432,
                230,
                24,
            );
            self.combo(
                PAGE_ANTIGRAVITY,
                1312,
                &ag_model_refs,
                ag_model,
                260,
                428,
                390,
                true,
            );
            self.label(PAGE_ANTIGRAVITY, 1305, "Effort tier", 680, 432, 100, 24);
            self.combo(
                PAGE_ANTIGRAVITY,
                1313,
                &["low", "medium", "high"],
                ag_effort,
                785,
                428,
                140,
                false,
            );
            self.label(
                PAGE_ANTIGRAVITY,
                1306,
                "agy.exe path (blank = auto)",
                26,
                474,
                230,
                24,
            );
            self.edit(
                PAGE_ANTIGRAVITY,
                1314,
                settings["antigravity"]["agy_path"].as_str().unwrap_or(""),
                260,
                470,
                390,
            );
            self.label(
                PAGE_ANTIGRAVITY,
                1307,
                "Sandbox folder",
                680,
                474,
                120,
                24,
            );
            self.edit(
                PAGE_ANTIGRAVITY,
                1315,
                settings["antigravity"]["sandbox_dir"].as_str().unwrap_or(""),
                805,
                470,
                301,
            );
            self.button(
                PAGE_ANTIGRAVITY,
                1380,
                "Save Antigravity settings",
                26,
                514,
                260,
            );
            self.button(
                PAGE_ANTIGRAVITY,
                1381,
                "Refresh models from agy",
                300,
                514,
                260,
            );
            self.button(
                PAGE_ANTIGRAVITY,
                1382,
                "Restart ACP worker",
                574,
                514,
                250,
            );
            self.button(
                PAGE_ANTIGRAVITY,
                1383,
                "Open sandbox folder",
                838,
                514,
                268,
            );
            self.label(
                PAGE_ANTIGRAVITY,
                1308,
                "SkyrimNet OpenRouter.yaml routing (requires Skyrim VR and Mod Organizer 2 to be closed)",
                26,
                568,
                1070,
                24,
            );
            self.button(
                PAGE_ANTIGRAVITY,
                1384,
                "Switch SkyrimNet -> Antigravity",
                26,
                600,
                275,
            );
            self.button(
                PAGE_ANTIGRAVITY,
                1385,
                "Restore SkyrimNet -> OpenRouter",
                315,
                600,
                275,
            );
            self.button(
                PAGE_ANTIGRAVITY,
                1386,
                "Open SkyrimNet config",
                604,
                600,
                245,
            );
            self.button(
                PAGE_ANTIGRAVITY,
                1387,
                "Open backups folder",
                863,
                600,
                243,
            );
            self.label(
                PAGE_ANTIGRAVITY,
                1309,
                "• Saving updates runtime/bard/settings.json immediately; the running relay applies backend, model, and effort changes on the next request.\r\n• Relay backend controls NPC dialogue, combat, evaluation, and vision; Bard lyrics provider controls custom song drafting.\r\n• Switching SkyrimNet routes updates OpenRouter.yaml slots to local_bard_composer (127.0.0.1:8097) while preserving openrouter_decisions and backing up originals.\r\n• Restoring routes restores the newest valid pre-Antigravity backup. All tool/filesystem requests from agy acp are auto-denied in headless relay mode.",
                26,
                650,
                1080,
                115,
            );

            // Page 2: PSVR2 feedback
            self.label(PAGE_FEEDBACK, 200, "L2 / R2 trigger feel", 26, 204, 860, 28);
            self.button(PAGE_FEEDBACK, 280, "Start Skyrim feedback", 26, 246, 230);
            self.button(PAGE_FEEDBACK, 281, "STOP CUSTOM TRIGGER EFFECTS", 275, 246, 350);
            self.check(
                PAGE_FEEDBACK,
                210,
                "Selection detent in supported menus",
                trigger.menus,
                26,
                295,
                460,
            );
            self.check(
                PAGE_FEEDBACK,
                211,
                "Progressive object / NPC grab resistance",
                trigger.grabs,
                490,
                295,
                450,
            );
            for (i, (id, title, value)) in [
                (220, "Menu click begins (2–7)", trigger.menu_start),
                (
                    221,
                    "Menu click ends (after start, up to 8)",
                    trigger.menu_end,
                ),
                (222, "Menu strength (1–4)", trigger.menu_strength),
                (223, "Grab resistance begins (0–8)", trigger.grab_start),
                (
                    224,
                    "Grab resistance ends (after start, up to 9)",
                    trigger.grab_end,
                ),
                (225, "Object strength (1–4)", trigger.object_strength),
                (226, "NPC strength (1–4)", trigger.actor_strength),
            ]
            .iter()
            .enumerate()
            {
                let y = 340 + i as i32 * 39;
                self.label(PAGE_FEEDBACK, 230 + i as u16, title, 26, y, 670, 27);
                self.edit(PAGE_FEEDBACK, *id, &value.to_string(), 770, y - 2, 125);
            }
            self.button(PAGE_FEEDBACK, 282, "Save trigger feel", 26, 636, 210);
            self.button(PAGE_FEEDBACK, 283, "Test menu · 2 sec", 250, 636, 210);
            self.button(PAGE_FEEDBACK, 284, "Test object · 2 sec", 475, 636, 210);
            self.button(PAGE_FEEDBACK, 285, "Test NPC · 2 sec", 700, 636, 210);
            self.label(PAGE_FEEDBACK,240,"Only L2/R2 have resistance actuators. Squeeze simulates a force curve; it does not deform game objects.\r\nEffects release on stale/invalid state, game exit, Stop, driver failure and closing this window.\r\nA driver-command failure requires an explicit Start. Hardware feel still needs a headset test.",26,694,925,74);

            // Page 3: Start & setup
            self.label(
                PAGE_STARTUP,
                300,
                "Start the installed VR dependencies",
                26,
                204,
                900,
                28,
            );
            self.button(PAGE_STARTUP, 380, "Start VR session dependencies", 26, 246, 330);
            self.button(PAGE_STARTUP, 381, "Start / check music relay", 375, 246, 275);
            let deps = self.config.dependencies.clone();
            for (i, d) in deps.iter().enumerate() {
                let y = 304 + i as i32 * 47;
                self.button(PAGE_STARTUP, 400 + i as u16, &format!("Open {}", d.label), 26, y, 460);
                self.label(PAGE_STARTUP, 430 + i as u16, "Checking process…", 510, y + 5, 410, 29);
            }
            self.check(
                PAGE_STARTUP,
                385,
                "Close KAT overlays once at session startup (or install zero-GPU stubs below)",
                self.config.close_kat_overlays,
                26,
                534,
                910,
            );
            self.button(PAGE_STARTUP, 382, "Official headset setup", 26, 568, 275);
            self.button(PAGE_STARTUP, 383, "Official CAPI tester", 320, 568, 250);
            self.button(PAGE_STARTUP, 384, "Close KAT VR overlays", 590, 568, 340);
            self.button(PAGE_STARTUP, 386, "Install KAT zero-GPU stubs (UAC)", 26, 612, 290);
            self.button(PAGE_STARTUP, 387, "Restore KAT overlays (UAC)", 328, 612, 250);
            self.label(PAGE_STARTUP, 388, "KAT overlays: checking…", 590, 618, 365, 28);
            self.label(PAGE_STARTUP,310,"Session startup opens configured prerequisites once and reports running processes. It does not launch Skyrim.\r\nUse MO2 after the headset and treadmill are ready. Close the CAPI tester before rearming feedback.\r\nZero-GPU stubs back up KAT's two Unity overlay EXEs as *.original.exe while keeping KAT Gateway and OCU walking intact.\r\nYour original helpers, driver, bindings, graphics configuration and recovery files are preserved.",26,656,930,114);

            // Page 4: Driver features
            self.label(
                PAGE_DRIVER,
                500,
                "Existing PSVR2Toolkit driver features",
                26,
                204,
                900,
                28,
            );
            let runtime = crate::runtime::options(&self.config).unwrap_or(
                json!({"useToolkitSync":true,"useEnhancedHaptics":true,"roomViewCamera":false}),
            );
            self.check(
                PAGE_DRIVER,
                510,
                "Custom controller LED synchronization",
                runtime["useToolkitSync"] == true,
                26,
                258,
                850,
            );
            self.check(
                PAGE_DRIVER,
                511,
                "Enhanced / PCM controller haptics",
                runtime["useEnhancedHaptics"] == true,
                26,
                302,
                850,
            );
            self.check(
                PAGE_DRIVER,
                512,
                "Enable SteamVR camera for Room View",
                runtime["roomViewCamera"] == true,
                26,
                346,
                850,
            );
            self.button(
                PAGE_DRIVER,
                580,
                "Apply driver options (close VR apps first)",
                26,
                404,
                480,
            );
            self.label(PAGE_DRIVER,501,"Changes require Skyrim, SteamVR and the official PlayStation VR2 App to be closed.\r\nA backup is saved before these three options are updated. Other graphics and driver settings are preserved.\r\n\r\nController prediction is provided by the toolkit. Night Mode, brightness and refresh rate are in SteamVR.\r\nPlay Area, eye calibration and assisted IPD adjustment are in Official headset setup.\r\n\r\nThis setup has no firmware change or jailbreak. Headset vibration and raw eye-camera / Babble access\r\nare unavailable; this panel adds no thunder, damage or rain headset effects.",26,467,930,230);

            // Page 5: NR guard
            self.label(PAGE_NR, 600, "Neural Rendering runtime guard", 26, 204, 900, 28);
            self.label(
                PAGE_NR,
                601,
                "Checking the reviewed NR runtime…",
                26,
                254,
                930,
                90,
            );
            self.button(PAGE_NR, 680, "Block NR runtime for next launch", 26, 370, 350);
            self.button(PAGE_NR, 681, "Restore NR runtime", 395, 370, 285);
            self.label(PAGE_NR,602,"Close Skyrim and Mod Organizer before changing this guard. Turn NR off in the current settings first.\r\n\r\nThis hides only the reviewed nvngx_dlssnr.dll using MO2's .mohidden suffix. The exact file hash is\r\nchecked before changing it. Restore reverses that rename. DLSS/DLAA and all settings are preserved.\r\n\r\nThe shader mod is shared: NR stays unavailable in every profile using it until restored.\r\nThis does not unload memory from a running game. A fresh launch is required, and memory savings\r\nstill need measurement in a comparable gameplay scene.",26,444,930,220);

            // Page 6: Song downloads
            self.label(PAGE_DOWNLOADS, 1100, "Existing Suno song variants", 26, 204, 930, 28);
            self.check(
                PAGE_DOWNLOADS,
                1140,
                "Auto-download both variants while the background music relay runs",
                settings["keep_both_suno_variants"] == true,
                26,
                246,
                930,
            );
            self.button(PAGE_DOWNLOADS, 1180, "Download new variants", 26, 292, 220);
            self.button(PAGE_DOWNLOADS, 1181, "Open to-import folder", 258, 292, 220);
            self.button(PAGE_DOWNLOADS, 1182, "Open cached folder", 490, 292, 220);
            self.button(PAGE_DOWNLOADS, 1183, "Open all song folders", 722, 292, 235);
            self.add(
                Some(PAGE_DOWNLOADS),
                1101,
                "EDIT",
                "Waiting for download status...",
                WS_BORDER | WS_VSCROLL | ES_MULTILINE as u32 | ES_READONLY as u32,
                26,
                344,
                1080,
                220,
            );
            self.label(
                PAGE_DOWNLOADS,
                1102,
                "Optional existing task ID (copy from the list above)",
                26,
                589,
                740,
                24,
            );
            self.edit(PAGE_DOWNLOADS, 1110, "", 26, 619, 510);
            self.button(PAGE_DOWNLOADS, 1184, "Add task", 550, 617, 150);
            self.button(PAGE_DOWNLOADS, 1185, "Recover expired links...", 712, 617, 245);
            self.button(PAGE_DOWNLOADS, 1186, "Refresh Suno credits", 26, 670, 220);
            self.label(PAGE_DOWNLOADS,1103,"Uses existing requests only. No new song is generated by these controls.\r\nImport MP3 + matching YAML together in SkyrimNet. Already cached files need no import.\r\nUnconfirmed means cache membership could not be established. Recovery may use credits.",258,668,700,102);

            // Page 7: Shader cache
            self.label(
                PAGE_SHADERS,
                1200,
                "Open Shaders / Community Shaders cache & headless pre-compile",
                26,
                204,
                930,
                26,
            );
            self.add(
                Some(PAGE_SHADERS),
                1201,
                "EDIT",
                "Checking MO2 profile and ShaderCache status…",
                WS_BORDER | WS_VSCROLL | ES_MULTILINE as u32 | ES_READONLY as u32,
                26,
                234,
                1080,
                144,
            );
            self.check(
                PAGE_SHADERS,
                1210,
                "Clear existing ShaderCache before pre-compile / reload & enforce upfront boot compile (bDepthBufferCulling=0)",
                true,
                26,
                386,
                1080,
            );
            self.button(PAGE_SHADERS, 1280, "Clear shaders & prepare reload", 26, 420, 310);
            self.button(PAGE_SHADERS, 1281, "Restore last shader backup", 348, 420, 280);
            self.button(PAGE_SHADERS, 1282, "Apply upfront settings only", 640, 420, 316);
            self.button(PAGE_SHADERS, 1284, "Pre-compile shaders now (no headset)", 26, 462, 350);
            self.button(PAGE_SHADERS, 1285, "Cancel pre-compile", 388, 462, 220);
            self.button(PAGE_SHADERS, 1283, "Open shader backups folder", 620, 462, 336);
            EnableWindow(self.controls[&1285], 0);
            self.label(
                PAGE_SHADERS,
                1203,
                "Headless pre-compile: idle — PSVR2 headset, PSVR2 App, and KAT can stay off",
                26,
                506,
                1080,
                24,
            );
            let prog = self.add(
                Some(PAGE_SHADERS),
                1204,
                "msctls_progress32",
                "",
                WS_BORDER | PBS_SMOOTH,
                26,
                532,
                1080,
                24,
            );
            if prog.is_null() {
                return Err("Could not create native Win32 progress bar control".into());
            }
            SendMessageW(prog, PBM_SETRANGE32, 0, 1000);
            SendMessageW(prog, PBM_SETPOS, 0, 0);
            self.label(
                PAGE_SHADERS,
                1202,
                "Close Skyrim VR, MO2, SteamVR, and PlayStation VR2 App before starting. Headset and KAT can stay off.\r\n\r\n• Pre-compile shaders now backs up active ShaderCache layers, temporarily runs SteamVR in headless null-driver\r\n  mode (1024x1024) + Skyrim VR via MO2 to compile all ~3,600 shaders to disk before Main Menu, then closes\r\n  Skyrim VR / MO2 / SteamVR automatically and restores your exact PSVR2 SteamVR settings.\r\n• Clear shaders moves every active ShaderCache in the selected MO2 profile into MO2's ShaderCache_Backups.\r\n• Upfront boot compile disables BackgroundShaderCompilationOnBoot and sets [VRPerformance] bDepthBufferCulling=0\r\n  so VolumetricLighting never wipes compiled shaders at Main Menu. No savegame load or save is needed.",
                26,
                566,
                1080,
                206,
            );
            self.show_page(PAGE_MUSIC);
        }
        Ok(())
    }
    unsafe fn show_page(&mut self, p: usize) {
        self.page = p;
        for (i, controls) in self.pages.iter().enumerate() {
            for h in controls {
                unsafe {
                    ShowWindow(*h, if i == p { SW_SHOW } else { SW_HIDE });
                }
            }
        }
    }
    fn text(&self, id: u16) -> String {
        unsafe {
            let h = self.controls[&id];
            let len = GetWindowTextLengthW(h);
            let mut buf = vec![0u16; (len + 1) as usize];
            let n = GetWindowTextW(h, buf.as_mut_ptr(), buf.len() as i32);
            String::from_utf16_lossy(&buf[..n as usize])
        }
    }
    fn checked(&self, id: u16) -> bool {
        unsafe { SendMessageW(self.controls[&id], BM_GETCHECK, 0, 0) == 1 }
    }
    fn set(&self, id: u16, text: &str) {
        if self.text(id) == text {
            return;
        }
        let value = wide(text);
        unsafe {
            SetWindowTextW(self.controls[&id], value.as_ptr());
        }
    }
    fn combo_selected_text(&self, id: u16) -> Option<String> {
        unsafe {
            let h = self.controls[&id];
            let idx = SendMessageW(h, CB_GETCURSEL, 0, 0) as isize;
            if idx < 0 {
                return None;
            }
            let len = SendMessageW(h, CB_GETLBTEXTLEN, idx as usize, 0) as isize;
            if len < 0 {
                return None;
            }
            let mut buf = vec![0u16; (len + 1) as usize];
            let n = SendMessageW(h, CB_GETLBTEXT, idx as usize, buf.as_mut_ptr() as isize);
            if n < 0 {
                return None;
            }
            Some(String::from_utf16_lossy(&buf[..n as usize]))
        }
    }
    fn set_combo_selection(&self, id: u16, value: &str, editable: bool) {
        let wide_val = wide(value);
        unsafe {
            let h = self.controls[&id];
            let idx = SendMessageW(h, CB_FINDSTRINGEXACT, usize::MAX, wide_val.as_ptr() as isize)
                as isize;
            if idx >= 0 {
                SendMessageW(h, CB_SETCURSEL, idx as usize, 0);
            } else if editable {
                SendMessageW(h, CB_SETCURSEL, usize::MAX, 0);
            }
            if editable {
                self.set(id, value);
            }
        }
    }
    fn replace_combo_items(&self, id: u16, items: &[String], preserve_text: &str) {
        unsafe {
            let h = self.controls[&id];
            SendMessageW(h, CB_RESETCONTENT, 0, 0);
            for item in items {
                let txt = wide(item);
                SendMessageW(h, CB_ADDSTRING, 0, txt.as_ptr() as isize);
            }
        }
        self.set_combo_selection(id, preserve_text, true);
    }
    fn command(&mut self, id: u16, notify: u16) -> Result<()> {
        if notify == CBN_SELCHANGE as u16 {
            match id {
                118 => {
                    if let Some(val) = self.combo_selected_text(118) {
                        self.set_combo_selection(1311, &val, false);
                    }
                    return Ok(());
                }
                1311 => {
                    if let Some(val) = self.combo_selected_text(1311) {
                        self.set_combo_selection(118, &val, false);
                    }
                    return Ok(());
                }
                1312 => {
                    if let Some(selected) = self.combo_selected_text(1312)
                        && let Some(tier) = antigravity_cli::effort_from_model(&selected)
                    {
                        self.set_combo_selection(1313, tier, false);
                    }
                    return Ok(());
                }
                1313 => {
                    if let Some(tier) = self.combo_selected_text(1313) {
                        let current_model = self.text(1312);
                        let updated = antigravity_cli::apply_effort_to_model(&current_model, &tier);
                        self.set_combo_selection(1312, &updated, true);
                        if let Some(actual_tier) = antigravity_cli::effort_from_model(&updated)
                            && actual_tier != tier
                        {
                            self.set_combo_selection(1313, actual_tier, false);
                            self.set(5, &format!("{updated} only supports {actual_tier} effort"));
                        }
                    }
                    return Ok(());
                }
                _ => {}
            }
        }
        match id{
            MUSIC=>unsafe{self.show_page(PAGE_MUSIC)},
            ANTIGRAVITY=>unsafe{self.show_page(PAGE_ANTIGRAVITY)},
            FEEDBACK=>unsafe{self.show_page(PAGE_FEEDBACK)},
            STARTUP=>unsafe{self.show_page(PAGE_STARTUP)},
            DRIVER=>unsafe{self.show_page(PAGE_DRIVER)},
            NR=>unsafe{self.show_page(PAGE_NR)},
            DOWNLOADS=>unsafe{self.show_page(PAGE_DOWNLOADS)},
            SHADERS=>unsafe{self.show_page(PAGE_SHADERS)},
            180=>{let duration=self.text(115);let duration=if duration.trim().is_empty(){Value::Null}else{json!(duration.trim().parse::<u64>().map_err(|_|"Suno seconds must be a number")?)};let minutes=self.text(116).trim().parse::<f64>().map_err(|_|"Interval must be a number")?;if !minutes.is_finite()||!(1. ..=1440.).contains(&minutes){return Err("Interval must be 1–1440 minutes".into())}let mut profiles=self.config.music_settings()?["model_profiles"].as_array().cloned().unwrap_or_default();for (i,p)in profiles.iter_mut().take(4).enumerate(){p["enabled"]=json!(self.checked(120+i as u16));}let lyrics_provider=self.text(118);self.set_combo_selection(1311,&lyrics_provider,false);self.service.send("music-save",json!({"lyrics_provider":lyrics_provider,"selected_model":self.text(110).trim(),"reasoning_effort":self.text(111),"voice_mode":self.text(112),"include_history":self.checked(113),"suno_duration_seconds":duration,"interval_real_gameplay_seconds":(minutes*60.).round()as u64,"native_real_interval_override":self.checked(117),"model_profiles":profiles}));},
            181=>self.service.send("dashboard",json!({})),182=>self.service.send("music-pause",json!({})),183=>self.service.send("music-arm-one",json!({})),184=>self.service.send("music-resume",json!({})),
            1380=>{
                let llm_backend=self.text(1310);
                let lyrics_provider=self.text(1311);
                let effort=antigravity_cli::normalize_effort(&self.text(1313)).to_owned();
                let raw_model=self.text(1312).trim().to_owned();
                if !antigravity_cli::is_valid_model_id(&raw_model){
                    return Err("Enter a valid Antigravity model ID (no slashes or spaces)".into());
                }
                let model=if antigravity_cli::effort_from_model(&raw_model).is_some(){
                    antigravity_cli::apply_effort_to_model(&raw_model,&effort)
                }else{
                    raw_model
                };
                self.set_combo_selection(1312,&model,true);
                self.set_combo_selection(118,&lyrics_provider,false);
                let agy_raw=self.text(1314).trim().to_owned();
                let sandbox_raw=self.text(1315).trim().to_owned();
                self.service.send("music-save",json!({
                    "llm_backend":llm_backend,
                    "lyrics_provider":lyrics_provider,
                    "antigravity":{
                        "model":model,
                        "effort":effort,
                        "agy_path":if agy_raw.is_empty(){Value::Null}else{json!(agy_raw)},
                        "sandbox_dir":if sandbox_raw.is_empty(){Value::Null}else{json!(sandbox_raw)},
                    }
                }));
            },
            1381=>{
                let agy_raw=self.text(1314).trim().to_owned();
                self.service.send("antigravity-refresh-models",json!({"agy_path":if agy_raw.is_empty(){Value::Null}else{json!(agy_raw)}}));
            },
            1382=>self.service.send("antigravity-restart",json!({})),
            1383=>self.service.send("antigravity-open-sandbox",json!({})),
            1384=>{
                let approved=unsafe{MessageBoxW(self.hwnd,wide("Switch SkyrimNet OpenRouter.yaml routes to local Antigravity (127.0.0.1:8097)?\r\n\r\nRequires Skyrim VR and Mod Organizer 2 to be closed.\r\n• Backs up OpenRouter.yaml into backups/<timestamp>-before-antigravity/.\r\n• Updates 'provider_id: openrouter' slots to 'local_bard_composer'.\r\n• Keeps 'openrouter_decisions' untouched on OpenRouter.").as_ptr(),wide("Switch SkyrimNet Routes to Antigravity").as_ptr(),MB_YESNO|MB_ICONQUESTION)};
                if approved==IDYES{self.service.send("skyrimnet-switch-antigravity",json!({}));}
            },
            1385=>{
                let approved=unsafe{MessageBoxW(self.hwnd,wide("Restore SkyrimNet OpenRouter.yaml from the newest valid pre-Antigravity backup?\r\n\r\nRequires Skyrim VR and Mod Organizer 2 to be closed.").as_ptr(),wide("Restore SkyrimNet OpenRouter Routes").as_ptr(),MB_YESNO|MB_ICONQUESTION|MB_DEFBUTTON2)};
                if approved==IDYES{self.service.send("skyrimnet-restore-openrouter",json!({}));}
            },
            1386=>self.service.send("skyrimnet-open-config",json!({})),
            1387=>self.service.send("skyrimnet-open-backups",json!({})),
            280=>self.service.send("feedback-start",json!({})),281=>self.service.send("feedback-stop",json!({})),
            282=>{let current=self.service.status.lock().unwrap().clone();let mut v=current["feedbackSettings"].clone();v["enabled"]=json!(current["feedback"]["enabled"]==true);v["menus"]=json!(self.checked(210));v["grabs"]=json!(self.checked(211));for(id,key)in[(220,"menuStart"),(221,"menuEnd"),(222,"menuStrength"),(223,"grabStart"),(224,"grabEnd"),(225,"objectStrength"),(226,"actorStrength")]{v[key]=json!(self.text(id).trim().parse::<u8>().map_err(|_|format!("{key} must be an integer"))?);}self.service.send("feedback-save",v);},
            283..=285=>self.service.send("feedback-test",json!({"profile":(["menu","object","actor"][(id-283)as usize])})),
            380=>self.service.send("start-session",json!({})),381=>self.service.send("music-start",json!({})),382=>self.service.send("headset-setup",json!({})),383=>self.service.send("hardware-tester",json!({})),
            384=>self.service.send("kat-close-overlays",json!({})),385=>self.service.send("kat-overlay-setting",json!({"enabled":self.checked(385)})),
            386=>self.service.send("kat-stub-install",json!({})),387=>self.service.send("kat-stub-restore",json!({})),
            400..=429=>{if let Some(d)=self.config.dependencies.get((id-400)as usize){self.service.send("launch",json!({"id":d.id}));}},
            580=>self.service.send("driver-options",json!({"useToolkitSync":self.checked(510),"useEnhancedHaptics":self.checked(511),"roomViewCamera":self.checked(512)})),
            680=>self.service.send("nr-disable",json!({})),681=>self.service.send("nr-restore",json!({})),
            1140=>self.service.send("music-downloads-auto",json!({"enabled":self.checked(1140)})),
            1180=>self.service.send("music-downloads-sync",json!({})),
            1181=>self.service.send("music-downloads-open",json!({"folder":"to_import"})),
            1182=>self.service.send("music-downloads-open",json!({"folder":"cached"})),
            1183=>self.service.send("music-downloads-open",json!({})),
            1184=>self.service.send("music-downloads-task",json!({"task_id":self.text(1110).trim()})),
            1185=>{
                let approved=unsafe{MessageBoxW(self.hwnd,wide("Recover links for this existing task?\r\n\r\nThis does not generate music. Suno does not publish a fixed recovery price; the provider may use credits. The app records the balance before and after and never retries an uncertain POST.").as_ptr(),wide("Recover existing Suno tracks").as_ptr(),MB_YESNO|MB_ICONQUESTION|MB_DEFBUTTON2)};
                if approved==IDYES{self.service.send("music-downloads-recover",json!({"task_id":self.text(1110).trim(),"consent":"Recover existing tracks; provider credit cost may change"}));}
            },
            1186=>self.service.send("music-downloads-credits",json!({})),
            1280=>self.service.send("shader-cache-clear",json!({"enforceBootCompile":self.checked(1210)})),
            1281=>self.service.send("shader-cache-restore",json!({})),
            1282=>self.service.send("shader-cache-enforce-settings",json!({})),
            1283=>self.service.send("shader-cache-open-backups",json!({})),
            1284=>{
                let clear=self.checked(1210);
                let prompt=if clear{
                    "Start headless shader pre-compile now (no VR headset needed)?\r\n\r\nThis will:\r\n1. Back up & clear active ShaderCache folders and enforce upfront boot compile\r\n2. Temporarily start SteamVR in headless null-driver mode + Skyrim VR via MO2\r\n3. Compile all shaders to disk with live progress (~20–30 min on full MGO)\r\n4. Automatically close Skyrim VR / MO2 / SteamVR and restore PSVR2 settings.\r\n\r\nKeep your PSVR2 headset, PSVR2 App, and KAT Gateway off during pre-compile."
                }else{
                    "Start incremental headless shader pre-compile now (no VR headset needed)?\r\n\r\nExisting ShaderCache folders will be kept and only missing/changed shaders will compile.\r\nSteamVR will temporarily run in headless null-driver mode and automatically restore PSVR2 settings on completion."
                };
                let approved=unsafe{MessageBoxW(self.hwnd,wide(prompt).as_ptr(),wide("Headless Shader Pre-Compile").as_ptr(),MB_YESNO|MB_ICONQUESTION)};
                if approved==IDYES{self.service.send("shader-precompile-start",json!({"clearCache":clear}));}
            },
            1285=>self.service.send("shader-precompile-cancel",json!({})),
            _=>{}
        }
        Ok(())
    }
    fn tick(&self) {
        let s = self.service.status.lock().unwrap().clone();
        let h = &s["musicHealth"];
        let m_cfg = &s["music"];
        let music = if h["available"] == true {
            let npc_backend = h["llm_backend"]
                .as_str()
                .or_else(|| m_cfg["llm_backend"].as_str())
                .unwrap_or("openrouter");
            let lyrics_backend = h["lyrics_provider"]
                .as_str()
                .or_else(|| m_cfg["lyrics_provider"].as_str())
                .unwrap_or("openrouter");
            let active_model = h["selected_model"].as_str().unwrap_or("ready");
            let openrouter_model = h["openrouter_model"]
                .as_str()
                .or_else(|| m_cfg["selected_model"].as_str())
                .unwrap_or("ready");
            let route_label = if npc_backend == lyrics_backend {
                format!("{npc_backend} · {active_model}")
            } else {
                format!("NPC {npc_backend} ({active_model}) · Bard {lyrics_backend} ({openrouter_model})")
            };
            format!(
                "Relay: {} · {} · {}",
                h["runtime"].as_str().unwrap_or("existing adapter"),
                route_label,
                if h["ongoing_native_suno_authorized"] == true {
                    "ongoing native requests"
                } else {
                    "new requests paused / allowance controlled"
                }
            )
        } else {
            "Music relay: unavailable — start it before Skyrim uses custom dialogue/music".into()
        };
        self.set(3, &music);

        // Update Antigravity model dropdown if refreshed via `agy models`.
        if let Some(models_arr) = s["antigravityModels"].as_array() {
            let ids: Vec<String> = models_arr
                .iter()
                .filter_map(|m| m["id"].as_str().map(str::to_owned))
                .collect();
            if !ids.is_empty() {
                let key = ids.join("|");
                let mut prev = self.last_models_key.borrow_mut();
                if *prev != key {
                    if !prev.is_empty() {
                        let current_text = self.text(1312);
                        let mut merged = ids;
                        if !current_text.trim().is_empty()
                            && !merged.iter().any(|m| m == current_text.trim())
                        {
                            merged.insert(0, current_text.trim().to_owned());
                        }
                        self.replace_combo_items(1312, &merged, &current_text);
                    }
                    *prev = key;
                }
            }
        }

        // Antigravity status summary box
        let npc_backend = h["llm_backend"]
            .as_str()
            .or_else(|| m_cfg["llm_backend"].as_str())
            .unwrap_or("openrouter");
        let lyrics_backend = h["lyrics_provider"]
            .as_str()
            .or_else(|| m_cfg["lyrics_provider"].as_str())
            .unwrap_or("openrouter");
        let ag = &h["antigravity"];
        let cli = &s["antigravityCli"];
        let cfg_ag_model = m_cfg["antigravity"]["model"]
            .as_str()
            .unwrap_or(antigravity_cli::DEFAULT_ANTIGRAVITY_MODEL);
        let cfg_ag_effort = m_cfg["antigravity"]["effort"]
            .as_str()
            .or_else(|| m_cfg["reasoning_effort"].as_str())
            .map(antigravity_cli::normalize_effort)
            .unwrap_or(antigravity_cli::DEFAULT_ANTIGRAVITY_EFFORT);
        let resolved_ag_model = ag["resolved_model"]
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| antigravity_cli::apply_effort_to_model(cfg_ag_model, cfg_ag_effort));
        let agy_line = if ag["agy_found"] == true {
            format!(
                "Found ({})",
                ag["agy_path"].as_str().unwrap_or("agy.exe")
            )
        } else if cli["found"] == true {
            format!("Found ({})", cli["path"].as_str().unwrap_or("agy.exe"))
        } else {
            format!(
                "MISSING ({})",
                ag["agy_error"]
                    .as_str()
                    .or_else(|| cli["error"].as_str())
                    .unwrap_or("install agy CLI or set agy.exe path")
            )
        };
        let acp_line = if h["available"] != true {
            "Relay stopped — start music relay first".to_owned()
        } else if ag.is_null() {
            "Legacy relay running (restart relay outside Skyrim session for live ACP telemetry)".to_owned()
        } else {
            let state = if ag["acp_running"] == true {
                if ag["acp_initialized"] == true {
                    "running (initialized)"
                } else {
                    "starting"
                }
            } else {
                "idle (auto-starts on first request)"
            };
            let total = ag["total_requests"].as_u64().unwrap_or(0);
            let failed = ag["failed_requests"].as_u64().unwrap_or(0);
            let active = ag["active_sessions"].as_u64().unwrap_or(0);
            let latency = ag["last_latency_ms"]
                .as_u64()
                .map(|ms| format!("{ms} ms"))
                .unwrap_or_else(|| "none yet".into());
            let last_model = ag["last_model"].as_str().unwrap_or("none");
            let err_note = ag["last_error"]
                .as_str()
                .map(|e| format!(" | Last error: {e}"))
                .unwrap_or_default();
            format!(
                "{state} · active sessions: {active} · requests: {total} ok/total ({failed} failed) · last: {latency} ({last_model}){err_note}"
            )
        };
        let route = &s["skyrimnetRoute"];
        let route_line = match route["state"].as_str() {
            Some("antigravity") => format!(
                "Antigravity active ({} local_bard_composer, {} openrouter, {} decisions preserved)",
                route["localBardComposerSlots"].as_u64().unwrap_or(0),
                route["openRouterSlots"].as_u64().unwrap_or(0),
                route["decisionsSlots"].as_u64().unwrap_or(0),
            ),
            Some("openrouter") => format!(
                "OpenRouter active ({} openrouter, {} local_bard_composer, {} decisions)",
                route["openRouterSlots"].as_u64().unwrap_or(0),
                route["localBardComposerSlots"].as_u64().unwrap_or(0),
                route["decisionsSlots"].as_u64().unwrap_or(0),
            ),
            Some("partial") => format!(
                "Mixed/partial ({} local_bard_composer, {} openrouter, {} decisions)",
                route["localBardComposerSlots"].as_u64().unwrap_or(0),
                route["openRouterSlots"].as_u64().unwrap_or(0),
                route["decisionsSlots"].as_u64().unwrap_or(0),
            ),
            Some("missing") | Some("unconfigured") => route["note"]
                .as_str()
                .unwrap_or("SkyrimNet OpenRouter.yaml not found")
                .to_owned(),
            _ => "Checking SkyrimNet OpenRouter.yaml…".to_owned(),
        };
        let route_blockers = route["runningBlockers"]
            .as_array()
            .map(|v| {
                v.iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default();
        let route_ready = if !route_blockers.is_empty() {
            format!("Close before changing routes: {route_blockers}")
        } else if let Some(e) = route["processCheckError"].as_str() {
            e.to_owned()
        } else {
            let bak = route["latestBackup"]
                .as_str()
                .map(|b| format!(" | Clean OpenRouter backup: {b}"))
                .unwrap_or_else(|| " | No pre-Antigravity backup found yet".into());
            format!("Skyrim VR & MO2 closed (route switch ready){bak}")
        };
        self.set(
            1301,
            &format!(
                "Configured backends: Relay (NPC/SkyrimNet) = {npc_backend} | Bard lyrics = {lyrics_backend}\r\nActive Antigravity model: {resolved_ag_model} (configured: {cfg_ag_model}, effort: {cfg_ag_effort})\r\nAntigravity CLI (agy): {agy_line}\r\nACP worker: {acp_line}\r\nSkyrimNet OpenRouter.yaml: {route_line}\r\n{route_ready}"
            ),
        );
        let a = &h["song_downloads"];
        let credit = a["credits"]
            .as_f64()
            .map(|v| v.to_string())
            .unwrap_or_else(|| "not checked".into());
        let difference = a["last_credit_difference"]
            .as_f64()
            .map(|v| v.to_string())
            .unwrap_or_else(|| "not checked".into());
        let mut download_status = format!(
            "{}: {}\r\n{} saved | {} to import | {} already cached | {} unconfirmed\r\nLast checked Suno credits: {} | Last recovery balance difference: {}\r\nFolder: {}\r\n\r\n",
            a["state"].as_str().unwrap_or("Unavailable"),
            a["message"]
                .as_str()
                .unwrap_or("Start the updated music relay"),
            a["saved"].as_u64().unwrap_or(0),
            a["to_import"].as_u64().unwrap_or(0),
            a["cached"].as_u64().unwrap_or(0),
            a["unconfirmed"].as_u64().unwrap_or(0),
            credit,
            difference,
            a["folder"].as_str().unwrap_or("")
        );
        if let Some(tasks) = a["tasks"].as_array() {
            for t in tasks {
                download_status.push_str(&format!(
                    "{}  {}  {}\r\n",
                    t["id"].as_str().unwrap_or(""),
                    t["state"].as_str().unwrap_or(""),
                    t["error"].as_str().unwrap_or("")
                ));
            }
        }
        self.set(1101, &download_status);
        let f = &s["feedback"];
        let feedback = if let Some(e) = f["error"].as_str().filter(|e| !e.is_empty()) {
            format!("PSVR2: {e}")
        } else {
            format!(
                "PSVR2: {} · {} · L2 {} / R2 {}",
                if f["enabled"] == true {
                    "armed"
                } else {
                    "stopped"
                },
                if f["bridgeFresh"] == true {
                    "fresh Skyrim state"
                } else {
                    "waiting for Skyrim bridge"
                },
                f["leftProfile"].as_str().unwrap_or("off"),
                f["rightProfile"].as_str().unwrap_or("off")
            )
        };
        self.set(4, &feedback);
        self.set(5, s["lastAction"].as_str().unwrap_or("Ready"));
        if let Some(deps) = s["dependencies"].as_array() {
            for (i, d) in deps.iter().enumerate() {
                self.set(
                    430 + i as u16,
                    if d["running"] == true {
                        "Running"
                    } else if d["exists"] != true {
                        "Executable missing — check configuration"
                    } else {
                        "Not running"
                    },
                );
            }
        }
        let kat_state = match s["katOverlayStubs"]["state"].as_str() {
            Some("stubbed") => "KAT overlays: zero-GPU stubs active",
            Some("original") => "KAT overlays: original Unity binaries",
            Some("partial") => "KAT overlays: partially stubbed",
            Some("conflict") => "KAT overlays: update conflict (review)",
            Some("kat-missing") => "KAT overlays: KAT Gateway not found",
            Some("stub-exe-missing") => "KAT overlays: build kat-overlay-stub.exe",
            _ => "KAT overlays: checking…",
        };
        self.set(388, kat_state);
        let n = &s["nrGuard"];
        let blockers = n["runningBlockers"]
            .as_array()
            .map(|v| {
                v.iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default();
        self.set(
            601,
            &format!(
                "NR runtime: {}\r\nCurrent NR checkbox: {}\r\n{}",
                n["state"].as_str().unwrap_or("checking"),
                if n["nrCheckboxEnabled"] == true {
                    "on"
                } else if n["nrCheckboxEnabled"] == false {
                    "off"
                } else {
                    "unknown"
                },
                if n["state"] == "unconfigured" {
                    n["note"]
                        .as_str()
                        .unwrap_or("NR guard paths are not configured.")
                        .to_owned()
                } else if !blockers.is_empty() {
                    format!("Close before changing: {blockers}")
                } else if let Some(e) = n["processCheckError"].as_str() {
                    e.to_owned()
                } else {
                    "Game and MO2 appear closed; full file verification runs before a change."
                        .into()
                }
            ),
        );
        let sc = &s["shaderCache"];
        let sc_state = match sc["state"].as_str() {
            Some("cleared-ready-for-boot-compile") => {
                "Cleared — ready for full upfront compile on next Skyrim VR launch"
            }
            Some("cleared-settings-need-attention") => {
                "Cleared — upfront boot-compile settings are not all enabled"
            }
            Some("cached-upfront-ready") => {
                "Compiled cache present — upfront boot compile ready"
            }
            Some("cached-settings-need-attention") => {
                "Compiled cache present — background compile or depth-culling wipe active"
            }
            Some("missing-mo2") => "Configured MO2 folder not found",
            Some("unconfigured") => "Shader cache paths could not be resolved",
            _ => "Checking…",
        };
        let sc_sources = sc["activeCaches"]
            .as_array()
            .filter(|a| !a.is_empty())
            .map(|a| {
                a.iter()
                    .map(|c| {
                        format!(
                            "{} ({})",
                            c["source"].as_str().unwrap_or("cache"),
                            c["shaderCount"].as_u64().unwrap_or(0)
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_else(|| "none (ready to compile clean)".into());
        let bg_str = match sc["backgroundCompileOnBoot"].as_bool() {
            Some(false) => "false (upfront)",
            Some(true) => "true (loads mid-game)",
            None => "unknown",
        };
        let dc_str = match sc["diskCacheEnabled"].as_bool() {
            Some(true) => "on",
            Some(false) => "OFF",
            None => "unknown",
        };
        let dbc_str = match sc["depthBufferCulling"].as_bool() {
            Some(false) => "0 (safe)",
            Some(true) => "1 (WIPES CACHE AT MAIN MENU)",
            None => "unknown",
        };
        let latest_bak = sc["latestBackup"]
            .as_object()
            .map(|b| {
                format!(
                    " | Latest: {} ({} shaders)",
                    b.get("folder").and_then(Value::as_str).unwrap_or("backup"),
                    b.get("totalShaders").and_then(Value::as_u64).unwrap_or(0)
                )
            })
            .unwrap_or_default();
        let sc_blockers = sc["runningBlockers"]
            .as_array()
            .map(|v| {
                v.iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default();
        let pc = &sc["precompile"];
        let pc_active = pc["active"] == true;
        let sc_ready_line = if pc_active {
            "Headless pre-compile running — SteamVR will automatically restore to PSVR2 on completion or cancel."
                .into()
        } else if matches!(sc["state"].as_str(), Some("unconfigured" | "missing-mo2")) {
            sc["note"]
                .as_str()
                .unwrap_or("Configure MO2 path in control-center.json.")
                .to_owned()
        } else if !sc_blockers.is_empty() {
            format!("Close before changing: {sc_blockers}")
        } else if let Some(e) = sc["processCheckError"].as_str() {
            e.to_owned()
        } else {
            "Ready: Skyrim VR and Mod Organizer 2 are closed.".into()
        };
        self.set(
            1201,
            &format!(
                "Status: {sc_state}\r\nActive MO2 profile: {}\r\nActive ShaderCache layers: {} ({} total shaders) — {sc_sources}\r\nUpfront boot compile: {} (BackgroundOnBoot={bg_str}, DiskCache={dc_str}, bDepthBufferCulling={dbc_str})\r\nManaged backups: {} saved{latest_bak}\r\n{sc_ready_line}",
                sc["profile"].as_str().unwrap_or("(not detected)"),
                sc["activeCacheCount"].as_u64().unwrap_or(0),
                sc["totalCachedShaders"].as_u64().unwrap_or(0),
                if sc["upfrontBootCompileReady"] == true {
                    "READY"
                } else {
                    "NOT READY"
                },
                sc["backupCount"].as_u64().unwrap_or(0),
            ),
        );
        self.set(
            1203,
            pc["message"].as_str().unwrap_or(
                "Headless pre-compile: idle — PSVR2 headset, PSVR2 App, and KAT can stay off",
            ),
        );
        let permille = pc["permille"].as_u64().unwrap_or(0).min(1000) as u16;
        if self.last_progress_permille.get() != permille {
            self.last_progress_permille.set(permille);
            unsafe {
                SendMessageW(self.controls[&1204], PBM_SETPOS, permille as usize, 0);
            }
        }
        if self.last_precompile_active.get() != Some(pc_active) {
            self.last_precompile_active.set(Some(pc_active));
            let idle_en = if pc_active { 0 } else { 1 };
            let cancel_en = if pc_active { 1 } else { 0 };
            unsafe {
                for id in [1210u16, 1280, 1281, 1282, 1284] {
                    EnableWindow(self.controls[&id], idle_en);
                }
                EnableWindow(self.controls[&1285], cancel_en);
            }
        }
    }
}
unsafe extern "system" fn window_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_NCCREATE {
        let create = unsafe { &*(lparam as *const CREATESTRUCTW) };
        unsafe {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.lpCreateParams as isize);
        }
    }
    let ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut App;
    if !ptr.is_null() {
        let app = unsafe { &mut *ptr };
        match msg {
            WM_COMMAND => {
                let id = (wparam & 0xffff) as u16;
                let notify = ((wparam >> 16) & 0xffff) as u16;
                if let Err(e) = app.command(id, notify) {
                    app.set(5, &e);
                }
                return 0;
            }
            WM_TIMER => {
                app.tick();
                if app.smoke {
                    unsafe {
                        DestroyWindow(hwnd);
                    }
                }
                return 0;
            }
            WM_CLOSE => {
                let s = app.service.status.lock().unwrap().clone();
                if s["shaderCache"]["precompile"]["active"] == true && s["preview"] != true {
                    let approved = unsafe {
                        MessageBoxW(
                            hwnd,
                            wide("Headless shader pre-compile is currently running.\r\n\r\nClose Control Center now? This will cancel pre-compile, close Skyrim VR and headless SteamVR, and restore your PSVR2 SteamVR settings.").as_ptr(),
                            wide("Headless Pre-Compile Running").as_ptr(),
                            MB_YESNO | MB_ICONWARNING | MB_DEFBUTTON2,
                        )
                    };
                    if approved != IDYES {
                        return 0;
                    }
                } else if s["gameRunning"] == true && s["preview"] != true {
                    unsafe {
                        MessageBoxW(hwnd,wide("Custom trigger effects will stop. The music relay stays running in the background because the active game uses it for NPC dialogue and custom songs.").as_ptr(),wide("Closing Control Center").as_ptr(),MB_OK|MB_ICONINFORMATION);
                    }
                }
                unsafe {
                    DestroyWindow(hwnd);
                }
                return 0;
            }
            WM_DESTROY => {
                unsafe {
                    PostQuitMessage(0);
                }
                return 0;
            }
            _ => {}
        }
    }
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}
pub fn run(
    config: Config,
    root: PathBuf,
    preview: bool,
    smoke: bool,
    start_session: bool,
) -> Result<()> {
    let service = Service::start(config.clone(), root, preview, start_session)?;
    unsafe {
        let icc = INITCOMMONCONTROLSEX {
            dwSize: std::mem::size_of::<INITCOMMONCONTROLSEX>() as u32,
            dwICC: ICC_PROGRESS_CLASS,
        };
        InitCommonControlsEx(&icc);
        let instance = GetModuleHandleW(null());
        let class = wide("SkyrimVRNativeControlCenter");
        let title = wide(if preview {
            "Skyrim VR Control Center — Preview"
        } else {
            "Skyrim VR Control Center"
        });
        let wc = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(window_proc),
            hInstance: instance,
            hCursor: LoadCursorW(null_mut(), IDC_ARROW),
            hbrBackground: (COLOR_WINDOW + 1) as _,
            lpszClassName: class.as_ptr(),
            ..std::mem::zeroed()
        };
        if RegisterClassW(&wc) == 0 {
            return Err("Could not register native window".into());
        }
        let font = CreateFontW(
            -17,
            0,
            0,
            0,
            FW_NORMAL as i32,
            0,
            0,
            0,
            DEFAULT_CHARSET as u32,
            OUT_DEFAULT_PRECIS as u32,
            CLIP_DEFAULT_PRECIS as u32,
            CLEARTYPE_QUALITY as u32,
            (DEFAULT_PITCH | FF_DONTCARE) as u32,
            wide("Segoe UI").as_ptr(),
        );
        let mut app = Box::new(App {
            hwnd: null_mut(),
            font,
            controls: HashMap::new(),
            pages: Default::default(),
            page: 0,
            service,
            config,
            profiles: vec![],
            smoke,
            last_progress_permille: Cell::new(0),
            last_precompile_active: Cell::new(None),
            last_models_key: RefCell::new(String::new()),
        });
        let hwnd = CreateWindowExW(
            WS_EX_APPWINDOW,
            class.as_ptr(),
            title.as_ptr(),
            WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            1146,
            900,
            null_mut(),
            null_mut(),
            instance,
            app.as_mut() as *mut App as _,
        );
        if hwnd.is_null() {
            DeleteObject(font);
            return Err(format!(
                "Could not create native window: {}",
                std::io::Error::last_os_error()
            ));
        }
        app.hwnd = hwnd;
        app.build()?;
        SetTimer(hwnd, 1, 500, None);
        if !smoke {
            ShowWindow(hwnd, SW_SHOW);
            ShowWindow(hwnd, SW_SHOWNORMAL);
        }
        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            if IsDialogMessageW(hwnd, &msg) == 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        KillTimer(hwnd, 1);
        DeleteObject(font);
        drop(app);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::{
        Foundation::RECT,
        Graphics::Gdi::MapWindowPoints,
    };

    #[test]
    fn all_eight_pages_build_within_client_bounds_without_overlap() {
        let dir = std::env::temp_dir().join(format!(
            "skyrim-cc-ui-{}-{}",
            std::process::id(),
            crate::config::now().to_bits()
        ));
        std::fs::create_dir_all(dir.join("runtime/bard")).unwrap();
        std::fs::create_dir_all(dir.join("runtime/psvr2")).unwrap();
        crate::config::write_json(
            &dir.join("control-center.json"),
            &json!({"agentPort": 0, "dependencies": []}),
        )
        .unwrap();
        crate::config::write_json(
            &dir.join("runtime/bard/settings.json"),
            &json!({
                "llm_backend": "antigravity",
                "lyrics_provider": "antigravity",
                "antigravity": {"model": "gemini-3.8-flash-low", "effort": "low"},
                "selected_model": "z-ai/glm-5.3-flash",
                "reasoning_effort": "low",
                "include_history": true,
                "voice_mode": "auto",
                "suno_duration_seconds": null,
                "interval_real_gameplay_seconds": 480,
                "native_real_interval_override": false,
                "keep_both_suno_variants": true,
                "model_profiles": [{"id": "z-ai/glm-5.3-flash", "label": "GLM 5.3 Flash", "enabled": true}]
            }),
        )
        .unwrap();

        let config = Config::load(&dir).unwrap();
        let service = Service::start(config.clone(), dir.clone(), true, false).unwrap();
        unsafe {
            let icc = INITCOMMONCONTROLSEX {
                dwSize: std::mem::size_of::<INITCOMMONCONTROLSEX>() as u32,
                dwICC: ICC_PROGRESS_CLASS,
            };
            InitCommonControlsEx(&icc);
            let instance = GetModuleHandleW(null());
            let class = wide("SkyrimVRUiGeometryTest");
            let wc = WNDCLASSW {
                lpfnWndProc: Some(window_proc),
                hInstance: instance,
                lpszClassName: class.as_ptr(),
                ..std::mem::zeroed()
            };
            RegisterClassW(&wc);
            let font = CreateFontW(
                -17,
                0,
                0,
                0,
                FW_NORMAL as i32,
                0,
                0,
                0,
                DEFAULT_CHARSET as u32,
                OUT_DEFAULT_PRECIS as u32,
                CLIP_DEFAULT_PRECIS as u32,
                CLEARTYPE_QUALITY as u32,
                (DEFAULT_PITCH | FF_DONTCARE) as u32,
                wide("Segoe UI").as_ptr(),
            );
            let mut app = Box::new(App {
                hwnd: null_mut(),
                font,
                controls: HashMap::new(),
                pages: Default::default(),
                page: 0,
                service,
                config,
                profiles: vec![],
                smoke: true,
                last_progress_permille: Cell::new(0),
                last_precompile_active: Cell::new(None),
                last_models_key: RefCell::new(String::new()),
            });
            let hwnd = CreateWindowExW(
                0,
                class.as_ptr(),
                wide("UI Test").as_ptr(),
                WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                1146,
                900,
                null_mut(),
                null_mut(),
                instance,
                app.as_mut() as *mut App as _,
            );
            assert!(!hwnd.is_null());
            app.hwnd = hwnd;
            app.build().unwrap();
            app.tick();

            let mut client = RECT {
                left: 0,
                top: 0,
                right: 0,
                bottom: 0,
            };
            GetClientRect(hwnd, &mut client);

            // Verify Antigravity controls exist and status text was populated by tick().
            for expected_id in [118u16, 1300, 1301, 1310, 1311, 1312, 1313, 1314, 1315, 1380, 1381, 1382, 1383, 1384, 1385, 1386, 1387] {
                assert!(app.controls.contains_key(&expected_id), "missing control {expected_id}");
            }
            let status_box = app.text(1301);
            assert!(status_box.contains("Configured backends:"));
            assert!(status_box.contains("gemini-3.8-flash-low"));

            let rect_of = |child: HWND| -> RECT {
                let mut r = RECT {
                    left: 0,
                    top: 0,
                    right: 0,
                    bottom: 0,
                };
                GetWindowRect(child, &mut r);
                MapWindowPoints(null_mut(), hwnd, &mut r as *mut RECT as *mut _, 2);
                r
            };

            let shared: Vec<(u16, RECT)> = app
                .controls
                .iter()
                .filter(|(_, h)| !app.pages.iter().any(|p| p.contains(h)))
                .map(|(id, h)| (*id, rect_of(*h)))
                .collect();

            for page_idx in 0..app.pages.len() {
                let mut visible = shared.clone();
                for h in &app.pages[page_idx] {
                    let id = *app.controls.iter().find(|(_, ch)| *ch == h).unwrap().0;
                    visible.push((id, rect_of(*h)));
                }
                for &(id, r) in &visible {
                    assert!(
                        r.left >= 0 && r.top >= 0 && r.right <= client.right && r.bottom <= client.bottom,
                        "control {id} on page {page_idx} outside client bounds: ({},{},{},{}) vs ({},{},{},{})",
                        r.left, r.top, r.right, r.bottom,
                        client.left, client.top, client.right, client.bottom
                    );
                }
                for i in 0..visible.len() {
                    for j in (i + 1)..visible.len() {
                        let (id_a, a) = visible[i];
                        let (id_b, b) = visible[j];
                        let overlaps = a.left < b.right
                            && a.right > b.left
                            && a.top < b.bottom
                            && a.bottom > b.top;
                        assert!(
                            !overlaps,
                            "controls {id_a} ({},{},{},{}) and {id_b} ({},{},{},{}) overlap on page {page_idx}",
                            a.left, a.top, a.right, a.bottom,
                            b.left, b.top, b.right, b.bottom
                        );
                    }
                }
            }

            DestroyWindow(hwnd);
            DeleteObject(font);
            drop(app);
        }
        std::fs::remove_dir_all(dir).unwrap();
    }
}
