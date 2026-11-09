use crate::config::{Result, wide};
use std::{os::windows::process::CommandExt, path::Path, process::Command};
use windows_sys::Win32::{
    Foundation::{CloseHandle, ERROR_NO_MORE_FILES, GetLastError, INVALID_HANDLE_VALUE},
    System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    },
    UI::{Shell::ShellExecuteW, WindowsAndMessaging::SW_SHOWNORMAL},
};
pub fn checked_processes() -> Result<Vec<(u32, String)>> {
    unsafe {
        let h = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if h == INVALID_HANDLE_VALUE {
            return Err("Cannot enumerate processes; close-state cannot be established".into());
        }
        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut items = Vec::new();
        let mut more = Process32FirstW(h, &mut entry);
        if more == 0 {
            CloseHandle(h);
            return Err("Cannot read process snapshot; close-state cannot be established".into());
        }
        while more != 0 {
            let n = entry
                .szExeFile
                .iter()
                .position(|v| *v == 0)
                .unwrap_or(entry.szExeFile.len());
            items.push((
                entry.th32ProcessID,
                String::from_utf16_lossy(&entry.szExeFile[..n]),
            ));
            more = Process32NextW(h, &mut entry);
        }
        let last_error = GetLastError();
        CloseHandle(h);
        if last_error != ERROR_NO_MORE_FILES {
            return Err(format!(
                "Incomplete process snapshot (error {last_error}); close-state cannot be established"
            ));
        }
        Ok(items)
    }
}
pub fn processes() -> Vec<(u32, String)> {
    checked_processes().unwrap_or_default()
}
pub fn is_running(name: &str) -> bool {
    processes()
        .iter()
        .any(|(_, n)| n.eq_ignore_ascii_case(name))
}
pub fn open(target: &str) -> Result<()> {
    let verb = wide("open");
    let file = wide(target);
    let dir = Path::new(target)
        .parent()
        .filter(|p| p.is_dir())
        .map(|p| wide(&p.to_string_lossy()));
    let r = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            file.as_ptr(),
            std::ptr::null(),
            dir.as_ref().map_or(std::ptr::null(), |v| v.as_ptr()),
            SW_SHOWNORMAL,
        )
    };
    if r as isize <= 32 {
        Err(format!(
            "Windows could not open the selected item (code {})",
            r as isize
        ))
    } else {
        Ok(())
    }
}
pub fn spawn_engine(exe: &Path, music_root: &Path, log: &Path) -> Result<u32> {
    if !exe.is_file() {
        return Err(format!("Native music engine is missing: {}", exe.display()));
    }
    let out = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(log)
        .map_err(|e| e.to_string())?;
    let err = out.try_clone().map_err(|e| e.to_string())?;
    let child = Command::new(exe)
        .arg("--app-dir")
        .arg(music_root)
        .arg("serve")
        .stdin(std::process::Stdio::null())
        .stdout(out)
        .stderr(err)
        .creation_flags(0x08000000)
        .spawn()
        .map_err(|e| e.to_string())?;
    Ok(child.id())
}
