# Skyrim VR Control Center

A unified native Rust/Win32 desktop companion for Skyrim VR combining SkyrimNet bard song preferences & Suno variant archiving, PSVR2 adaptive trigger feedback, VR session startup, a reversible DLSS Neural Rendering (NR) runtime guard, and a Mod Organizer 2 (MO2) Open Shaders / Community Shaders cache & upfront boot-compile manager.

All three desktop binaries (`skyrim-vr-control-center.exe`, `skyrim-bard-engine.exe`, and `kat-overlay-stub.exe`) are native Rust with no Python runtime dependency. The repository also includes the companion SKSEVR C++ telemetry plugin (`skse-plugin/`).

## Repository layout

- `crates/control-center/` — Win32 UI, PSVR2 CAPI hardware watchdog, VR dependency launcher, SteamVR driver option editor, KAT overlay closer & zero-GPU stub manager, NR file guard, MO2 `ShaderCache` manager, and authenticated loopback agent API.
- `crates/bard-engine/` — Loopback HTTP OpenRouter lyrics/dialogue relay (`127.0.0.1:8097`) and existing Suno dual-variant archiver.
- `crates/kat-overlay-stub/` — Minimal zero-GPU / zero-OpenVR Win32 replacement executable for KAT Gateway's two optional Unity VR overlays (`KATArmViewOverlay.exe` and `KATGatewayOverlay.exe`).
- `skse-plugin/` — SKSEVR 2.0.12 C++ telemetry plugin (`PSVR2SkyrimState.dll`) publishing menu and HIGGS held-object state over loopback UDP (`127.0.0.1:19462`).
- `scripts/build-release.ps1` — Runs offline workspace tests, Clippy lints, release compilation, and stages all three `.exe` binaries in the root directory.
- `scripts/manage-kat-overlay-stubs.ps1` — Checks status or runs reversible UAC-elevated installation/restoration of KAT zero-GPU overlay stubs.
- `control-center.example.json` & `examples/` — Sanitized configuration templates (copy to `control-center.json` and `runtime/` for local setup).
- `runtime/` *(gitignored)* — Local user data (`runtime/bard/` for song settings, authorizations, history, drafts, and `Song Downloads/`; `runtime/psvr2/` for trigger feel settings, toolkit manifest, tools, and driver backups).

## Start

Keep `skyrim-vr-control-center.exe`, `skyrim-bard-engine.exe`, and `control-center.json` together in the root directory (with local runtime data under `runtime/bard` and `runtime/psvr2`). Double-click the Control Center or use the desktop shortcut (`--start-session`). It attaches to an existing music relay or starts `skyrim-bard-engine.exe` on loopback port `8097`. The visible panel owns its own PSVR2 trigger effects and loopback UDP `19462` listener.

**Start VR session dependencies** opens configured session prerequisites (such as the official PlayStation VR2 app, KAT Gateway, and OCU) in order, skipping processes already present. SteamVR and MO2 have separate buttons; the app does not launch Skyrim. Nothing is installed at Windows logon.

The saved `closeKatOverlays` option defaults to true under explicit user authorization. Session startup checks for the two approved KAT VR overlays for at most ten seconds, requests normal closure at most once per overlay executable, and may terminate only that first process handle whose executable path exactly matches an approved file. The KAT main app, input service, bindings, and SteamVR settings are preserved. The explicit **Close KAT VR overlays** button does the same once. KAT Gateway restarts the wrist-overlay process while SteamVR is active; the launcher reports a detected replacement and leaves it running (use the official VR HUD / VR Armview visibility toggle). A close pass is not a permanent process disable, and no VRAM reduction is claimed.

For users who never use KAT's in-VR overlays and only need KAT Gateway running for treadmill walking (e.g., OCU `KATReader`), **Install KAT zero-GPU stubs (UAC)** on the **Start & setup** tab (or `.\scripts\manage-kat-overlay-stubs.ps1 install`) backs up `KATArmViewOverlay.exe` and `KATGatewayOverlay.exe` in place as `*.original.exe` and replaces them with `kat-overlay-stub.exe` via standard Windows UAC elevation while KAT Gateway and Skyrim VR are closed. The stub imports no graphics, UI, Unity, or OpenVR libraries, satisfies KAT Gateway's 0.5-second process-name watchdog without creating a D3D11/GPU context, and exits automatically once both `KAT Gateway.exe` and `vrserver.exe` exit. **Restore KAT overlays (UAC)** (or `.\scripts\manage-kat-overlay-stubs.ps1 restore`) restores the exact `*.original.exe` Unity binaries. `KAT Gateway.exe`, KAT services/drivers, and OCU `KATReader` remain untouched, and no unmeasured VRAM saving is reported.

Closing the panel releases its trigger effects. The music relay remains running in the background because the live shared default route also carries NPC dialogue. Closing during Skyrim displays this distinction. The app never kills a legacy relay; an occupied music port is attached read-only for status until a safe transition is performed. An occupied feedback port leaves this app's feedback disarmed and visibly reports the conflict.

## Antigravity local LLM & SkyrimNet routing

The **Antigravity** tab in Control Center manages local `agy acp` inference (`127.0.0.1:8097`) alongside OpenRouter fallback:

- **Backend & model controls**: Separately configure `llm_backend` (SkyrimNet NPC dialogue, combat, evaluation, and vision) and `lyrics_provider` (Bard song drafting) between `antigravity` and `openrouter`, select an Antigravity model ID and effort tier (`low`, `medium`, `high`), optionally override `agy.exe` / sandbox paths, query `agy models` to refresh available models, or restart an idle ACP child worker without interrupting port `8097`.
- **Reversible SkyrimNet `OpenRouter.yaml` route switcher**: Shows exact `local_bard_composer`, `openrouter`, and `openrouter_decisions` slot counts and requires both `SkyrimVR.exe` and `ModOrganizer.exe` to be closed before switching or restoring. Switching creates a timestamped pre-Antigravity backup (`backups/<timestamp>-before-antigravity/`) only when `provider_id: openrouter` slots exist (preserving existing clean backups on repeated checks) and leaves `openrouter_decisions` untouched; restoring selects the newest valid pre-Antigravity backup containing `provider_id: openrouter` slots.
- **Headless ACP safeguards**: `agy acp` runs with `CREATE_NO_WINDOW` inside its sandbox folder and auto-denies all tool/filesystem/terminal requests from the agent.

## Song preferences and native music

Preferences are read from `runtime/bard/settings.json` on every save and patched only in the selected fields. Provider credentials, authorization records, history paths, draft files, and unknown settings remain in their existing locations. No credential is copied into Control Center configuration.

Saved model profiles have enable controls, and the editable picker supports custom `author/model` IDs. Custom models are added without inheriting another model's price caps. Disabled models are rejected. Thinking, full cleaned history, explicit/Auto singer voice, requested Suno length, and the optional active-play interval are preserved. Manual voice changes lyric context and standalone delivery metadata; final native Suno voice remains game-owned. The Suno seconds field applies only to explicit standalone review/submission; it cannot change the game's direct native duration.

**Save** makes no API generation call. **Pause new custom lyrics** gates new native lyric requests. **Arm one song** uses the engine's one-request allowance; **Resume authorized ongoing** requires an existing explicit human authorization record. Native Suno remains direct HTTPS and about 12 credits per generation. These controls cannot cancel an already submitted provider task or guarantee suppression of native fallback. The Rust engine exposes its exact scope in `/health`.

The **Song downloads** tab saves both variants from already-generated native Suno requests. Automatic downloading uses the existing both-variants preference, or **Download new variants** can run once. Files are separated into `Already cached by SkyrimNet`, `To import into SkyrimNet`, and `Unconfirmed` under `runtime/bard/Song Downloads`. Each audio file has a matching YAML record for the normal SkyrimNet import form. Track IDs and content hashes prevent duplicate archive copies. Native logs and cache files are read incrementally; no live game API or database is queried for this feature. Imported songs remain per-save; cache-file matches do not establish active-save playlist membership.

Expired-link recovery is a separate manual action for an existing task and displays a confirmation because provider credit cost is not fixed in its documentation. The app records balances before/after and never retries an uncertain recovery POST. **Refresh Suno credits** is read-only. Recovery does not generate a song. Recovered audio keeps its actual format; M4A may require conversion before SkyrimNet import. Existing MP3 downloads import directly with their YAML metadata.

Automatic importing is not performed. Dashboard-only restrictions remain intact. Native timing is the default; no external generation scheduler is launched. The normal unmarked dialogue relay preserves original request bytes and selected models. Mode/route installation is not changed at app startup.

## Feedback and driver features

The PSVR2 policy, CAPI binding, and telemetry listener are native Rust. The verified experimental-2 CAPI hash and 56-byte trigger ABI are enforced. Only L2/R2 receive custom adaptive effects. Menu detents and progressive object/NPC profiles retain gentle strength limits of 1–4.

The hardware watchdog runs separately from music HTTP, application startup, and NR file verification. Stale/invalid telemetry, game process exit, Stop, driver failure, and shutdown release effects. Duplicate/out-of-order same-process packets do not extend freshness. Driver errors latch feedback off until explicit Start; saving settings does not silently rearm it. Manual hardware tests last two seconds and require an active driver. The Stop path bypasses the general worker for both native UI and authenticated agent requests.

Driver options preserve unrelated fields and back up the existing SteamVR configuration before updating only LED synchronization, enhanced haptics, and Room View camera. Applying them requires Skyrim, SteamVR, and the official PSVR2 app to be closed; failed or incomplete process enumeration blocks mutation. No firmware, jailbreak, graphics, physics, savegame, or controller-binding changes are made.

## NR guard

When `nrGuard` paths are configured in `control-center.json`, the NR page shows the reviewed runtime's current filename and process blockers. Block/restore is an explicit action requiring both Skyrim and MO2 to be closed. Blocking additionally requires the existing NR checkbox to be off. The action verifies the exact file size/hash and absence of alternate NR runtimes, then renames only `nvngx_dlssnr.dll` to `.mohidden` without overwriting another file. Restore reverses the rename. Shared-mod scope and journaling are visible; the guard does not change DLSS/DLAA or configuration.

This guard does not unload a running game's allocation. Memory savings and game compatibility need a fresh-launch comparison. The app reports no measured VRAM saving.

## Shader cache & headless pre-compile

The **Shader cache** tab reads `ModOrganizer.ini` (`selected_profile` and `[customExecutables]`) and the active profile's `modlist.txt` / `settings.ini` (auto-derived from `nrGuard` or the `"mo2"` dependency target, or explicitly configured via `shaderCache` in `control-center.json`) to inspect all active `ShaderCache` layers in VFS priority order (`overwrite\ShaderCache`, `+enabled_mod\ShaderCache`, and `Data\ShaderCache`), compiled shader counts (`Manifest.json`), and upfront boot-compile settings.

While Skyrim VR and Mod Organizer 2 are closed:

- **Pre-compile shaders now (no headset)** runs a full desktop pre-compile with a native Win32 progress bar, live `compiled / total` counter, elapsed timer, and ETA — while your PSVR2 headset, controllers, PlayStation VR2 App, and KAT Gateway remain completely off. It backs up and clears active `ShaderCache` layers (when the checkbox is checked), enforces upfront boot compile, saves a crash-safe byte-exact backup of `steamvr.vrsettings`, temporarily activates SteamVR's built-in `driver_null` (`1024x1024`), launches Skyrim VR through MO2's SKSEVR shortcut (`moshortcut://:...` so RootBuilder and USVFS run normally), waits until all shaders finish compiling and `Info.ini` + `Manifest.json` flush to disk at Main Menu, then automatically closes Skyrim VR, waits for MO2 RootBuilder cleanup to exit, stops headless SteamVR, and restores your exact PSVR2 `steamvr.vrsettings`. **Cancel pre-compile** (or closing Control Center) safely aborts and restores PSVR2 SteamVR settings.
- **Clear shaders & prepare reload** moves every active `ShaderCache` folder in the selected profile into `<mo2Root>\ShaderCache_Backups\backup_<timestamp>` (retaining up to `maxBackups`, default `3`) so older lower-priority output-mod caches cannot leak underneath `overwrite`. Disabled `-` mods, `UnifiedWaterCache`, SkyrimNet data, and non-shader `overwrite` files are untouched.
- When **Clear existing ShaderCache before pre-compile / reload & enforce upfront boot compile** is checked (or via **Apply upfront settings only**), the app backs up and patches the active profile's `SKSE\Plugins\CommunityShaders\SettingsUser.json` (`Menu.BackgroundShaderCompilationOnBoot = false`, `General["Enable Disk Cache"] = true`) and active `skyrimvr.ini` (`[VRPerformance] bDepthBufferCulling=0`) so all shaders compile before the Main Menu and `VolumetricLighting::DataLoaded` does not wipe the newly compiled disk cache at Main Menu load.
- **Restore last shader backup** restores all slots from the most recent managed backup (safely parking any partial aborted `ShaderCache` into `discarded_partial_<timestamp>` rather than overwriting it), and **Open shader backups folder** opens `ShaderCache_Backups` in Explorer.

## Agent interface

Run commands from the application directory, or supply `--root <directory>`:

```powershell
.\skyrim-vr-control-center.exe --inspect
.\skyrim-vr-control-center.exe --status
.\skyrim-vr-control-center.exe --kat-stub-status
.\skyrim-vr-control-center.exe --shader-cache-status
.\skyrim-vr-control-center.exe --command feedback-stop
.\skyrim-vr-control-center.exe --command music-pause
.\skyrim-vr-control-center.exe --command start-session
.\skyrim-vr-control-center.exe --command kat-close-overlays
.\skyrim-vr-control-center.exe --command shader-cache-clear '{"enforceBootCompile":true}'
.\skyrim-vr-control-center.exe --command shader-cache-restore
.\skyrim-vr-control-center.exe --command shader-precompile-start '{"clearCache":true}'
.\skyrim-vr-control-center.exe --command shader-precompile-cancel
.\skyrim-vr-control-center.exe --command launch '{"id":"mo2"}'
.\skyrim-vr-control-center.exe --preview
.\skyrim-vr-control-center.exe --smoke-test
```

The GUI-subsystem executable supports redirected standard output. The agent API listens only on `127.0.0.1:19464`. `GET /status` returns sanitized status. `POST /command` requires the random local bearer token in `.local/agent-token`; the CLI reads it without printing it. A JSON body contains `action` and optional `body`. No unauthenticated mutation endpoint or shell execution API exists.

Actions include `music-downloads-sync`, `music-downloads-open`, `music-downloads-auto`, `music-downloads-task`, `music-downloads-recover`, and `music-downloads-credits`, `antigravity-refresh-models`, `antigravity-restart`, `antigravity-open-sandbox`, `skyrimnet-switch-antigravity`, `skyrimnet-restore-openrouter`, `skyrimnet-open-config`, and `skyrimnet-open-backups`, plus the music, feedback, startup, driver, KAT, NR, and shader-cache (`shader-cache-clear`, `shader-cache-restore`, `shader-cache-enforce-settings`, `shader-cache-open-backups`, `shader-precompile-start`, `shader-precompile-cancel`) actions. Agent actions use the same validation and authorization as the visible controls.

`--preview` opens all eight panels read-only with services, live feedback, dependency launches, and mutations disabled. `--smoke-test` builds that native window hidden and closes it.

## Build and verification

Build, lint, test, and stage both binaries from the workspace root in one command:

```powershell
.\scripts\build-release.ps1
```

Or run the workspace Cargo commands directly:

```powershell
cargo test --workspace --offline
cargo clippy --workspace --offline -- -D warnings
cargo build --workspace --offline --release
```

Hardware tests use fake CAPI only, and automated suites never issue paid API calls.
