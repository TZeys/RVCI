#![windows_subsystem = "windows"]

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
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
    dials: Vec<DialConfig>,
    #[serde(default)]
    buttons: Vec<ButtonConfig>,
}

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

fn set_startup_launch(enable: bool) -> Result<()> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let path = hkcu.open_subkey_with_flags("Software\\Microsoft\\Windows\\CurrentVersion\\Run", KEY_ALL_ACCESS)?;
    if enable {
        let exe_path = std::env::current_exe()?;
        path.set_value("RVSC", &exe_path.to_str().unwrap_or_default())?;
    } else {
        let _ = path.delete_value("RVSC");
    }
    Ok(())
}

fn check_startup_enabled() -> bool {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    if let Ok(path) = hkcu.open_subkey("Software\\Microsoft\\Windows\\CurrentVersion\\Run") {
        if let Ok(val) = path.get_value::<String, _>("RVSC") {
            if let Ok(exe_path) = std::env::current_exe() {
                return val == exe_path.to_str().unwrap_or_default();
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

fn run_volume_logic_loop(config_path: PathBuf, osd_tx: Sender<(String, f32)>) {
    let mut current_config_sig = String::new();
    let mut smoothers: Vec<Smoother> = Vec::new();
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
             if let Err(_) = run_serial_processing(&config, &config_path, &mut smoothers, &osd_tx) {
                std::thread::sleep(Duration::from_secs(2));
             }
        } else {
            std::thread::sleep(Duration::from_secs(2));
        }
    }
}

fn run_serial_processing(config: &AppConfig, config_path: &PathBuf, smoothers: &mut Vec<Smoother>, osd_tx: &Sender<(String, f32)>) -> Result<()> {
    let port = serialport::new(&config.serial.port, config.serial.baud)
        .timeout(Duration::from_millis(config.serial.timeout))
        .open()
        .context("Failed to open serial port")?;

    let mut reader = BufReader::new(port);
    let mut line_buf = String::new();
    let mut last_update = Instant::now();

    let mut last_applied_values: Vec<f32> = vec![-1.0; config.dials.len()];
    let mut last_osd_raw_values: Vec<f32> = vec![-999.0; config.dials.len()];

    
    let mut button_states: Vec<bool> = vec![false; config.buttons.len()];

    let mut pid_name_cache: HashMap<u32, String> = HashMap::new();
    let mut mic_device_cache: HashMap<String, IAudioEndpointVolume> = HashMap::new();
    let mut output_device_cache: HashMap<String, IAudioEndpointVolume> = HashMap::new();
    let mut cache_counter = 0;

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

    unsafe { let _ = CoInitializeEx(None, COINIT_MULTITHREADED); }
    let last_file_mod = std::fs::metadata(config_path).and_then(|m| m.modified()).ok();

    loop {
        if let Ok(meta) = std::fs::metadata(config_path) {
            if let Ok(mod_time) = meta.modified() {
                if Some(mod_time) != last_file_mod { return Ok(()); }
            }
        }

        line_buf.clear();

        match reader.read_line(&mut line_buf) {
            Ok(bytes) if bytes > 0 => {
                let line = line_buf.trim();
                if line.is_empty() { continue; }

                if line == "WORKS 1" {
                    switch_device(&config.work_device_1);
                    continue;
                } else if line == "WORKS 2" {
                    switch_device(&config.work_device_2);
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

                if last_update.elapsed() < Duration::from_millis(25) { continue; }
                last_update = Instant::now();

                cache_counter += 1;
                if cache_counter > 200 {
                    pid_name_cache.clear();
                    mic_device_cache.clear();
                    output_device_cache.clear();
                    cache_counter = 0;
                }

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

                        if (smoothed - last_applied_values[i]).abs() < 0.005 {
                            continue;
                        }

                        last_applied_values[i] = smoothed;

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
                                let _ = osd_tx.send((display_name, smoothed));
                            }
                        }

                        unsafe {
                            match dial_cfg.dial_type.as_str() {
                                "system" => {
                                    if let Ok(vol) = AudioController::get_system_volume() {
                                        let _ = vol.SetMasterVolumeLevelScalar(smoothed, std::ptr::null());
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
                                                let _ = vol.SetMasterVolumeLevelScalar(smoothed, std::ptr::null());
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
                                                let _ = vol.SetMasterVolumeLevelScalar(smoothed, std::ptr::null());
                                            }
                                        }
                                    }
                                },
                                "process" | "all_others" => {
                                    if let Ok(mgr) = AudioController::get_session_manager() {
                                        if let Ok(enum_sess) = mgr.GetSessionEnumerator() {
                                            if let Ok(count) = enum_sess.GetCount() {
                                                for s_idx in 0..count {
                                                    if let Ok(sess) = enum_sess.GetSession(s_idx) {
                                                        if let Ok(s2) = Interface::cast::<IAudioSessionControl2>(&sess) {
                                                            if let Ok(pid) = s2.GetProcessId() {
                                                                if pid == 0 { continue; }

                                                                let pname = pid_name_cache.entry(pid).or_insert_with(|| {
                                                                    AudioController::get_process_name(pid)
                                                                });

                                                                let should_change = if dial_cfg.dial_type == "all_others" {
                                                                    !process_map.contains(&pname.to_lowercase())
                                                                } else {
                                                                    match &dial_cfg.process_name {
                                                                        Some(target) => {
                                                                            let mut clean_target = target.clone();
                                                                            if clean_target.to_lowercase().ends_with(".exe") {
                                                                                clean_target.truncate(clean_target.len() - 4);
                                                                            }
                                                                            pname.to_lowercase() == clean_target.to_lowercase()
                                                                        },
                                                                        None => false,
                                                                    }
                                                                };

                                                                if should_change {
                                                                    if let Ok(simple_vol) = Interface::cast::<ISimpleAudioVolume>(&sess) {
                                                                        let _ = simple_vol.SetMasterVolume(smoothed, std::ptr::null());
                                                                    }
                                                                }
                                                            }
                                                        }
                                                    }
                                                }
                                            }
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


const ACCENT: Color32 = Color32::from_rgb(45, 140, 255);
const ACCENT_HOVER: Color32 = Color32::from_rgb(80, 165, 255);
const DESTRUCTIVE: Color32 = Color32::from_rgb(255, 69, 58);
const SUCCESS: Color32 = Color32::from_rgb(48, 209, 88);
const PANEL_BG: Color32 = Color32::from_rgb(18, 18, 20);
const WIDGET_BG: Color32 = Color32::from_rgb(32, 32, 36);
const ROW_HOVER: Color32 = Color32::from_rgb(40, 40, 46);

const BAUD_RATES: [u32; 5] = [9600, 19200, 38400, 57600, 115200];

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
}

impl RvciApp {
    fn new(cc: &eframe::CreationContext<'_>, config_path: PathBuf) -> Self {
        configure_visuals(&cc.egui_ctx);

        
        let hb_ctx = cc.egui_ctx.clone();
        std::thread::spawn(move || loop {
            std::thread::sleep(Duration::from_millis(120));
            hb_ctx.request_repaint();
        });

        let cfg = if let Ok(file) = File::open(&config_path) {
            serde_json::from_reader(BufReader::new(file)).unwrap_or_default()
        } else {
            AppConfig::default()
        };

        
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

        
        hide_window_native(hwnd);

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
            for ev in &i.events {
                if let egui::Event::Key { key, pressed: true, modifiers, .. } = ev {
                    if *key == egui::Key::Escape || *key == egui::Key::Enter {
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
        egui::CentralPanel::default().show(ctx, |ui| {
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    ui.add_space(4.0);
                    ui.heading(RichText::new("RVCI Config").size(26.0).strong());
                    ui.add_space(8.0);

                    self.section_serial(ui);
                    ui.add_space(6.0);
                    self.section_general(ui);
                    ui.add_space(10.0);
                    self.section_routing(ui);
                    ui.add_space(10.0);
                    self.section_knobs(ui);
                    ui.add_space(10.0);
                    self.section_buttons(ui);
                    ui.add_space(12.0);
                    self.section_footer(ui);
                });
        });
    }

    fn section_serial(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(RichText::new("Serial Port:").strong());
            let cur = self.cfg.serial.port.clone();
            ComboBox::from_id_salt("serial_port")
                .selected_text(if cur.is_empty() { "None".to_string() } else { cur })
                .show_ui(ui, |ui| {
                    let ports = self.com_ports.clone();
                    for p in &ports {
                        ui.selectable_value(&mut self.cfg.serial.port, p.clone(), p);
                    }
                });
            ComboBox::from_id_salt("baud")
                .selected_text(self.cfg.serial.baud.to_string())
                .show_ui(ui, |ui| {
                    for b in BAUD_RATES {
                        ui.selectable_value(&mut self.cfg.serial.baud, b, b.to_string());
                    }
                });
            if accent_button(ui, "Update").clicked() {
                self.rescan();
            }
        });
    }

    fn section_general(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(RichText::new("Max Pot Value:").strong());
            let mut v = self.cfg.value_max;
            if ui.add(egui::DragValue::new(&mut v).speed(1.0).range(1.0..=8192.0)).changed() {
                self.cfg.value_max = v;
            }
        });
        ui.horizontal(|ui| {
            ui.label(RichText::new("Volume Curve:").strong());
            let mut log = self.cfg.use_logarithmic_scale;
            ComboBox::from_id_salt("curve")
                .selected_text(if log { "Logarithmic" } else { "Linear (Default)" })
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut log, false, "Linear (Default)");
                    ui.selectable_value(&mut log, true, "Logarithmic");
                });
            self.cfg.use_logarithmic_scale = log;
        });
    }

    fn section_routing(&mut self, ui: &mut egui::Ui) {
        ui.heading(RichText::new("Audio Routing").size(18.0).strong());
        let devices = self.playback_devices.clone();
        for (n, label) in [(1u8, "Output 1:"), (2u8, "Output 2:")] {
            ui.horizontal(|ui| {
                ui.label(RichText::new(label).strong());
                let stored = if n == 1 { self.cfg.work_device_1.clone() } else { self.cfg.work_device_2.clone() };
                let display = routing_display(&stored, &devices);
                ComboBox::from_id_salt(format!("wd{}", n))
                    .selected_text(display)
                    .show_ui(ui, |ui| {
                        let target = if n == 1 { &mut self.cfg.work_device_1 } else { &mut self.cfg.work_device_2 };
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
    }

    fn section_knobs(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading(RichText::new("Knob Mappings").size(18.0).strong());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if accent_button(ui, "+ Add Knob").clicked() {
                    self.cfg.dials.push(DialConfig {
                        dial_type: "system".to_string(),
                        process_name: None,
                        inverted: false,
                    });
                }
            });
        });

        let processes = self.active_processes.clone();
        let captures = self.capture_devices.clone();
        let playbacks = self.playback_devices.clone();

        let mut remove: Option<usize> = None;
        let len = self.cfg.dials.len();
        for i in 0..len {
            ui.push_id(("knob", i), |ui| {
                hover_row(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(format!("{}:", i + 1)).strong());

                        
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
                        let has_target = matches!(dtype.as_str(), "process" | "microphone" | "output_device");
                        ui.add_enabled_ui(has_target, |ui| {
                            let cur = self.cfg.dials[i].process_name.clone().unwrap_or_else(|| "None".to_string());
                            ComboBox::from_id_salt("ktarget")
                                .selected_text(if cur.is_empty() { "None".to_string() } else { cur.clone() })
                                .width(180.0)
                                .show_ui(ui, |ui| {
                                    let mut sel = cur.clone();
                                    let changed = {
                                        let mut c = ui.selectable_value(&mut sel, "None".to_string(), "None").changed();
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

                        ui.checkbox(&mut self.cfg.dials[i].inverted, "Inv");

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
            self.cfg.dials.remove(idx);
        }
    }

    fn section_buttons(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading(RichText::new("Button Mappings").size(18.0).strong());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if accent_button(ui, "+ Add Button").clicked() {
                    self.cfg.buttons.push(ButtonConfig::default());
                }
            });
        });

        let dial_count = self.cfg.dials.len();
        let mut remove: Option<usize> = None;
        let len = self.cfg.buttons.len();
        for i in 0..len {
            ui.push_id(("btn", i), |ui| {
                hover_row(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(format!("Button {}:", i + 1)).strong());

                        
                        let cur_action = self.cfg.buttons[i].action.clone();
                        ComboBox::from_id_salt("baction")
                            .selected_text(action_label(&cur_action))
                            .width(110.0)
                            .show_ui(ui, |ui| {
                                let a = &mut self.cfg.buttons[i].action;
                                ui.selectable_value(a, "none".to_string(), "None");
                                ui.selectable_value(a, "mute_dial".to_string(), "Mute Knob");
                                ui.selectable_value(a, "media".to_string(), "Media");
                                ui.selectable_value(a, "keys".to_string(), "Keys");
                            });

                        
                        let action = self.cfg.buttons[i].action.clone();
                        match action.as_str() {
                            "mute_dial" => {
                                if dial_count == 0 {
                                    ui.add_enabled(false, egui::Button::new("(no knobs)"));
                                } else {
                                    let di = self.cfg.buttons[i].dial_index.min(dial_count - 1);
                                    self.cfg.buttons[i].dial_index = di;
                                    ComboBox::from_id_salt("bknob")
                                        .selected_text(format!("Knob {}", di + 1))
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
                                let cur_idx = MEDIA_TOKENS.iter().position(|t| *t == cur_tok).unwrap_or(0);
                                ComboBox::from_id_salt("bmedia")
                                    .selected_text(MEDIA_LABELS[cur_idx])
                                    .width(150.0)
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
                                    "Press keys...".to_string()
                                } else {
                                    format_combo(&self.cfg.buttons[i].modifiers, &self.cfg.buttons[i].key_combo)
                                };
                                let btn = egui::Button::new(label)
                                    .fill(if capturing { ACCENT } else { WIDGET_BG })
                                    .min_size(egui::vec2(170.0, 0.0));
                                if ui.add(btn).clicked() {
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
            
            if self.key_capture == Some(idx) { self.key_capture = None; }
            self.cfg.buttons.remove(idx);
        }
    }

    fn section_footer(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.checkbox(&mut self.startup_enabled, "Launch at Startup");
            ui.checkbox(&mut self.cfg.debug_mode, "Debug Mode");
            ui.checkbox(&mut self.cfg.enable_osd, "Show OSD");
        });
        ui.add_space(6.0);
        ui.separator();
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            if ui.add(egui::Button::new("Close").min_size(egui::vec2(110.0, 30.0))).clicked() {
                ui.ctx().send_viewport_cmd(ViewportCommand::Visible(false));
            }

            
            let (label, fill) = match self.save_flash {
                Some((t, true)) if t.elapsed() < Duration::from_millis(1200) => {
                    ("Saved (ok)".to_string(), SUCCESS)
                }
                Some((t, false)) if t.elapsed() < Duration::from_millis(1800) => {
                    ("Save Failed".to_string(), DESTRUCTIVE)
                }
                _ => ("Save Changes".to_string(), ACCENT),
            };
            let save = egui::Button::new(RichText::new(label).strong().color(Color32::WHITE))
                .fill(fill)
                .min_size(egui::vec2(160.0, 30.0));
            if ui.add(save).clicked() {
                self.save();
            }

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(RichText::new("Made by TZey").size(13.0).color(Color32::from_gray(120)));
            });
        });
    }

}

impl eframe::App for RvciApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        
        ctx.request_repaint_after(Duration::from_millis(100));

        
        if !self.user_opened.load(Ordering::SeqCst) {
            hide_window_native(self.hwnd);
            ctx.request_repaint_after(Duration::from_millis(30));
        }

        
        if self.want_show.swap(false, Ordering::SeqCst) {
            self.rescan();
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
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{
        IsIconic, SetForegroundWindow, ShowWindow, SW_RESTORE, SW_SHOW,
    };
    let hwnd = HWND(hwnd as *mut c_void);
    unsafe {
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
    let mut visuals = egui::Visuals::dark();
    visuals.panel_fill = PANEL_BG;
    visuals.window_fill = PANEL_BG;
    visuals.extreme_bg_color = WIDGET_BG;
    visuals.widgets.inactive.bg_fill = WIDGET_BG;
    visuals.widgets.inactive.weak_bg_fill = WIDGET_BG;
    visuals.widgets.hovered.bg_fill = ROW_HOVER;
    visuals.widgets.hovered.weak_bg_fill = ROW_HOVER;
    visuals.selection.bg_fill = ACCENT;
    visuals.hyperlink_color = ACCENT;
    let rounding = egui::Rounding::same(8.0);
    visuals.widgets.inactive.rounding = rounding;
    visuals.widgets.hovered.rounding = rounding;
    visuals.widgets.active.rounding = rounding;
    visuals.widgets.open.rounding = rounding;
    visuals.window_rounding = egui::Rounding::same(10.0);
    ctx.set_visuals(visuals);

    let mut style = (*ctx.style()).clone();
    style.spacing.item_spacing = egui::vec2(8.0, 8.0);
    style.spacing.button_padding = egui::vec2(10.0, 6.0);
    ctx.set_style(style);
}


fn accent_button(ui: &mut egui::Ui, text: &str) -> egui::Response {
    let resp = ui.add(
        egui::Button::new(RichText::new(text).strong().color(Color32::WHITE))
            .fill(ACCENT),
    );
    if resp.hovered() {
        let t = ui.ctx().animate_bool(resp.id, true);
        let tint = lerp_color(ACCENT, ACCENT_HOVER, t);
        ui.painter().rect_filled(resp.rect, egui::Rounding::same(8.0), tint);
        
        ui.painter().text(
            resp.rect.center(),
            egui::Align2::CENTER_CENTER,
            text,
            egui::FontId::proportional(14.0),
            Color32::WHITE,
        );
    } else {
        ui.ctx().animate_bool(resp.id, false);
    }
    resp
}

fn delete_button(ui: &mut egui::Ui) -> egui::Response {
    ui.add(
        egui::Button::new(RichText::new("X").strong().color(Color32::WHITE))
            .fill(DESTRUCTIVE)
            .min_size(egui::vec2(28.0, 24.0)),
    )
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
            Color32::from_rgba_unmultiplied(ROW_HOVER.r(), ROW_HOVER.g(), ROW_HOVER.b(), (t * 90.0) as u8),
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
            
            let mut got: Option<(String, f32)> = None;
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

            std::thread::sleep(Duration::from_millis(30));
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

    
    osd::spawn(osd_rx);

    std::thread::spawn(move || { run_volume_logic_loop(path_clone, osd_tx); });

    let mut viewport = ViewportBuilder::default()
        .with_title("RVCI Configuration")
        .with_inner_size([600.0, 860.0])
        .with_min_inner_size([440.0, 480.0])
        .with_visible(false);
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
