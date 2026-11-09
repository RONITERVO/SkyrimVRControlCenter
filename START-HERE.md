# Quick start and maintenance

## Starting a VR session

Use the desktop **Skyrim VR Control Center** shortcut. It opens the unified native Rust panel, starts `skyrim-bard-engine.exe` in the background, and opens configured session prerequisites (PlayStation VR2 App, KAT Gateway, and OCU) if they are not already running. SteamVR and Mod Organizer also have buttons in **Start & setup**. Launch Skyrim through your usual MO2 profile.

Leave OCU open for the KAT reader and keep the official PSVR2 app running. The Rust music relay stays in the background when the panel closes because normal NPC dialogue shares its route. Reopening the panel attaches to the same relay.

## Consolidated single-folder layout

Everything now lives inside this single directory:

- `runtime\bard\` — Integrated Bard settings, authorizations, adapter token, cleaned songwriting history, drafts, and `Song Downloads\`.
- `runtime\psvr2\` — Integrated PSVR2 trigger feel settings (`.local\settings.json`), toolkit manifest (`installation.json`), official `tools\` (`HeadsetSetup` and `CAPIApps`), and `backup-before-1.0\`.
- `crates\control-center\` & `crates\bard-engine\` — Unified Rust Cargo workspace.
- `skse-plugin\` — C++ SKSEVR telemetry bridge source (`PSVR2SkyrimState.dll`).
- `backups\` — Archived legacy Python scripts (`backups\legacy-python-helpers\`) and pre-consolidation snapshots.

All personal runtime folders (`runtime\`, `.local\`, `backups\`), `control-center.json`, and `.exe` binaries are listed in `.gitignore`, while sanitized templates live in `control-center.example.json` and `examples\`.

## Updating / rebuilding

After editing any Rust code under `crates\`, rebuild, test, lint, and stage both `.exe` binaries in one step from this folder:

```powershell
.\scripts\build-release.ps1
```

*(Close the Control Center UI and stop `skyrim-bard-engine.exe` outside a Skyrim session before replacing running `.exe` files.)*

## Both song variants

Open **Song downloads** in the panel. Automatic saving uses your enabled both-variants preference, and **Download new variants** performs a one-time scan of existing requests without generating music. Files are saved under `runtime\bard\Song Downloads`, separated into **Already cached by SkyrimNet**, **To import into SkyrimNet**, and **Unconfirmed**, each with matching YAML metadata for SkyrimNet's Voice / Bards / Songs import page.
