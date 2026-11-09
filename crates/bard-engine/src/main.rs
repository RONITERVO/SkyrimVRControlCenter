pub mod antigravity;
mod archive;
mod blocking;
mod config;
mod delivery;
mod engine;
mod lyrics;
mod server;
mod song;
use anyhow::{bail, Context, Result};
use serde_json::json;
use std::{collections::HashMap, path::PathBuf, time::Duration};

async fn run() -> Result<()> {
    let mut options = HashMap::new();
    let mut command = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--help" || arg == "-h" {
            println!("skyrim-bard-engine --app-dir <existing composer root> <command> [options]\nAliases: --music-root for --app-dir\nCommands:\n  serve [--port 8097]             Native lyric gateway; loopback only\n  status                          Read-only JSON configuration status\n  draft --context <json> [--output <new dir>]   Explicit paid lyrics call; no Suno\n  prepare-suno --draft <dir>       Prepare an approval-required request\n  submit-approved --draft <dir>    Submit exactly once with existing explicit approval/hash\n  poll-existing --draft <dir>      Poll a known task, never generate\n  download-existing --draft <dir> Save and verify all existing variants\nNo command changes native routing, providers, or game databases. No external scheduler.");
            return Ok(());
        }
        if arg.starts_with("--") {
            let value = args.next().context("Missing option value")?;
            options.insert(arg, value);
        } else if command.replace(arg).is_some() {
            bail!("Only one command is allowed");
        }
    }
    let allowed = [
        "--app-dir",
        "--music-root",
        "--port",
        "--context",
        "--events",
        "--output",
        "--draft",
        "--voice",
        "--singer",
    ];
    if options.keys().any(|k| !allowed.contains(&k.as_str())) {
        bail!("Unknown option; use --help");
    }
    let root = PathBuf::from(
        options
            .get("--app-dir")
            .or_else(|| options.get("--music-root"))
            .context("Pass --app-dir with the existing composer directory")?,
    );
    let command = command.unwrap_or_else(|| "status".into());
    if command == "serve" {
        let port = options
            .get("--port")
            .map(|v| v.parse())
            .transpose()?
            .unwrap_or(8097);
        return server::serve(root, port).await;
    }
    let s = if command == "status" {
        config::read_settings(&root)?
    } else {
        config::load(&root)?
    };
    let draft = || {
        options
            .get("--draft")
            .map(PathBuf::from)
            .context("--draft directory is required")
    };
    let result = match command.as_str() {
        "status" => engine::offline_status(&root)?,
        "draft" => {
            let p = options
                .get("--context")
                .or_else(|| options.get("--events"))
                .context("--context JSON is required; Rust does not scan live game databases")?;
            let mut context = config::read_json(&PathBuf::from(p))?;
            if let Some(v) = options.get("--voice") {
                let gender = song::voice(&json!(v)).context("--voice must be male or female")?;
                if !context["requirements"].is_object() {
                    context["requirements"] = json!({});
                }
                context["requirements"]["vocal_gender"] = json!(gender);
                if let Some(name) = options.get("--singer") {
                    context["selected_singer"] = json!({"name":name,"gender":gender});
                }
            }
            let output = options.get("--output").map(PathBuf::from);
            let folder = lyrics::make_draft(&root, &s, context, output.as_deref()).await?;
            json!({"draft":folder,"suno_submitted":false})
        }
        "prepare-suno" => {
            let folder = draft()?;
            let song = song::prepare_suno(&s, &folder)?;
            json!({"prepared":folder,"title":song["title"],"suno_submitted":false})
        }
        "submit-approved" => delivery::submit_approved(&draft()?, &s).await?,
        "poll-existing" => {
            let folder = draft()?;
            let body = delivery::poll_existing(&folder, &s, Duration::from_secs(600)).await?;
            json!({"result":folder.join("suno-result.json"),"tracks_ready":body["data"]["response"]["sunoData"].as_array().map_or(0,Vec::len),"new_generation_submitted":false})
        }
        "download-existing" => delivery::download_existing(&draft()?, &s).await?,
        _ => bail!("Unknown command; use --help"),
    };
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}
#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!(
            "{}",
            json!({"error":error.to_string(),"automatic_retry":false,"runtime":"rust"})
        );
        std::process::exit(2);
    }
}
