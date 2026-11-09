use crate::config::{Result, wide};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs,
    net::UdpSocket,
    path::Path,
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::{FreeLibrary, HMODULE},
    System::LibraryLoader::{
        GetProcAddress, LOAD_LIBRARY_SEARCH_DEFAULT_DIRS, LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR,
        LoadLibraryExW,
    },
};

pub const CAPI_SHA256: &str = "a1487f3bb83748a3a7747a4b807b593549cd6ecbc5efd4881e057e9f201adc53";
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct Settings {
    pub enabled: bool,
    pub menus: bool,
    pub grabs: bool,
    pub menu_start: u8,
    pub menu_end: u8,
    pub menu_strength: u8,
    pub grab_start: u8,
    pub grab_end: u8,
    pub object_strength: u8,
    pub actor_strength: u8,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: true,
            menus: true,
            grabs: true,
            menu_start: 5,
            menu_end: 6,
            menu_strength: 2,
            grab_start: 2,
            grab_end: 8,
            object_strength: 3,
            actor_strength: 2,
        }
    }
}
impl Settings {
    pub fn validate(&self) -> Result<()> {
        if !(2..=7).contains(&self.menu_start)
            || self.menu_end <= self.menu_start
            || self.menu_end > 8
        {
            return Err("Menu travel must be start 2–7, end after start and at most 8".into());
        }
        if self.grab_start >= self.grab_end || self.grab_end > 9 {
            return Err("Grab travel must satisfy 0 <= start < end <= 9".into());
        }
        if [
            self.menu_strength,
            self.object_strength,
            self.actor_strength,
        ]
        .iter()
        .any(|v| !(1..=4).contains(v))
        {
            return Err("Resistance strength is limited to 1–4".into());
        }
        Ok(())
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Profile {
    Off,
    Menu,
    Object,
    Actor,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(C)]
pub struct TriggerCommand {
    pub mode: i32,
    pub padding: [u8; 4],
    pub data: [u8; 48],
}
impl TriggerCommand {
    pub fn new(p: Profile, s: &Settings) -> Self {
        let mut c = Self {
            mode: 0,
            padding: [0; 4],
            data: [0; 48],
        };
        match p {
            Profile::Off => {}
            Profile::Menu => {
                c.mode = 2;
                c.data[..3].copy_from_slice(&[s.menu_start, s.menu_end, s.menu_strength]);
            }
            Profile::Object | Profile::Actor => {
                c.mode = 5;
                c.data[..4].copy_from_slice(&[
                    s.grab_start,
                    s.grab_end,
                    1,
                    if p == Profile::Object {
                        s.object_strength
                    } else {
                        s.actor_strength
                    },
                ]);
            }
        }
        c
    }
}
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Held {
    None,
    Object,
    Actor,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct State {
    pub schema_version: u32,
    #[serde(rename = "type")]
    pub kind: String,
    pub pid: u32,
    pub seq: u64,
    pub menu_open: bool,
    pub game_loaded: bool,
    pub held_left: Held,
    pub held_right: Held,
}
pub fn parse_state(raw: &[u8]) -> Result<State> {
    if raw.len() > 4096 {
        return Err("Packet too large".into());
    }
    let s: State = serde_json::from_slice(raw).map_err(|e| e.to_string())?;
    if s.schema_version != 1 || s.kind != "state" {
        return Err("Unsupported bridge schema".into());
    }
    Ok(s)
}
pub fn profiles(s: &Settings, state: Option<&State>, fresh: bool) -> [Profile; 2] {
    if !s.enabled || !fresh {
        return [Profile::Off; 2];
    }
    let Some(st) = state else {
        return [Profile::Off; 2];
    };
    if s.menus && st.menu_open {
        return [Profile::Menu; 2];
    }
    if !st.game_loaded || !s.grabs {
        return [Profile::Off; 2];
    }
    [st.held_left, st.held_right].map(|h| match h {
        Held::None => Profile::Off,
        Held::Object => Profile::Object,
        Held::Actor => Profile::Actor,
    })
}
pub trait Hardware {
    fn connect(&mut self) -> Result<bool>;
    fn send(&mut self, side: u8, command: &TriggerCommand) -> Result<()>;
    fn release(&mut self);
}
type Init = unsafe extern "C" fn() -> i32;
type Deinit = unsafe extern "C" fn();
type Active = unsafe extern "C" fn() -> bool;
type SendEffect = unsafe extern "C" fn(u8, *const TriggerCommand) -> i32;
pub struct Capi {
    path: std::path::PathBuf,
    lib: HMODULE,
    init: Option<Init>,
    deinit: Option<Deinit>,
    active: Option<Active>,
    send_effect: Option<SendEffect>,
    initialized: bool,
}
impl Capi {
    pub fn new(path: std::path::PathBuf) -> Self {
        Self {
            path,
            lib: std::ptr::null_mut(),
            init: None,
            deinit: None,
            active: None,
            send_effect: None,
            initialized: false,
        }
    }
    fn load(&mut self) -> Result<()> {
        let bytes = fs::read(&self.path).map_err(|_| {
            "Verified PSVR2 CAPI DLL is missing; check the existing toolkit installation"
                .to_string()
        })?;
        if format!("{:x}", Sha256::digest(&bytes)) != CAPI_SHA256 {
            return Err(
                "CAPI differs from verified experimental-2 build; feedback remains disarmed".into(),
            );
        }
        let path = wide(&self.path.to_string_lossy());
        self.lib = unsafe {
            LoadLibraryExW(
                path.as_ptr(),
                std::ptr::null_mut(),
                LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_DEFAULT_DIRS,
            )
        };
        if self.lib.is_null() {
            return Err(format!(
                "Cannot load CAPI: {}",
                std::io::Error::last_os_error()
            ));
        }
        unsafe {
            self.init = Some(std::mem::transmute::<
                unsafe extern "system" fn() -> isize,
                Init,
            >(
                GetProcAddress(self.lib, c"psvr2_toolkit_init".as_ptr().cast())
                    .ok_or("Missing CAPI init export")?,
            ));
            self.deinit = Some(std::mem::transmute::<
                unsafe extern "system" fn() -> isize,
                Deinit,
            >(
                GetProcAddress(self.lib, c"psvr2_toolkit_deinit".as_ptr().cast())
                    .ok_or("Missing CAPI deinit export")?,
            ));
            self.active = Some(std::mem::transmute::<
                unsafe extern "system" fn() -> isize,
                Active,
            >(
                GetProcAddress(self.lib, c"psvr2_toolkit_get_driver_active".as_ptr().cast())
                    .ok_or("Missing CAPI active export")?,
            ));
            self.send_effect = Some(std::mem::transmute::<
                unsafe extern "system" fn() -> isize,
                SendEffect,
            >(
                GetProcAddress(
                    self.lib,
                    c"psvr2_toolkit_set_trigger_effect".as_ptr().cast(),
                )
                .ok_or("Missing CAPI trigger export")?,
            ));
        }
        Ok(())
    }
}
impl Hardware for Capi {
    fn connect(&mut self) -> Result<bool> {
        if self.lib.is_null() {
            self.load()?
        }
        if !self.initialized {
            let code = unsafe { self.init.ok_or("CAPI unavailable")?() };
            if code != 0 {
                return Err(format!("CAPI initialization result {code}"));
            }
            self.initialized = true;
        }
        Ok(unsafe { self.active.ok_or("CAPI unavailable")?() })
    }
    fn send(&mut self, side: u8, c: &TriggerCommand) -> Result<()> {
        let code = unsafe { self.send_effect.ok_or("CAPI unavailable")?(side, c) };
        if code == 0 {
            Ok(())
        } else {
            Err(format!("CAPI trigger result {code}"))
        }
    }
    fn release(&mut self) {
        if self.initialized {
            let _ = self.send(2, &TriggerCommand::new(Profile::Off, &Settings::default()));
            if let Some(f) = self.deinit {
                unsafe { f() }
            }
            self.initialized = false;
        }
    }
}
impl Drop for Capi {
    fn drop(&mut self) {
        self.release();
        if !self.lib.is_null() {
            unsafe {
                FreeLibrary(self.lib);
            }
        }
    }
}
pub fn capi_path(root: &Path) -> Result<std::path::PathBuf> {
    Ok(
        crate::config::feedback_install_path(root, "driverDir", "Missing installed driver directory")?
            .join("psvr2_toolkit_capi.dll"),
    )
}

pub struct Engine<H: Hardware> {
    pub settings: Settings,
    pub hardware: H,
    socket: Option<UdpSocket>,
    pub state: Option<State>,
    last_packet: Option<Instant>,
    last_commands: Option<[TriggerCommand; 2]>,
    pub active: bool,
    pub fault: String,
    test: Option<(Profile, Instant)>,
    pub output: [Profile; 2],
}
impl<H: Hardware> Engine<H> {
    pub fn new(settings: Settings, hardware: H, port: Option<u16>) -> Result<Self> {
        settings.validate()?;
        let socket = if let Some(p) = port {
            let s=UdpSocket::bind(("127.0.0.1",p)).map_err(|_|"PSVR2 feedback port is already in use. Close the old PSVR2 Controls panel before starting this feedback engine".to_string())?;
            s.set_nonblocking(true).map_err(|e| e.to_string())?;
            Some(s)
        } else {
            None
        };
        Ok(Self {
            settings,
            hardware,
            socket,
            state: None,
            last_packet: None,
            last_commands: None,
            active: false,
            fault: String::new(),
            test: None,
            output: [Profile::Off; 2],
        })
    }
    pub fn receive(&mut self, raw: &[u8], now: Instant) -> Result<()> {
        let st = parse_state(raw)?;
        if self
            .state
            .as_ref()
            .is_some_and(|old| old.pid == st.pid && st.seq <= old.seq)
        {
            return Ok(());
        }
        self.state = Some(st);
        self.last_packet = Some(now);
        Ok(())
    }
    pub fn poll(&mut self, now: Instant) {
        let mut b = [0; 8192];
        while let Some(s) = &self.socket {
            let r = s.recv_from(&mut b);
            match r {
                Ok((n, addr)) if addr.ip().is_loopback() => {
                    if self.receive(&b[..n], now).is_err() {
                        self.last_packet = None;
                    }
                }
                _ => break,
            }
        }
    }
    pub fn arm(&mut self) {
        self.settings.enabled = true;
        self.fault.clear();
        self.last_commands = None;
        self.test = None;
    }
    pub fn stop(&mut self) {
        self.settings.enabled = false;
        self.test = None;
        self.last_commands = None;
        self.hardware.release();
        self.active = false;
        self.output = [Profile::Off; 2];
    }
    pub fn settings(&mut self, s: Settings) -> Result<()> {
        s.validate()?;
        self.settings = s;
        self.test = None;
        self.last_commands = None;
        if !self.fault.is_empty() {
            self.settings.enabled = false;
        }
        Ok(())
    }
    pub fn test(&mut self, p: Profile, now: Instant) -> Result<()> {
        if p == Profile::Off {
            return Err("Use Stop to release feedback".into());
        }
        if !self.active || !self.fault.is_empty() {
            return Err(
                "Start feedback with SteamVR and controllers connected before testing".into(),
            );
        }
        self.test = Some((p, now + Duration::from_secs(2)));
        self.last_commands = None;
        Ok(())
    }
    pub fn step(&mut self, now: Instant, process_alive: bool) -> Value {
        self.poll(now);
        let fresh = process_alive
            && self
                .last_packet
                .is_some_and(|t| now.saturating_duration_since(t) < Duration::from_millis(3500));
        let mut desired = profiles(&self.settings, self.state.as_ref(), fresh);
        if let Some((p, until)) = self.test {
            if now < until {
                desired = [p; 2]
            } else {
                self.test = None;
            }
        }
        if !self.fault.is_empty() {
            desired = [Profile::Off; 2];
        }
        let work = (|| -> Result<()> {
            if !self.fault.is_empty() {
                return Ok(());
            }
            let was = self.active;
            self.active = self.hardware.connect()?;
            if self.active {
                let commands = desired.map(|p| TriggerCommand::new(p, &self.settings));
                if !was || self.last_commands != Some(commands) {
                    for (i, c) in commands.iter().enumerate() {
                        self.hardware.send(i as u8, c)?;
                    }
                    self.last_commands = Some(commands);
                }
            } else {
                self.last_commands = None;
            }
            Ok(())
        })();
        if let Err(e) = work {
            self.hardware.release();
            self.active = false;
            self.last_commands = None;
            self.test = None;
            self.settings.enabled = false;
            self.fault = format!("Stopped after driver error: {e}. Use Start to rearm.");
        }
        self.output = if self.active && self.fault.is_empty() {
            desired
        } else {
            [Profile::Off; 2]
        };
        json!({"updatedAt":crate::config::now(),"driverActive":self.active,"bridgeFresh":fresh,"bridge":self.state,"enabled":self.settings.enabled,"leftProfile":self.output[0],"rightProfile":self.output[1],"testActive":self.test.is_some(),"error":self.fault,"runtime":"rust"})
    }
}
impl<H: Hardware> Drop for Engine<H> {
    fn drop(&mut self) {
        self.hardware.release();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[derive(Default)]
    struct Fake {
        sent: Vec<(u8, TriggerCommand)>,
        releases: usize,
        fail_right: bool,
    }
    impl Hardware for Fake {
        fn connect(&mut self) -> Result<bool> {
            Ok(true)
        }
        fn send(&mut self, s: u8, c: &TriggerCommand) -> Result<()> {
            if s == 1 && self.fail_right {
                return Err("test right failure".into());
            }
            self.sent.push((s, *c));
            Ok(())
        }
        fn release(&mut self) {
            self.releases += 1;
        }
    }
    fn state(seq: u64) -> Vec<u8> {
        serde_json::to_vec(&json!({"schemaVersion":1,"type":"state","pid":123,"seq":seq,"menuOpen":false,"gameLoaded":true,"heldLeft":"actor","heldRight":"object"})).unwrap()
    }
    #[test]
    fn abi_matches_vendor() {
        assert_eq!(std::mem::size_of::<TriggerCommand>(), 56);
        assert_eq!(std::mem::offset_of!(TriggerCommand, data), 8);
        let c = TriggerCommand::new(Profile::Menu, &Settings::default());
        assert_eq!(c.mode, 2);
        assert_eq!(&c.data[..3], &[5, 6, 2]);
        let c = TriggerCommand::new(Profile::Actor, &Settings::default());
        assert_eq!(c.mode, 5);
        assert_eq!(&c.data[..4], &[2, 8, 1, 2]);
    }
    #[test]
    fn rejects_bad_packets() {
        for s in [b"[]".as_slice(), b"{}", br#"{"schemaVersion":true}"#] {
            assert!(parse_state(s).is_err())
        }
        let mut v: Value = serde_json::from_slice(&state(1)).unwrap();
        v["pid"] = json!(true);
        assert!(parse_state(&serde_json::to_vec(&v).unwrap()).is_err());
    }
    #[test]
    fn stale_stop_process_exit_and_coalescing() {
        let mut e = Engine::new(Settings::default(), Fake::default(), None).unwrap();
        let now = Instant::now();
        e.receive(&state(2), now).unwrap();
        e.step(now, true);
        assert_eq!(e.output, [Profile::Actor, Profile::Object]);
        e.step(now, true);
        assert_eq!(e.hardware.sent.len(), 2);
        e.receive(&state(1), now).unwrap();
        assert_eq!(e.state.as_ref().unwrap().seq, 2);
        e.step(now, false);
        assert_eq!(e.output, [Profile::Off; 2]);
        e.step(now + Duration::from_secs(4), true);
        assert_eq!(e.output, [Profile::Off; 2]);
        e.stop();
        assert!(!e.settings.enabled);
        assert!(e.hardware.releases > 0);
    }
    #[test]
    fn partial_failure_latches_until_explicit_start() {
        let mut e = Engine::new(
            Settings::default(),
            Fake {
                fail_right: true,
                ..Default::default()
            },
            None,
        )
        .unwrap();
        let now = Instant::now();
        e.receive(&state(1), now).unwrap();
        let s = e.step(now, true);
        assert!(!s["enabled"].as_bool().unwrap());
        assert!(e.hardware.releases > 0);
        let count = e.hardware.sent.len();
        e.hardware.fail_right = false;
        e.step(now, true);
        assert_eq!(e.hardware.sent.len(), count);
        e.settings(Settings::default()).unwrap();
        assert!(!e.settings.enabled);
        e.arm();
        e.step(now, true);
        assert!(e.active);
        assert_eq!(e.output, [Profile::Actor, Profile::Object]);
    }
    #[test]
    fn manual_test_expires() {
        let mut e = Engine::new(Settings::default(), Fake::default(), None).unwrap();
        let now = Instant::now();
        e.step(now, true);
        e.test(Profile::Menu, now).unwrap();
        e.step(now, true);
        assert_eq!(e.output, [Profile::Menu; 2]);
        e.step(now + Duration::from_secs(3), true);
        assert_eq!(e.output, [Profile::Off; 2]);
    }
    #[test]
    fn limits_and_menu_priority() {
        let mut s = Settings {
            object_strength: 5,
            ..Settings::default()
        };
        assert!(s.validate().is_err());
        s = Settings::default();
        let mut st = parse_state(&state(1)).unwrap();
        st.menu_open = true;
        assert_eq!(profiles(&s, Some(&st), true), [Profile::Menu; 2]);
        assert_eq!(profiles(&s, Some(&st), false), [Profile::Off; 2]);
    }
}
