# Skyrim Bard Engine — Rust

Native Windows x64 music backend for the combined Skyrim VR controls application. It has no Python runtime dependency and reads the existing composer settings, cleaned role history and native adapter token in place. It is staged here; building it does not install it, modify SkyrimNet configuration, interrupt the running Python gateway or call a paid provider.

```powershell
cargo test --offline
cargo build --release --offline
.\target\release\skyrim-bard-engine.exe --app-dir .\runtime\bard status
```

After closing Skyrim and intentionally stopping the previous gateway, the UI can launch the adjacent executable with `--music-root <composer directory> serve`. `--app-dir` is an equivalent spelling. The server binds only `127.0.0.1:8097`; an occupied port causes a clean failure before journals are touched. `--port` is available for isolated fixture testing. The existing native route and token must already be configured. Starting the engine never edits provider routing or game databases.

## Implemented live path

Version 0.1.1 shares one HTTP connection pool across NPC, lyric and optional clock requests. Credentials remain per-request and are reread from the existing configuration; pooling does not cache an old API key. Provider retries and redirects stay disabled. Native request bodies and ordinary streamed responses remain unchanged.

The live server moves filesystem access, history preparation, durable journal writes and contended synchronous state locks to a blocking pool with at most four active jobs. Async network workers do not perform those disk operations. Before-provider spending records still flush to disk, and pause/arm and allowance consumption retain the same shared critical section. Cancelling an HTTP request does not interrupt a journal write already executing. Explicit standalone CLI delivery commands retain their original behavior; this change targets the live game relay.

Offline regressions verify actual reuse of one TCP socket, live credential reload, unchanged payload/header/stream forwarding, no automatic retry of a 429 response, concurrent progress while another stream is incomplete, async progress while a blocking job waits, cached duplicate handling, and the existing spending/deadline safeguards. These checks establish behavior; they do not measure in-game FPS or predict a fixed response-time saving.

- `GET /health` exposes `runtime: "rust"`, version, the prior UI status keys and explicit capability fields. `HEAD /` and `/health` support native connection warmers.
- Authenticated `POST /v1/chat/completions` (also `/chat/completions`) classifies `SKYRIM_BARD_COMPOSER_NATIVE_CONTEXT_V1`. Marked requests require exactly one singer name, explicit singer gender and each native context field.
- Unmarked requests retain their original request bytes, models, parameters and streaming response bytes. The original OpenRouter key is read only from the configured existing `Providers.yaml`; neither the key nor unrelated dialogue is logged or copied. Named Balanced variants remain direct because this engine does not modify routes. Invalid song preferences do not prevent relay startup or health responses; health reports `song_configuration_valid` and `song_configuration_error` separately.
- The existing selected model, profile enablement, price cap, reasoning effort, temperature, output limit and history toggle are respected. There is no alternate-model retry. Provider failover may serve the same selected model. The current GLM 5.3 Flash/high settings are read rather than replaced with new defaults.
- Cleaned `data/history.turns.json` user/model roles become user/assistant API turns. Removed thinking is not reconstructed. Stream reasoning counts as first content, is counted for timing metadata, and is never retained. The maximum first-content deadline is 30 seconds; the absolute maximum completion deadline is 300 seconds. Headers and empty heartbeats do not extend the first deadline.
- Singer metadata or an explicit voice override is required before key loading/provider I/O. The voice is frozen in draft context. Delivery corrects model voice metadata to that frozen voice and retains the original model result.
- Drafts preserve `context.json`, `song.json`, `answer.raw.json`, `attempts.json`, `result.json`, `request-metadata.json`, `review.md`, `song.suno.json`, `suno-request.review.json`, `suno-formatting-edits.json` and `suno-approval-required.json`. Suno duration is excluded from lyric instructions. Lyrics and original audio are not truncated.
- The native response uses `Title:`, `Style:` and `Lyrics:` or OpenAI-compatible SSE. Existing Python-compatible request hashes and completed/failed native job journals are recognized. Interrupted/uncertain lyric jobs stay failed; they are not automatically retried. A second different native request is rejected while composition is active.

## Spending controls and direct native Suno

SkyrimNet beta28 sends its Suno requests directly to `https://api.sunoapi.org/api/v1`; native submission and playback do not pass through this gateway. `POST /suno/generate` therefore returns 410. The optional download worker below discovers already-issued task IDs from native log records; it does not intercept submission.

Authenticated `POST /controls/arm-one` requires exactly `{"consent":"Allow one native Suno generation, estimated 12 credits"}`. It preserves unknown settings and the prior recorded ongoing authorization, changes active mode to one-song, and arms one fresh native lyric request. That allowance is consumed and journaled **before lyric-provider I/O**. The old Python implementation consumed allowance only inside the Suno proxy route that the native client does not use. A failed/uncertain lyric request consumes its allowance and is never automatically retried.

`POST /controls/cancel-allowance` changes active mode to one-song and clears allowance. The UI can resume the already explicitly authorized ongoing mode by restoring `native_spending_mode: "native_automatic"`; a nonempty `native_recurring_suno_authorization` is required. These controls gate **new native lyrics only**. They cannot cancel a Suno task already submitted, prevent a separate native fallback path, or establish the number of credits the native application actually spent. Completed identical native requests return their cached lyrics without another lyric call.

The spending gate rereads authorization under the same mutex used by pause/arm, so a previous model/history snapshot cannot undo a completed pause. Pausing remains available even when song preferences are invalid.

Requested Suno duration and the explicit `vocalGender` field belong to standalone reviewed delivery. The native response only carries Title/Style/Lyrics: its duration and actual vocal gender are controlled by SkyrimNet's direct Suno request. A manual voice override changes lyric instructions and frozen standalone metadata, but is not a reliable native Suno voice control. Auto mode uses the explicitly supplied native singer metadata.

Native timing performs no game API polling. The optional real-gameplay minimum polls only the loopback game-time API and counts healthy advancing wall-clock samples. Its cooldown records a lyric request time, not confirmed native Suno spending.

## Existing-song downloads (0.2.0)

The user explicitly requested one-click and optional automatic download of both native Suno variants. `keep_both_suno_variants` now controls that automatic download worker. This only downloads existing music; it is not a generation scheduler. The Control Center's Song downloads tab offers Download new variants, folder buttons, add-existing-task, credit refresh, and separately approved link recovery.

`data/song-downloads-config.json` supplies `log_directory`. Native `SkyrimNet*.log` files are scanned incrementally, at most 2MiB per file per pass, accepting only actual SunoInterface submitted-task records. Normal idle scans are30seconds apart. Catch-up and pending recovery use short waits. No live game endpoint, SQLite database, or native cache file is modified. Native-cache hashes are recalculated only when file size/modification time changes. The worker runs independently of NPC relay and hardware feedback.

Files go under the existing composer root's `Song Downloads` folder:

- `Already cached by SkyrimNet`: a matching native cached track ID or exact audio hash was found.
- `To import into SkyrimNet`: this variant did not match, while a sibling from the same request did.
- `Unconfirmed`: neither sibling could be verified in the current cache files. This is not a guess about what the player heard.

These are cache-file observations, not proof of active-save playlist membership. Imported/re-encoded audio may not match by hash; unknown cases should be reviewed. A matching hash can recognize unchanged imported audio even when the cache filename differs. Files are reclassified on subsequent scans. Native caches can span saves; imports remain per-save.

Each audio file has a same-name YAML record with title, explicit provider voice, lyrics, tags, and a stable `suno_` ID. Select audio and YAML together in SkyrimNet's browser import. Alternate titles include an ID suffix. Leave "This is my song" off for NPC use. Missing provider voice is left blank for manual selection; it is never inferred. Actual media format is probed: recovered M4A files keep `.m4a` and need conversion to a SkyrimNet-supported import format rather than being falsely labelled MP3. All downloads are bounded to100MiB, SHA256-indexed, duration-probed, and fully decoded for verification; audio is never truncated or regenerated.

Track IDs prevent repeated downloads. Exact content hashes avoid saving identical audio twice under different IDs. Completed-file recovery handles interrupted index/move updates. Existing differing files are not overwritten. Failed partial transfers can be retried as downloads; an uncertain recovery POST is never automatically repeated. The index lives in `data/song-downloads-index.json`; do not delete it to troubleshoot a provider failure.

Authenticated local endpoints:

- `POST /archive/sync` queues an existing-task scan/download.
- `POST /archive/task` accepts `task_id` for an already-generated request.
- `POST /archive/credits` refreshes the balance.
- `POST /archive/recover` requires `task_id` and exact consent `Recover existing tracks; provider credit cost may change`.

Recovery uses the original request's HTTPS callback and the documented `/suno/recovery` endpoint, then polls `/suno/recovery/record-info`. The service sometimes returns code200 with null data while pending; it is not treated as completion. The recovery POST has a durable exclusive journal and no retry; a known recovery ID can be resumed. Balance-before/after is shown as an observation, not a guaranteed tariff. A single explicitly approved test on2026-10-29 recovered both tracks with616credits before and after. No extra music was generated. Automatic recovery is not enabled.

The archive API transport permits only GET task details, GET credits, GET recovery details, and the explicitly approved recovery POST. Generation endpoints are rejected. Provider credentials are read from the existing canonical Suno configuration, never copied into download settings or sent to audio hosts. A descriptive app User-Agent avoids the service's rejection of Python's default client signature. TLS verification stays enabled and redirects stay disabled.

## Explicit standalone commands

`draft --context <JSON> [--output <new directory>]` makes one selected-model lyrics call only. No live database scanner or external scheduler runs. Optional `--voice male|female` and `--singer <name>` provide explicit test context.

`prepare-suno --draft <directory>` produces review files and a payload hash without submission. `submit-approved --draft <directory>` requires an existing explicit human authorization and matching exact payload hash. It creates an exclusive submission journal before the single canonical HTTPS POST. An ambiguous result is never retried; use provider history to reconcile it. Ongoing native authorization does not authorize this independent command.

`poll-existing --draft <directory>` resumes a known `suno-submission.json` task ID without generating another song. `download-existing --draft <directory>` downloads all variants from a completed result, retains each original MP3, measures duration with `ffprobe`, fully decodes it with `ffmpeg`, and writes `audio-verification.json`. These two installed command-line tools are required for audio verification. Downloads do not follow redirects, send provider keys to audio URLs, trim, regenerate, or import audio.

## Antigravity ACP Backend

Version 0.2.1 introduces a native Antigravity ACP (`agy acp`) backend:

- **Zero-Credit Local Inference**: Routes SkyrimNet NPC dialogue, combat, actions, vision, and Bard lyrics through the local Antigravity CLI without consuming OpenRouter credits.
- **Fast Response**: Uses persistent JSON-RPC over `stdio` to the `agy acp` server. Requests execute in ~2–3 seconds on `gemini-3.8-flash-low`.
- **Strict Headless Safety**: Automatically auto-denies tool execution and workspace access requests from the agent, guaranteeing zero side effects on the host filesystem.
- **Full OpenAI API Compatibility**: Emits standard SSE streaming chunk deltas (`chat.completion.chunk`) and complete responses (`chat.completion`), including vision base64 image parsing.
- **Seamless OpenRouter Fallback**: Set `"llm_backend": "openrouter"` in `settings.json` (or via the Control Center UI) to revert all relay traffic back to OpenRouter.
- **SkyrimNet Route Automation**: Run `scripts\Switch-SkyrimNet-To-Antigravity.ps1` to route SkyrimNet variants to the relay on port 8097 while keeping the `decisions` slot on OpenRouter (`openrouter_decisions`). Revert at any time with `scripts\Restore-SkyrimNet-OpenRouter.ps1`.

## Deliberate limits

This port implements the current OpenRouter native integration and explicit reviewed delivery workflow. It does not implement the superseded Gemini key rotation, raw history export conversion, external draft/paid schedulers, route installation/rollback, SQLite scanning, or dashboard automation. It refuses unsupported provider settings. Existing cleaned history must be available.

The SkyrimNet `/bards?api=import` dashboard request gate remains intact. Rust download output can be imported through the authorized dashboard; this engine does not spoof a dashboard request, modify SQLite or bypass a 403 response.

Tests use temporary fixtures and loopback mock servers only. No live provider completion or in-game playback was performed as part of staging.
