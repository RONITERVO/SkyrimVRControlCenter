# PSVR2 Skyrim state bridge

Experimental SKSEVR telemetry plugin for Skyrim VR 1.4.15. It reports menu and HIGGS held-object state to the companion controller application. It does not load PSVR2Toolkit, drive hardware, alter bindings, change physics, or save data in a savegame.

The bridge provides menu-open detents and post-grab material profiles through the companion app. It does **not** report accepted menu selections, pressure, pre-grab targeting, damage, rain, or thunder. Object/actor classification supports simulated trigger compliance only; it does not deform Skyrim objects or NPCs.

## Requirements and staged installation

- Skyrim VR 1.4.15 and SKSEVR 2.0.12.
- VR Address Library for SKSEVR, including `Data/SKSE/Plugins/version-1-4-15-0.csv` visible to the game through MO2.
- HIGGS exposing interface `HIGGS001` for grab reporting. Without HIGGS, menus work and both held values remain `none`.
- A companion listener on UDP `127.0.0.1:19462`; no listener is required for the game to continue.

The built artifact is `dist/RelWithDebInfo/PSVR2SkyrimState.dll`, with its matching PDB beside it. The reviewer may install the DLL in a **separate MO2 mod**, at `SKSE/Plugins/PSVR2SkyrimState.dll`. No files have been installed by this project. Disable that mod and restart Skyrim to remove the bridge. The PDB is optional for runtime but useful for crash diagnostics.

The plugin rejects every runtime except Skyrim VR 1.4.15. It logs to SKSE's log directory as `PSVR2SkyrimState.log`. CommonLib requires the VR Address Library; verify that dependency before enabling the mod. In-game compatibility and controller behavior still require a supervised launch test.

## Protocol

UTF-8 JSON datagrams, destination `127.0.0.1:19462`, no listener or inbound command socket in the DLL:

```json
{"schemaVersion":1,"type":"state","pid":1234,"seq":7,"menuOpen":false,"heldLeft":"object","heldRight":"actor","gameLoaded":true}
```

- `seq` increases for every packet from that Skyrim process, starting at 1.
- `pid` identifies the sending process. This is local telemetry, not an authenticated security boundary.
- Held enums are `none`, `object`, or `actor`; actor means a HIGGS-grabbed reference whose form type is ActorCharacter.
- Heartbeats arrive about once per second. Changed state is published promptly after a queued main-thread task runs.
- A successful load/new game permits gameplay state. Pre-load, return to Main Menu, and Loading Menu clear it. Failed load leaves it cleared.
- Main/Loading screens and missing UI report all fields off. Console/Fader/Mist clear menu and held state. The HUD and other overlays do not arm a menu detent.
- If the main thread stops refreshing for over three seconds, the sender emits all-off state. Depending on heartbeat timing, this arrives approximately 3–4 seconds after the last refresh.
- The companion must also clear effects on process exit, missing packets, invalid state, its own stop/disconnect, and CAPI failure. Suggested packet timeout: 3.5 seconds.

Menu allowlist: InventoryMenu, MagicMenu, ContainerMenu, BarterMenu, Dialogue Menu, Crafting Menu, MessageBoxMenu, Journal Menu, TweenMenu, Book Menu, GiftMenu, Training Menu, Sleep/Wait Menu.

## Implementation boundaries

The HIGGS client keeps upstream interface revision 001 and virtual function order. Its adaptation only changes type names and the CommonLib messaging wrapper. The API is requested at SKSE PostPostLoad. Grab, drop, stash, and consume callbacks queue a coalesced SKSE main-thread task. Callback object pointers are ignored; the task reads current HIGGS state and never retains game objects. Menu callbacks and a 1 Hz refresh schedule use the same task path.

The network worker reads a small synchronized value snapshot and sends with a nonblocking UDP socket. It never calls Skyrim or HIGGS APIs. SKSE tasks are tagged by load generation, so a task queued before a load transition cannot publish its old snapshot. The DLL is pinned for the process lifetime because its worker must not outlive the module. No trampolines, function patches, input hooks, Papyrus scripts, ESP files, CAPI imports, or firmware operations are used.

## Rebuild and tests

Use Visual Studio 2022 with the Desktop development with C++ workload and a Windows SDK. All dependency sources are pinned and already present under `external/`; CMake does not download anything.

```powershell
$cmakePath = 'C:/Program Files/Microsoft Visual Studio/2022/Community/Common7/IDE/CommonExtensions/Microsoft/CMake/CMake/bin/cmake.exe'
& $cmakePath -S . -B build -G 'Visual Studio 17 2022' -A x64
& $cmakePath --build build --config RelWithDebInfo --parallel 10
powershell -NoProfile -ExecutionPolicy Bypass -File tests/Test-Transport.ps1
```

The transport test uses port **19463**, never the companion's production port. It validates field names/types, PID, monotonic sequence, menu/grab/reset transitions, heartbeats, stale-state clearing, and resumption. It does not load Skyrim or exercise HIGGS/menus.

Pinned sources:

- [CommonLibSSE-NG b93280e](https://github.com/CharmedBaryon/CommonLibSSE-NG/tree/b93280e832f263dbef44e44cbe2936622a02f91a), built VR-only, statically linked.
- [HIGGS API 93bf67b](https://github.com/adamhynek/higgs/tree/93bf67b1bc4c4a11a20ccaef0d5012781d0d7eee), `include/higgsinterface001.h` and `src/higgsinterface001.cpp`.
- [spdlog 1.15.3 / 6fa3601](https://github.com/gabime/spdlog/tree/6fa36017cfd5731d617e1a934f0e5ea9c4445b13), statically linked, using `std::format`.
- [rapidcsv 8.90 / 03133b5](https://github.com/d99kris/rapidcsv/tree/03133b59e17ca92ecf886b611d87a03ad2bb2a8f), header library used by CommonLib's VR address database.

Upstream license files remain with the dependency sources. This is a local prototype, not a published release.


## 0.1.1 startup correction — 24 October 2026

The first live test's SKSE log rejected 0.1.0 during Query. SKSEVR 2.0.12
passes runtimeVersion 0x010400F1 (1.4.15.1), including its distribution tag;
CommonLib's executable-file constant is 1.4.15.0. Both Query and Load now
validate the actual tagged interface version. No engine, HIGGS, transport,
hardware, controller bindings or graphics policy changed.

The actual DLL Query export failed the live-version regression before the
fix and passed afterwards. Eight ABI cases passed, including editor, SE/AE,
wrong VR revisions and null interface rejection. The isolated UDP transport
test and seven fake-hardware companion tests also passed. The installed DLL
has been replaced with backup retained; it takes effect on the next Skyrim
launch. Real game load, menu callbacks, HIGGS grabs and physical trigger feel
remain pending the user's next headset test.
