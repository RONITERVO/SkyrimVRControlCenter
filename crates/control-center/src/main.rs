#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
mod config;
mod feedback;
mod feedback_worker;
mod http;
mod kat;
mod nr_guard;
mod platform;
mod runtime;
mod service;
mod shader_cache;
mod shader_precompile;
mod ui;
use config::Result;
use serde_json::json;
use std::{fs, path::PathBuf};
fn main() {
    if let Err(e) = run() {
        eprintln!("{e}");
        if !std::env::args().any(|a| {
            a.starts_with("--status")
                || a == "--inspect"
                || a == "--command"
                || a == "--smoke-test"
                || a.starts_with("--kat-stub-")
                || a == "--shader-cache-status"
        }) {
            unsafe {
                windows_sys::Win32::UI::WindowsAndMessaging::MessageBoxW(
                    std::ptr::null_mut(),
                    config::wide(&e).as_ptr(),
                    config::wide("Skyrim VR Control Center").as_ptr(),
                    windows_sys::Win32::UI::WindowsAndMessaging::MB_ICONERROR,
                );
            }
        }
        std::process::exit(1);
    }
}
fn run() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let root = if let Some(i) = args.iter().position(|a| a == "--root") {
        PathBuf::from(args.get(i + 1).ok_or("--root requires a folder")?)
    } else {
        let dir = exe.parent().unwrap();
        if dir.join("control-center.json").is_file() {
            dir.to_path_buf()
        } else {
            std::env::current_dir().map_err(|e| e.to_string())?
        }
    };
    if args.iter().any(|a| a == "--kat-stub-status") {
        println!(
            "{}",
            serde_json::to_string_pretty(&kat::stub_status(&root)).unwrap()
        );
        return Ok(());
    }
    if args.iter().any(|a| a == "--kat-stub-install") {
        match kat::install_stubs(&root) {
            Ok(v) => {
                println!("{}", serde_json::to_string_pretty(&v).unwrap());
                return Ok(());
            }
            Err(e) => {
                kat::record_cli_error(&root, "install", &e);
                return Err(e);
            }
        }
    }
    if args.iter().any(|a| a == "--kat-stub-restore") {
        match kat::restore_stubs(&root) {
            Ok(v) => {
                println!("{}", serde_json::to_string_pretty(&v).unwrap());
                return Ok(());
            }
            Err(e) => {
                kat::record_cli_error(&root, "restore", &e);
                return Err(e);
            }
        }
    }
    let config = config::Config::load(&root)?;
    if args.iter().any(|a| a == "--shader-cache-status") {
        println!(
            "{}",
            serde_json::to_string_pretty(&shader_cache::status(
                config.effective_shader_cache().as_ref()
            ))
            .unwrap()
        );
        return Ok(());
    }
    if args.iter().any(|a| a == "--inspect") {
        println!("{}",serde_json::to_string_pretty(&json!({"music":config::public_music(&config.music_settings()?),"feedback":config.feedback_settings()?,"dependencies":config.dependencies,"nativeMusicEnginePresent":root.join("skyrim-bard-engine.exe").is_file(),"katOverlayStubs":kat::stub_status(&root),"shaderCache":shader_cache::status(config.effective_shader_cache().as_ref()),"credentialsCopied":false})).unwrap());
        return Ok(());
    }
    if args.iter().any(|a| a == "--status") {
        println!(
            "{}",
            serde_json::to_string_pretty(&http::request(
                config.agent_port,
                "GET",
                "/status",
                None,
                None
            )?)
            .unwrap()
        );
        return Ok(());
    }
    if let Some(i) = args.iter().position(|a| a == "--command") {
        let action = args.get(i + 1).ok_or("--command requires an action")?;
        let body = if let Some(arg) = args.get(i + 2).filter(|s| !s.starts_with("--")) {
            serde_json::from_str(arg).map_err(|_| "Command body must be JSON")?
        } else {
            json!({})
        };
        let token = fs::read_to_string(root.join(".local/agent-token"))
            .map_err(|_| "Visible Control Center is not running")?;
        let value = http::request(
            config.agent_port,
            "POST",
            "/command",
            Some(&json!({"action":action,"body":body})),
            Some(&token),
        )?;
        println!("{}", serde_json::to_string_pretty(&value).unwrap());
        return Ok(());
    }
    let smoke = args.iter().any(|a| a == "--smoke-test");
    let preview = smoke || args.iter().any(|a| a == "--preview");
    ui::run(
        config,
        root,
        preview,
        smoke,
        args.iter().any(|a| a == "--start-session"),
    )?;
    if smoke {
        println!(
            "{}",
            json!({"nativeWindowBuilt":true,"hardwareAccess":false,"settingsWritten":false,"generationRequested":false})
        );
    }
    Ok(())
}
