#![windows_subsystem = "windows"]

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use std::collections::HashMap;
use std::ffi::c_void;

use eframe::egui;
use egui::{Color32, ComboBox, RichText, ViewportBuilder, ViewportCommand};

use winreg::enums::*;
use winreg::RegKey;

use tray_icon::{
    menu::{Menu, MenuItem, MenuEvent},
    TrayIcon, TrayIconBuilder, Icon, TrayIconEvent, MouseButton,
};

use windows::core::{Interface, interface, GUID, PCWSTR, IUnknown, IUnknown_Vtbl};
use windows::Win32::Foundation::CloseHandle;
use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
use windows::Win32::Media::Audio::*;
use windows::Win32::System::Com::*;
use windows::Win32::System::ProcessStatus::GetModuleBaseNameW;
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_QUERY_INFORMATION, PROCESS_VM_READ,
};
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::System::Console::AllocConsole;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYBD_EVENT_FLAGS,
    KEYEVENTF_KEYUP, KEYEVENTF_EXTENDEDKEY, VIRTUAL_KEY,
};
use windows::core::HSTRING;
use windows::Data::Xml::Dom::XmlDocument;
use windows::UI::Notifications::{ToastNotification, ToastNotificationManager};
use windows::Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID;

const CLSID_PolicyConfigClient: GUID = GUID::from_u128(0x870af99c_171d_4f9e_af0d_e63df40c2bc9);

#[interface("f8679f50-850a-41cf-9c72-430f290290c8")]
pub unsafe trait IPolicyConfig: IUnknown {
    fn GetMixFormat(&self, pszdeviceid: PCWSTR, ppformat: *mut *mut c_void) -> windows::core::HRESULT;
    fn GetDeviceFormat(&self, pszdeviceid: PCWSTR, bdefault: i32, ppformat: *mut *mut c_void) -> windows::core::HRESULT;
    fn ResetDeviceFormat(&self, pszdeviceid: PCWSTR) -> windows::core::HRESULT;
    fn SetDeviceFormat(&self, pszdeviceid: PCWSTR, pformat: *const c_void, pformatext: *const c_void) -> windows::core::HRESULT;
    fn GetProcessingPeriod(&self, pszdeviceid: PCWSTR, bdefault: i32, pdefaultperiod: *mut i64, pminimumperiod: *mut i64) -> windows::core::HRESULT;
    fn SetProcessingPeriod(&self, pszdeviceid: PCWSTR, pdefaultperiod: *const i64) -> windows::core::HRESULT;
    fn GetShareMode(&self, pszdeviceid: PCWSTR, pmode: *mut i32) -> windows::core::HRESULT;
    fn SetShareMode(&self, pszdeviceid: PCWSTR, mode: i32) -> windows::core::HRESULT;
    fn GetPropertyValue(&self, pszdeviceid: PCWSTR, bfxenable: i32, pkey: *const c_void, pv: *mut c_void) -> windows::core::HRESULT;
    fn SetPropertyValue(&self, pszdeviceid: PCWSTR, bfxenable: i32, pkey: *const c_void, pv: *const c_void) -> windows::core::HRESULT;
    fn SetDefaultEndpoint(&self, pszdeviceid: PCWSTR, role: ERole) -> windows::core::Result<()>;
    fn SetEndpointVisibility(&self, pszdeviceid: PCWSTR, bvisible: i32) -> windows::core::HRESULT;
}

fn default_true() -> bool { true }

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq)]
struct SerialConfig { port: String, baud: u32, timeout: u64 }

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq)]
struct DialConfig {
    #[serde(rename = "type")] dial_type: String,
    process_name: Option<String>,
    #[serde(default)]
    inverted: bool,
}

fn default_string_none() -> String { "None".to_string() }

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq)]
struct ButtonConfig {
    #[serde(default = "default_string_none")]
    action: String,

    #[serde(default)]
    dial_index: usize,

    #[serde(default)]
    media_key: String,

    #[serde(default)]
    modifiers: Vec<String>,
    #[serde(default)]
    key_combo: String,
}

impl Default for ButtonConfig {
    fn default() -> Self {
        Self {
            action: "none".to_string(),
            dial_index: 0,
            media_key: "play_pause".to_string(),
            modifiers: vec![],
            key_combo: String::new(),
        }
    }
}

#[derive(Deserialize, Serialize, Debug, Clone)]
struct AppConfig {
    serial: SerialConfig,
    value_max: f32,
    work_device_1: String,
    work_device_2: String,
    #[serde(default)]
    debug_mode: bool,
    #[serde(default)]
    use_logarithmic_scale: bool,
    #[serde(default = "default_true")]
    enable_osd: bool,
    #[serde(default = "default_theme")]
    theme: String,
    dials: Vec<DialConfig>,
    #[serde(default)]
    buttons: Vec<ButtonConfig>,
}

fn default_theme() -> String { "Pink".to_string() }

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            serial: SerialConfig { port: "COM3".to_string(), baud: 115200, timeout: 50 },
            value_max: 720.0,
            work_device_1: "None".to_string(),
            work_device_2: "None".to_string(),
            debug_mode: false,
            use_logarithmic_scale: false,
            enable_osd: true,
            theme: default_theme(),
            dials: vec![],
            buttons: vec![],
        }
    }
}

fn get_exe_dir() -> PathBuf {
    std::env::current_exe().map(|p| p.parent().map(|p| p.to_path_buf()).unwrap_or(p)).unwrap_or_else(|_| PathBuf::from("."))
}

fn get_config_path() -> PathBuf {
    let mut path = dirs::config_dir().unwrap_or_else(|| PathBuf::from("."));
    path.push("RVCI");
    if !path.exists() { let _ = std::fs::create_dir_all(&path); }
    path.join("mapping.json")
}

const STARTUP_VALUE: &str = "RVCI";

const STARTUP_VALUE_LEGACY: &str = "RVSC";
const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";

fn set_startup_launch(enable: bool) -> Result<()> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let path = hkcu.open_subkey_with_flags(RUN_KEY, KEY_ALL_ACCESS)?;

    let _ = path.delete_value(STARTUP_VALUE_LEGACY);
    if enable {
        let exe_path = std::env::current_exe()?;
        path.set_value(STARTUP_VALUE, &exe_path.to_str().unwrap_or_default())?;
    } else {
        let _ = path.delete_value(STARTUP_VALUE);
    }
    Ok(())
}

fn check_startup_enabled() -> bool {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    if let Ok(path) = hkcu.open_subkey(RUN_KEY) {
        if let Ok(exe_path) = std::env::current_exe() {
            let exe = exe_path.to_str().unwrap_or_default();

            for name in [STARTUP_VALUE, STARTUP_VALUE_LEGACY] {
                if let Ok(val) = path.get_value::<String, _>(name) {
                    if val == exe { return true; }
                }
            }
        }
    }
    false
}

struct AudioController;
impl AudioController {
    unsafe fn get_system_volume() -> Result<IAudioEndpointVolume> {
        let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let device: IMMDevice = enumerator.GetDefaultAudioEndpoint(eRender, eMultimedia)?;
        Ok(device.Activate(CLSCTX_ALL, None)?)
    }

    unsafe fn get_session_manager() -> Result<IAudioSessionManager2> {
        let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let device: IMMDevice = enumerator.GetDefaultAudioEndpoint(eRender, eMultimedia)?;
        Ok(device.Activate(CLSCTX_ALL, None)?)
    }

    unsafe fn get_endpoint_volume(device_name: &str, data_flow: EDataFlow) -> Result<IAudioEndpointVolume> {
        let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let collection = enumerator.EnumAudioEndpoints(data_flow, DEVICE_STATE_ACTIVE)?;
        let count = collection.GetCount()?;
        let target = device_name.to_lowercase();

        for i in 0..count {
            if let Ok(item) = collection.Item(i) {
                if let Some(name) = Self::device_friendly_name(&item) {
                    if name.to_lowercase() == target {
                        return item.Activate::<IAudioEndpointVolume>(CLSCTX_ALL, None).map_err(anyhow::Error::from);
                    }
                }
            }
        }

        for i in 0..count {
            if let Ok(item) = collection.Item(i) {
                if let Some(name) = Self::device_friendly_name(&item) {
                    if name.to_lowercase().contains(&target) || target.contains(&name.to_lowercase()) {
                        return item.Activate::<IAudioEndpointVolume>(CLSCTX_ALL, None).map_err(anyhow::Error::from);
                    }
                }
            }
        }
        Err(anyhow::anyhow!("Audio endpoint not found"))
    }

    unsafe fn device_friendly_name(item: &IMMDevice) -> Option<String> {
        if let Ok(store) = item.OpenPropertyStore(STGM_READ) {
            if let Ok(prop) = store.GetValue(&PKEY_Device_FriendlyName) {
                let pwsz = prop.Anonymous.Anonymous.Anonymous.pwszVal;
                if !pwsz.is_null() {
                    return Some(pwsz.to_string().unwrap_or_default());
                }
            }
        }
        None
    }

    unsafe fn get_mic_volume(mic_name: &str) -> Result<IAudioEndpointVolume> {
        Self::get_endpoint_volume(mic_name, eCapture)
    }

    unsafe fn get_output_device_volume(device_name: &str) -> Result<IAudioEndpointVolume> {
        Self::get_endpoint_volume(device_name, eRender)
    }

    fn get_process_name(pid: u32) -> String {
        unsafe {
            if let Ok(handle) = OpenProcess(PROCESS_QUERY_INFORMATION | PROCESS_VM_READ, false, pid) {
                let mut buffer = [0u16; 1024];
                let len = GetModuleBaseNameW(handle, None, &mut buffer);
                let _ = CloseHandle(handle);
                if len > 0 {
                    let mut name = String::from_utf16_lossy(&buffer[..len as usize]).to_string();
                    if name.to_lowercase().ends_with(".exe") {
                        name.truncate(name.len() - 4);
                    }
                    return name;
                }
            }
        }
        String::new()
    }
}

fn token_to_vk(token: &str) -> Option<(u16, bool)> {
    let t = token.trim().to_lowercase();

    if t.len() == 1 {
        let c = t.chars().next().unwrap();
        if c.is_ascii_alphabetic() { return Some((c.to_ascii_uppercase() as u16, false)); }
        if c.is_ascii_digit() { return Some((c as u16, false)); }
    }
    let vk: u16 = match t.as_str() {

        "ctrl" | "control" | "lctrl" => 0xA2,
        "rctrl" => 0xA3,
        "shift" | "lshift" => 0xA0,
        "rshift" => 0xA1,
        "alt" | "lalt" => 0xA4,
        "ralt" => 0xA5,
        "win" | "lwin" | "super" | "meta" => 0x5B,
        "rwin" => 0x5C,

        "space" | "spacebar" => 0x20,
        "enter" | "return" => 0x0D,
        "tab" => 0x09,
        "esc" | "escape" => 0x1B,
        "backspace" | "bksp" => 0x08,
        "delete" | "del" => 0x2E,
        "insert" | "ins" => 0x2D,
        "home" => 0x24,
        "end" => 0x23,
        "pageup" | "pgup" => 0x21,
        "pagedown" | "pgdn" => 0x22,
        "capslock" | "caps" => 0x14,
        "printscreen" | "prtsc" => 0x2C,

        "up" => 0x26, "down" => 0x28, "left" => 0x25, "right" => 0x27,

        "f1" => 0x70, "f2" => 0x71, "f3" => 0x72, "f4" => 0x73,
        "f5" => 0x74, "f6" => 0x75, "f7" => 0x76, "f8" => 0x77,
        "f9" => 0x78, "f10" => 0x79, "f11" => 0x7A, "f12" => 0x7B,
        "f13" => 0x7C, "f14" => 0x7D, "f15" => 0x7E, "f16" => 0x7F,

        "-" | "minus" => 0xBD, "=" | "plus" | "equals" => 0xBB,
        "[" => 0xDB, "]" => 0xDD, "\\" | "backslash" => 0xDC,
        ";" | "semicolon" => 0xBA, "'" | "quote" => 0xDE,
        "," | "comma" => 0xBC, "." | "period" => 0xBE, "/" | "slash" => 0xBF,
        "`" | "grave" | "backtick" => 0xC0,

        "media_play_pause" | "play_pause" | "playpause" => 0xB3,
        "media_next" | "next" => 0xB0,
        "media_prev" | "media_previous" | "prev" => 0xB1,
        "media_stop" | "stop" => 0xB2,
        "vol_up" | "volume_up" => 0xAF,
        "vol_down" | "volume_down" => 0xAE,
        "vol_mute" | "volume_mute" | "mute" => 0xAD,
        _ => return None,
    };
    let extended = matches!(vk,
        0x2E | 0x2D | 0x24 | 0x23 | 0x21 | 0x22 |
        0x26 | 0x28 | 0x25 | 0x27 |
        0x5B | 0x5C |
        0xA3 | 0xA5 |
        0xAD | 0xAE | 0xAF | 0xB0 | 0xB1 | 0xB2 | 0xB3
    );
    Some((vk, extended))
}

struct KeyEmu;
impl KeyEmu {
    unsafe fn make_input(vk: u16, extended: bool, key_up: bool) -> INPUT {
        let mut flags: KEYBD_EVENT_FLAGS = KEYBD_EVENT_FLAGS(0);
        if extended { flags |= KEYEVENTF_EXTENDEDKEY; }
        if key_up { flags |= KEYEVENTF_KEYUP; }
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(vk),
                    wScan: 0,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        }
    }

    unsafe fn send_inputs(inputs: &[INPUT]) {
        if inputs.is_empty() { return; }
        SendInput(inputs, std::mem::size_of::<INPUT>() as i32);
    }

    fn tap(token: &str) {
        if let Some((vk, ext)) = token_to_vk(token) {
            unsafe {
                let down = Self::make_input(vk, ext, false);
                let up = Self::make_input(vk, ext, true);
                Self::send_inputs(&[down, up]);
            }
        }
    }

    fn send_combo(modifiers: &[String], main_key: &str) {
        let mut downs: Vec<INPUT> = Vec::new();
        let mut ups: Vec<INPUT> = Vec::new();
        unsafe {
            for m in modifiers {
                if let Some((vk, ext)) = token_to_vk(m) {
                    downs.push(Self::make_input(vk, ext, false));
                    ups.insert(0, Self::make_input(vk, ext, true));
                }
            }
            if !main_key.trim().is_empty() {
                if let Some((vk, ext)) = token_to_vk(main_key) {
                    downs.push(Self::make_input(vk, ext, false));
                    ups.insert(0, Self::make_input(vk, ext, true));
                } else {
                    return;
                }
            }
            Self::send_inputs(&downs);
            Self::send_inputs(&ups);
        }
    }
}

struct AudioScanner;
impl AudioScanner {
    fn get_active_sessions() -> Vec<String> {
        let mut names = HashSet::new();
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            if let Ok(mgr) = AudioController::get_session_manager() {
                if let Ok(enum_sess) = mgr.GetSessionEnumerator() {
                    if let Ok(count) = enum_sess.GetCount() {
                        for i in 0..count {
                            if let Ok(sess) = enum_sess.GetSession(i) {
                                if let Ok(s2) = Interface::cast::<IAudioSessionControl2>(&sess) {
                                    if let Ok(pid) = s2.GetProcessId() {
                                        if pid != 0 {
                                            let name = AudioController::get_process_name(pid);
                                            if !name.is_empty() { names.insert(name); }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        let mut list: Vec<String> = names.into_iter().collect();
        list.sort();
        list
    }

    fn get_devices_with_ids(data_flow: EDataFlow) -> Vec<(String, String)> {
        let mut devices = Vec::new();
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            let enumerator: Result<IMMDeviceEnumerator, _> = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL);
            if let Ok(enumerator) = enumerator {
                if let Ok(collection) = enumerator.EnumAudioEndpoints(data_flow, DEVICE_STATE_ACTIVE) {
                    if let Ok(count) = collection.GetCount() {
                        for i in 0..count {
                            if let Ok(item) = collection.Item(i) {
                                let mut id_string = String::new();
                                if let Ok(id_pwstr) = item.GetId() {
                                    id_string = id_pwstr.to_string().unwrap_or_default();
                                }
                                let mut name_string = String::new();
                                if let Ok(store) = item.OpenPropertyStore(STGM_READ) {
                                    if let Ok(prop) = store.GetValue(&PKEY_Device_FriendlyName) {
                                        let pwsz = prop.Anonymous.Anonymous.Anonymous.pwszVal;
                                        if !pwsz.is_null() {
                                            name_string = pwsz.to_string().unwrap_or_default();
                                        }
                                    }
                                }
                                if !name_string.is_empty() && !id_string.is_empty() {
                                    devices.push((name_string, id_string));
                                }
                            }
                        }
                    }
                }
            }
        }
        devices.sort_by(|a, b| a.0.cmp(&b.0));
        devices
    }

    fn get_playback_devices_with_ids() -> Vec<(String, String)> {
        Self::get_devices_with_ids(eRender)
    }

    fn get_capture_devices_with_ids() -> Vec<(String, String)> {
        Self::get_devices_with_ids(eCapture)
    }

    fn get_com_ports() -> Vec<String> {
        serialport::available_ports()
            .unwrap_or_default()
            .into_iter()
            .map(|p| p.port_name)
            .collect()
    }
}

struct Smoother { last_value: f32 }
impl Smoother {
    fn new() -> Self { Self { last_value: 0.0 } }
    fn process(&mut self, new_val: f32) -> f32 {
        let delta = new_val - self.last_value;
        if delta.abs() >= 0.08 { self.last_value = new_val; return new_val; }
        let smoothed = self.last_value + delta * 0.35;
        self.last_value = smoothed;
        smoothed
    }
}

fn switch_device(clean_name: &str) {
    if clean_name == "None" || clean_name.is_empty() { return; }
    let all_devices = AudioScanner::get_playback_devices_with_ids();
    let match_result = all_devices.iter()
        .find(|(name, _id)| name.to_lowercase().contains(&clean_name.to_lowercase()));

    if let Some((_, real_id)) = match_result {
        unsafe {
            if let Ok(policy) = CoCreateInstance::<_, IPolicyConfig>(&CLSID_PolicyConfigClient, None, CLSCTX_ALL) {
                let mut id_utf16: Vec<u16> = real_id.encode_utf16().collect();
                id_utf16.push(0);
                let pcwstr_id = PCWSTR(id_utf16.as_ptr());

                let _ = policy.SetDefaultEndpoint(pcwstr_id, eConsole);
                let _ = policy.SetDefaultEndpoint(pcwstr_id, eMultimedia);
                let _ = policy.SetDefaultEndpoint(pcwstr_id, eCommunications);
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SerialStatus {
    Idle,
    Connected,
    InUse,
    NotFound,
    Error,
}
type SharedStatus = Arc<Mutex<SerialStatus>>;

fn set_status(status: &SharedStatus, new: SerialStatus) {
    if let Ok(mut s) = status.lock() {
        *s = new;
    }
}

fn classify_serial_error(e: &serialport::Error) -> SerialStatus {
    let msg = e.to_string().to_lowercase();
    if msg.contains("denied") || msg.contains("in use") || msg.contains("being used") {
        return SerialStatus::InUse;
    }
    match e.kind() {
        serialport::ErrorKind::NoDevice => SerialStatus::NotFound,
        serialport::ErrorKind::Io(io_kind) if io_kind == std::io::ErrorKind::PermissionDenied => {
            SerialStatus::InUse
        }
        _ => SerialStatus::Error,
    }
}

fn run_volume_logic_loop(config_path: PathBuf, osd_tx: Sender<(String, f32)>, status: SharedStatus) {
    let mut current_config_sig = String::new();
    let mut smoothers: Vec<Smoother> = Vec::new();
    let mut last_seen_status = SerialStatus::Idle;
    let mut last_inuse_notify: Option<Instant> = None;
    loop {
        let config_result = File::open(&config_path).and_then(|f| {
            serde_json::from_reader::<_, AppConfig>(BufReader::new(f))
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
        });

        if let Ok(config) = config_result {
            let new_sig = format!("{}{}", config.serial.port, config.serial.baud);
            if new_sig != current_config_sig {
                current_config_sig = new_sig;
                smoothers = (0..config.dials.len()).map(|_| Smoother::new()).collect();
            }
             if run_serial_processing(&config, &config_path, &mut smoothers, &osd_tx, &status).is_ok() {
                last_seen_status = SerialStatus::Connected;
             } else {
                let cur = status.lock().map(|g| *g).unwrap_or(SerialStatus::Idle);
                if cur != last_seen_status && cur == SerialStatus::InUse {
                    let debounced = last_inuse_notify
                        .map(|t| t.elapsed() < Duration::from_secs(20))
                        .unwrap_or(false);
                    if !debounced {
                        last_inuse_notify = Some(Instant::now());
                        notify_toast(
                            "RVCI - COM port already in use",
                            &format!(
                                "Couldn't open {} - it's currently being used by another application \
                                 (e.g. the Arduino Serial Monitor). Close that program and try again.",
                                config.serial.port
                            ),
                        );
                    }
                }
                last_seen_status = cur;
                std::thread::sleep(Duration::from_secs(2));
             }
        } else {
            std::thread::sleep(Duration::from_secs(2));
        }
    }
}

struct SessionCache {
    list: Vec<(String, ISimpleAudioVolume)>,
    fetched: Option<Instant>,
}

impl SessionCache {
    fn new() -> Self { Self { list: Vec::new(), fetched: None } }

    fn stale(&self) -> bool {
        self.fetched.map(|t| t.elapsed() > Duration::from_secs(2)).unwrap_or(true)
    }

    unsafe fn refresh(&mut self) {
        self.list.clear();
        self.fetched = Some(Instant::now());
        if let Ok(mgr) = AudioController::get_session_manager() {
            if let Ok(enum_sess) = mgr.GetSessionEnumerator() {
                if let Ok(count) = enum_sess.GetCount() {
                    for s_idx in 0..count {
                        if let Ok(sess) = enum_sess.GetSession(s_idx) {
                            if let Ok(s2) = Interface::cast::<IAudioSessionControl2>(&sess) {
                                if let Ok(pid) = s2.GetProcessId() {
                                    if pid == 0 { continue; }
                                    if let Ok(vol) = Interface::cast::<ISimpleAudioVolume>(&sess) {
                                        let name = AudioController::get_process_name(pid).to_lowercase();
                                        self.list.push((name, vol));
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

fn run_serial_processing(config: &AppConfig, config_path: &PathBuf, smoothers: &mut Vec<Smoother>, osd_tx: &Sender<(String, f32)>, status: &SharedStatus) -> Result<()> {
    let port = match serialport::new(&config.serial.port, config.serial.baud)
        .timeout(Duration::from_millis(config.serial.timeout))
        .open()
    {
        Ok(p) => p,
        Err(e) => {
            set_status(status, classify_serial_error(&e));
            return Err(anyhow::anyhow!("Failed to open serial port: {e}"));
        }
    };
    set_status(status, SerialStatus::Connected);

    let mut reader = BufReader::new(port);
    let mut line_buf = String::new();
    let mut last_update = Instant::now();

    let mut last_applied_values: Vec<f32> = vec![-1.0; config.dials.len()];
    let mut last_osd_raw_values: Vec<f32> = vec![-999.0; config.dials.len()];

    let mut button_states: Vec<bool> = vec![false; config.buttons.len()];

    let mut mic_device_cache: HashMap<String, IAudioEndpointVolume> = HashMap::new();
    let mut output_device_cache: HashMap<String, IAudioEndpointVolume> = HashMap::new();
    let mut system_volume: Option<(IAudioEndpointVolume, Instant)> = None;
    let mut sessions = SessionCache::new();
    let mut last_value_line = String::new();
    let mut settle: u32 = 0;

    let mut process_map: HashSet<String> = HashSet::new();
    for dial in &config.dials {
        if let Some(name) = &dial.process_name {
            let mut clean_name = name.clone();
            if clean_name.to_lowercase().ends_with(".exe") {
                clean_name.truncate(clean_name.len() - 4);
            }
            process_map.insert(clean_name.to_lowercase());
        }
    }

    let dial_targets: Vec<Option<String>> = config.dials.iter().map(|d| {
        d.process_name.as_ref().and_then(|n| {
            if n.as_str() == "None" { return None; }
            let mut clean = n.to_lowercase();
            if clean.ends_with(".exe") { clean.truncate(clean.len() - 4); }
            Some(clean)
        })
    }).collect();

    unsafe { let _ = CoInitializeEx(None, COINIT_MULTITHREADED); }
    let last_file_mod = std::fs::metadata(config_path).and_then(|m| m.modified()).ok();

    let mut last_cfg_check = Instant::now();

    loop {
        if last_cfg_check.elapsed() >= Duration::from_millis(1000) {
            last_cfg_check = Instant::now();
            if let Ok(meta) = std::fs::metadata(config_path) {
                if let Ok(mod_time) = meta.modified() {
                    if Some(mod_time) != last_file_mod { return Ok(()); }
                }
            }
        }

        line_buf.clear();

        match reader.read_line(&mut line_buf) {
            Ok(bytes) if bytes > 0 => {
                let line = line_buf.trim();
                if line.is_empty() { continue; }

                if line == "WORKS 1" {
                    switch_device(&config.work_device_1);
                    system_volume = None;
                    sessions.fetched = None;
                    continue;
                } else if line == "WORKS 2" {
                    switch_device(&config.work_device_2);
                    system_volume = None;
                    sessions.fetched = None;
                    continue;
                }

                if let Some(rest) = line.strip_prefix("BTN ") {
                    let mut it = rest.split_whitespace();
                    if let (Some(id_s), Some(state_s)) = (it.next(), it.next()) {
                        if let (Ok(id), Ok(state_i)) = (id_s.parse::<usize>(), state_s.parse::<i32>()) {
                            let new_state = state_i != 0;
                            if id < config.buttons.len() {
                                let prev = if id < button_states.len() { button_states[id] } else { false };
                                while button_states.len() <= id { button_states.push(false); }
                                button_states[id] = new_state;
                                let rising = !prev && new_state;

                                let btn = &config.buttons[id];
                                match btn.action.as_str() {
                                    "mute_dial" => {

                                        if btn.dial_index < last_applied_values.len() {
                                            last_applied_values[btn.dial_index] = -1.0;
                                        }
                                        settle = settle.max(4);
                                    },
                                    "media" => {
                                        if rising { KeyEmu::tap(&btn.media_key); }
                                    },
                                    "keys" => {
                                        if rising { KeyEmu::send_combo(&btn.modifiers, &btn.key_combo); }
                                    },
                                    _ => {}
                                }
                            }
                        }
                    }
                    continue;
                }

                if line == last_value_line {
                    if settle == 0 { continue; }
                } else {
                    last_value_line.clear();
                    last_value_line.push_str(line);
                    settle = 14;
                }

                if last_update.elapsed() < Duration::from_millis(25) { continue; }
                last_update = Instant::now();
                if settle > 0 { settle -= 1; }

                let parts: Vec<&str> = line.split('|').collect();

                if config.dials.is_empty() || parts.len() < config.dials.len() { continue; }

                for (i, dial_cfg) in config.dials.iter().enumerate() {
                    let part = parts[i];
                    if let Ok(raw_val) = part.parse::<f32>() {

                        if i >= last_osd_raw_values.len() { last_osd_raw_values.push(-999.0); }

                        let mut trigger_osd = false;
                        if (raw_val - last_osd_raw_values[i]).abs() > 15.0 {
                            trigger_osd = true;
                            last_osd_raw_values[i] = raw_val;
                        }

                        let mut normalized = raw_val.clamp(0.0, config.value_max) / config.value_max;

                        if dial_cfg.inverted {
                            normalized = 1.0 - normalized;
                        }

                        if config.use_logarithmic_scale {
                            normalized = normalized.powf(3.0);
                        }

                        if i >= smoothers.len() { smoothers.push(Smoother::new()); }
                        if i >= last_applied_values.len() { last_applied_values.push(-1.0); }

                        let mut smoothed = smoothers[i].process(normalized);

                        let dial_muted = config.buttons.iter().enumerate().any(|(bid, b)| {
                            b.action == "mute_dial"
                                && b.dial_index == i
                                && button_states.get(bid).copied().unwrap_or(false)
                        });
                        if dial_muted {
                            smoothed = 0.0;
                        }

                        let quantized = (smoothed * 200.0).round() / 200.0;
                        if (quantized - last_applied_values[i]).abs() < 0.0074 {
                            continue;
                        }

                        last_applied_values[i] = quantized;
                        let level = quantized.clamp(0.0, 1.0);

                        let target_lbl = dial_cfg.process_name.as_deref().unwrap_or("Unassigned");

                        if config.enable_osd && trigger_osd {
                            let display_name = match dial_cfg.dial_type.as_str() {
                                "system" => "Master Volume".to_string(),
                                "all_others" => "Other Apps".to_string(),
                                _ => {
                                    let mut clean = target_lbl.to_string();
                                    if clean.to_lowercase().ends_with(".exe") {
                                        clean.truncate(clean.len() - 4);
                                    }
                                    clean
                                }
                            };
                            if display_name != "None" && display_name != "Unassigned" {
                                let _ = osd_tx.send((display_name, level));
                            }
                        }

                        unsafe {
                            match dial_cfg.dial_type.as_str() {
                                "system" => {
                                    let refetch = system_volume
                                        .as_ref()
                                        .map(|(_, t)| t.elapsed() > Duration::from_secs(3))
                                        .unwrap_or(true);
                                    if refetch {
                                        system_volume = AudioController::get_system_volume().ok().map(|v| (v, Instant::now()));
                                    }
                                    if let Some((vol, _)) = &system_volume {
                                        if vol.SetMasterVolumeLevelScalar(level, std::ptr::null()).is_err() {
                                            system_volume = None;
                                        }
                                    }
                                },
                                "microphone" => {
                                    if let Some(target) = &dial_cfg.process_name {
                                        if target != "None" {
                                            let vol_opt = mic_device_cache.get(target).cloned().or_else(|| {
                                                if let Ok(v) = AudioController::get_mic_volume(target) {
                                                    mic_device_cache.insert(target.clone(), v.clone());
                                                    Some(v)
                                                } else {
                                                    None
                                                }
                                            });
                                            if let Some(vol) = vol_opt {
                                                if vol.SetMasterVolumeLevelScalar(level, std::ptr::null()).is_err() {
                                                    mic_device_cache.remove(target);
                                                }
                                            }
                                        }
                                    }
                                },
                                "output_device" => {

                                    if let Some(target) = &dial_cfg.process_name {
                                        if target != "None" {
                                            let vol_opt = output_device_cache.get(target).cloned().or_else(|| {
                                                if let Ok(v) = AudioController::get_output_device_volume(target) {
                                                    output_device_cache.insert(target.clone(), v.clone());
                                                    Some(v)
                                                } else {
                                                    None
                                                }
                                            });
                                            if let Some(vol) = vol_opt {
                                                if vol.SetMasterVolumeLevelScalar(level, std::ptr::null()).is_err() {
                                                    output_device_cache.remove(target);
                                                }
                                            }
                                        }
                                    }
                                },
                                "process" | "all_others" => {
                                    let target = dial_targets.get(i).and_then(|t| t.as_ref());
                                    if dial_cfg.dial_type == "process" && target.is_none() { continue; }
                                    if sessions.stale() { sessions.refresh(); }
                                    for (pname, vol) in &sessions.list {
                                        let should_change = if dial_cfg.dial_type == "all_others" {
                                            !process_map.contains(pname)
                                        } else {
                                            Some(pname) == target
                                        };
                                        if should_change {
                                            let _ = vol.SetMasterVolume(level, std::ptr::null());
                                        }
                                    }
                                },
                                _ => {}
                            }
                        }
                    }
                }
            },
            Err(e) => {
                if e.kind() == std::io::ErrorKind::TimedOut {
                    std::thread::sleep(Duration::from_millis(10));
                    continue;
                } else {
                    return Err(anyhow::anyhow!("Serial error"));
                }
            },
            _ => {
                std::thread::sleep(Duration::from_millis(10));
                continue;
            }
        }
    }
}

fn load_tray_icon(filename: &str) -> Icon {
    let path = get_exe_dir().join(filename);
    if let Ok(img) = image::open(&path) {
        let rgba = img.into_rgba8();
        let (w, h) = rgba.dimensions();
        if let Ok(icon) = Icon::from_rgba(rgba.into_raw(), w, h) { return icon; }
    }
    let (width, height) = (32, 32);
    let mut rgba = Vec::with_capacity((width * height * 4) as usize);
    for _ in 0..height { for _ in 0..width { rgba.extend_from_slice(&[255, 0, 0, 255]); } }
    Icon::from_rgba(rgba, width, height).unwrap_or_else(|_| panic!("Icon error"))
}

fn load_window_icon(filename: &str) -> Option<egui::IconData> {
    let path = get_exe_dir().join(filename);
    if let Ok(img) = image::open(&path) {
        let rgba = img.into_rgba8();
        let (w, h) = rgba.dimensions();
        return Some(egui::IconData { rgba: rgba.into_raw(), width: w, height: h });
    }
    None
}

fn extract_clean_name(full_name: &str) -> String {
    if full_name == "None" { return "None".to_string(); }
    if let (Some(start), Some(end)) = (full_name.find('('), full_name.rfind(')')) {
        if start < end { return full_name[start+1..end].trim().to_string(); }
    }
    full_name.to_string()
}

fn format_combo(modifiers: &[String], main_key: &str) -> String {
    let mut parts: Vec<String> = Vec::new();
    for m in modifiers {
        let pretty = match m.to_lowercase().as_str() {
            "ctrl" | "control" => "Ctrl",
            "shift" => "Shift",
            "alt" => "Alt",
            "win" | "super" | "meta" => "Win",
            other => { parts.push(capitalize_token(other)); continue; }
        };
        parts.push(pretty.to_string());
    }
    if !main_key.is_empty() {
        parts.push(capitalize_token(main_key));
    }
    if parts.is_empty() { "Click to record".to_string() } else { parts.join(" + ") }
}

fn capitalize_token(t: &str) -> String {
    if t.chars().count() == 1 { return t.to_uppercase(); }
    let lower = t.to_lowercase();
    match lower.as_str() {
        "space" => "Space".to_string(),
        "enter" | "return" => "Enter".to_string(),
        "tab" => "Tab".to_string(),
        "esc" | "escape" => "Esc".to_string(),
        "backspace" => "Backspace".to_string(),
        "delete" | "del" => "Delete".to_string(),
        "up" => "Up".to_string(),
        "down" => "Down".to_string(),
        "left" => "Left".to_string(),
        "right" => "Right".to_string(),
        "home" => "Home".to_string(),
        "end" => "End".to_string(),
        "pageup" => "PageUp".to_string(),
        "pagedown" => "PageDown".to_string(),
        "insert" => "Insert".to_string(),
        _ => {
            let mut c = lower.chars();
            match c.next() {
                Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                None => lower,
            }
        }
    }
}

const MEDIA_LABELS: [&str; 7] = [
    "Play/Pause", "Next Track", "Prev Track", "Stop", "Volume Up", "Volume Down", "Volume Mute",
];
const MEDIA_TOKENS: [&str; 7] = ["play_pause", "next", "prev", "stop", "vol_up", "vol_down", "vol_mute"];

fn egui_key_to_token(key: egui::Key) -> Option<&'static str> {
    use egui::Key::*;
    let s = match key {
        A => "a", B => "b", C => "c", D => "d", E => "e", F => "f", G => "g",
        H => "h", I => "i", J => "j", K => "k", L => "l", M => "m", N => "n",
        O => "o", P => "p", Q => "q", R => "r", S => "s", T => "t", U => "u",
        V => "v", W => "w", X => "x", Y => "y", Z => "z",
        Num0 => "0", Num1 => "1", Num2 => "2", Num3 => "3", Num4 => "4",
        Num5 => "5", Num6 => "6", Num7 => "7", Num8 => "8", Num9 => "9",
        F1 => "f1", F2 => "f2", F3 => "f3", F4 => "f4", F5 => "f5", F6 => "f6",
        F7 => "f7", F8 => "f8", F9 => "f9", F10 => "f10", F11 => "f11", F12 => "f12",
        Space => "space", Tab => "tab", Backspace => "backspace", Delete => "delete",
        Enter => "enter", Escape => "esc",
        Insert => "insert", Home => "home", End => "end", PageUp => "pageup",
        PageDown => "pagedown",
        ArrowUp => "up", ArrowDown => "down", ArrowLeft => "left", ArrowRight => "right",
        Minus => "-", Equals => "=", Comma => ",", Period => ".", Slash => "/",
        Semicolon => ";", Backslash => "\\", Backtick => "`",
        OpenBracket => "[", CloseBracket => "]",
        _ => return None,
    };
    Some(s)
}

#[derive(Clone, Copy)]
struct Palette {
    bg: Color32,
    card_bg: Color32,
    card_border: Color32,
    widget_bg: Color32,
    row_hover: Color32,
    text: Color32,
    text_muted: Color32,
    accent: Color32,
    accent2: Color32,
    accent_hover: Color32,
    destructive: Color32,
    destructive_hover: Color32,
    success: Color32,
    extreme_bg: Color32,
    faint_bg: Color32,
    dark: bool,
}

const fn rgb(r: u8, g: u8, b: u8) -> Color32 {
    Color32::from_rgb(r, g, b)
}

fn mix(a: Color32, b: Color32, t: f32) -> Color32 {
    lerp_color(a, b, t)
}

fn dark_theme(accent: Color32, accent2: Color32, hover: Color32, tint: f32) -> Palette {
    Palette {
        bg:          mix(rgb(13, 14, 18),    accent, tint * 0.45),
        card_bg:     mix(rgb(22, 24, 30),    accent, tint * 0.70),
        card_border: mix(rgb(38, 41, 49),    accent, tint * 1.30),
        widget_bg:   mix(rgb(32, 35, 43),    accent, tint * 0.85),
        row_hover:   mix(rgb(46, 50, 60),    accent, tint * 1.45),
        text:        rgb(233, 236, 241),
        text_muted:  mix(rgb(140, 148, 161), accent, tint * 0.50),
        accent,
        accent2,
        accent_hover: hover,
        destructive: rgb(248, 81, 73),
        destructive_hover: rgb(255, 120, 112),
        success: rgb(63, 185, 80),
        extreme_bg:  mix(rgb(17, 19, 24),    accent, tint * 0.40),
        faint_bg:    mix(rgb(29, 32, 39),    accent, tint * 0.80),
        dark: true,
    }
}

fn light_theme(accent: Color32, accent2: Color32, hover: Color32, tint: f32) -> Palette {
    Palette {
        bg:          mix(rgb(235, 237, 242), accent, tint * 0.35),
        card_bg:     mix(rgb(252, 253, 255), accent, tint * 0.22),
        card_border: mix(rgb(212, 217, 225), accent, tint * 0.90),
        widget_bg:   mix(rgb(255, 255, 255), accent, tint * 0.18),
        row_hover:   mix(rgb(223, 227, 235), accent, tint * 1.10),
        text:        rgb(24, 28, 36),
        text_muted:  mix(rgb(92, 100, 112), accent, tint * 0.40),
        accent,
        accent2,
        accent_hover: hover,
        destructive: rgb(205, 45, 42),
        destructive_hover: rgb(224, 70, 66),
        success: rgb(30, 140, 64),
        extreme_bg:  rgb(255, 255, 255),
        faint_bg:    mix(rgb(231, 234, 240), accent, tint * 0.60),
        dark: false,
    }
}

fn build_themes() -> Vec<(&'static str, Palette)> {
    vec![
        ("Pink",     dark_theme(rgb(233, 54, 120), rgb(247, 120, 170), rgb(245, 92, 152), 0.09)),
        ("Blue",     dark_theme(rgb(56, 139, 253), rgb(56, 139, 253), rgb(92, 168, 255), 0.06)),
        ("Emerald",  dark_theme(rgb(16, 185, 129), rgb(16, 185, 129), rgb(52, 211, 153), 0.07)),
        ("Amber",    dark_theme(rgb(245, 158, 11), rgb(245, 158, 11), rgb(251, 191, 36), 0.06)),
        ("Purple",   dark_theme(rgb(139, 92, 246), rgb(139, 92, 246), rgb(167, 139, 250), 0.08)),
        ("Crimson",  dark_theme(rgb(244, 63, 94),  rgb(244, 63, 94),  rgb(251, 113, 133), 0.07)),
        ("Teal",     dark_theme(rgb(20, 184, 166), rgb(20, 184, 166), rgb(45, 212, 191), 0.07)),
        ("Slate",    dark_theme(rgb(120, 134, 156), rgb(120, 134, 156), rgb(160, 172, 190), 0.05)),
        ("Sunset",   dark_theme(rgb(251, 146, 60), rgb(236, 55, 110), rgb(251, 130, 120), 0.09)),
        ("Aurora",   dark_theme(rgb(59, 130, 246), rgb(16, 185, 129), rgb(80, 180, 220), 0.08)),
        ("Grape",    dark_theme(rgb(139, 92, 246), rgb(236, 72, 153), rgb(180, 120, 220), 0.09)),
        ("Midnight", dark_theme(rgb(96, 165, 250), rgb(96, 165, 250), rgb(130, 190, 255), 0.15)),
        ("Daylight", light_theme(rgb(37, 99, 235), rgb(37, 99, 235), rgb(59, 130, 246), 0.05)),
        ("Rosé",     light_theme(rgb(219, 39, 119), rgb(244, 114, 182), rgb(236, 72, 153), 0.06)),
        ("Sand",     light_theme(rgb(180, 118, 40), rgb(205, 150, 80), rgb(198, 138, 60), 0.11)),
    ]
}

static THEMES: std::sync::OnceLock<Vec<(&'static str, Palette)>> = std::sync::OnceLock::new();
fn themes() -> &'static [(&'static str, Palette)] {
    THEMES.get_or_init(build_themes)
}

static THEME_IDX: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

thread_local! {
    static LAYOUT_W: std::cell::Cell<f32> = const { std::cell::Cell::new(600.0) };
}
fn set_layout_w(w: f32) { LAYOUT_W.with(|c| c.set(w)); }
fn layout_w() -> f32 { LAYOUT_W.with(|c| c.get()) }

fn pal() -> &'static Palette {
    let t = themes();
    let i = THEME_IDX.load(Ordering::Relaxed);
    &t[if i < t.len() { i } else { 0 }].1
}

fn set_theme_by_name(name: &str) {
    if let Some(i) = themes().iter().position(|t| t.0 == name) {
        THEME_IDX.store(i, Ordering::Relaxed);
    }
}

fn accent() -> Color32 { pal().accent }
fn accent_hover() -> Color32 { pal().accent_hover }
fn destructive() -> Color32 { pal().destructive }
fn destructive_hover() -> Color32 { pal().destructive_hover }
fn success() -> Color32 { pal().success }
fn bg() -> Color32 { pal().bg }
fn card_bg() -> Color32 { pal().card_bg }
fn card_border() -> Color32 { pal().card_border }
fn widget_bg() -> Color32 { pal().widget_bg }
fn row_hover() -> Color32 { pal().row_hover }
fn text() -> Color32 { pal().text }
fn text_muted() -> Color32 { pal().text_muted }

fn is_light_color(c: Color32) -> bool {
    let l = 0.299 * c.r() as f32 + 0.587 * c.g() as f32 + 0.114 * c.b() as f32;
    l > 150.0
}

fn contrast_text(fill: Color32) -> Color32 {
    if is_light_color(fill) { Color32::from_rgb(20, 22, 28) } else { Color32::WHITE }
}

fn paint_vgrad(painter: &egui::Painter, rect: egui::Rect, top: Color32, bottom: Color32, rounding: f32) {
    painter.rect_filled(rect, egui::Rounding::same(rounding), bottom);
    let inset = rounding.min(rect.width() * 0.5).min(rect.height() * 0.5);
    let r = rect.shrink(inset);
    use egui::epaint::{Mesh, Vertex, WHITE_UV};
    let mut mesh = Mesh::default();
    let v = |p: egui::Pos2, c: Color32| Vertex { pos: p, uv: WHITE_UV, color: c };
    mesh.vertices.push(v(r.left_top(), top));
    mesh.vertices.push(v(r.right_top(), top));
    mesh.vertices.push(v(r.right_bottom(), bottom));
    mesh.vertices.push(v(r.left_bottom(), bottom));
    mesh.indices.extend_from_slice(&[0, 1, 2, 0, 2, 3]);
    painter.add(mesh);
}

const BAUD_RATES: [u32; 5] = [9600, 19200, 38400, 57600, 115200];

const GITHUB_URL: &str = "https://github.com/tzeys/rvci";
const GITHUB_PNG: &[u8] = include_bytes!("../assets/github.png");

struct RvciApp {
    config_path: PathBuf,
    cfg: AppConfig,

    com_ports: Vec<String>,
    playback_devices: Vec<String>,
    capture_devices: Vec<String>,
    active_processes: Vec<String>,

    startup_enabled: bool,
    save_flash: Option<(Instant, bool)>,
    key_capture: Option<usize>,
    key_capture_since: Instant,

    _tray: Option<TrayIcon>,
    want_show: Arc<AtomicBool>,

    user_opened: Arc<AtomicBool>,
    hwnd: isize,

    proc_rx: std::sync::mpsc::Receiver<Vec<String>>,

    github_tex: Option<egui::TextureHandle>,

    show_themes: bool,
}

impl RvciApp {
    fn new(cc: &eframe::CreationContext<'_>, config_path: PathBuf) -> Self {
        configure_visuals(&cc.egui_ctx);

        let cfg: AppConfig = if let Ok(file) = File::open(&config_path) {
            serde_json::from_reader(BufReader::new(file)).unwrap_or_default()
        } else {
            AppConfig::default()
        };

        let mut cfg = cfg;
        if cfg.theme == "RVCI Pink" { cfg.theme = "Pink".to_string(); }
        set_theme_by_name(&cfg.theme);

        let open_item = MenuItem::new("Open Settings", true, None);
        let quit_item = MenuItem::new("Quit", true, None);
        let open_id = open_item.id().clone();
        let quit_id = quit_item.id().clone();
        let tray_menu = Menu::new();
        let _ = tray_menu.append(&open_item);
        let _ = tray_menu.append(&quit_item);
        let tray = TrayIconBuilder::new()
            .with_menu(Box::new(tray_menu))
            .with_tooltip("RVCI")
            .with_icon(load_tray_icon("rvci.ico"))
            .build()
            .ok();

        let hwnd: isize = {
            use raw_window_handle::{HasWindowHandle, RawWindowHandle};
            match cc.window_handle() {
                Ok(h) => match h.as_raw() {
                    RawWindowHandle::Win32(w) => w.hwnd.get(),
                    _ => 0,
                },
                Err(_) => 0,
            }
        };

        let want_show = Arc::new(AtomicBool::new(false));
        let user_opened = Arc::new(AtomicBool::new(false));

        {
            let ws = want_show.clone();
            let uo = user_opened.clone();
            let ctx = cc.egui_ctx.clone();
            MenuEvent::set_event_handler(Some(move |ev: MenuEvent| {
                if ev.id == open_id {

                    uo.store(true, Ordering::SeqCst);
                    show_window_native(hwnd);
                    ws.store(true, Ordering::SeqCst);
                    ctx.request_repaint();
                } else if ev.id == quit_id {

                    std::process::exit(0);
                }
            }));
        }
        {
            let ws = want_show.clone();
            let uo = user_opened.clone();
            let ctx = cc.egui_ctx.clone();
            TrayIconEvent::set_event_handler(Some(move |ev: TrayIconEvent| {

                if let TrayIconEvent::Click { button: MouseButton::Left, .. } = ev {
                    uo.store(true, Ordering::SeqCst);
                    show_window_native(hwnd);
                    ws.store(true, Ordering::SeqCst);
                    ctx.request_repaint();
                }
            }));
        }

        let (proc_tx, proc_rx) = std::sync::mpsc::channel::<Vec<String>>();
        {
            let uo = user_opened.clone();
            let ctx = cc.egui_ctx.clone();
            std::thread::spawn(move || {
                let mut last: Vec<String> = Vec::new();
                loop {
                    if uo.load(Ordering::SeqCst) {
                        let procs = AudioScanner::get_active_sessions();
                        if procs != last {
                            last = procs.clone();
                            if proc_tx.send(procs).is_err() {
                                break;
                            }
                            ctx.request_repaint();
                        }
                        std::thread::sleep(Duration::from_secs(2));
                    } else {

                        std::thread::sleep(Duration::from_secs(1));
                    }
                }
            });
        }

        let mut app = Self {
            config_path,
            cfg,
            com_ports: Vec::new(),
            playback_devices: Vec::new(),
            capture_devices: Vec::new(),
            active_processes: Vec::new(),
            startup_enabled: check_startup_enabled(),
            save_flash: None,
            key_capture: None,
            key_capture_since: Instant::now(),
            _tray: tray,
            want_show,
            user_opened,
            hwnd,
            proc_rx,
            github_tex: None,
            show_themes: false,
        };
        app.rescan();
        app
    }

    fn rescan(&mut self) {
        self.com_ports = AudioScanner::get_com_ports();
        self.playback_devices = AudioScanner::get_playback_devices_with_ids()
            .into_iter().map(|d| d.0).collect();
        self.capture_devices = AudioScanner::get_capture_devices_with_ids()
            .into_iter().map(|d| d.0).collect();
        self.active_processes = AudioScanner::get_active_sessions();
    }

    fn save(&mut self) {
        let _ = set_startup_launch(self.startup_enabled);

        self.cfg.work_device_1 = extract_clean_name(&self.cfg.work_device_1);
        self.cfg.work_device_2 = extract_clean_name(&self.cfg.work_device_2);
        let ok = File::create(&self.config_path)
            .ok()
            .and_then(|f| serde_json::to_writer_pretty(f, &self.cfg).ok())
            .is_some();
        self.save_flash = Some((Instant::now(), ok));
    }

    fn handle_key_capture(&mut self, ctx: &egui::Context) {
        let Some(idx) = self.key_capture else { return; };

        let mut result: Option<Option<(Vec<String>, String)>> = None;
        ctx.input(|i| {

            let held = i.modifiers;
            let clipboard_mods = || {
                let mut mods = vec!["ctrl".to_string()];
                if held.shift { mods.push("shift".to_string()); }
                if held.alt { mods.push("alt".to_string()); }
                mods
            };
            for ev in &i.events {
                match ev {
                    egui::Event::Copy => {
                        result = Some(Some((clipboard_mods(), "c".to_string())));
                        break;
                    }
                    egui::Event::Cut => {
                        result = Some(Some((clipboard_mods(), "x".to_string())));
                        break;
                    }
                    egui::Event::Paste(_) => {
                        result = Some(Some((clipboard_mods(), "v".to_string())));
                        break;
                    }
                    egui::Event::Key { key, pressed: true, modifiers, .. } => {

                        if (*key == egui::Key::Escape || *key == egui::Key::Enter)
                            && !(modifiers.ctrl || modifiers.command || modifiers.alt || modifiers.shift)
                        {
                            result = Some(None);
                            break;
                        }
                        if let Some(tok) = egui_key_to_token(*key) {
                            let mut mods: Vec<String> = Vec::new();
                            if modifiers.ctrl || modifiers.command { mods.push("ctrl".to_string()); }
                            if modifiers.shift { mods.push("shift".to_string()); }
                            if modifiers.alt { mods.push("alt".to_string()); }
                            result = Some(Some((mods, tok.to_string())));
                            break;
                        }
                    }
                    _ => {}
                }
            }
        });

        if result.is_none()
            && self.key_capture_since.elapsed() > Duration::from_millis(180)
            && ctx.input(|i| i.pointer.any_pressed())
        {
            result = Some(None);
        }

        if let Some(outcome) = result {
            if let Some((mods, key)) = outcome {
                if idx < self.cfg.buttons.len() {
                    self.cfg.buttons[idx].modifiers = mods;
                    self.cfg.buttons[idx].key_combo = key;
                }
            }
            self.key_capture = None;
        }

        ctx.input_mut(|i| {
            i.events.retain(|e| !matches!(e, egui::Event::Key { .. } | egui::Event::Text(_)));
        });
    }

    fn show_main_panel(&mut self, ctx: &egui::Context) {

        configure_visuals(ctx);
        self.section_actionbar(ctx);
        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(bg()).inner_margin(egui::Margin {
                left: 18.0,
                right: 18.0,
                top: 14.0,
                bottom: 14.0,
            }))
            .show(ctx, |ui| {
                self.section_header(ui);
                ui.add_space(14.0);

                let content_w = (ui.available_width() - 18.0).max(300.0);
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.set_max_width(content_w);
                        ui.spacing_mut().item_spacing.y = 12.0;

                        let two_col = content_w >= 1000.0;
                        if two_col {

                            let gutter = 16.0;
                            ui.spacing_mut().item_spacing.x = gutter;

                            let col_w = ((content_w - gutter) / 2.0).floor();
                            set_layout_w(col_w);
                            ui.columns(2, |cols| {
                                cols[0].spacing_mut().item_spacing.y = 12.0;
                                cols[1].spacing_mut().item_spacing.y = 12.0;

                                self.section_serial(&mut cols[0]);
                                self.section_general(&mut cols[0]);
                                self.section_routing(&mut cols[0]);
                                self.section_options(&mut cols[0]);

                                self.section_knobs(&mut cols[1]);
                                self.section_buttons(&mut cols[1]);
                            });
                        } else {
                            set_layout_w(content_w);
                            self.section_serial(ui);
                            self.section_general(ui);
                            self.section_routing(ui);
                            self.section_knobs(ui);
                            self.section_buttons(ui);
                            self.section_options(ui);
                        }
                        ui.add_space(2.0);
                    });
            });

        self.themes_window(ctx);
    }

    fn section_header(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(RichText::new("Configuration").size(17.0).strong().color(text()));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ghost_button(ui, "Update", egui::vec2(0.0, 30.0))
                    .on_hover_text("Rescan devices and processes")
                    .clicked()
                {
                    self.rescan();
                }
            });
        });
        ui.add_space(8.0);
        let sep = ui.available_rect_before_wrap();
        let y = sep.top();
        ui.painter().hline(
            sep.left()..=sep.right(),
            y,
            egui::Stroke::new(1.0, card_border()),
        );
    }

    fn section_serial(&mut self, ui: &mut egui::Ui) {
        card(ui, "Connection", |ui| {
            labeled_row(ui, "Serial Port", |ui| {
                let cur = self.cfg.serial.port.clone();
                ComboBox::from_id_salt("serial_port")
                    .selected_text(if cur.is_empty() { "None".to_string() } else { cur })
                    .width(flex_w(ui, 0.0))
                    .show_ui(ui, |ui| {
                        let ports = self.com_ports.clone();
                        if ports.is_empty() {
                            ui.label(RichText::new("No COM ports found").color(text_muted()));
                        }
                        for p in &ports {
                            ui.selectable_value(&mut self.cfg.serial.port, p.clone(), p);
                        }
                    });
            });
            labeled_row(ui, "Baud Rate", |ui| {
                ComboBox::from_id_salt("baud")
                    .selected_text(self.cfg.serial.baud.to_string())
                    .width(flex_w(ui, 0.0))
                    .show_ui(ui, |ui| {
                        for b in BAUD_RATES {
                            ui.selectable_value(&mut self.cfg.serial.baud, b, b.to_string());
                        }
                    });
            });
        });
    }

    fn section_general(&mut self, ui: &mut egui::Ui) {
        card(ui, "Behavior", |ui| {
            labeled_row(ui, "Max Pot Value", |ui| {
                let mut v = self.cfg.value_max;
                if ui
                    .add_sized(
                        [flex_w(ui, 0.0), 26.0],
                        egui::DragValue::new(&mut v).speed(1.0).range(1.0..=8192.0),
                    )
                    .changed()
                {
                    self.cfg.value_max = v;
                }
            });
            labeled_row(ui, "Volume Curve", |ui| {
                let mut log = self.cfg.use_logarithmic_scale;
                ComboBox::from_id_salt("curve")
                    .selected_text(if log { "Logarithmic" } else { "Linear (Default)" })
                    .width(flex_w(ui, 0.0))
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut log, false, "Linear (Default)");
                        ui.selectable_value(&mut log, true, "Logarithmic");
                    });
                self.cfg.use_logarithmic_scale = log;
            });
        });
    }

    fn themes_window(&mut self, ctx: &egui::Context) {
        if !self.show_themes {
            return;
        }

        let screen = ctx.screen_rect();
        let backdrop = egui::Area::new(egui::Id::new("theme_backdrop"))
            .order(egui::Order::Middle)
            .fixed_pos(screen.min)
            .show(ctx, |ui| {
                let (rect, resp) =
                    ui.allocate_exact_size(screen.size(), egui::Sense::click());
                ui.painter()
                    .rect_filled(rect, egui::Rounding::ZERO, Color32::from_black_alpha(150));
                resp
            });
        if backdrop.inner.clicked() {
            self.show_themes = false;
        }

        let mut open = self.show_themes;
        let frame = egui::Frame::window(&ctx.style())
            .fill(card_bg())
            .stroke(egui::Stroke::new(1.0, card_border()))
            .rounding(egui::Rounding::same(10.0))
            .inner_margin(egui::Margin::same(20.0))
            .shadow(egui::epaint::Shadow {
                offset: egui::vec2(0.0, 8.0),
                blur: 28.0,
                spread: 0.0,
                color: Color32::from_black_alpha(120),
            });
        egui::Window::new(RichText::new("Themes").size(16.0).strong())
            .open(&mut open)
            .order(egui::Order::Foreground)
            .collapsible(false)
            .resizable(false)
            .frame(frame)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(ctx, |ui| {
                ui.set_width(506.0);
                ui.add_space(4.0);
                let cur = self.cfg.theme.clone();
                let mut pick: Option<String> = None;
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing = egui::vec2(12.0, 12.0);
                    for (name, p) in themes() {
                        let name = *name;
                        if theme_tile(ui, name, p, cur.as_str() == name) {
                            pick = Some(name.to_string());
                        }
                    }
                });
                ui.add_space(4.0);
                if let Some(name) = pick {
                    set_theme_by_name(&name);
                    self.cfg.theme = name;
                }
            });
        self.show_themes = open;
    }

    fn section_routing(&mut self, ui: &mut egui::Ui) {
        let devices = self.playback_devices.clone();
        card(ui, "Output Switcher", |ui| {
            for (n, label) in [(1u8, "Output 1"), (2u8, "Output 2")] {
                labeled_row(ui, label, |ui| {
                    let stored = if n == 1 {
                        self.cfg.work_device_1.clone()
                    } else {
                        self.cfg.work_device_2.clone()
                    };
                    let display = routing_display(&stored, &devices);

                    let w = flex_w(ui, 62.0);
                    ComboBox::from_id_salt(format!("wd{}", n))
                        .selected_text(elide(ui, &display, w - 30.0))
                        .width(w)
                        .show_ui(ui, |ui| {
                            let target = if n == 1 {
                                &mut self.cfg.work_device_1
                            } else {
                                &mut self.cfg.work_device_2
                            };
                            ui.selectable_value(target, "None".to_string(), "None");
                            for d in &devices {
                                ui.selectable_value(target, d.clone(), d);
                            }
                        });
                    if ui.button("Test").clicked() {
                        let name = extract_clean_name(&stored);
                        std::thread::spawn(move || switch_device(&name));
                    }
                });
            }
        });
    }

    fn section_knobs(&mut self, ui: &mut egui::Ui) {
        let processes = self.active_processes.clone();
        let captures = self.capture_devices.clone();
        let playbacks = self.playback_devices.clone();

        let (add_clicked, _) = card_with_action(ui, "Knob Mappings", "+ Add Knob", |ui| {
            if self.cfg.dials.is_empty() {
                empty_hint(ui, "No knobs yet. Add one to map a potentiometer.");
                return;
            }
            let mut remove: Option<usize> = None;
            let len = self.cfg.dials.len();
            for i in 0..len {
                ui.push_id(("knob", i), |ui| {
                    hover_row(ui, |ui| {
                        ui.horizontal(|ui| {
                            index_badge(ui, i + 1);

                            let cur_type = self.cfg.dials[i].dial_type.clone();
                            ComboBox::from_id_salt("ktype")
                                .selected_text(dial_type_label(&cur_type))
                                .width(120.0)
                                .show_ui(ui, |ui| {
                                    let t = &mut self.cfg.dials[i].dial_type;
                                    ui.selectable_value(t, "system".to_string(), "System");
                                    ui.selectable_value(t, "process".to_string(), "Process");
                                    ui.selectable_value(t, "all_others".to_string(), "Others");
                                    ui.selectable_value(t, "microphone".to_string(), "Microphone");
                                    ui.selectable_value(t, "output_device".to_string(), "Output Device");
                                });

                            let dtype = self.cfg.dials[i].dial_type.clone();
                            if dtype == "system" || dtype == "all_others" {
                                self.cfg.dials[i].process_name = None;
                            }

                            let options: &[String] = match dtype.as_str() {
                                "process" => &processes,
                                "microphone" => &captures,
                                "output_device" => &playbacks,
                                _ => &[],
                            };
                            let has_target =
                                matches!(dtype.as_str(), "process" | "microphone" | "output_device");
                            ui.add_enabled_ui(has_target, |ui| {
                                let cur = self.cfg.dials[i]
                                    .process_name
                                    .clone()
                                    .unwrap_or_else(|| "None".to_string());

                                let w = flex_w(ui, 132.0);
                                let disp = if cur.is_empty() { "None".to_string() } else { cur.clone() };
                                ComboBox::from_id_salt("ktarget")
                                    .selected_text(elide(ui, &disp, w - 30.0))
                                    .width(w)
                                    .show_ui(ui, |ui| {
                                        let mut sel = cur.clone();
                                        let changed = {
                                            let mut c = ui
                                                .selectable_value(&mut sel, "None".to_string(), "None")
                                                .changed();
                                            for o in options {
                                                c |= ui.selectable_value(&mut sel, o.clone(), o).changed();
                                            }
                                            c
                                        };
                                        if changed {
                                            self.cfg.dials[i].process_name =
                                                if sel == "None" { None } else { Some(sel) };
                                        }
                                    });
                            });

                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if delete_button(ui).clicked() {
                                    remove = Some(i);
                                }
                                ui.checkbox(&mut self.cfg.dials[i].inverted, "Invert");
                            });
                        });
                    });
                });
            }
            if let Some(idx) = remove {
                self.cfg.dials.remove(idx);
            }
        });

        if add_clicked {
            self.cfg.dials.push(DialConfig {
                dial_type: "system".to_string(),
                process_name: None,
                inverted: false,
            });
        }
    }

    fn section_buttons(&mut self, ui: &mut egui::Ui) {
        let dial_count = self.cfg.dials.len();

        let (add_clicked, _) = card_with_action(ui, "Button Mappings", "+ Add Button", |ui| {
            if self.cfg.buttons.is_empty() {
                empty_hint(ui, "No buttons yet. Add one per physical button, in wiring order.");
                return;
            }
            let mut remove: Option<usize> = None;
            let len = self.cfg.buttons.len();
            for i in 0..len {
                ui.push_id(("btn", i), |ui| {
                    hover_row(ui, |ui| {
                        ui.horizontal(|ui| {
                            index_badge(ui, i + 1);

                            let cur_action = self.cfg.buttons[i].action.clone();
                            ComboBox::from_id_salt("baction")
                                .selected_text(action_label(&cur_action))
                                .width(120.0)
                                .show_ui(ui, |ui| {
                                    let a = &mut self.cfg.buttons[i].action;
                                    ui.selectable_value(a, "none".to_string(), "None");
                                    ui.selectable_value(a, "mute_dial".to_string(), "Mute Knob");
                                    ui.selectable_value(a, "media".to_string(), "Media");
                                    ui.selectable_value(a, "keys".to_string(), "Keys");
                                });

                            let action = self.cfg.buttons[i].action.clone();

                            let mid_w = flex_w(ui, 48.0);
                            match action.as_str() {
                                "mute_dial" => {
                                    if dial_count == 0 {
                                        ui.add_enabled(false, egui::Button::new("(no knobs)"));
                                    } else {
                                        let di = self.cfg.buttons[i].dial_index.min(dial_count - 1);
                                        self.cfg.buttons[i].dial_index = di;
                                        ComboBox::from_id_salt("bknob")
                                            .selected_text(format!("Knob {}", di + 1))
                                            .width(mid_w)
                                            .show_ui(ui, |ui| {
                                                for k in 0..dial_count {
                                                    ui.selectable_value(
                                                        &mut self.cfg.buttons[i].dial_index,
                                                        k,
                                                        format!("Knob {}", k + 1),
                                                    );
                                                }
                                            });
                                    }
                                }
                                "media" => {
                                    let cur_tok = self.cfg.buttons[i].media_key.clone();
                                    let cur_idx =
                                        MEDIA_TOKENS.iter().position(|t| *t == cur_tok).unwrap_or(0);
                                    ComboBox::from_id_salt("bmedia")
                                        .selected_text(MEDIA_LABELS[cur_idx])
                                        .width(mid_w)
                                        .show_ui(ui, |ui| {
                                            for (idx, label) in MEDIA_LABELS.iter().enumerate() {
                                                ui.selectable_value(
                                                    &mut self.cfg.buttons[i].media_key,
                                                    MEDIA_TOKENS[idx].to_string(),
                                                    *label,
                                                );
                                            }
                                        });
                                }
                                "keys" => {
                                    let capturing = self.key_capture == Some(i);
                                    let label = if capturing {
                                        "Press keys…".to_string()
                                    } else {
                                        format_combo(
                                            &self.cfg.buttons[i].modifiers,
                                            &self.cfg.buttons[i].key_combo,
                                        )
                                    };
                                    let (base, hover) = if capturing {
                                        (accent(), accent_hover())
                                    } else {
                                        (widget_bg(), row_hover())
                                    };
                                    if styled_button(ui, &label, base, hover, egui::vec2(mid_w, 30.0))
                                        .clicked()
                                    {
                                        if capturing {
                                            self.key_capture = None;
                                        } else {
                                            self.key_capture = Some(i);
                                            self.key_capture_since = Instant::now();
                                        }
                                    }
                                }
                                _ => {
                                    ui.add_enabled(false, egui::Button::new("-"));
                                }
                            }

                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if delete_button(ui).clicked() {
                                    remove = Some(i);
                                }
                            });
                        });
                    });
                });
            }
            if let Some(idx) = remove {
                if self.key_capture == Some(idx) {
                    self.key_capture = None;
                }
                self.cfg.buttons.remove(idx);
            }
        });

        if add_clicked {
            self.cfg.buttons.push(ButtonConfig::default());
        }
    }

    fn section_options(&mut self, ui: &mut egui::Ui) {
        card(ui, "Options", |ui| {

            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing.x = 18.0;
                ui.checkbox(&mut self.startup_enabled, "Launch at startup");
                ui.checkbox(&mut self.cfg.debug_mode, "Debug console");
                ui.checkbox(&mut self.cfg.enable_osd, "On-screen display");
            });
        });
    }

    fn section_actionbar(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::bottom("actionbar")
            .show_separator_line(false)
            .frame(
                egui::Frame::none()
                    .fill(card_bg())
                    .inner_margin(egui::Margin::symmetric(18.0, 12.0)),
            )
            .show(ctx, |ui| {
                let r = ui.max_rect();
                ui.painter().hline(
                    r.left() - 18.0..=r.right() + 18.0,
                    r.top() - 12.0,
                    egui::Stroke::new(1.0, card_border()),
                );
                ui.horizontal(|ui| {
                    let (label, base, hover) = match self.save_flash {
                        Some((t, true)) if t.elapsed() < Duration::from_millis(1200) => {
                            ("Saved", success(), success())
                        }
                        Some((t, false)) if t.elapsed() < Duration::from_millis(1800) => {
                            ("Save failed", destructive(), destructive_hover())
                        }
                        _ => ("Save Changes", accent(), accent_hover()),
                    };
                    if styled_button(ui, label, base, hover, egui::vec2(130.0, 32.0)).clicked() {
                        self.save();
                    }

                    if ghost_button(ui, "Close", egui::vec2(0.0, 32.0)).clicked() {
                        ui.ctx().send_viewport_cmd(ViewportCommand::Visible(false));
                    }

                    if ghost_button(ui, "Themes", egui::vec2(0.0, 32.0)).clicked() {
                        self.show_themes = !self.show_themes;
                    }

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        self.credit(ui);
                    });
                });
            });
    }

    fn credit(&mut self, ui: &mut egui::Ui) {
        self.github_link(ui);
        ui.add_space(10.0);
        ui.label(RichText::new("Made by TZey").size(12.5).color(text_muted()));
    }

    fn github_link(&mut self, ui: &mut egui::Ui) {
        if self.github_tex.is_none() {
            self.github_tex = load_github_texture(ui.ctx());
        }
        let size = egui::vec2(19.0, 19.0);
        if let Some(tex) = &self.github_tex {
            let src = egui::load::SizedTexture::new(tex.id(), size);
            let resp = ui
                .add(egui::Image::new(src).tint(text_muted()).sense(egui::Sense::click()))
                .on_hover_cursor(egui::CursorIcon::PointingHand)
                .on_hover_text("Open the RVCI repo on GitHub");
            if resp.hovered() {

                egui::Image::new(src).tint(text()).paint_at(ui, resp.rect);
            }
            if resp.clicked() {
                ui.ctx().open_url(egui::OpenUrl::new_tab(GITHUB_URL));
            }
        } else {

            if ui.link(RichText::new("GitHub").size(12.5).color(text_muted())).clicked() {
                ui.ctx().open_url(egui::OpenUrl::new_tab(GITHUB_URL));
            }
        }
    }

}

fn load_github_texture(ctx: &egui::Context) -> Option<egui::TextureHandle> {
    let img = image::load_from_memory(GITHUB_PNG).ok()?;
    let rgba = img.into_rgba8();
    let (w, h) = rgba.dimensions();
    let mut color = egui::ColorImage::new([w as usize, h as usize], Color32::TRANSPARENT);
    for (i, px) in rgba.pixels().enumerate() {

        color.pixels[i] = Color32::from_rgba_unmultiplied(255, 255, 255, px[3]);
    }
    Some(ctx.load_texture("github_mark", color, egui::TextureOptions::LINEAR))
}

fn theme_tile(ui: &mut egui::Ui, name: &str, p: &Palette, selected: bool) -> bool {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(154.0, 100.0), egui::Sense::click());
    let painter = ui.painter();
    let round = egui::Rounding::same(10.0);

    painter.rect_filled(rect, round, p.bg);

    let card_r = egui::Rect::from_min_max(
        rect.min + egui::vec2(12.0, 12.0),
        egui::pos2(rect.right() - 12.0, rect.bottom() - 30.0),
    );
    painter.rect_filled(card_r, egui::Rounding::same(7.0), p.card_bg);
    painter.rect_stroke(card_r, egui::Rounding::same(7.0), egui::Stroke::new(1.0, p.card_border));

    let bar = egui::Rect::from_min_size(
        card_r.min + egui::vec2(8.0, 8.0),
        egui::vec2(4.0, card_r.height() - 16.0),
    );
    paint_vgrad(painter, bar, p.accent2, p.accent, 2.0);

    let mut y = card_r.top() + 9.0;
    for w in [0.60_f32, 0.42, 0.50] {
        let row = egui::Rect::from_min_size(
            egui::pos2(bar.right() + 8.0, y),
            egui::vec2((card_r.width() - 30.0) * w, 6.5),
        );
        painter.rect_filled(row, egui::Rounding::same(3.0), p.widget_bg);
        y += 11.0;
    }

    let pill = egui::Rect::from_min_size(
        egui::pos2(card_r.right() - 32.0, card_r.bottom() - 15.0),
        egui::vec2(24.0, 9.0),
    );
    paint_vgrad(painter, pill, p.accent2, p.accent, 4.0);

    painter.text(
        egui::pos2(rect.left() + 13.0, rect.bottom() - 15.0),
        egui::Align2::LEFT_CENTER,
        name,
        egui::FontId::proportional(13.5),
        p.text,
    );

    let (bw, bc) = if selected {
        (2.0, accent())
    } else if resp.hovered() {
        (1.5, text_muted())
    } else {
        (1.0, card_border())
    };
    painter.rect_stroke(rect, round, egui::Stroke::new(bw, bc));

    resp.clicked()
}

fn index_badge(ui: &mut egui::Ui, n: usize) {
    let size = egui::vec2(18.0, 24.0);
    let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        n.to_string(),
        egui::FontId::proportional(12.5),
        text_muted(),
    );
}

const APP_AUMID: &str = "TZey.RVCI";

fn register_toast_identity() {
    let key_path = format!("Software\\Classes\\AppUserModelId\\{}", APP_AUMID);
    if let Ok((key, _)) = RegKey::predef(HKEY_CURRENT_USER).create_subkey(&key_path) {
        let _ = key.set_value("DisplayName", &"RVCI");
        let icon = get_exe_dir().join("rvci.ico");
        if icon.exists() {
            if let Some(s) = icon.to_str() {
                let _ = key.set_value("IconUri", &s);
            }
        }
    }
    unsafe {
        let mut w: Vec<u16> = APP_AUMID.encode_utf16().collect();
        w.push(0);
        let _ = SetCurrentProcessExplicitAppUserModelID(PCWSTR(w.as_ptr()));
    }
}

fn notify_toast(title: &str, body: &str) {
    let title = title.to_string();
    let body = body.to_string();
    std::thread::spawn(move || {
        let _ = show_toast(&title, &body);
    });
}

fn show_toast(title: &str, body: &str) -> windows::core::Result<()> {
    let xml = format!(
        "<toast><visual><binding template=\"ToastGeneric\">\
         <text>{}</text><text>{}</text></binding></visual></toast>",
        xml_escape(title),
        xml_escape(body)
    );
    let doc = XmlDocument::new()?;
    doc.LoadXml(&HSTRING::from(xml))?;
    let toast = ToastNotification::CreateToastNotification(&doc)?;
    let notifier =
        ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(APP_AUMID))?;
    notifier.Show(&toast)
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn empty_hint(ui: &mut egui::Ui, text: &str) {
    ui.vertical_centered(|ui| {
        ui.add_space(6.0);
        ui.label(RichText::new(text).size(13.0).color(text_muted()));
        ui.add_space(6.0);
    });
}

impl eframe::App for RvciApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {

        if self.want_show.swap(false, Ordering::SeqCst) {
            self.rescan();
        }

        if !self.user_opened.load(Ordering::SeqCst) {
            hide_window_native(self.hwnd);
            return;
        }

        while let Ok(procs) = self.proc_rx.try_recv() {
            self.active_processes = procs;
        }

        if ctx.input(|i| i.viewport().close_requested()) {
            ctx.send_viewport_cmd(ViewportCommand::CancelClose);
            self.user_opened.store(false, Ordering::SeqCst);
            hide_window_native(self.hwnd);
        }

        self.handle_key_capture(ctx);
        self.show_main_panel(ctx);

        if self.save_flash.is_some() {
            ctx.request_repaint_after(Duration::from_millis(50));
        }
    }
}

fn show_window_native(hwnd: isize) {
    if hwnd == 0 {
        return;
    }
    use windows::Win32::Foundation::{HWND, RECT};
    use windows::Win32::UI::WindowsAndMessaging::{
        GetWindowRect, IsIconic, SetForegroundWindow, SetWindowPos, ShowWindow,
        SystemParametersInfoW, SPI_GETWORKAREA, SWP_NOSIZE, SWP_NOZORDER,
        SW_RESTORE, SW_SHOW, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS,
    };
    let hwnd = HWND(hwnd as *mut c_void);
    unsafe {
        let mut rc = RECT::default();
        if GetWindowRect(hwnd, &mut rc).is_ok() && rc.left <= -20000 {
            let mut work = RECT::default();
            let _ = SystemParametersInfoW(
                SPI_GETWORKAREA,
                0,
                Some(&mut work as *mut RECT as *mut core::ffi::c_void),
                SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
            );
            let w = rc.right - rc.left;
            let h = rc.bottom - rc.top;
            let x = work.left + ((work.right - work.left) - w).max(0) / 2;
            let y = work.top + ((work.bottom - work.top) - h).max(0) / 2;
            let _ = SetWindowPos(hwnd, None, x, y, 0, 0, SWP_NOSIZE | SWP_NOZORDER);
        }
        if IsIconic(hwnd).as_bool() {
            let _ = ShowWindow(hwnd, SW_RESTORE);
        } else {
            let _ = ShowWindow(hwnd, SW_SHOW);
        }
        let _ = SetForegroundWindow(hwnd);
    }
}

fn hide_window_native(hwnd: isize) {
    if hwnd == 0 {
        return;
    }
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{ShowWindow, SW_HIDE};
    let hwnd = HWND(hwnd as *mut c_void);
    unsafe {
        let _ = ShowWindow(hwnd, SW_HIDE);
    }
}

fn configure_visuals(ctx: &egui::Context) {
    let mut visuals = if pal().dark { egui::Visuals::dark() } else { egui::Visuals::light() };
    visuals.panel_fill = bg();
    visuals.window_fill = card_bg();
    visuals.extreme_bg_color = pal().extreme_bg;
    visuals.faint_bg_color = pal().faint_bg;
    visuals.override_text_color = Some(text());

    visuals.widgets.noninteractive.bg_fill = card_bg();
    visuals.widgets.noninteractive.fg_stroke = egui::Stroke::new(1.0, text_muted());
    visuals.widgets.inactive.bg_fill = widget_bg();
    visuals.widgets.inactive.weak_bg_fill = widget_bg();
    visuals.widgets.inactive.fg_stroke = egui::Stroke::new(1.0, text());
    visuals.widgets.inactive.bg_stroke = egui::Stroke::new(1.0, card_border());
    visuals.widgets.hovered.bg_fill = row_hover();
    visuals.widgets.hovered.weak_bg_fill = row_hover();
    visuals.widgets.hovered.bg_stroke = egui::Stroke::new(1.0, mix(card_border(), text_muted(), 0.45));
    visuals.widgets.hovered.fg_stroke = egui::Stroke::new(1.0, text());
    visuals.widgets.active.bg_fill = row_hover();
    visuals.widgets.active.bg_stroke = egui::Stroke::new(1.0, accent());
    visuals.widgets.open.bg_fill = widget_bg();

    visuals.selection.bg_fill = accent().linear_multiply(0.55);
    visuals.selection.stroke = egui::Stroke::new(1.0, accent());
    visuals.hyperlink_color = accent();
    visuals.window_stroke = egui::Stroke::new(1.0, card_border());
    visuals.popup_shadow = egui::epaint::Shadow {
        offset: egui::vec2(0.0, 4.0),
        blur: 16.0,
        spread: 0.0,
        color: Color32::from_black_alpha(120),
    };

    let rounding = egui::Rounding::same(6.0);
    for w in [
        &mut visuals.widgets.noninteractive,
        &mut visuals.widgets.inactive,
        &mut visuals.widgets.hovered,
        &mut visuals.widgets.active,
        &mut visuals.widgets.open,
    ] {
        w.rounding = rounding;
    }
    visuals.window_rounding = egui::Rounding::same(12.0);
    visuals.menu_rounding = egui::Rounding::same(10.0);
    ctx.set_visuals(visuals);

    let mut style = (*ctx.style()).clone();
    use egui::{FontFamily::Proportional, FontId, TextStyle};
    style.text_styles = [
        (TextStyle::Heading, FontId::new(21.0, Proportional)),
        (TextStyle::Body, FontId::new(14.5, Proportional)),
        (TextStyle::Button, FontId::new(14.5, Proportional)),
        (TextStyle::Monospace, FontId::new(13.0, egui::FontFamily::Monospace)),
        (TextStyle::Small, FontId::new(11.5, Proportional)),
    ]
    .into();
    style.spacing.item_spacing = egui::vec2(9.0, 9.0);
    style.spacing.button_padding = egui::vec2(12.0, 7.0);
    style.spacing.interact_size.y = 30.0;
    style.spacing.combo_width = 180.0;
    style.spacing.window_margin = egui::Margin::same(0.0);
    style.visuals.clip_rect_margin = 3.0;
    ctx.set_style(style);
}

fn card<R>(ui: &mut egui::Ui, title: &str, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    egui::Frame::none()
        .fill(card_bg())
        .stroke(egui::Stroke::new(1.0, card_border()))
        .rounding(egui::Rounding::same(10.0))
        .inner_margin(egui::Margin::symmetric(16.0, 14.0))
        .show(ui, |ui| {

            ui.set_width(layout_w().min(ui.available_width()));
            if !title.is_empty() {
                ui.label(
                    RichText::new(title.to_uppercase())
                        .size(11.5)
                        .strong()
                        .color(text_muted()),
                );
                ui.add_space(10.0);
            }
            add(ui)
        })
        .inner
}

fn card_with_action<R>(
    ui: &mut egui::Ui,
    title: &str,
    action: &str,
    add: impl FnOnce(&mut egui::Ui) -> R,
) -> (bool, R) {
    let mut clicked = false;
    let inner = card(ui, "", |ui| {
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(title.to_uppercase())
                    .size(11.5)
                    .strong()
                    .color(text_muted()),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                clicked = text_button(ui, action).clicked();
            });
        });
        ui.add_space(10.0);
        add(ui)
    });
    (clicked, inner)
}

const LABEL_COL_W: f32 = 132.0;

fn labeled_row<R>(ui: &mut egui::Ui, label: &str, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(egui::vec2(LABEL_COL_W, 28.0), egui::Sense::hover());
        ui.painter().text(
            egui::pos2(rect.left(), rect.center().y),
            egui::Align2::LEFT_CENTER,
            label,
            egui::FontId::proportional(14.5),
            text_muted(),
        );
        add(ui)
    })
    .inner
}

fn elide(ui: &egui::Ui, text: &str, max_w: f32) -> String {
    let font = egui::FontId::proportional(14.5);
    let measure = |s: &str| {
        ui.painter()
            .layout_no_wrap(s.to_string(), font.clone(), Color32::WHITE)
            .size()
            .x
    };
    if max_w <= 0.0 || measure(text) <= max_w {
        return text.to_string();
    }
    let mut s = text.to_string();
    while s.chars().count() > 1 && measure(&format!("{}…", s)) > max_w {
        s.pop();
    }
    format!("{}…", s.trim_end())
}

fn flex_w(ui: &egui::Ui, reserve: f32) -> f32 {
    let gap = ui.spacing().item_spacing.x;

    (ui.available_width() - reserve - gap * 2.0).max(90.0)
}

fn styled_button(
    ui: &mut egui::Ui,
    text: &str,
    base: Color32,
    hover: Color32,
    min: egui::Vec2,
) -> egui::Response {
    let font = egui::FontId::proportional(14.5);
    let galley = ui
        .painter()
        .layout_no_wrap(text.to_string(), font, Color32::PLACEHOLDER);
    let pad = egui::vec2(14.0, 8.0);
    let desired = egui::vec2(
        (galley.size().x + pad.x * 2.0).max(min.x),
        (galley.size().y + pad.y * 2.0).max(min.y),
    );
    let (rect, resp) = ui.allocate_exact_size(desired, egui::Sense::click());
    let t = ui.ctx().animate_bool(resp.id, resp.hovered());
    let down = resp.is_pointer_button_down_on();

    let mut fill = lerp_color(base, hover, t);
    if down {
        fill = lerp_color(fill, Color32::BLACK, 0.12);
    }
    ui.painter().rect_filled(rect, egui::Rounding::same(6.0), fill);
    let text_pos = rect.center() - galley.size() / 2.0;
    ui.painter().galley(text_pos, galley, contrast_text(fill));
    resp
}

fn ghost_button(ui: &mut egui::Ui, label: &str, min: egui::Vec2) -> egui::Response {
    let font = egui::FontId::proportional(14.0);
    let galley = ui
        .painter()
        .layout_no_wrap(label.to_string(), font, Color32::PLACEHOLDER);
    let pad = egui::vec2(13.0, 7.0);
    let desired = egui::vec2(
        (galley.size().x + pad.x * 2.0).max(min.x),
        (galley.size().y + pad.y * 2.0).max(min.y),
    );
    let (rect, resp) = ui.allocate_exact_size(desired, egui::Sense::click());
    let t = ui.ctx().animate_bool(resp.id, resp.hovered());
    let down = resp.is_pointer_button_down_on();
    if t > 0.0 || down {
        let mut fill = row_hover();
        if down {
            fill = lerp_color(fill, Color32::BLACK, 0.10);
        }
        let a = if down { 255 } else { (t * 255.0) as u8 };
        ui.painter().rect_filled(
            rect,
            egui::Rounding::same(6.0),
            Color32::from_rgba_unmultiplied(fill.r(), fill.g(), fill.b(), a),
        );
    }
    ui.painter().rect_stroke(
        rect,
        egui::Rounding::same(6.0),
        egui::Stroke::new(1.0, card_border()),
    );
    let pos = rect.center() - galley.size() / 2.0;
    ui.painter().galley(pos, galley, text());
    resp
}

fn text_button(ui: &mut egui::Ui, label: &str) -> egui::Response {
    let font = egui::FontId::proportional(13.5);
    let galley = ui
        .painter()
        .layout_no_wrap(label.to_string(), font, Color32::PLACEHOLDER);
    let pad = egui::vec2(8.0, 5.0);
    let desired = galley.size() + pad * 2.0;
    let (rect, resp) = ui.allocate_exact_size(desired, egui::Sense::click());
    let t = ui.ctx().animate_bool(resp.id, resp.hovered());
    if t > 0.0 {
        let a = accent();
        ui.painter().rect_filled(
            rect,
            egui::Rounding::same(6.0),
            Color32::from_rgba_unmultiplied(a.r(), a.g(), a.b(), (t * 28.0) as u8),
        );
    }
    let col = lerp_color(accent(), accent_hover(), t);
    ui.painter().galley(rect.min + pad, galley, col);
    resp
}

fn delete_button(ui: &mut egui::Ui) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(26.0, 26.0), egui::Sense::click());
    let t = ui.ctx().animate_bool(resp.id, resp.hovered());
    if t > 0.0 {
        let d = destructive();
        ui.painter().rect_filled(
            rect,
            egui::Rounding::same(6.0),
            Color32::from_rgba_unmultiplied(d.r(), d.g(), d.b(), (t * 45.0) as u8),
        );
    }
    let col = lerp_color(text_muted(), destructive(), t);
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        "×",
        egui::FontId::proportional(17.0),
        col,
    );
    resp.on_hover_text("Remove")
}

fn hover_row<R>(ui: &mut egui::Ui, add_contents: impl FnOnce(&mut egui::Ui) -> R) -> R {
    let frame = egui::Frame::none()
        .inner_margin(egui::Margin::symmetric(6.0, 4.0))
        .rounding(egui::Rounding::same(6.0));
    let r = frame.show(ui, add_contents);
    let hovered = ui.rect_contains_pointer(r.response.rect);
    let t = ui.ctx().animate_bool(r.response.id, hovered);
    if t > 0.0 {
        ui.painter().rect_filled(
            r.response.rect,
            egui::Rounding::same(6.0),
            Color32::from_rgba_unmultiplied(row_hover().r(), row_hover().g(), row_hover().b(), (t * 90.0) as u8),
        );
    }
    r.inner
}

fn lerp_color(a: Color32, b: Color32, t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let l = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t) as u8;
    Color32::from_rgb(l(a.r(), b.r()), l(a.g(), b.g()), l(a.b(), b.b()))
}

fn dial_type_label(t: &str) -> &'static str {
    match t {
        "process" => "Process",
        "all_others" => "Others",
        "microphone" => "Microphone",
        "output_device" => "Output Device",
        _ => "System",
    }
}

fn action_label(a: &str) -> &'static str {
    match a {
        "mute_dial" => "Mute Knob",
        "media" => "Media",
        "keys" => "Keys",
        _ => "None",
    }
}

fn routing_display(stored: &str, devices: &[String]) -> String {
    if stored == "None" || stored.is_empty() {
        return "None".to_string();
    }
    if let Some(d) = devices.iter().find(|d| d.to_lowercase().contains(&stored.to_lowercase())) {
        return d.clone();
    }
    stored.to_string()
}

mod osd {
    use std::sync::mpsc::Receiver;
    use std::time::{Duration, Instant};

    use windows::core::w;
    use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
    use windows::Win32::Graphics::Gdi::{
        BeginPaint, CreatePen, CreateSolidBrush, DeleteObject, DrawTextW, EndPaint, FillRect,
        InvalidateRect, RoundRect, SelectObject, SetBkMode, SetTextColor, UpdateWindow, DT_CENTER,
        DT_SINGLELINE, DT_VCENTER, HBRUSH, PAINTSTRUCT, PS_SOLID, TRANSPARENT,
    };
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DispatchMessageW, GetClientRect, GetWindowLongPtrW,
        LoadCursorW, PeekMessageW, RegisterClassExW, SetLayeredWindowAttributes,
        SetWindowLongPtrW, SetWindowPos, ShowWindow, SystemParametersInfoW, TranslateMessage,
        GWLP_USERDATA, HWND_TOPMOST, IDC_ARROW, LWA_ALPHA, LWA_COLORKEY, MSG, PM_REMOVE,
        SET_WINDOW_POS_FLAGS, SPI_GETWORKAREA, SW_HIDE, SW_SHOWNOACTIVATE,
        SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, WINDOW_EX_STYLE, WNDCLASSEXW, WS_EX_LAYERED,
        WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
    };

    const OSD_W: i32 = 300;
    const OSD_H: i32 = 92;
    const KEY_RGB: u32 = 0x00FF_00FF;
    const PANEL_ALPHA: u8 = 185;

    fn rgb(r: u8, g: u8, b: u8) -> COLORREF {
        COLORREF((r as u32) | ((g as u32) << 8) | ((b as u32) << 16))
    }

    struct OsdState {
        text: Vec<u16>,
        vol: f32,
    }

    fn to_utf16(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }

    pub fn spawn(rx: Receiver<(String, f32)>) {
        std::thread::spawn(move || unsafe { run(rx) });
    }

    unsafe fn run(rx: Receiver<(String, f32)>) {
        let hinstance = match GetModuleHandleW(None) {
            Ok(h) => h,
            Err(_) => return,
        };
        let class_name = w!("RVCI_OSD_WNDCLASS");

        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(wndproc),
            hInstance: hinstance.into(),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            lpszClassName: class_name,
            ..Default::default()
        };
        RegisterClassExW(&wc);

        let mut work = RECT::default();
        let _ = SystemParametersInfoW(
            SPI_GETWORKAREA,
            0,
            Some(&mut work as *mut RECT as *mut core::ffi::c_void),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        );
        let (work_w, work_b) = if work.right > work.left {
            (work.right - work.left, work.bottom)
        } else {
            (1920, 1040)
        };
        let x = work.left + (work_w - OSD_W) / 2;
        let y = work_b - OSD_H - 24;

        let ex_style: WINDOW_EX_STYLE = WS_EX_LAYERED
            | WS_EX_TRANSPARENT
            | WS_EX_TOPMOST
            | WS_EX_TOOLWINDOW
            | WS_EX_NOACTIVATE;

        let hwnd = match CreateWindowExW(
            ex_style,
            class_name,
            w!("RVCI OSD"),
            WS_POPUP,
            x,
            y,
            OSD_W,
            OSD_H,
            None,
            None,
            Some(hinstance.into()),
            None,
        ) {
            Ok(h) => h,
            Err(_) => return,
        };

        let _ = SetLayeredWindowAttributes(
            hwnd,
            COLORREF(KEY_RGB),
            PANEL_ALPHA,
            LWA_COLORKEY | LWA_ALPHA,
        );

        let state = Box::new(OsdState { text: Vec::new(), vol: 0.0 });
        let state_ptr = Box::into_raw(state);
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, state_ptr as isize);

        let mut visible = false;
        let mut deadline = Instant::now();
        let mut msg = MSG::default();

        loop {

            let timeout = if visible {
                Duration::from_millis(30)
            } else {
                Duration::from_millis(250)
            };
            let mut got: Option<(String, f32)> = match rx.recv_timeout(timeout) {
                Ok(m) => Some(m),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => None,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
            };

            while let Ok(m) = rx.try_recv() {
                got = Some(m);
            }
            if let Some((name, vol)) = got {
                (*state_ptr).text = to_utf16(&name);
                (*state_ptr).vol = vol.clamp(0.0, 1.0);
                visible = true;
                deadline = Instant::now() + Duration::from_millis(1800);

                let mut work = RECT::default();
                let _ = SystemParametersInfoW(
                    SPI_GETWORKAREA,
                    0,
                    Some(&mut work as *mut RECT as *mut core::ffi::c_void),
                    SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
                );
                let (osd_x, osd_y) = if work.right > work.left {
                    let w = work.right - work.left;
                    (work.left + (w - OSD_W) / 2, work.bottom - OSD_H - 24)
                } else {
                    ((1920 - OSD_W) / 2, 1040 - OSD_H - 24)
                };
                let _ = SetWindowPos(
                    hwnd,
                    Some(HWND_TOPMOST),
                    osd_x,
                    osd_y,
                    OSD_W,
                    OSD_H,
                    SET_WINDOW_POS_FLAGS(0),
                );
                let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
                let _ = InvalidateRect(Some(hwnd), None, true);
                let _ = UpdateWindow(hwnd);
            }

            while PeekMessageW(&mut msg, Some(hwnd), 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }

            if visible && Instant::now() >= deadline {
                let _ = ShowWindow(hwnd, SW_HIDE);
                visible = false;
            }
        }
    }

    unsafe extern "system" fn wndproc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        const WM_PAINT: u32 = 0x000F;
        if msg == WM_PAINT {
            paint(hwnd);
            return LRESULT(0);
        }
        DefWindowProcW(hwnd, msg, wparam, lparam)
    }

    unsafe fn paint(hwnd: HWND) {
        let state_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const OsdState;

        let mut rc = RECT::default();
        if GetClientRect(hwnd, &mut rc).is_err() {
            return;
        }

        let mut ps = PAINTSTRUCT::default();
        let hdc = BeginPaint(hwnd, &mut ps);

        let key_brush: HBRUSH = CreateSolidBrush(COLORREF(KEY_RGB));
        FillRect(hdc, &rc, key_brush);
        let _ = DeleteObject(key_brush.into());

        let panel_brush: HBRUSH = CreateSolidBrush(rgb(22, 22, 26));
        let border_pen = CreatePen(PS_SOLID, 2, rgb(245, 245, 245));
        let old_brush = SelectObject(hdc, panel_brush.into());
        let old_pen = SelectObject(hdc, border_pen.into());
        let inset = 2;
        let _ = RoundRect(
            hdc,
            rc.left + inset,
            rc.top + inset,
            rc.right - inset,
            rc.bottom - inset,
            16,
            16,
        );

        SetBkMode(hdc, TRANSPARENT);
        SetTextColor(hdc, rgb(240, 240, 245));
        if !state_ptr.is_null() {
            let text = &(*state_ptr).text;
            if !text.is_empty() {
                let mut text_rc = RECT {
                    left: rc.left + 16,
                    top: rc.top + 12,
                    right: rc.right - 16,
                    bottom: rc.top + 40,
                };
                let mut buf = text.clone();
                DrawTextW(hdc, &mut buf, &mut text_rc, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
            }
        }

        let pad = 18;
        let bar_left = rc.left + pad;
        let bar_right = rc.right - pad;
        let bar_top = rc.bottom - 30;
        let bar_bottom = bar_top + 9;

        let track_brush: HBRUSH = CreateSolidBrush(rgb(70, 70, 78));
        let track_pen = CreatePen(PS_SOLID, 2, rgb(10, 10, 10));
        SelectObject(hdc, track_brush.into());
        SelectObject(hdc, track_pen.into());
        let _ = RoundRect(hdc, bar_left, bar_top, bar_right, bar_bottom, 9, 9);

        let vol = if state_ptr.is_null() { 0.0 } else { (*state_ptr).vol.clamp(0.0, 1.0) };
        let fill_right = bar_left + ((bar_right - bar_left) as f32 * vol) as i32;
        if fill_right > bar_left + 2 {
            let fill_brush: HBRUSH = CreateSolidBrush(rgb(245, 245, 245));
            let fill_pen = CreatePen(PS_SOLID, 1, rgb(10, 10, 10));
            SelectObject(hdc, fill_brush.into());
            SelectObject(hdc, fill_pen.into());
            let _ = RoundRect(hdc, bar_left, bar_top, fill_right, bar_bottom, 9, 9);
            SelectObject(hdc, track_brush.into());
            let _ = DeleteObject(fill_brush.into());
            let _ = DeleteObject(fill_pen.into());
        }

        SelectObject(hdc, old_brush);
        SelectObject(hdc, old_pen);
        let _ = DeleteObject(panel_brush.into());
        let _ = DeleteObject(border_pen.into());
        let _ = DeleteObject(track_brush.into());
        let _ = DeleteObject(track_pen.into());

        let _ = EndPaint(hwnd, &ps);
    }
}

fn main() -> Result<()> {

    register_toast_identity();

    let path = get_config_path();

    let debug_mode_enabled = if let Ok(file) = File::open(&path) {
        let config: AppConfig = serde_json::from_reader(BufReader::new(file)).unwrap_or_default();
        config.debug_mode
    } else {
        false
    };

    if debug_mode_enabled {
        unsafe {
            let _ = AllocConsole();
        }
        println!("==========================================");
        println!(" RVCI Debug Console Initialized");
        println!(" Close this window to kill the app completely");
        println!(" Uncheck 'Debug Mode' in settings to disable");
        println!("==========================================");
    }

    let path_clone = path.clone();
    let (osd_tx, osd_rx) = std::sync::mpsc::channel::<(String, f32)>();

    let serial_status: SharedStatus = Arc::new(Mutex::new(SerialStatus::Idle));

    osd::spawn(osd_rx);

    std::thread::spawn(move || { run_volume_logic_loop(path_clone, osd_tx, serial_status); });

    let mut viewport = ViewportBuilder::default()
        .with_title("RVCI Configuration")
        .with_inner_size([600.0, 860.0])
        .with_min_inner_size([620.0, 540.0])
        .with_position([-32000.0, -32000.0])
        .with_visible(true);
    if let Some(icon) = load_window_icon("rvci.ico") {
        viewport = viewport.with_icon(Arc::new(icon));
    }

    let native_options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    let run = eframe::run_native(
        "RVCI",
        native_options,
        Box::new(move |cc| Ok(Box::new(RvciApp::new(cc, path)))),
    );

    if let Err(e) = run {
        return Err(anyhow::anyhow!("eframe error: {e}"));
    }
    Ok(())
}
