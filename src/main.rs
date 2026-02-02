#![windows_subsystem = "windows"] // comment out for debug terminal

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use std::process::Command; 
use std::os::windows::process::CommandExt; 
use std::collections::HashMap;

//fltk imports
use fltk::{
    app,
    button::{Button, CheckButton},
    enums::{Color, FrameType},
    frame::Frame,
    group::{Flex, Pack, Scroll},
    menu::Choice,
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
use windows::core::Interface; 
use windows::Win32::Foundation::CloseHandle;
use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
use windows::Win32::Media::Audio::*; 
use windows::Win32::System::Com::*; 
use windows::Win32::System::ProcessStatus::GetModuleBaseNameW;
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_QUERY_INFORMATION, PROCESS_VM_READ,
};
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;

const CREATE_NO_WINDOW: u32 = 0x08000000;

//conf
#[derive(Deserialize, Serialize, Debug, Clone, PartialEq)]
struct SerialConfig { port: String, baud: u32, timeout: u64 }

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq)]
struct DialConfig {
    #[serde(rename = "type")] dial_type: String,
    process_name: Option<String>,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
struct AppConfig {
    serial: SerialConfig,
    value_max: f32,
    soundvolumeview_path: String,
    work_device_1: String, 
    work_device_2: String, 
    dials: Vec<DialConfig>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            serial: SerialConfig { port: "COM3".to_string(), baud: 9600, timeout: 50 },
            value_max: 1024.0,
            soundvolumeview_path: "".to_string(),
            work_device_1: "None".to_string(),
            work_device_2: "None".to_string(),
            dials: vec![],
        }
    }
}

//path

fn get_exe_dir() -> PathBuf {
    std::env::current_exe().map(|p| p.parent().map(|p| p.to_path_buf()).unwrap_or(p)).unwrap_or_else(|_| PathBuf::from("."))
}

fn get_config_path() -> PathBuf {
    let mut path = dirs::config_dir().unwrap_or_else(|| PathBuf::from("."));
    path.push("RVCI");
    if !path.exists() {
        let _ = std::fs::create_dir_all(&path);
    }
    path.join("mapping.json")
}

//reg

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

//audio methods

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
    fn get_process_name(pid: u32) -> String {
        unsafe {
            if let Ok(handle) = OpenProcess(PROCESS_QUERY_INFORMATION | PROCESS_VM_READ, false, pid) {
                let mut buffer = [0u16; 1024];
                let len = GetModuleBaseNameW(handle, None, &mut buffer);
                let _ = CloseHandle(handle);
                if len > 0 { return String::from_utf16_lossy(&buffer[..len as usize]).to_lowercase(); }
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

    fn get_playback_devices_with_ids() -> Vec<(String, String)> {
        let mut devices = Vec::new();
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            let enumerator: Result<IMMDeviceEnumerator, _> = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL);
            if let Ok(enumerator) = enumerator {
                if let Ok(collection) = enumerator.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE) {
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

    fn get_com_ports() -> Vec<String> {
        serialport::available_ports()
            .unwrap_or_default()
            .into_iter()
            .map(|p| p.port_name)
            .collect()
    }
}

//logic loop

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

fn switch_device(svv_path: &str, clean_name: &str) {
    if clean_name == "None" || clean_name.is_empty() { return; }
    let exe_path = PathBuf::from(svv_path);
    if !exe_path.exists() { return; }

    let all_devices = AudioScanner::get_playback_devices_with_ids();
    let match_result = all_devices.iter()
        .find(|(name, _id)| name.to_lowercase().contains(&clean_name.to_lowercase()));

    if let Some((_, real_id)) = match_result {
        let _ = Command::new(&exe_path)
            .arg("/SetDefault")
            .arg(real_id)
            .arg("all")
            .creation_flags(CREATE_NO_WINDOW)
            .spawn();
    }
}

fn run_volume_logic_loop(config_path: PathBuf) {
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
             if let Err(_) = run_serial_processing(&config, &config_path, &mut smoothers) {
                std::thread::sleep(Duration::from_secs(2));
             }
        } else {
            std::thread::sleep(Duration::from_secs(2));
        }
    }
}

fn run_serial_processing(config: &AppConfig, config_path: &PathBuf, smoothers: &mut Vec<Smoother>) -> Result<()> {
    let port = serialport::new(&config.serial.port, config.serial.baud)
        .timeout(Duration::from_millis(config.serial.timeout))
        .open()
        .context("Failed to open serial port")?;
    
    let mut reader = BufReader::new(port);
    let mut line_buf = String::new();
    let mut last_update = Instant::now();
    
 
    let mut last_applied_values: Vec<f32> = vec![-1.0; config.dials.len()];


    let mut pid_name_cache: HashMap<u32, String> = HashMap::new();
    let mut cache_counter = 0;

    let mut process_map: HashSet<String> = HashSet::new();
    for dial in &config.dials {
        if let Some(name) = &dial.process_name { process_map.insert(name.to_lowercase()); }
    }
    
    unsafe { let _ = CoInitializeEx(None, COINIT_MULTITHREADED); }
    let last_file_mod = std::fs::metadata(config_path).and_then(|m| m.modified()).ok();

    loop {
        // Check for config file changes
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
                
                // Handle Buttons
                if line == "WORKS 1" {
                    switch_device(&config.soundvolumeview_path, &config.work_device_1);
                    continue; 
                } else if line == "WORKS 2" {
                    switch_device(&config.soundvolumeview_path, &config.work_device_2);
                    continue;
                }

                if last_update.elapsed() < Duration::from_millis(25) { continue; }
                last_update = Instant::now();

                cache_counter += 1;
                if cache_counter > 200 {
                    pid_name_cache.clear();
                    cache_counter = 0;
                }

                let parts: Vec<&str> = line.split('|').collect();
                if parts.len() != config.dials.len() { continue; }

                for (i, part) in parts.iter().enumerate() {
                    if let Ok(raw_val) = part.parse::<f32>() {
                        let normalized = raw_val.clamp(0.0, config.value_max) / config.value_max;
                        
                        if i >= smoothers.len() { smoothers.push(Smoother::new()); }
                        if i >= last_applied_values.len() { last_applied_values.push(-1.0); }

                        let smoothed = smoothers[i].process(normalized);
                        
                        if (smoothed - last_applied_values[i]).abs() < 0.005 {
                            continue;
                        }
                        
                        last_applied_values[i] = smoothed;
                        
                        let dial_cfg = &config.dials[i];

                        unsafe {
                            match dial_cfg.dial_type.as_str() {
                                "system" => {
                                    if let Ok(vol) = AudioController::get_system_volume() {
                                        let _ = vol.SetMasterVolumeLevelScalar(smoothed, std::ptr::null());
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
                                                                    !process_map.contains(pname)
                                                                } else {
                                                                    match &dial_cfg.process_name {
                                                                        Some(target) => pname == &target.to_lowercase(),
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

//GUI Rendering

const BG_COLOR: Color = Color::from_rgb(1, 1, 1);      
const WIDGET_BG: Color = Color::from_rgb(40, 40, 40);     
const TEXT_COLOR: Color = Color::White;

fn style_widget<W: WidgetExt>(w: &mut W) {
    w.set_color(WIDGET_BG);
    w.set_label_color(TEXT_COLOR);
    w.set_frame(FrameType::FlatBox);
}

fn style_choice(w: &mut Choice) {
    w.set_color(WIDGET_BG);
    w.set_text_color(TEXT_COLOR);
    w.set_frame(FrameType::FlatBox);
    w.set_selection_color(Color::from_rgb(80, 80, 80)); 
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

fn refresh_knobs_ui(scroll_pack: &mut Pack, dials: &Vec<DialConfig>, active_processes: &[String]) {
    scroll_pack.clear(); 
    scroll_pack.begin();
    for (i, dial) in dials.iter().enumerate() {
        let mut row = Flex::default().row().with_size(0, 35);
        row.set_pad(10);
        let mut lbl = Frame::default().with_label(&format!("{}:", i + 1));
        lbl.set_label_color(TEXT_COLOR);
        let mut choice_type = Choice::default();
        style_choice(&mut choice_type);
        choice_type.add_choice("System|Process|Others");
        let sel_idx = match dial.dial_type.as_str() { "process" => 1, "all_others" => 2, _ => 0 };
        choice_type.set_value(sel_idx);
        let mut choice_proc = Choice::default();
        style_choice(&mut choice_proc);
        if dial.dial_type == "process" {
            choice_proc.activate();
            for p in active_processes { choice_proc.add_choice(p); }
            if let Some(pname) = &dial.process_name {
                if let Some(idx) = active_processes.iter().position(|x| x == pname) {
                    choice_proc.set_value(idx as i32);
                }
            }
        } else {
            choice_proc.deactivate();
            choice_proc.set_color(BG_COLOR);
        }
        let mut cp_clone = choice_proc.clone();
        let active_procs_clone = active_processes.to_vec();
        choice_type.set_callback(move |c| {
            if c.value() == 1 { 
                cp_clone.activate();
                cp_clone.set_color(WIDGET_BG);
                cp_clone.clear();
                for p in &active_procs_clone { cp_clone.add_choice(p); }
            } else {
                cp_clone.deactivate();
                cp_clone.set_color(BG_COLOR);
            }
        });
        let mut btn_del = Button::default().with_label("X");
        style_widget(&mut btn_del);
        btn_del.set_label_color(Color::from_rgb(255, 100, 100));
        let mut sp = scroll_pack.clone();
        let r = row.clone(); 
        btn_del.set_callback(move |_| {
            sp.remove(&r);
            sp.redraw();
            if let Some(mut p) = sp.parent() { p.redraw(); }
        });
        row.end();
        let _ = row.fixed(&lbl, 30);
        let _ = row.fixed(&btn_del, 30);
    }
    scroll_pack.end();
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

fn build_gui_and_run(config_path: PathBuf) -> Result<()> {
    let app = app::App::default();
    let (bg_r, bg_g, bg_b) = BG_COLOR.to_rgb();
    app::set_background_color(bg_r, bg_g, bg_b);
    let (fg_r, fg_g, fg_b) = TEXT_COLOR.to_rgb();
    app::set_foreground_color(fg_r, fg_g, fg_b);
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

    let mut win = Window::default().with_size(420, 720).with_label("RVCI Configuration");
    if let Some(ref ico) = taskbar_icon { win.set_icon(Some(ico.clone())); }
    win.set_color(BG_COLOR);
    
    let mut col = Flex::default().column().with_size(420, 720).center_of_parent();
    col.set_margin(15);
    col.set_pad(10);

    let mut title = Frame::default().with_label("RVCI Configuration");
    title.set_label_size(24);

    let mut row_serial = Flex::default().row();
    let lbl_port = Frame::default().with_label("Serial:");
    let mut choice_port = Choice::default();
    style_choice(&mut choice_port);
    let mut choice_baud = Choice::default();
    style_choice(&mut choice_baud);
    for baud in [9600, 19200, 38400, 57600, 115200] { choice_baud.add_choice(&baud.to_string()); }
    let mut btn_scan = Button::default().with_label("Scan");
    style_widget(&mut btn_scan);
    row_serial.end();
    let _ = row_serial.fixed(&lbl_port, 60);
    let _ = row_serial.fixed(&choice_baud, 90);
    let _ = row_serial.fixed(&btn_scan, 60);

    let mut lbl_switcher = Frame::default().with_label("Audio Switcher");
    lbl_switcher.set_label_size(16);

    let mut row_wd1 = Flex::default().row();
    let lbl_wd1 = Frame::default().with_label("Output 1:");
    let mut choice_wd1 = Choice::default();
    style_choice(&mut choice_wd1);
    let mut btn_test1 = Button::default().with_label("Test");
    style_widget(&mut btn_test1);
    row_wd1.end();
    let _ = row_wd1.fixed(&lbl_wd1, 70);
    let _ = row_wd1.fixed(&btn_test1, 50);

    let mut row_wd2 = Flex::default().row();
    let lbl_wd2 = Frame::default().with_label("Output 2:");
    let mut choice_wd2 = Choice::default();
    style_choice(&mut choice_wd2);
    let mut btn_test2 = Button::default().with_label("Test");
    style_widget(&mut btn_test2);
    row_wd2.end();
    let _ = row_wd2.fixed(&lbl_wd2, 70);
    let _ = row_wd2.fixed(&btn_test2, 50);

    let mut row_knobs_header = Flex::default().row();
    let mut lbl_knobs = Frame::default().with_label("Knob Mappings");
    lbl_knobs.set_label_size(16);
    let mut btn_add = Button::default().with_label("+ Add");
    style_widget(&mut btn_add);
    row_knobs_header.end();
    let _ = row_knobs_header.fixed(&btn_add, 60);

    let mut scroll = Scroll::default();
    scroll.set_color(BG_COLOR);
    let mut scroll_pack = Pack::default().with_size(380, 0); 
    scroll_pack.set_spacing(5);
    scroll_pack.end();
    scroll.end();

    let row_footer = Flex::default().row(); 
    let mut check_startup = CheckButton::default().with_label("Launch on Startup");
    check_startup.set_label_color(TEXT_COLOR);
    if check_startup_enabled() { check_startup.set_value(true); }
    let mut lbl_credits = Frame::default().with_label("Made by TZey");
    lbl_credits.set_label_size(12);
    lbl_credits.set_label_color(Color::from_rgb(150, 150, 150));
    row_footer.end();

    let row_btns = Flex::default().row(); 
    let mut btn_apply = Button::default().with_label("Apply"); 
    style_widget(&mut btn_apply);
    btn_apply.set_color(Color::from_rgb(60, 60, 60)); 
    let mut btn_cancel = Button::default().with_label("Close");
    style_widget(&mut btn_cancel);
    row_btns.end();

    col.end();
    let _ = col.fixed(&title, 40);
    let _ = col.fixed(&row_serial, 30);
    let _ = col.fixed(&lbl_switcher, 25);
    let _ = col.fixed(&row_wd1, 30);
    let _ = col.fixed(&row_wd2, 30);
    let _ = col.fixed(&row_knobs_header, 30);
    let _ = col.fixed(&row_footer, 25);
    let _ = col.fixed(&row_btns, 40);

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
        let procs = AudioScanner::get_active_sessions();
        refresh_knobs_ui(&mut scroll_pack, &cfg.dials, &procs);
    }

    // Callbacks
    {
        let mut scroll_pack = scroll_pack.clone();
        let state = state.clone();
        let mut refresh_logic = refresh_all_data.clone();
        btn_scan.set_callback(move |_| {
            refresh_logic();
            let cfg = state.lock().unwrap();
            let procs = AudioScanner::get_active_sessions();
            refresh_knobs_ui(&mut scroll_pack, &cfg.dials, &procs);
        });
    }

    {
        let state = state.clone();
        let mut scroll_pack = scroll_pack.clone();
        btn_add.set_callback(move |_| {
            let mut cfg = state.lock().unwrap();
            cfg.dials.push(DialConfig { dial_type: "system".to_string(), process_name: None });
            let procs = AudioScanner::get_active_sessions();
            refresh_knobs_ui(&mut scroll_pack, &cfg.dials, &procs);
        });
    }

    {
        let state = state.clone();
        let scroll_pack = scroll_pack.clone();
        let choice_port = choice_port.clone();
        let choice_baud = choice_baud.clone();
        let choice_wd1 = choice_wd1.clone();
        let choice_wd2 = choice_wd2.clone();
        let check_startup = check_startup.clone();
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
            
            let mut new_dials = Vec::new();
            for i in 0..scroll_pack.children() {
                if let Some(row) = scroll_pack.child(i) {
                    if let Some(node) = row.as_group() {
                        if node.children() >= 3 {
                            let c_type = unsafe { Choice::from_widget_ptr(node.child(1).unwrap().as_widget_ptr()) };
                            let c_proc = unsafe { Choice::from_widget_ptr(node.child(2).unwrap().as_widget_ptr()) };
                            let t_str = match c_type.value() { 1 => "process", 2 => "all_others", _ => "system" }.to_string();
                            let p_str = if c_proc.active() { c_proc.choice() } else { None };
                            new_dials.push(DialConfig { dial_type: t_str, process_name: p_str });
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

    win.hide();
    loop {
        app::check();
        if let Ok(event) = MenuEvent::receiver().try_recv() {
            if event.id == open_id {
                refresh_all_data();
                let cfg = state.lock().unwrap();
                let procs = AudioScanner::get_active_sessions();
                refresh_knobs_ui(&mut scroll_pack, &cfg.dials, &procs);
                win.show();
            } else if event.id == quit_id { app.quit(); break; }
        }
        if let Ok(event) = TrayIconEvent::receiver().try_recv() {
             if let TrayIconEvent::Click { button: MouseButton::Left, .. } = event {
                refresh_all_data();
                let cfg = state.lock().unwrap();
                let procs = AudioScanner::get_active_sessions();
                refresh_knobs_ui(&mut scroll_pack, &cfg.dials, &procs);
                win.show();
             }
        }
        std::thread::sleep(Duration::from_millis(16));
    }
    Ok(())
}

fn main() -> Result<()> {
    let path = get_config_path();
    let path_clone = path.clone();
    std::thread::spawn(move || { run_volume_logic_loop(path_clone); });
    build_gui_and_run(path)
}