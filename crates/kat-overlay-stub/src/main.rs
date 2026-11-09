#![cfg_attr(not(test), windows_subsystem = "windows")]
//! Zero-GPU replacement stub for KAT Gateway's optional Unity VR overlays
//! (`KATArmViewOverlay.exe` and `KATGatewayOverlay.exe`).
//!
//! Never loads Unity, Direct3D, DXGI, Vulkan, OpenGL, or OpenVR, so it creates
//! zero GPU/VRAM context while satisfying KAT Gateway's 0.5-second process-name
//! watchdog (`Process.GetProcessesByName(...)`). Automatically exits once both
//! `KAT Gateway.exe` and `vrserver.exe` have exited.

use std::{path::Path, ptr::null, thread, time::Duration};
use windows_sys::Win32::{
    Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, INVALID_HANDLE_VALUE},
    System::{
        Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
            TH32CS_SNAPPROCESS,
        },
        Threading::CreateMutexW,
    },
};

const POLL_INTERVAL: Duration = Duration::from_secs(5);
const STARTUP_GRACE_POLLS: u32 = 3;

fn wide_nul(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn mutex_name_for_exe(exe_path: &Path) -> String {
    let stem = exe_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("kat_overlay");
    let sanitized: String = stem
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect();
    format!(r"Local\SkyrimVRControlCenter_KatOverlayStub_{sanitized}")
}

fn should_stay_alive(poll_index: u32, running_processes: &[String]) -> bool {
    let host_active = running_processes.iter().any(|name| {
        name.eq_ignore_ascii_case("KAT Gateway.exe") || name.eq_ignore_ascii_case("vrserver.exe")
    });
    if host_active {
        return true;
    }
    poll_index < STARTUP_GRACE_POLLS
}

fn running_process_names() -> Option<Vec<String>> {
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap == INVALID_HANDLE_VALUE || snap.is_null() {
            return None;
        }
        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut names = Vec::new();
        if Process32FirstW(snap, &mut entry) != 0 {
            loop {
                let len = entry
                    .szExeFile
                    .iter()
                    .position(|&c| c == 0)
                    .unwrap_or(entry.szExeFile.len());
                names.push(String::from_utf16_lossy(&entry.szExeFile[..len]));
                if Process32NextW(snap, &mut entry) == 0 {
                    break;
                }
            }
        }
        CloseHandle(snap);
        Some(names)
    }
}

struct MutexGuard(HANDLE);

impl Drop for MutexGuard {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}

fn acquire_single_instance(name: &str) -> Option<MutexGuard> {
    let w = wide_nul(name);
    unsafe {
        let handle = CreateMutexW(null(), 0, w.as_ptr());
        if handle.is_null() {
            return None;
        }
        if GetLastError() == ERROR_ALREADY_EXISTS {
            CloseHandle(handle);
            return None;
        }
        Some(MutexGuard(handle))
    }
}

fn main() {
    if std::env::args().any(|a| a == "--self-test") {
        return;
    }
    let exe = std::env::current_exe().unwrap_or_else(|_| "kat-overlay-stub.exe".into());
    let mutex_name = mutex_name_for_exe(&exe);
    let Some(_instance_guard) = acquire_single_instance(&mutex_name) else {
        // Another instance for this exact overlay executable is already running.
        return;
    };

    let mut poll_index = 0u32;
    loop {
        thread::sleep(POLL_INTERVAL);
        if let Some(names) = running_process_names()
            && !should_stay_alive(poll_index, &names)
        {
            break;
        }
        poll_index = poll_index.saturating_add(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizes_mutex_name_per_overlay_stem() {
        assert_eq!(
            mutex_name_for_exe(Path::new(
                r"C:\Program Files (x86)\KAT Gateway\KATArmViewOverlay\KATArmViewOverlay.exe"
            )),
            r"Local\SkyrimVRControlCenter_KatOverlayStub_katarmviewoverlay"
        );
        assert_eq!(
            mutex_name_for_exe(Path::new(
                r"C:\Program Files (x86)\KAT Gateway\overlay\KATGatewayOverlay.exe"
            )),
            r"Local\SkyrimVRControlCenter_KatOverlayStub_katgatewayoverlay"
        );
    }

    #[test]
    fn lifecycle_stays_alive_while_gateway_or_steamvr_runs_and_exits_after_grace() {
        let empty: Vec<String> = vec![];
        assert!(should_stay_alive(0, &empty));
        assert!(should_stay_alive(2, &empty));
        assert!(!should_stay_alive(3, &empty));

        let gateway_only = vec!["KAT Gateway.exe".to_owned()];
        assert!(should_stay_alive(10, &gateway_only));

        let steamvr_only = vec!["vrserver.exe".to_owned()];
        assert!(should_stay_alive(10, &steamvr_only));

        let unrelated = vec!["KAT Gateway Service.exe".to_owned(), "explorer.exe".to_owned()];
        assert!(!should_stay_alive(3, &unrelated));
    }
}

