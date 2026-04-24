#![windows_subsystem = "windows"]

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use std::collections::HashMap;
use std::ffi::c_void;

//fltk imports
use fltk::{
    app,
    button::{Button, CheckButton},
    enums::{Color, FrameType, Font},
    frame::Frame,
    group::{Flex, Pack, Scroll},
    menu::Choice,
    misc::Progress,
    input::{Input, IntInput}, 
    prelude::*,
    window::Window,
    image::RgbImage, 
};

//winreg bs
use winreg::enums::*;
use winreg::RegKey;

//tray icons
use tray_icon::{
    menu::{Menu, MenuItem, MenuEvent},
    TrayIconBuilder, Icon, TrayIconEvent, MouseButton,
};

//WAPI imports
use windows::core::{Interface, interface, GUID, PCWSTR, PCSTR, IUnknown, IUnknown_Vtbl}; 
use windows::Win32::Foundation::CloseHandle;
use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
use windows::Win32::Media::Audio::*; 
use windows::Win32::System::Com::*; 
use windows::Win32::System::ProcessStatus::GetModuleBaseNameW;
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_QUERY_INFORMATION, PROCESS_VM_READ,
};
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::System::Console::AllocConsole;

// ==========================================
// DIRECT WINDOWS API HOOKS
// ==========================================

#[link(name = "dwmapi")]
extern "system" {
    fn DwmSetWindowAttribute(
        hwnd: *mut c_void,
        dwAttribute: u32,
        pvAttribute: *const c_void,
        cbAttribute: u32,
    ) -> i32;
}

const DWMWA_USE_IMMERSIVE_DARK_MODE_WIN11: u32 = 20;
const DWMWA_USE_IMMERSIVE_DARK_MODE_WIN10: u32 = 19;
const DWMWA_WINDOW_CORNER_PREFERENCE: u32 = 33;
const DWMWCP_ROUND: u32 = 2; 

#[link(name = "user32")]
extern "system" {
    fn SetWindowPos(hWnd: *mut c_void, hWndInsertAfter: *mut c_void, X: i32, Y: i32, cx: i32, cy: i32, uFlags: u32) -> i32;
    fn SetLayeredWindowAttributes(hwnd: *mut c_void, crKey: u32, bAlpha: u8, dwFlags: u32) -> i32;

    #[cfg(target_arch = "x86_64")]
    fn GetWindowLongPtrW(hWnd: *mut c_void, nIndex: i32) -> isize;
    #[cfg(target_arch = "x86_64")]
    fn SetWindowLongPtrW(hWnd: *mut c_void, nIndex: i32, dwNewLong: isize) -> isize;

    #[cfg(target_arch = "x86")]
    fn GetWindowLongW(hWnd: *mut c_void, nIndex: i32) -> isize;
    #[cfg(target_arch = "x86")]
    fn SetWindowLongW(hWnd: *mut c_void, nIndex: i32, dwNewLong: isize) -> isize;
}

#[cfg(target_arch = "x86_64")]
unsafe fn get_win_long(hwnd: *mut c_void, idx: i32) -> isize { GetWindowLongPtrW(hwnd, idx) }
#[cfg(target_arch = "x86_64")]
unsafe fn set_win_long(hwnd: *mut c_void, idx: i32, val: isize) -> isize { SetWindowLongPtrW(hwnd, idx, val) }

#[cfg(target_arch = "x86")]
unsafe fn get_win_long(hwnd: *mut c_void, idx: i32) -> isize { GetWindowLongW(hwnd, idx) }
#[cfg(target_arch = "x86")]
unsafe fn set_win_long(hwnd: *mut c_void, idx: i32, val: i32) -> isize { SetWindowLongW(hwnd, idx, val as isize) }

const HWND_TOPMOST: isize = -1;
const SWP_NOSIZE: u32 = 0x0001;
const SWP_NOMOVE: u32 = 0x0002;
const GWL_EXSTYLE: i32 = -20;
const WS_EX_LAYERED: isize = 0x00080000;
const WS_EX_TRANSPARENT: isize = 0x00000020;
const LWA_COLORKEY: u32 = 0x00000001;
const LWA_ALPHA: u32 = 0x00000002;

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

    unsafe fn get_mic_volume(mic_name: &str) -> Result<IAudioEndpointVolume> {
        let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let collection = enumerator.EnumAudioEndpoints(eCapture, DEVICE_STATE_ACTIVE)?;
        let count = collection.GetCount()?;
        for i in 0..count {
            if let Ok(item) = collection.Item(i) {
                if let Ok(store) = item.OpenPropertyStore(STGM_READ) {
                    if let Ok(prop) = store.GetValue(&PKEY_Device_FriendlyName) {
                        let pwsz = prop.Anonymous.Anonymous.Anonymous.pwszVal;
                        if !pwsz.is_null() {
                            let name = pwsz.to_string().unwrap_or_default();
                            if name.to_lowercase() == mic_name.to_lowercase() {
                                return item.Activate::<IAudioEndpointVolume>(CLSCTX_ALL, None).map_err(anyhow::Error::from);
                            }
                        }
                    }
                }
            }
        }
        Err(anyhow::anyhow!("Microphone not found"))
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
    println!("DEBUG: Attempting to switch output device to -> '{}'", clean_name);

    let all_devices = AudioScanner::get_playback_devices_with_ids();
    let match_result = all_devices.iter()
        .find(|(name, _id)| name.to_lowercase().contains(&clean_name.to_lowercase()));

    if let Some((found_name, real_id)) = match_result {
        println!("DEBUG: Found matching device: '{}' (ID: {})", found_name, real_id);
        unsafe {
            if let Ok(policy) = CoCreateInstance::<_, IPolicyConfig>(&CLSID_PolicyConfigClient, None, CLSCTX_ALL) {
                let mut id_utf16: Vec<u16> = real_id.encode_utf16().collect();
                id_utf16.push(0); 
                let pcwstr_id = PCWSTR(id_utf16.as_ptr());

                let _ = policy.SetDefaultEndpoint(pcwstr_id, eConsole);
                let _ = policy.SetDefaultEndpoint(pcwstr_id, eMultimedia);
                let _ = policy.SetDefaultEndpoint(pcwstr_id, eCommunications);
                println!("DEBUG: Successfully switched to '{}'", found_name);
            } else {
                println!("ERROR: Failed to instantiate IPolicyConfig COM object.");
            }
        }
    } else {
        println!("ERROR: Could not find playback device matching '{}'", clean_name);
    }
}

fn run_volume_logic_loop(config_path: PathBuf, osd_tx: app::Sender<(String, f32)>) {
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
                println!("DEBUG: Serial configuration updated. Target: {} @ {}", config.serial.port, config.serial.baud);
                smoothers = (0..config.dials.len()).map(|_| Smoother::new()).collect();
            }
             if let Err(e) = run_serial_processing(&config, &config_path, &mut smoothers, &osd_tx) {
                println!("DEBUG: Serial Connection Error: {}. Retrying in 2 seconds...", e);
                std::thread::sleep(Duration::from_secs(2));
             }
        } else {
            std::thread::sleep(Duration::from_secs(2));
        }
    }
}

fn run_serial_processing(config: &AppConfig, config_path: &PathBuf, smoothers: &mut Vec<Smoother>, osd_tx: &app::Sender<(String, f32)>) -> Result<()> {
    let port = serialport::new(&config.serial.port, config.serial.baud)
        .timeout(Duration::from_millis(config.serial.timeout))
        .open()
        .context("Failed to open serial port")?;
    
    println!("DEBUG: Connected to serial port successfully.");
    
    let mut reader = BufReader::new(port);
    let mut line_buf = String::new();
    let mut last_update = Instant::now();
    
    let mut last_applied_values: Vec<f32> = vec![-1.0; config.dials.len()];

    let mut pid_name_cache: HashMap<u32, String> = HashMap::new();
    let mut mic_device_cache: HashMap<String, IAudioEndpointVolume> = HashMap::new();
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

                if last_update.elapsed() < Duration::from_millis(25) { continue; }
                last_update = Instant::now();

                cache_counter += 1;
                if cache_counter > 200 {
                    pid_name_cache.clear();
                    mic_device_cache.clear(); 
                    cache_counter = 0;
                }

                let parts: Vec<&str> = line.split('|').collect();
                if parts.len() != config.dials.len() { continue; }

                for (i, part) in parts.iter().enumerate() {
                    if let Ok(raw_val) = part.parse::<f32>() {
                        let dial_cfg = &config.dials[i];
                        
                        let mut normalized = raw_val.clamp(0.0, config.value_max) / config.value_max;
                        
                        if dial_cfg.inverted {
                            normalized = 1.0 - normalized;
                        }

                        if config.use_logarithmic_scale {
                            normalized = normalized.powf(3.0);
                        }
                        
                        if i >= smoothers.len() { smoothers.push(Smoother::new()); }
                        if i >= last_applied_values.len() { last_applied_values.push(-1.0); }

                        let smoothed = smoothers[i].process(normalized);
                        
                        if (smoothed - last_applied_values[i]).abs() < 0.005 {
                            continue;
                        }
                        
                        last_applied_values[i] = smoothed;
                        
                        let target_lbl = dial_cfg.process_name.as_deref().unwrap_or("Unassigned");
                        println!("DEBUG: [Knob {}] {} ({}) -> {:.3}", i + 1, dial_cfg.dial_type, target_lbl, smoothed);

                        if config.enable_osd {
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
                                osd_tx.send((display_name, smoothed));
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
            _ => {
                std::thread::sleep(Duration::from_millis(10));
                continue;
            }
        }
    }
}

// ==========================================
// MODERN iOS-STYLE GUI RENDERING & THEME
// ==========================================

const WIDGET_BG: Color = Color::from_rgb(28, 28, 30);             // Secondary Grouped Background
const WIDGET_HOVER: Color = Color::from_rgb(44, 44, 46);          // Tertiary Hover State
const TEXT_COLOR: Color = Color::from_rgb(255, 255, 255);         // Pure White
const ACCENT_COLOR: Color = Color::from_rgb(10, 132, 255);        // iOS Dark Mode Blue
const ACCENT_HOVER: Color = Color::from_rgb(64, 156, 255);        // Lighter iOS Blue
const DESTRUCTIVE_COLOR: Color = Color::from_rgb(255, 69, 58);    // iOS Red
const DESTRUCTIVE_HOVER: Color = Color::from_rgb(255, 105, 97);   // Lighter iOS Red

fn style_widget<W: WidgetExt>(w: &mut W) {
    w.set_color(WIDGET_BG);
    w.set_label_color(TEXT_COLOR);
    w.set_frame(FrameType::RFlatBox); 
    w.set_selection_color(WIDGET_HOVER);
    w.clear_visible_focus(); 
}

fn style_choice(w: &mut Choice) {
    w.set_color(WIDGET_BG);
    w.set_text_color(TEXT_COLOR);
    w.set_frame(FrameType::RFlatBox);
    w.set_selection_color(WIDGET_HOVER);
    w.clear_visible_focus();
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

fn load_win_icon(filename: &str) -> Option<RgbImage> {
    let path = get_exe_dir().join(filename);
    if let Ok(img) = image::open(&path) {
        let rgba = img.into_rgba8();
        let (w, h) = rgba.dimensions();
        return RgbImage::new(&rgba.into_raw(), w as i32, h as i32, fltk::enums::ColorDepth::Rgba8).ok();
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

fn refresh_knobs_ui(scroll_pack: &mut Pack, dials: &Vec<DialConfig>, active_processes: &[String], capture_devices: &[String]) {
    scroll_pack.clear(); 
    scroll_pack.begin();
    
    let scroll_w = scroll_pack.w();

    for (i, dial) in dials.iter().enumerate() {
        let mut row = Flex::default().with_size(scroll_w, 40).row(); 
        row.set_pad(10);
        row.set_frame(FrameType::NoBox); 
        
        let mut lbl = Frame::default().with_label(&format!("{}:", i + 1));
        lbl.set_label_color(TEXT_COLOR);
        lbl.set_label_font(Font::HelveticaBold); 
        
        let mut choice_type = Choice::default();
        style_choice(&mut choice_type);
        choice_type.add_choice("System|Process|Others|Microphone");
        
        let sel_idx = match dial.dial_type.as_str() { 
            "process" => 1, 
            "all_others" => 2, 
            "microphone" => 3,
            _ => 0 
        };
        choice_type.set_value(sel_idx);
        
        let mut choice_proc = Choice::default();
        style_choice(&mut choice_proc);

        let mut available_choices = match dial.dial_type.as_str() {
            "process" => active_processes.to_vec(),
            "microphone" => capture_devices.to_vec(),
            _ => Vec::new(),
        };

        if let Some(pname) = &dial.process_name {
            let mut clean_pname = pname.clone();
            if clean_pname.to_lowercase().ends_with(".exe") {
                clean_pname.truncate(clean_pname.len() - 4);
            }
            if !clean_pname.is_empty() && clean_pname != "None" && !available_choices.contains(&clean_pname) {
                available_choices.push(clean_pname.clone());
            }
        }
        available_choices.sort();
        available_choices.insert(0, "None".to_string());

        for p in &available_choices { choice_proc.add_choice(p); }

        if dial.dial_type == "process" || dial.dial_type == "microphone" {
            choice_proc.activate();
            let mut target = dial.process_name.clone().unwrap_or_else(|| "None".to_string());
            if target.to_lowercase().ends_with(".exe") {
                target.truncate(target.len() - 4);
            }
            
            if let Some(idx) = available_choices.iter().position(|x| x.to_lowercase() == target.to_lowercase()) {
                choice_proc.set_value(idx as i32);
            } else {
                choice_proc.set_value(0); 
            }
        } else {
            choice_proc.deactivate();
            choice_proc.set_color(Color::from_rgb(20, 20, 22)); 
            choice_proc.set_value(0); 
        }

        let mut cp_clone = choice_proc.clone();
        let active_procs_clone = active_processes.to_vec();
        let capture_devices_clone = capture_devices.to_vec();
        
        choice_type.set_callback(move |c| {
            if c.value() == 1 { 
                cp_clone.activate();
                cp_clone.set_color(WIDGET_BG);
                cp_clone.clear();
                cp_clone.add_choice("None");
                for p in &active_procs_clone { cp_clone.add_choice(p); }
                if cp_clone.value() < 0 { cp_clone.set_value(0); }
            } else if c.value() == 3 {
                cp_clone.activate();
                cp_clone.set_color(WIDGET_BG);
                cp_clone.clear();
                cp_clone.add_choice("None");
                for p in &capture_devices_clone { cp_clone.add_choice(p); }
                if cp_clone.value() < 0 { cp_clone.set_value(0); }
            } else {
                cp_clone.deactivate();
                cp_clone.set_color(Color::from_rgb(20, 20, 22));
                cp_clone.set_value(0);
            }
        });

        let mut check_inv = CheckButton::default().with_label("Inv");
        check_inv.set_color(WIDGET_BG); 
        check_inv.set_label_color(TEXT_COLOR);
        check_inv.set_value(dial.inverted);
        check_inv.clear_visible_focus();

        let mut btn_del = Button::default().with_label("X");
        style_widget(&mut btn_del);
        btn_del.set_color(DESTRUCTIVE_COLOR);
        btn_del.set_selection_color(DESTRUCTIVE_HOVER);
        btn_del.set_label_color(Color::White);
        btn_del.set_label_font(Font::HelveticaBold);
        
        row.end();
        
        row.fixed(&lbl, 25);
        row.fixed(&check_inv, 45);
        row.fixed(&btn_del, 35);
        
        let mut sp = scroll_pack.clone();
        let r = row.clone(); 
        btn_del.set_callback(move |_| {
            sp.remove(&r);
            sp.redraw();
            if let Some(mut p) = sp.parent() { p.redraw(); }
        });
    }
    scroll_pack.end();

    let total_height = dials.len() as i32 * 50; 
    scroll_pack.set_size(scroll_w, total_height);

    scroll_pack.redraw();
    if let Some(mut parent) = scroll_pack.parent() { parent.redraw(); }
}

fn populate_choice(choice: &mut Choice, items: &[String], selected_clean: &str, allow_none: bool) {
    choice.clear();
    if allow_none { choice.add_choice("None"); }
    for item in items { choice.add_choice(item); }
    if selected_clean == "None" && allow_none { choice.set_value(0); return; }
    let offset = if allow_none { 1 } else { 0 };
    if let Some(idx) = items.iter().position(|x| x == selected_clean) {
        choice.set_value((idx + offset) as i32);
    } else if let Some(idx) = items.iter().position(|x| x.contains(selected_clean)) {
        choice.set_value((idx + offset) as i32);
    } else if allow_none { choice.set_value(0); }
}

unsafe fn apply_main_window_theme(hwnd: *mut c_void) {
    let preference: u32 = DWMWCP_ROUND;
    DwmSetWindowAttribute(hwnd, DWMWA_WINDOW_CORNER_PREFERENCE, &preference as *const u32 as *const c_void, 4);
    
    let dark_mode: u32 = 1;
    DwmSetWindowAttribute(hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE_WIN11, &dark_mode as *const u32 as *const c_void, 4);
    DwmSetWindowAttribute(hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE_WIN10, &dark_mode as *const u32 as *const c_void, 4);
}


fn build_gui_and_run(config_path: PathBuf, osd_rx: app::Receiver<(String, f32)>) -> Result<()> {
    let app = app::App::default();
    
    app::set_scheme(app::Scheme::Base);
    app::set_visible_focus(false); 
    
    app::background(0, 0, 0);                 
    app::background2(28, 28, 30);             
    app::foreground(255, 255, 255);           
    
    app::set_font(Font::Helvetica);
    app::set_font_size(14); 

    let taskbar_icon = load_win_icon("rvci.ico");
    let open_item = MenuItem::new("Open Settings", true, None);
    let quit_item = MenuItem::new("Quit", true, None);
    let open_id = open_item.id().clone();
    let quit_id = quit_item.id().clone();
    
    let tray_menu = Menu::new();
    let _ = tray_menu.append(&open_item);
    let _ = tray_menu.append(&quit_item);

    let _tray_icon = TrayIconBuilder::new()
        .with_menu(Box::new(tray_menu))
        .with_tooltip("RVCI")
        .with_icon(load_tray_icon("rvci.ico"))
        .build()?;

    let mut win = Window::default().with_size(600, 970).with_label("RVCI");
    win.make_resizable(true);
    win.size_range(440, 640, 0, 0); 
    
    if let Some(ref ico) = taskbar_icon { win.set_icon(Some(ico.clone())); }
    win.set_frame(FrameType::FlatBox);
    win.set_color(Color::Black); 
    
    let mut col = Flex::default_fill().column();
    col.set_frame(FrameType::FlatBox);
    col.set_color(Color::Black); 
    col.set_margin(30); 
    col.set_pad(20);    

    let mut title = Frame::default().with_label("RVCI Config"); 
    title.set_label_size(28); 
    title.set_label_color(TEXT_COLOR);
    title.set_label_font(Font::HelveticaBold);
    title.set_align(fltk::enums::Align::Left | fltk::enums::Align::Inside);

    let label_w = 125; 

    let mut row_serial = Flex::default().row();
    row_serial.set_frame(FrameType::NoBox); 
    row_serial.set_pad(10);
    let mut lbl_port = Frame::default().with_label("Serial Port:");
    lbl_port.set_label_color(TEXT_COLOR);
    lbl_port.set_align(fltk::enums::Align::Left | fltk::enums::Align::Inside);
    
    let mut choice_port = Choice::default();
    style_choice(&mut choice_port);
    let mut choice_baud = Choice::default();
    style_choice(&mut choice_baud);
    for baud in [9600, 19200, 38400, 57600, 115200] { choice_baud.add_choice(&baud.to_string()); }
    
    let mut btn_scan = Button::default().with_label("Update");
    style_widget(&mut btn_scan);
    btn_scan.set_label_font(Font::HelveticaBold);
    btn_scan.set_color(ACCENT_COLOR);
    btn_scan.set_selection_color(ACCENT_HOVER);
    
    row_serial.end();
    let _ = row_serial.fixed(&lbl_port, label_w);
    let _ = row_serial.fixed(&choice_baud, 95);
    let _ = row_serial.fixed(&btn_scan, 80);

    let mut row_max_val = Flex::default().row();
    row_max_val.set_frame(FrameType::NoBox);
    row_max_val.set_pad(10);
    
    let mut lbl_max_val = Frame::default().with_label("Max Pot Value:");
    lbl_max_val.set_label_color(TEXT_COLOR);
    lbl_max_val.set_align(fltk::enums::Align::Left | fltk::enums::Align::Inside);
    
    let mut fake_bubble = Flex::default().row();
    fake_bubble.set_frame(FrameType::RFlatBox);
    fake_bubble.set_color(WIDGET_BG);

    fake_bubble.set_margin(2); 
    
    let mut spacer_left = Button::default();
    spacer_left.set_frame(FrameType::NoBox);

    let mut input_max_val = IntInput::default();
    input_max_val.set_frame(FrameType::FlatBox); 
    input_max_val.set_color(WIDGET_BG); 
    input_max_val.set_text_color(TEXT_COLOR);
    input_max_val.set_cursor_color(Color::White); 
    input_max_val.set_selection_color(WIDGET_HOVER);

    let mut spacer_right = Button::default();
    spacer_right.set_frame(FrameType::NoBox);
    
    fake_bubble.end();
    
    let mut ic1 = input_max_val.clone();
    spacer_left.set_callback(move |_| { let _ = ic1.take_focus(); });
    let mut ic2 = input_max_val.clone();
    spacer_right.set_callback(move |_| { let _ = ic2.take_focus(); });
    
    let _ = fake_bubble.fixed(&input_max_val, 50); 
    
    row_max_val.end();
    let _ = row_max_val.fixed(&lbl_max_val, label_w);

    let mut row_curve = Flex::default().row();
    row_curve.set_frame(FrameType::NoBox);
    row_curve.set_pad(10);
    let mut lbl_curve = Frame::default().with_label("Volume Curve:");
    lbl_curve.set_label_color(TEXT_COLOR);
    lbl_curve.set_align(fltk::enums::Align::Left | fltk::enums::Align::Inside);
    let mut choice_curve = Choice::default();
    style_choice(&mut choice_curve);
    choice_curve.add_choice("Linear (Default)|Logarithmic");
    row_curve.end();
    let _ = row_curve.fixed(&lbl_curve, label_w);

    let mut lbl_switcher = Frame::default().with_label("Audio Routing");
    lbl_switcher.set_label_color(TEXT_COLOR);
    lbl_switcher.set_label_size(18);
    lbl_switcher.set_label_font(Font::HelveticaBold);
    lbl_switcher.set_align(fltk::enums::Align::Left | fltk::enums::Align::Inside);

    let mut row_wd1 = Flex::default().row();
    row_wd1.set_frame(FrameType::NoBox);
    row_wd1.set_pad(10);
    let mut lbl_wd1 = Frame::default().with_label("Output 1:");
    lbl_wd1.set_label_color(TEXT_COLOR);
    lbl_wd1.set_align(fltk::enums::Align::Left | fltk::enums::Align::Inside);
    let mut choice_wd1 = Choice::default();
    style_choice(&mut choice_wd1);
    let mut btn_test1 = Button::default().with_label("Test");
    style_widget(&mut btn_test1);
    row_wd1.end();
    let _ = row_wd1.fixed(&lbl_wd1, label_w);
    let _ = row_wd1.fixed(&btn_test1, 70);

    let mut row_wd2 = Flex::default().row();
    row_wd2.set_frame(FrameType::NoBox);
    row_wd2.set_pad(10);
    let mut lbl_wd2 = Frame::default().with_label("Output 2:");
    lbl_wd2.set_label_color(TEXT_COLOR);
    lbl_wd2.set_align(fltk::enums::Align::Left | fltk::enums::Align::Inside);
    let mut choice_wd2 = Choice::default();
    style_choice(&mut choice_wd2);
    let mut btn_test2 = Button::default().with_label("Test");
    style_widget(&mut btn_test2);
    row_wd2.end();
    let _ = row_wd2.fixed(&lbl_wd2, label_w);
    let _ = row_wd2.fixed(&btn_test2, 70);

    let mut row_knobs_header = Flex::default().row();
    row_knobs_header.set_frame(FrameType::NoBox);
    row_knobs_header.set_pad(10);
    let mut lbl_knobs = Frame::default().with_label("Knob Mappings");
    lbl_knobs.set_label_color(TEXT_COLOR);
    lbl_knobs.set_label_size(18);
    lbl_knobs.set_label_font(Font::HelveticaBold);
    lbl_knobs.set_align(fltk::enums::Align::Left | fltk::enums::Align::Inside);
    let mut btn_add = Button::default().with_label("+ Add Knob");
    style_widget(&mut btn_add);
    btn_add.set_label_font(Font::HelveticaBold);
    btn_add.set_color(ACCENT_COLOR);
    btn_add.set_selection_color(ACCENT_HOVER);
    row_knobs_header.end();
    let _ = row_knobs_header.fixed(&btn_add, 110);

    let mut scroll = Scroll::default();
    scroll.set_type(fltk::group::ScrollType::VerticalAlways); 
    scroll.set_scrollbar_size(12); 
    scroll.set_frame(FrameType::FlatBox);
    scroll.set_color(Color::Black); 

    let mut scroll_pack = Pack::default().with_size(460, 0); 
    scroll_pack.set_type(fltk::group::PackType::Vertical);
    scroll_pack.set_spacing(10);
    scroll_pack.set_frame(FrameType::NoBox); 
    
    scroll_pack.handle(|sp, ev| {
        if ev == fltk::enums::Event::Resize {
            let w = sp.w();
            for i in 0..sp.children() {
                if let Some(mut child) = sp.child(i) {
                    child.resize(child.x(), child.y(), w, child.h());
                }
            }
        }
        false
    });

    scroll_pack.end();
    scroll.end();

    let mut row_footer1 = Flex::default().row(); 
    row_footer1.set_frame(FrameType::NoBox);
    let mut check_startup = CheckButton::default().with_label(" Launch at Startup");
    check_startup.set_color(WIDGET_BG); 
    check_startup.set_label_color(TEXT_COLOR);
    check_startup.clear_visible_focus();
    if check_startup_enabled() { check_startup.set_value(true); }
    
    let mut check_debug = CheckButton::default().with_label(" Debug Mode");
    check_debug.set_color(WIDGET_BG);
    check_debug.set_label_color(TEXT_COLOR);
    check_debug.clear_visible_focus();

    let mut check_osd = CheckButton::default().with_label("Show OSD");
    check_osd.set_color(WIDGET_BG);
    check_osd.set_label_color(TEXT_COLOR);
    check_osd.clear_visible_focus();
    row_footer1.end();
    let _ = row_footer1.fixed(&check_startup, 160);
    let _ = row_footer1.fixed(&check_debug, 120);
    let _ = row_footer1.fixed(&check_osd, 110);

    let mut row_btns = Flex::default().row(); 
    row_btns.set_frame(FrameType::NoBox);
    row_btns.set_pad(15);
    
    let mut btn_cancel = Button::default().with_label("Close");
    style_widget(&mut btn_cancel);
    btn_cancel.set_label_font(Font::HelveticaBold);
    
    let mut btn_apply = Button::default().with_label("Save Changes"); 
    style_widget(&mut btn_apply);
    btn_apply.set_color(ACCENT_COLOR); 
    btn_apply.set_selection_color(ACCENT_HOVER); 
    btn_apply.set_label_font(Font::HelveticaBold);
    
    row_btns.end();
    let _ = row_btns.fixed(&btn_cancel, 120);

    let mut row_footer2 = Flex::default().row();
    row_footer2.set_frame(FrameType::NoBox);
    let mut lbl_author = Frame::default().with_label("Made by TZey");
    lbl_author.set_label_color(Color::from_rgb(100, 100, 105)); 
    lbl_author.set_label_size(13);
    lbl_author.set_label_font(Font::Helvetica);
    row_footer2.end();

    col.end();
    let _ = col.fixed(&title, 45);
    let _ = col.fixed(&row_serial, 40);
    let _ = col.fixed(&row_max_val, 40); 
    let _ = col.fixed(&row_curve, 40);
    let _ = col.fixed(&lbl_switcher, 35);
    let _ = col.fixed(&row_wd1, 40);
    let _ = col.fixed(&row_wd2, 40);
    let _ = col.fixed(&row_knobs_header, 35);
    let _ = col.fixed(&row_footer1, 35);
    let _ = col.fixed(&row_btns, 50); 
    let _ = col.fixed(&row_footer2, 20);

    win.end();
    win.hide(); 
    
    let mut osd_bg_win = Window::default().with_size(240, 70);
    osd_bg_win.set_border(false);
    osd_bg_win.set_override(); 
    osd_bg_win.set_frame(FrameType::FlatBox); 
    osd_bg_win.set_color(Color::Black);       
    osd_bg_win.end();

    let mut osd_fg_win = Window::default().with_size(240, 70);
    osd_fg_win.set_border(false);
    osd_fg_win.set_override(); 
    osd_fg_win.set_frame(FrameType::FlatBox);
    osd_fg_win.set_color(Color::Black); 

    let mut osd_lbl = Frame::default().with_size(220, 25).with_pos(10, 10);
    osd_lbl.set_frame(FrameType::NoBox);
    osd_lbl.set_label_color(Color::White); 
    osd_lbl.set_label_font(fltk::enums::Font::HelveticaBold); 
    osd_lbl.set_label_size(14);
    osd_lbl.set_align(fltk::enums::Align::Center | fltk::enums::Align::Inside);

    let mut osd_bar = Progress::default().with_size(200, 6).with_pos(20, 46);
    osd_bar.set_frame(FrameType::RFlatBox); 
    osd_bar.set_color(Color::from_rgb(60, 60, 65)); 
    osd_bar.set_selection_color(Color::White);      
    osd_bar.set_minimum(0.0);
    osd_bar.set_maximum(1.0);
    
    osd_fg_win.end();

    let config = if let Ok(file) = File::open(&config_path) {
        serde_json::from_reader(BufReader::new(file)).unwrap_or_default()
    } else { AppConfig::default() };
    let state = Arc::new(Mutex::new(config));
    
    let mut refresh_all_data = {
        let mut choice_port = choice_port.clone();
        let mut choice_wd1 = choice_wd1.clone();
        let mut choice_wd2 = choice_wd2.clone();
        let state = state.clone();
        move || {
            let cfg = state.lock().unwrap();
            let ports = AudioScanner::get_com_ports();
            populate_choice(&mut choice_port, &ports, &cfg.serial.port, false);
            let devices_with_ids = AudioScanner::get_playback_devices_with_ids();
            let device_names: Vec<String> = devices_with_ids.iter().map(|d| d.0.clone()).collect();
            populate_choice(&mut choice_wd1, &device_names, &cfg.work_device_1, true);
            populate_choice(&mut choice_wd2, &device_names, &cfg.work_device_2, true);
        }
    };

    refresh_all_data();
    {
        let cfg = state.lock().unwrap();
        if let Some(idx) = [9600, 19200, 38400, 57600, 115200].iter().position(|&x| x == cfg.serial.baud) {
             choice_baud.set_value(idx as i32);
        }
        
        input_max_val.set_value(&(cfg.value_max as i32).to_string());
        
        check_debug.set_value(cfg.debug_mode);
        check_osd.set_value(cfg.enable_osd);
        choice_curve.set_value(if cfg.use_logarithmic_scale { 1 } else { 0 });

        let procs = AudioScanner::get_active_sessions();
        let capture_devices: Vec<String> = AudioScanner::get_capture_devices_with_ids().into_iter().map(|d| d.0).collect();
        refresh_knobs_ui(&mut scroll_pack, &cfg.dials, &procs, &capture_devices);
    }

    {
        let mut scroll_pack = scroll_pack.clone();
        let state = state.clone();
        let mut refresh_logic = refresh_all_data.clone();
        btn_scan.set_callback(move |_| {
            refresh_logic();
            let cfg = state.lock().unwrap();
            let procs = AudioScanner::get_active_sessions();
            let capture_devices: Vec<String> = AudioScanner::get_capture_devices_with_ids().into_iter().map(|d| d.0).collect();
            refresh_knobs_ui(&mut scroll_pack, &cfg.dials, &procs, &capture_devices);
        });
    }

    {
        let state = state.clone();
        let mut scroll_pack = scroll_pack.clone();
        btn_add.set_callback(move |_| {
            let mut cfg = state.lock().unwrap();
            
            let mut current_dials = Vec::new();
            for i in 0..scroll_pack.children() {
                if let Some(row) = scroll_pack.child(i) {
                    if let Some(node) = row.as_group() {
                        if node.children() >= 5 { 
                            let c_type = unsafe { Choice::from_widget_ptr(node.child(1).unwrap().as_widget_ptr()) };
                            let c_proc = unsafe { Choice::from_widget_ptr(node.child(2).unwrap().as_widget_ptr()) };
                            let c_inv = unsafe { CheckButton::from_widget_ptr(node.child(3).unwrap().as_widget_ptr()) };
                            
                            let t_str = match c_type.value() { 
                                1 => "process", 
                                2 => "all_others", 
                                3 => "microphone",
                                _ => "system" 
                            }.to_string();
                            
                            let p_str = if c_proc.active() { 
                                c_proc.choice().and_then(|val| if val == "None" { None } else { Some(val) })
                            } else { 
                                None 
                            };
                            
                            current_dials.push(DialConfig { 
                                dial_type: t_str, 
                                process_name: p_str,
                                inverted: c_inv.value()
                            });
                        }
                    }
                }
            }
            cfg.dials = current_dials;
            cfg.dials.push(DialConfig { dial_type: "system".to_string(), process_name: None, inverted: false });
            let procs = AudioScanner::get_active_sessions();
            let capture_devices: Vec<String> = AudioScanner::get_capture_devices_with_ids().into_iter().map(|d| d.0).collect();
            refresh_knobs_ui(&mut scroll_pack, &cfg.dials, &procs, &capture_devices);
        });
    }

    {
        let state = state.clone();
        let scroll_pack = scroll_pack.clone();
        let choice_port = choice_port.clone();
        let choice_baud = choice_baud.clone();
        let choice_curve = choice_curve.clone();
        let choice_wd1 = choice_wd1.clone();
        let choice_wd2 = choice_wd2.clone();
        let check_startup = check_startup.clone();
        let check_debug = check_debug.clone();
        let check_osd = check_osd.clone();
        let input_max_val = input_max_val.clone();
        let path = config_path.clone();
        
        btn_apply.set_callback(move |_| {
            let _ = set_startup_launch(check_startup.value());
            let mut cfg = state.lock().unwrap();
            if let Some(port) = choice_port.choice() { cfg.serial.port = port; }
            if let Some(baud_str) = choice_baud.choice() {
                if let Ok(b) = baud_str.parse::<u32>() { cfg.serial.baud = b; }
            }
            if let Some(s) = choice_wd1.choice() { cfg.work_device_1 = extract_clean_name(&s); }
            if let Some(s) = choice_wd2.choice() { cfg.work_device_2 = extract_clean_name(&s); }
            
            if let Ok(v) = input_max_val.value().trim().parse::<f32>() {
                cfg.value_max = v;
            }
            
            cfg.debug_mode = check_debug.value();
            cfg.enable_osd = check_osd.value();
            cfg.use_logarithmic_scale = choice_curve.value() == 1;

            let mut new_dials = Vec::new();
            for i in 0..scroll_pack.children() {
                if let Some(row) = scroll_pack.child(i) {
                    if let Some(node) = row.as_group() {
                        if node.children() >= 5 { 
                            let c_type = unsafe { Choice::from_widget_ptr(node.child(1).unwrap().as_widget_ptr()) };
                            let c_proc = unsafe { Choice::from_widget_ptr(node.child(2).unwrap().as_widget_ptr()) };
                            let c_inv = unsafe { CheckButton::from_widget_ptr(node.child(3).unwrap().as_widget_ptr()) };
                            
                            let t_str = match c_type.value() { 
                                1 => "process", 
                                2 => "all_others", 
                                3 => "microphone",
                                _ => "system" 
                            }.to_string();
                            
                            let p_str = if c_proc.active() { 
                                c_proc.choice().and_then(|val| if val == "None" { None } else { Some(val) })
                            } else { 
                                None 
                            };
                            
                            new_dials.push(DialConfig { 
                                dial_type: t_str, 
                                process_name: p_str,
                                inverted: c_inv.value()
                            });
                        }
                    }
                }
            }
            cfg.dials = new_dials;
            if let Ok(f) = File::create(&path) { let _ = serde_json::to_writer_pretty(f, &*cfg); }
        });
    }

    {
        let mut win = win.clone();
        btn_cancel.set_callback(move |_| win.hide());
    }

    let mut last_osd_update = Instant::now();
    let mut osd_is_visible = false;

    loop {
        app::check();

        if let Ok(event) = MenuEvent::receiver().try_recv() {
            if event.id == open_id {
                refresh_all_data();
                let cfg = state.lock().unwrap();
                let procs = AudioScanner::get_active_sessions();
                let capture_devices: Vec<String> = AudioScanner::get_capture_devices_with_ids().into_iter().map(|d| d.0).collect();
                refresh_knobs_ui(&mut scroll_pack, &cfg.dials, &procs, &capture_devices);
                
                win.show();
                app::flush(); 
                win.redraw();

                unsafe { apply_main_window_theme(win.raw_handle()); }
            } else if event.id == quit_id { app.quit(); break; }
        }
        
        if let Ok(event) = TrayIconEvent::receiver().try_recv() {
             if let TrayIconEvent::Click { button: MouseButton::Left, .. } = event {
                refresh_all_data();
                let cfg = state.lock().unwrap();
                let procs = AudioScanner::get_active_sessions();
                let capture_devices: Vec<String> = AudioScanner::get_capture_devices_with_ids().into_iter().map(|d| d.0).collect();
                refresh_knobs_ui(&mut scroll_pack, &cfg.dials, &procs, &capture_devices);
                
                win.show();
                app::flush(); 
                win.redraw();

                unsafe { apply_main_window_theme(win.raw_handle()); }
             }
        }

        let mut got_msg = false;
        let mut final_app = String::new();
        let mut final_vol = 0.0;

        while let Some((app_name, vol_level)) = osd_rx.recv() {
            got_msg = true;
            final_app = app_name;
            final_vol = vol_level;
        }

        if got_msg {
            osd_lbl.set_label(&final_app);
            osd_bar.set_value(final_vol as f64);
            
            if !osd_is_visible {
                
                let mut primary_screen_idx = 0;
                for i in 0..app::screen_count() {
                    let (x, y, _w, _h) = app::screen_xywh(i);
                    if x == 0 && y == 0 {
                        primary_screen_idx = i;
                        break;
                    }
                }

                let (screen_x, screen_y, screen_w, screen_h) = app::screen_xywh(primary_screen_idx);
                
                let scale = ((screen_h as f32 / 1080.0) * 0.7).max(0.5); 

                let osd_w = (240.0 * scale) as i32;
                let osd_h = (70.0 * scale) as i32;

                let desired_x = screen_x + (screen_w / 2) - (osd_w / 2);
                let desired_y = screen_y + screen_h - osd_h - (120.0 * scale) as i32;
                let pos_x = desired_x.clamp(screen_x, screen_x + screen_w - osd_w);
                let pos_y = desired_y.clamp(screen_y, screen_y + screen_h - osd_h);

                osd_bg_win.resize(pos_x, pos_y, osd_w, osd_h);
                osd_fg_win.resize(pos_x, pos_y, osd_w, osd_h);

                osd_lbl.resize((10.0 * scale) as i32, (10.0 * scale) as i32, (220.0 * scale) as i32, (25.0 * scale) as i32);
                osd_lbl.set_label_size((14.0 * scale) as i32);
                osd_bar.resize((20.0 * scale) as i32, (46.0 * scale) as i32, (200.0 * scale) as i32, (6.0 * scale) as i32);

                osd_bg_win.show();
                osd_fg_win.show();

                unsafe {
                    let preference: u32 = DWMWCP_ROUND;

                    // --- LAYER 1: The Tinted Box ---
                    let bg_hwnd = osd_bg_win.raw_handle();
                    DwmSetWindowAttribute(bg_hwnd, DWMWA_WINDOW_CORNER_PREFERENCE, &preference as *const u32 as *const c_void, 4);

                    let mut ex_style = get_win_long(bg_hwnd, GWL_EXSTYLE);
                    ex_style |= WS_EX_LAYERED | WS_EX_TRANSPARENT; 
                    set_win_long(bg_hwnd, GWL_EXSTYLE, ex_style);
                    
                    SetLayeredWindowAttributes(bg_hwnd as _, 0, 180, LWA_ALPHA); 
                    SetWindowPos(bg_hwnd, HWND_TOPMOST as *mut c_void, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE);

                    // --- LAYER 2: The Text ---
                    let fg_hwnd = osd_fg_win.raw_handle();
                    DwmSetWindowAttribute(fg_hwnd, DWMWA_WINDOW_CORNER_PREFERENCE, &preference as *const u32 as *const c_void, 4);

                    let mut ex_style_fg = get_win_long(fg_hwnd, GWL_EXSTYLE);
                    ex_style_fg |= WS_EX_LAYERED | WS_EX_TRANSPARENT; 
                    set_win_long(fg_hwnd, GWL_EXSTYLE, ex_style_fg);

                    SetLayeredWindowAttributes(fg_hwnd as _, 0x00000000, 0, LWA_COLORKEY);

                    SetWindowPos(fg_hwnd, HWND_TOPMOST as *mut c_void, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE);
                }
                
                osd_is_visible = true;
            }
            
            osd_bg_win.redraw();
            osd_fg_win.redraw();
            last_osd_update = Instant::now();
        }

        if osd_is_visible && last_osd_update.elapsed() > Duration::from_millis(1500) {
            osd_bg_win.hide();
            osd_fg_win.hide();
            osd_is_visible = false;
        }

        std::thread::sleep(Duration::from_millis(16));
    }
    Ok(())
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
    let (osd_tx, osd_rx) = app::channel::<(String, f32)>();

    std::thread::spawn(move || { run_volume_logic_loop(path_clone, osd_tx); });
    build_gui_and_run(path, osd_rx)
}