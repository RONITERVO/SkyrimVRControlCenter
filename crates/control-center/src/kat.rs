//! KAT VR overlay management: bounded closure of the two optional Unity VR
//! overlay executables, plus reversible zero-GPU stub replacement (`*.original.exe`
//! backup + `kat-overlay-stub.exe`) leaving KAT Gateway, drivers, and OCU
//! KATReader walking input untouched.
use crate::{
    config::{Result, read_json, wide, write_json},
    platform,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    fs::{self, File},
    io::Read,
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::{CloseHandle, ERROR_ACCESS_DENIED, HANDLE, HWND, LPARAM, WAIT_OBJECT_0},
    Storage::FileSystem::{MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW},
    System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
        PROCESS_TERMINATE, QueryFullProcessImageNameW, TerminateProcess, WaitForSingleObject,
    },
    UI::{
        Shell::{SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW},
        WindowsAndMessaging::{
            EnumWindows, GetWindowThreadProcessId, PostMessageW, SW_HIDE, WM_CLOSE,
        },
    },
};

const APPROVED: &[(&str, &str)] = &[
    (
        "KATArmViewOverlay.exe",
        r"C:\Program Files (x86)\KAT Gateway\KATArmViewOverlay\KATArmViewOverlay.exe",
    ),
    (
        "KATGatewayOverlay.exe",
        r"C:\Program Files (x86)\KAT Gateway\overlay\KATGatewayOverlay.exe",
    ),
];

const STUB_EXE_NAME: &str = "kat-overlay-stub.exe";
const JOURNAL_REL_PATH: &str = ".local/kat-overlay-stub-journal.json";
const MAX_STUB_BYTES: u64 = 2 * 1024 * 1024;
const UNITY_MARKER: &[u8] = b"UnityPlayer.dll";
const FORBIDDEN_STUB_MARKERS: &[&[u8]] = &[
    b"UnityPlayer.dll",
    b"d3d11.dll",
    b"d3d12.dll",
    b"dxgi.dll",
    b"openvr_api.dll",
];

fn backup_path_for(live: &Path) -> PathBuf {
    let stem = live
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("overlay");
    live.with_file_name(format!("{stem}.original.exe"))
}

fn approved_targets() -> Vec<(String, PathBuf, PathBuf)> {
    APPROVED
        .iter()
        .map(|(name, path)| {
            let live = PathBuf::from(path);
            let backup = backup_path_for(&live);
            ((*name).to_owned(), live, backup)
        })
        .collect()
}

fn check_no_reparse(path: &Path) -> Result<()> {
    use std::os::windows::fs::MetadataExt;
    for ancestor in path.ancestors() {
        if !ancestor.exists() {
            continue;
        }
        let metadata = fs::symlink_metadata(ancestor)
            .map_err(|e| format!("Cannot inspect {}: {e}", ancestor.display()))?;
        if metadata.file_attributes() & 0x400 != 0 {
            return Err(format!(
                "Reparse/symlink paths are not allowed for KAT overlay management: {}",
                ancestor.display()
            ));
        }
    }
    Ok(())
}

fn contains_subslice(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || haystack.len() < needle.len() {
        return false;
    }
    let needle_lower: Vec<u8> = needle.iter().map(|b| b.to_ascii_lowercase()).collect();
    haystack
        .windows(needle_lower.len())
        .any(|w| w.iter().zip(&needle_lower).all(|(a, b)| a.to_ascii_lowercase() == *b))
}

fn sha256_and_unity_flag(path: &Path) -> Result<(u64, String, bool)> {
    let bytes = fs::read(path).map_err(|e| format!("Cannot read {}: {e}", path.display()))?;
    let is_unity = contains_subslice(&bytes, UNITY_MARKER);
    let digest = format!("{:x}", Sha256::digest(&bytes));
    Ok((bytes.len() as u64, digest, is_unity))
}

fn validate_stub_source(stub_exe: &Path) -> Result<(u64, String, Vec<u8>)> {
    if !stub_exe.is_file() {
        return Err(format!(
            "Stub binary is missing at {}. Run scripts\\build-release.ps1 first.",
            stub_exe.display()
        ));
    }
    let meta = fs::metadata(stub_exe).map_err(|e| e.to_string())?;
    if meta.len() == 0 || meta.len() > MAX_STUB_BYTES {
        return Err(format!(
            "Stub binary size ({}) is invalid: {}",
            meta.len(),
            stub_exe.display()
        ));
    }
    let bytes = fs::read(stub_exe).map_err(|e| e.to_string())?;
    for marker in FORBIDDEN_STUB_MARKERS {
        if contains_subslice(&bytes, marker) {
            return Err(format!(
                "Stub binary unexpectedly references {}; refusing to install.",
                String::from_utf8_lossy(marker)
            ));
        }
    }
    let hash = format!("{:x}", Sha256::digest(&bytes));
    Ok((meta.len(), hash, bytes))
}

fn is_stubbed_on_disk(live: &Path, backup: &Path) -> bool {
    if !live.is_file() || !backup.is_file() {
        return false;
    }
    let Ok(mut f) = File::open(live) else {
        return false;
    };
    let Ok(meta) = f.metadata() else {
        return false;
    };
    if meta.len() > MAX_STUB_BYTES {
        return false;
    }
    let mut buf = Vec::with_capacity(meta.len() as usize);
    if f.read_to_end(&mut buf).is_err() {
        return false;
    }
    !contains_subslice(&buf, UNITY_MARKER)
}

/// Fast metadata-only status check safe for the 2-second UI refresh loop.
pub fn stub_status(root: &Path) -> Value {
    stub_status_for(&approved_targets(), &root.join(STUB_EXE_NAME))
}

fn stub_status_for(targets: &[(String, PathBuf, PathBuf)], stub_exe: &Path) -> Value {
    let stub_len = fs::metadata(stub_exe).ok().filter(|m| m.is_file()).map(|m| m.len());
    let mut items = Vec::with_capacity(targets.len());
    let mut stubbed_count = 0usize;
    let mut original_count = 0usize;
    let mut missing_count = 0usize;
    let mut conflict_count = 0usize;

    for (name, live, backup) in targets {
        let live_len = fs::metadata(live).ok().filter(|m| m.is_file()).map(|m| m.len());
        let backup_len = fs::metadata(backup).ok().filter(|m| m.is_file()).map(|m| m.len());
        let state = match (live_len, backup_len) {
            (None, None) => {
                missing_count += 1;
                "missing"
            }
            (Some(_), None) => {
                original_count += 1;
                "original"
            }
            (Some(l), Some(b)) => {
                // Original Unity bootstrapper is ~680 KB and matches backup size if KAT updated over the stub.
                if Some(l) == stub_len || (l != b && l <= MAX_STUB_BYTES && b > l) {
                    stubbed_count += 1;
                    "stubbed"
                } else {
                    conflict_count += 1;
                    "conflict"
                }
            }
            (None, Some(_)) => {
                conflict_count += 1;
                "live-missing-backup-present"
            }
        };
        items.push(json!({
            "name": name,
            "state": state,
            "livePath": live,
            "backupPath": backup,
            "liveBytes": live_len,
            "backupBytes": backup_len,
        }));
    }

    let overall = if conflict_count > 0 {
        "conflict"
    } else if missing_count == targets.len() {
        "kat-missing"
    } else if stubbed_count == targets.len() {
        "stubbed"
    } else if original_count == targets.len() {
        if stub_len.is_some() {
            "original"
        } else {
            "stub-exe-missing"
        }
    } else {
        "partial"
    };

    json!({
        "state": overall,
        "stubBinaryPresent": stub_len.is_some(),
        "stubBinaryBytes": stub_len,
        "overlays": items,
    })
}

fn kat_blockers() -> Result<Vec<String>> {
    Ok(platform::checked_processes()?
        .into_iter()
        .filter(|(_, n)| {
            n.eq_ignore_ascii_case("KAT Gateway.exe")
                || n.eq_ignore_ascii_case("SkyrimVR.exe")
                || n.eq_ignore_ascii_case("KAT_Gateway_Updater.exe")
                || n.eq_ignore_ascii_case("GatewayUpdate.exe")
        })
        .map(|(pid, name)| format!("{name} (PID {pid})"))
        .collect())
}

pub fn ensure_kat_closed() -> Result<()> {
    let running = kat_blockers()?;
    if running.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "Close KAT Gateway and Skyrim VR before changing KAT overlay stubs. Running: {}",
            running.join(", ")
        ))
    }
}

fn move_file_win32(from: &Path, to: &Path, allow_replace: bool) -> std::io::Result<()> {
    if !allow_replace && to.exists() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!("Destination already exists: {}", to.display()),
        ));
    }
    let from_w = wide(&from.to_string_lossy());
    let to_w = wide(&to.to_string_lossy());
    let flags = if allow_replace {
        MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH
    } else {
        MOVEFILE_WRITE_THROUGH
    };
    let ok = unsafe { MoveFileExW(from_w.as_ptr(), to_w.as_ptr(), flags) };
    if ok == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn is_access_denied_io(err: &std::io::Error) -> bool {
    err.kind() == std::io::ErrorKind::PermissionDenied
        || err.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32)
}

#[derive(Debug)]
enum SwapError {
    AccessDenied(String),
    Other(String),
}

impl SwapError {
    fn into_message(self) -> String {
        match self {
            Self::AccessDenied(m) | Self::Other(m) => m,
        }
    }
}

fn map_io_err(context: &str, err: std::io::Error) -> SwapError {
    let msg = format!("{context}: {err}");
    if is_access_denied_io(&err) {
        SwapError::AccessDenied(msg)
    } else {
        SwapError::Other(msg)
    }
}

fn install_stubs_inner(
    targets: &[(String, PathBuf, PathBuf)],
    stub_exe: &Path,
    journal_path: Option<&Path>,
) -> std::result::Result<Value, SwapError> {
    let (stub_bytes_len, stub_hash, stub_bytes) =
        validate_stub_source(stub_exe).map_err(SwapError::Other)?;
    let mut actions = Vec::with_capacity(targets.len());

    for (name, live, backup) in targets {
        check_no_reparse(live).map_err(SwapError::Other)?;
        if backup.exists() {
            check_no_reparse(backup).map_err(SwapError::Other)?;
            let (orig_len, orig_hash, backup_is_unity) =
                sha256_and_unity_flag(backup).map_err(SwapError::Other)?;
            if !backup_is_unity {
                return Err(SwapError::Other(format!(
                    "Backup {} does not appear to be an original Unity executable; nothing was changed.",
                    backup.display()
                )));
            }
            if live.exists() {
                let (_, live_hash, live_is_unity) =
                    sha256_and_unity_flag(live).map_err(SwapError::Other)?;
                if live_is_unity {
                    return Err(SwapError::Other(format!(
                        "{name} appears to have been updated by KAT Gateway while an older backup exists at {}; review before stubbing.",
                        backup.display()
                    )));
                }
                if live_hash == stub_hash {
                    actions.push(json!({
                        "name": name,
                        "action": "already-stubbed",
                        "livePath": live,
                        "backupPath": backup,
                        "originalBytes": orig_len,
                        "originalSha256": orig_hash,
                        "stubSha256": stub_hash,
                    }));
                    continue;
                }
            }
            let staged = live.with_file_name(format!("{name}.stub.tmp"));
            fs::write(&staged, &stub_bytes)
                .map_err(|e| map_io_err(&format!("Cannot write staged stub for {name}"), e))?;
            if let Err(e) = move_file_win32(&staged, live, true) {
                let _ = fs::remove_file(&staged);
                return Err(map_io_err(&format!("Cannot activate stub for {name}"), e));
            }
            actions.push(json!({
                "name": name,
                "action": "updated-stub",
                "livePath": live,
                "backupPath": backup,
                "originalBytes": orig_len,
                "originalSha256": orig_hash,
                "stubBytes": stub_bytes_len,
                "stubSha256": stub_hash,
            }));
        } else {
            if !live.is_file() {
                return Err(SwapError::Other(format!(
                    "Expected KAT overlay binary is missing: {}",
                    live.display()
                )));
            }
            let (orig_len, orig_hash, live_is_unity) =
                sha256_and_unity_flag(live).map_err(SwapError::Other)?;
            if !live_is_unity {
                return Err(SwapError::Other(format!(
                    "{name} is not a Unity executable and has no .original.exe backup; nothing was changed."
                )));
            }
            let staged = live.with_file_name(format!("{name}.stub.tmp"));
            fs::write(&staged, &stub_bytes)
                .map_err(|e| map_io_err(&format!("Cannot stage stub in KAT folder for {name}"), e))?;
            let (_, staged_hash, _) = sha256_and_unity_flag(&staged).map_err(|e| {
                let _ = fs::remove_file(&staged);
                SwapError::Other(e)
            })?;
            if staged_hash != stub_hash {
                let _ = fs::remove_file(&staged);
                return Err(SwapError::Other(format!(
                    "Staged stub hash mismatch for {name}; nothing was changed."
                )));
            }
            if let Err(e) = move_file_win32(live, backup, false) {
                let _ = fs::remove_file(&staged);
                return Err(map_io_err(
                    &format!("Cannot move original {name} to backup"),
                    e,
                ));
            }
            if let Err(e) = move_file_win32(&staged, live, false) {
                // Roll back original immediately so the live executable is never left missing.
                let _ = move_file_win32(backup, live, false);
                let _ = fs::remove_file(&staged);
                return Err(map_io_err(&format!("Cannot place stub for {name}"), e));
            }
            actions.push(json!({
                "name": name,
                "action": "installed-stub",
                "livePath": live,
                "backupPath": backup,
                "originalBytes": orig_len,
                "originalSha256": orig_hash,
                "stubBytes": stub_bytes_len,
                "stubSha256": stub_hash,
            }));
        }
    }

    let record = json!({
        "state": "stubbed",
        "action": "install",
        "message": "KAT zero-GPU overlay stubs installed; original Unity overlays backed up as *.original.exe. KAT Gateway and OCU walking input are untouched.",
        "stubSha256": stub_hash,
        "stubBytes": stub_bytes_len,
        "overlays": actions,
    });
    if let Some(j) = journal_path {
        let _ = write_json(j, &record);
    }
    Ok(record)
}

fn restore_stubs_inner(
    targets: &[(String, PathBuf, PathBuf)],
    journal_path: Option<&Path>,
) -> std::result::Result<Value, SwapError> {
    let mut actions = Vec::with_capacity(targets.len());

    for (name, live, backup) in targets {
        check_no_reparse(live).map_err(SwapError::Other)?;
        if !backup.exists() {
            if !live.is_file() {
                return Err(SwapError::Other(format!(
                    "Neither {name} nor its .original.exe backup exists at {}",
                    live.display()
                )));
            }
            let (orig_len, orig_hash, live_is_unity) =
                sha256_and_unity_flag(live).map_err(SwapError::Other)?;
            if !live_is_unity {
                return Err(SwapError::Other(format!(
                    "{name} has no .original.exe backup and does not look like the original Unity binary."
                )));
            }
            actions.push(json!({
                "name": name,
                "action": "already-original",
                "livePath": live,
                "originalBytes": orig_len,
                "originalSha256": orig_hash,
            }));
            continue;
        }

        check_no_reparse(backup).map_err(SwapError::Other)?;
        let (orig_len, orig_hash, backup_is_unity) =
            sha256_and_unity_flag(backup).map_err(SwapError::Other)?;
        if !backup_is_unity {
            return Err(SwapError::Other(format!(
                "Backup {} is not a Unity overlay executable; refusing to restore.",
                backup.display()
            )));
        }

        let parked_stub = live.with_file_name(format!("{name}.stub-removing.tmp"));
        let had_live = if live.exists() {
            let (_, _, live_is_unity) = sha256_and_unity_flag(live).map_err(SwapError::Other)?;
            if live_is_unity {
                return Err(SwapError::Other(format!(
                    "Both {name} and its .original.exe backup are Unity executables; review manually so nothing is overwritten."
                )));
            }
            move_file_win32(live, &parked_stub, true).map_err(|e| {
                map_io_err(&format!("Cannot move stub aside before restoring {name}"), e)
            })?;
            true
        } else {
            false
        };

        if let Err(e) = move_file_win32(backup, live, false) {
            if had_live {
                let _ = move_file_win32(&parked_stub, live, false);
            }
            return Err(map_io_err(&format!("Cannot restore original {name}"), e));
        }
        if had_live {
            let _ = fs::remove_file(&parked_stub);
        }

        actions.push(json!({
            "name": name,
            "action": "restored-original",
            "livePath": live,
            "backupPath": backup,
            "originalBytes": orig_len,
            "originalSha256": orig_hash,
        }));
    }

    let record = json!({
        "state": "original",
        "action": "restore",
        "message": "Original KAT Unity overlays restored from *.original.exe backups.",
        "overlays": actions,
    });
    if let Some(j) = journal_path {
        let _ = write_json(j, &record);
    }
    Ok(record)
}

pub fn install_stubs(root: &Path) -> Result<Value> {
    ensure_kat_closed()?;
    let _ = close_non_stubbed(false);
    install_stubs_inner(
        &approved_targets(),
        &root.join(STUB_EXE_NAME),
        Some(&root.join(JOURNAL_REL_PATH)),
    )
    .map_err(SwapError::into_message)
}

pub fn restore_stubs(root: &Path) -> Result<Value> {
    ensure_kat_closed()?;
    // Close any lingering stub process too before moving files back.
    let _ = close_matching(false, true);
    restore_stubs_inner(&approved_targets(), Some(&root.join(JOURNAL_REL_PATH)))
        .map_err(SwapError::into_message)
}

fn run_elevated_self(root: &Path, flag: &str) -> Result<Value> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let journal = root.join(JOURNAL_REL_PATH);
    let _ = fs::remove_file(&journal);

    let verb = wide("runas");
    let file = wide(&exe.to_string_lossy());
    let params = wide(&format!("{flag} --root \"{}\"", root.display()));
    let dir = wide(&root.to_string_lossy());

    let mut sei: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
    sei.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
    sei.fMask = SEE_MASK_NOCLOSEPROCESS;
    sei.lpVerb = verb.as_ptr();
    sei.lpFile = file.as_ptr();
    sei.lpParameters = params.as_ptr();
    sei.lpDirectory = dir.as_ptr();
    sei.nShow = SW_HIDE;

    let launched = unsafe { ShellExecuteExW(&mut sei) };
    if launched == 0 {
        let err = std::io::Error::last_os_error();
        return Err(format!(
            "Administrator UAC prompt was cancelled or declined ({err}); KAT overlays were left unchanged."
        ));
    }
    if sei.hProcess.is_null() {
        return Err("Elevated helper did not return a process handle.".into());
    }
    let wait = unsafe { WaitForSingleObject(sei.hProcess, 60_000) };
    let mut exit_code = 1u32;
    unsafe {
        GetExitCodeProcess(sei.hProcess, &mut exit_code);
        CloseHandle(sei.hProcess);
    }
    if wait != WAIT_OBJECT_0 {
        return Err("Timed out waiting for elevated KAT stub operation to finish.".into());
    }
    if let Ok(record) = read_json(&journal) {
        if exit_code == 0 {
            return Ok(record);
        }
        if let Some(err) = record["error"].as_str() {
            return Err(err.to_owned());
        }
    }
    if exit_code == 0 {
        Ok(stub_status(root))
    } else {
        Err(format!(
            "Elevated KAT stub helper exited with code {exit_code}."
        ))
    }
}

pub fn install_or_elevate(root: &Path) -> Result<Value> {
    ensure_kat_closed()?;
    let stub_exe = root.join(STUB_EXE_NAME);
    validate_stub_source(&stub_exe)?;
    let _ = close_non_stubbed(false);
    match install_stubs_inner(
        &approved_targets(),
        &stub_exe,
        Some(&root.join(JOURNAL_REL_PATH)),
    ) {
        Ok(v) => Ok(v),
        Err(SwapError::AccessDenied(_)) => run_elevated_self(root, "--kat-stub-install"),
        Err(SwapError::Other(e)) => Err(e),
    }
}

pub fn restore_or_elevate(root: &Path) -> Result<Value> {
    ensure_kat_closed()?;
    let _ = close_matching(false, true);
    match restore_stubs_inner(&approved_targets(), Some(&root.join(JOURNAL_REL_PATH))) {
        Ok(v) => Ok(v),
        Err(SwapError::AccessDenied(_)) => run_elevated_self(root, "--kat-stub-restore"),
        Err(SwapError::Other(e)) => Err(e),
    }
}

pub fn record_cli_error(root: &Path, action: &str, err: &str) {
    let _ = write_json(
        &root.join(JOURNAL_REL_PATH),
        &json!({
            "state": "error",
            "action": action,
            "error": err,
        }),
    );
}

#[derive(Default)]
struct CloseOnce {
    attempted: HashMap<String, u32>,
    reported: std::collections::HashSet<String>,
}
#[derive(Debug, PartialEq)]
enum Decision {
    Close,
    Ignore,
    Respawn,
}
impl CloseOnce {
    fn observe(&mut self, name: &str, pid: u32) -> Decision {
        let name = name.to_ascii_lowercase();
        match self.attempted.get(&name) {
            None => {
                self.attempted.insert(name, pid);
                Decision::Close
            }
            Some(original) if *original == pid => Decision::Ignore,
            Some(_) => {
                if self.reported.insert(name) {
                    Decision::Respawn
                } else {
                    Decision::Ignore
                }
            }
        }
    }
}
struct Target {
    handle: HANDLE,
    pid: u32,
    name: String,
    asked: Instant,
}
impl Drop for Target {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.handle);
        }
    }
}
unsafe extern "system" fn close_window(hwnd: HWND, ctx: LPARAM) -> i32 {
    let pid = unsafe { *(ctx as *const u32) };
    let mut owner = 0;
    unsafe {
        GetWindowThreadProcessId(hwnd, &mut owner);
        if owner == pid {
            PostMessageW(hwnd, WM_CLOSE, 0, 0);
        }
    }
    1
}
fn approved_path(name: &str, path: &str) -> bool {
    APPROVED
        .iter()
        .any(|(n, p)| name.eq_ignore_ascii_case(n) && path.eq_ignore_ascii_case(p))
}
fn target(pid: u32, name: &str) -> Result<(Target, PathBuf)> {
    unsafe {
        let h = OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_TERMINATE | PROCESS_SYNCHRONIZE,
            0,
            pid,
        );
        if h.is_null() {
            return Err(format!(
                "Cannot verify {name} (PID {pid}); it was left unchanged"
            ));
        }
        let mut chars = vec![0u16; 32768];
        let mut n = chars.len() as u32;
        if QueryFullProcessImageNameW(h, 0, chars.as_mut_ptr(), &mut n) == 0 {
            CloseHandle(h);
            return Err(format!(
                "Cannot read {name}'s exact path; it was left unchanged"
            ));
        }
        let path = String::from_utf16_lossy(&chars[..n as usize]);
        if !approved_path(name, &path) {
            CloseHandle(h);
            return Err(format!(
                "{name} is outside the two approved installation paths; it was left unchanged"
            ));
        }
        Ok((
            Target {
                handle: h,
                pid,
                name: name.to_owned(),
                asked: Instant::now(),
            },
            PathBuf::from(path),
        ))
    }
}

pub fn close(wait_for_startup: bool) -> Result<Value> {
    close_non_stubbed(wait_for_startup)
}

fn close_non_stubbed(wait_for_startup: bool) -> Result<Value> {
    let all_stubbed = APPROVED.iter().all(|(_, p)| {
        let live = Path::new(p);
        is_stubbed_on_disk(live, &backup_path_for(live))
    });
    if all_stubbed {
        return Ok(json!({
            "message": "KAT zero-GPU overlay stubs are active; Unity overlays stay disabled with no startup wait.",
            "results": [],
            "respawned": false,
            "stubsActive": true,
            "boundedSeconds": 0,
        }));
    }
    close_matching(wait_for_startup, false)
}

fn close_matching(wait_for_startup: bool, include_stubs: bool) -> Result<Value> {
    let deadline = Instant::now() + Duration::from_secs(if wait_for_startup { 10 } else { 3 });
    let mut targets = HashMap::<u32, Target>::new();
    let mut results = vec![];
    let mut pass = CloseOnce::default();
    let mut respawned = false;
    loop {
        for (pid, name) in platform::checked_processes()? {
            if !APPROVED.iter().any(|(n, _)| name.eq_ignore_ascii_case(n))
                || targets.contains_key(&pid)
            {
                continue;
            }
            match pass.observe(&name, pid) {
                Decision::Ignore => continue,
                Decision::Respawn => {
                    respawned = true;
                    results.push(json!({"name":name,"pid":pid,"respawned":true,"warning":"KAT restarted its VR overlay; install the KAT zero-GPU stubs to prevent Unity overlay launches permanently."}));
                    continue;
                }
                Decision::Close => {}
            }
            match target(pid, &name) {
                Ok((t, exe_path)) => {
                    if !include_stubs && is_stubbed_on_disk(&exe_path, &backup_path_for(&exe_path)) {
                        results.push(json!({"name":t.name,"pid":t.pid,"stubActive":true,"skippedClose":true}));
                        continue;
                    }
                    unsafe {
                        EnumWindows(Some(close_window), &t.pid as *const u32 as isize);
                    }
                    targets.insert(pid, t);
                }
                Err(e) => {
                    results.push(json!({"name":name,"error":e}));
                }
            }
        }
        let mut completed = vec![];
        for (pid, t) in &targets {
            unsafe {
                if WaitForSingleObject(t.handle, 0) == WAIT_OBJECT_0 {
                    completed.push(*pid);
                    results.push(json!({"name":t.name,"closed":"gracefully"}));
                } else if t.asked.elapsed() >= Duration::from_secs(2) {
                    if TerminateProcess(t.handle, 0) != 0 {
                        let done = WaitForSingleObject(t.handle, 500) == WAIT_OBJECT_0;
                        results.push(json!({"name":t.name,"closed":if done{"terminated after close request"}else{"termination requested"}}));
                    } else {
                        results.push(json!({"name":t.name,"error":"Could not terminate verified overlay; main KAT processes were untouched"}));
                    }
                    completed.push(*pid);
                }
            }
        }
        for pid in completed {
            targets.remove(&pid);
        }
        if Instant::now() >= deadline {
            break;
        }
        thread::sleep(Duration::from_millis(400));
    }
    for t in targets.values() {
        results.push(json!({"name":t.name,"closeRequested":true,"stillRunningAtDeadline":true}));
    }
    Ok(
        json!({"message":if respawned{"KAT restarted a VR overlay; install KAT zero-GPU stubs on the Startup tab to keep Unity overlays disabled."}else{"Closed each approved KAT overlay at most once; Gateway and its input service were preserved."},"results":results,"respawned":respawned,"boundedSeconds":if wait_for_startup{10}else{3}}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(label: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "skyrim-kat-stub-{}-{}-{}",
            std::process::id(),
            crate::config::now().to_bits(),
            label
        ));
        fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn exact_paths_only() {
        assert!(approved_path("KATGatewayOverlay.exe", APPROVED[1].1));
        assert!(!approved_path("KAT Gateway.exe", APPROVED[1].1));
        assert!(!approved_path(
            "KATGatewayOverlay.exe",
            r"C:\other\KATGatewayOverlay.exe"
        ));
        assert!(!approved_path("KATArmViewOverlay.exe", APPROVED[1].1));
    }

    #[test]
    fn fake_executor_closes_each_overlay_once_and_reports_respawn() {
        let mut pass = CloseOnce::default();
        let mut closed = vec![];
        let mut warnings = vec![];
        let frames = vec![
            vec![
                (10, APPROVED[0].0, APPROVED[0].1),
                (20, APPROVED[1].0, APPROVED[1].1),
            ],
            vec![(10, APPROVED[0].0, APPROVED[0].1)],
            vec![
                (11, APPROVED[0].0, APPROVED[0].1),
                (21, APPROVED[1].0, APPROVED[1].1),
            ],
            vec![
                (12, APPROVED[0].0, APPROVED[0].1),
                (22, APPROVED[1].0, APPROVED[1].1),
            ],
        ];
        for frame in frames {
            for (pid, name, path) in frame {
                match pass.observe(name, pid) {
                    Decision::Close => {
                        if approved_path(name, path) {
                            closed.push(pid);
                        }
                    }
                    Decision::Respawn => warnings.push(name.to_owned()),
                    Decision::Ignore => {}
                }
            }
        }
        assert_eq!(closed, vec![10, 20]);
        assert_eq!(warnings.len(), 2);
        let mut new_action = CloseOnce::default();
        assert_eq!(new_action.observe(APPROVED[0].0, 12), Decision::Close);
    }

    #[test]
    fn install_and_restore_stubs_roundtrip_and_idempotency() {
        let dir = temp_dir("roundtrip");
        let arm_live = dir.join("KATArmViewOverlay.exe");
        let arm_bak = backup_path_for(&arm_live);
        let gw_live = dir.join("KATGatewayOverlay.exe");
        let gw_bak = backup_path_for(&gw_live);
        let stub_exe = dir.join("kat-overlay-stub.exe");
        let journal = dir.join("journal.json");

        // Fake Unity launcher payloads (> stub length and containing UnityPlayer.dll)
        let orig_arm = b"MZ-fake-unity-arm-launcher-imports-UnityPlayer.dll-end-padding-1234567890";
        let orig_gw = b"MZ-fake-unity-gw-launcher-imports-UnityPlayer.dll-end-padding-0987654321";
        let stub_v1 = b"MZ-clean-zero-gpu-stub-v1";
        let stub_v2 = b"MZ-clean-zero-gpu-stub-v2-updated";

        fs::write(&arm_live, orig_arm).unwrap();
        fs::write(&gw_live, orig_gw).unwrap();
        fs::write(&stub_exe, stub_v1).unwrap();

        let targets = vec![
            ("KATArmViewOverlay.exe".into(), arm_live.clone(), arm_bak.clone()),
            ("KATGatewayOverlay.exe".into(), gw_live.clone(), gw_bak.clone()),
        ];

        assert_eq!(stub_status_for(&targets, &stub_exe)["state"], "original");

        // Install v1
        let installed = install_stubs_inner(&targets, &stub_exe, Some(&journal)).unwrap();
        assert_eq!(installed["state"], "stubbed");
        assert_eq!(fs::read(&arm_live).unwrap(), stub_v1);
        assert_eq!(fs::read(&gw_live).unwrap(), stub_v1);
        assert_eq!(fs::read(&arm_bak).unwrap(), orig_arm);
        assert_eq!(fs::read(&gw_bak).unwrap(), orig_gw);
        assert!(is_stubbed_on_disk(&arm_live, &arm_bak));
        assert_eq!(stub_status_for(&targets, &stub_exe)["state"], "stubbed");

        // Re-running install with identical stub is idempotent
        let again = install_stubs_inner(&targets, &stub_exe, Some(&journal)).unwrap();
        assert_eq!(again["overlays"][0]["action"], "already-stubbed");
        assert_eq!(fs::read(&arm_bak).unwrap(), orig_arm);

        // Updating stub to v2 updates live stub without touching original backups
        fs::write(&stub_exe, stub_v2).unwrap();
        let updated = install_stubs_inner(&targets, &stub_exe, Some(&journal)).unwrap();
        assert_eq!(updated["overlays"][0]["action"], "updated-stub");
        assert_eq!(fs::read(&arm_live).unwrap(), stub_v2);
        assert_eq!(fs::read(&arm_bak).unwrap(), orig_arm);

        // Restore brings back exact original bytes and removes .original.exe backups
        let restored = restore_stubs_inner(&targets, Some(&journal)).unwrap();
        assert_eq!(restored["state"], "original");
        assert_eq!(fs::read(&arm_live).unwrap(), orig_arm);
        assert_eq!(fs::read(&gw_live).unwrap(), orig_gw);
        assert!(!arm_bak.exists());
        assert!(!gw_bak.exists());
        assert_eq!(stub_status_for(&targets, &stub_exe)["state"], "original");

        // Re-running restore when already original is idempotent
        let restored_again = restore_stubs_inner(&targets, Some(&journal)).unwrap();
        assert_eq!(restored_again["overlays"][0]["action"], "already-original");

        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn refuses_to_clobber_if_kat_updated_unity_exe_while_backup_exists() {
        let dir = temp_dir("conflict");
        let arm_live = dir.join("KATArmViewOverlay.exe");
        let arm_bak = backup_path_for(&arm_live);
        let stub_exe = dir.join("kat-overlay-stub.exe");

        fs::write(&arm_bak, b"MZ-old-UnityPlayer.dll-v1").unwrap();
        fs::write(&arm_live, b"MZ-new-UnityPlayer.dll-v2").unwrap();
        fs::write(&stub_exe, b"MZ-clean-stub").unwrap();

        let targets = vec![(
            "KATArmViewOverlay.exe".into(),
            arm_live.clone(),
            arm_bak.clone(),
        )];
        assert_eq!(stub_status_for(&targets, &stub_exe)["state"], "conflict");
        assert!(install_stubs_inner(&targets, &stub_exe, None).is_err());
        assert!(restore_stubs_inner(&targets, None).is_err());

        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn rejects_stub_referencing_graphics_or_openvr_dlls() {
        let dir = temp_dir("forbidden");
        for bad in [b"imports d3d11.dll".as_slice(), b"imports openvr_api.dll".as_slice()] {
            let p = dir.join("bad-stub.exe");
            fs::write(&p, bad).unwrap();
            assert!(validate_stub_source(&p).is_err());
        }
        fs::remove_dir_all(dir).unwrap();
    }
}
