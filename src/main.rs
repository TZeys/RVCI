#![cfg_attr(not(test), windows_subsystem = "windows")]

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use std::collections::HashMap;
use std::ffi::c_void;

use eframe::egui;
use egui::{Color32, RichText, ViewportBuilder, ViewportCommand};

use winreg::enums::*;
use winreg::RegKey;

use tray_icon::{
    menu::{Menu, MenuItem, MenuEvent},
    TrayIcon, TrayIconBuilder, Icon, TrayIconEvent, MouseButton,
};

use windows::core::{Interface, interface, GUID, PCWSTR, PWSTR, IUnknown, IUnknown_Vtbl};
use windows::Win32::Foundation::CloseHandle;
use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
use windows::Win32::Media::Audio::*;
use windows::Win32::System::Com::*;
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::System::Com::StructuredStorage::PropVariantClear;
use windows::Win32::System::Variant::VT_LPWSTR;
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYBD_EVENT_FLAGS,
    KEYEVENTF_KEYUP, KEYEVENTF_EXTENDEDKEY, VIRTUAL_KEY,
};
use windows::core::HSTRING;
use windows::Data::Xml::Dom::XmlDocument;
use windows::UI::Notifications::{ToastNotification, ToastNotificationManager};
use windows::Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID;

macro_rules! log_at {
    ($lvl:expr, $($arg:tt)*) => {
        crate::diag::record($lvl, module_path!(), &format!($($arg)*))
    };
}
macro_rules! log_error { ($($arg:tt)*) => { log_at!(crate::diag::Level::Error, $($arg)*) } }
macro_rules! log_warn  { ($($arg:tt)*) => { log_at!(crate::diag::Level::Warn,  $($arg)*) } }
macro_rules! log_info  { ($($arg:tt)*) => { log_at!(crate::diag::Level::Info,  $($arg)*) } }
macro_rules! log_debug { ($($arg:tt)*) => { log_at!(crate::diag::Level::Debug, $($arg)*) } }

mod diag {
    use std::collections::VecDeque;
    use std::fs::{self, File, OpenOptions};
    use std::io::Write;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Mutex, OnceLock};
    use std::time::Instant;

    use windows::core::w;
    use windows::Win32::Foundation::{CloseHandle, GENERIC_WRITE, HANDLE, SYSTEMTIME};
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    use windows::Win32::System::Console::{
        AllocConsole, FreeConsole, GetConsoleWindow, SetConsoleCtrlHandler,
        SetConsoleScreenBufferSize, SetConsoleTitleW, WriteConsoleW, COORD, CTRL_BREAK_EVENT,
        CTRL_CLOSE_EVENT, CTRL_C_EVENT,
    };
    use windows::Win32::System::Diagnostics::Debug::{
        SetUnhandledExceptionFilter, EXCEPTION_POINTERS,
    };
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::System::SystemInformation::{
        GetLocalTime, GlobalMemoryStatusEx, MEMORYSTATUSEX,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        DeleteMenu, GetSystemMenu, MF_BYCOMMAND, SC_CLOSE,
    };
    use winreg::enums::HKEY_LOCAL_MACHINE;
    use winreg::RegKey;

    pub const MAX_LOG_BYTES: u64 = 2 * 1024 * 1024;
    const KEPT_LOGS: u32 = 3;
    const KEPT_CRASHES: usize = 20;
    const RECENT_LINES: usize = 120;
    const CONSOLE_REPLAY_LINES: usize = 60;

    #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
    pub enum Level {
        Debug,
        Info,
        Warn,
        Error,
    }

    impl Level {
        fn tag(self) -> &'static str {
            match self {
                Level::Debug => "DEBUG",
                Level::Info => "INFO",
                Level::Warn => "WARN",
                Level::Error => "ERROR",
            }
        }
    }

    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub enum ConsoleAction {
        Open,
        Close,
    }

    pub fn reconcile(desired: bool, actual: bool) -> Option<ConsoleAction> {
        match (desired, actual) {
            (true, false) => Some(ConsoleAction::Open),
            (false, true) => Some(ConsoleAction::Close),
            _ => None,
        }
    }

    struct State {
        file: Option<File>,
        bytes: u64,
        log_dir: PathBuf,
        console: Option<isize>,
        recent: VecDeque<String>,
    }

    static STATE: OnceLock<Mutex<State>> = OnceLock::new();
    static STARTED: OnceLock<Instant> = OnceLock::new();
    static ENVIRONMENT: OnceLock<String> = OnceLock::new();
    static PHASE: Mutex<&'static str> = Mutex::new("startup");
    static IN_CRASH: AtomicBool = AtomicBool::new(false);
    static CONSOLE_OPEN: AtomicBool = AtomicBool::new(false);

    fn state() -> Option<&'static Mutex<State>> {
        STATE.get()
    }

    fn lock(m: &Mutex<State>) -> std::sync::MutexGuard<'_, State> {
        m.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn now() -> SYSTEMTIME {
        unsafe { GetLocalTime() }
    }

    fn stamp(t: &SYSTEMTIME) -> String {
        format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}",
            t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond, t.wMilliseconds
        )
    }

    fn file_stamp(t: &SYSTEMTIME) -> String {
        format!(
            "{:04}{:02}{:02}-{:02}{:02}{:02}",
            t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond
        )
    }

    fn short_module(module: &str) -> &str {
        match module.split_once("::") {
            Some((_, rest)) => rest,
            None => "app",
        }
    }

    fn thread_label() -> String {
        std::thread::current()
            .name()
            .map(|s| s.to_string())
            .unwrap_or_else(|| "worker".to_string())
    }

    pub fn uptime_secs() -> f64 {
        STARTED
            .get()
            .map(|s| s.elapsed().as_secs_f64())
            .unwrap_or(0.0)
    }

    pub fn set_phase(phase: &'static str) {
        if let Ok(mut p) = PHASE.lock() {
            *p = phase;
        }
    }

    fn phase() -> &'static str {
        PHASE.lock().map(|p| *p).unwrap_or("unknown")
    }

    pub fn log_dir() -> PathBuf {
        state()
            .map(|m| lock(m).log_dir.clone())
            .unwrap_or_else(|| PathBuf::from("."))
    }

    pub fn crash_dir_for(log_dir: &Path) -> PathBuf {
        log_dir.join("crashes")
    }

    pub fn rotate(log_dir: &Path) {
        let current = log_dir.join("rvci.log");
        let too_big = fs::metadata(&current).map(|m| m.len() >= MAX_LOG_BYTES).unwrap_or(false);
        if !too_big {
            return;
        }
        let _ = fs::remove_file(log_dir.join(format!("rvci.{}.log", KEPT_LOGS)));
        for i in (1..KEPT_LOGS).rev() {
            let from = log_dir.join(format!("rvci.{}.log", i));
            let to = log_dir.join(format!("rvci.{}.log", i + 1));
            if from.exists() {
                let _ = fs::rename(&from, &to);
            }
        }
        let _ = fs::rename(&current, log_dir.join("rvci.1.log"));
    }

    fn open_log(log_dir: &Path) -> (Option<File>, u64) {
        let path = log_dir.join("rvci.log");
        match OpenOptions::new().create(true).append(true).open(&path) {
            Ok(f) => {
                let size = f.metadata().map(|m| m.len()).unwrap_or(0);
                (Some(f), size)
            }
            Err(_) => (None, 0),
        }
    }

    pub fn init(log_dir: PathBuf) {
        let _ = STARTED.set(Instant::now());
        let _ = fs::create_dir_all(&log_dir);
        let _ = fs::create_dir_all(crash_dir_for(&log_dir));
        rotate(&log_dir);
        let (file, bytes) = open_log(&log_dir);
        let _ = STATE.set(Mutex::new(State {
            file,
            bytes,
            log_dir,
            console: None,
            recent: VecDeque::with_capacity(RECENT_LINES),
        }));
        let _ = ENVIRONMENT.set(gather_environment());
        install_crash_handlers();
    }

    pub fn record(level: Level, module: &str, message: &str) {
        let Some(cell) = state() else { return };
        let line = format!(
            "{} {:<5} {:<8} {:<7} {}",
            stamp(&now()),
            level.tag(),
            thread_label(),
            short_module(module),
            message
        );

        let mut st = lock(cell);
        if st.recent.len() == RECENT_LINES {
            st.recent.pop_front();
        }
        st.recent.push_back(line.clone());

        if st.bytes >= MAX_LOG_BYTES {
            st.file = None;
            let dir = st.log_dir.clone();
            rotate(&dir);
            let (f, b) = open_log(&dir);
            st.file = f;
            st.bytes = b;
        }
        let payload = format!("{line}\r\n");
        let stored = match st.file.as_mut() {
            Some(file) => file
                .write_all(payload.as_bytes())
                .and_then(|_| file.flush())
                .is_ok(),
            None => false,
        };
        if stored {
            st.bytes += payload.len() as u64;
        }
        if let Some(raw) = st.console {
            write_console(HANDLE(raw as *mut core::ffi::c_void), &line);
        }
    }

    fn write_console(handle: HANDLE, line: &str) {
        let mut utf16: Vec<u16> = line.encode_utf16().collect();
        utf16.push(b'\r' as u16);
        utf16.push(b'\n' as u16);
        unsafe {
            let _ = WriteConsoleW(handle, &utf16, None, None);
        }
    }

    pub fn console_is_open() -> bool {
        CONSOLE_OPEN.load(Ordering::SeqCst)
    }

    pub fn set_console(desired: bool, reason: &str) {
        let Some(cell) = state() else { return };
        let action = {
            let st = lock(cell);
            reconcile(desired, st.console.is_some())
        };
        let Some(action) = action else { return };

        match action {
            ConsoleAction::Open => {
                record(
                    Level::Info,
                    "RVCI::diag",
                    &format!("console: opening ({reason})"),
                );
                match open_console() {
                    Some(handle) => {
                        let replay: Vec<String> = {
                            let mut st = lock(cell);
                            st.console = Some(handle.0 as isize);
                            st.recent
                                .iter()
                                .rev()
                                .take(CONSOLE_REPLAY_LINES)
                                .rev()
                                .cloned()
                                .collect()
                        };
                        CONSOLE_OPEN.store(true, Ordering::SeqCst);
                        write_console(handle, "");
                        write_console(
                            handle,
                            "RVCI debug console. Closing it is disabled on purpose; untick \"Debug console\" in settings to close it.",
                        );
                        write_console(
                            handle,
                            &format!("Log file: {}", log_dir().join("rvci.log").display()),
                        );
                        write_console(handle, &format!("--- replaying last {} lines ---", replay.len()));
                        for line in &replay {
                            write_console(handle, line);
                        }
                        write_console(handle, "--- live ---");
                        record(Level::Info, "RVCI::diag", "console: opened");
                    }
                    None => {
                        record(
                            Level::Error,
                            "RVCI::diag",
                            "console: AllocConsole failed, debug console unavailable",
                        );
                    }
                }
            }
            ConsoleAction::Close => {
                record(
                    Level::Info,
                    "RVCI::diag",
                    &format!("console: closing ({reason})"),
                );
                let handle = {
                    let mut st = lock(cell);
                    st.console.take()
                };
                CONSOLE_OPEN.store(false, Ordering::SeqCst);
                if let Some(raw) = handle {
                    close_console(HANDLE(raw as *mut core::ffi::c_void));
                }
                record(Level::Info, "RVCI::diag", "console: closed");
            }
        }
    }

    fn open_console() -> Option<HANDLE> {
        unsafe {
            if AllocConsole().is_err() && GetConsoleWindow().is_invalid() {
                return None;
            }
            let _ = SetConsoleTitleW(w!("RVCI debug console"));
            let handle = CreateFileW(
                w!("CONOUT$"),
                GENERIC_WRITE.0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_FLAGS_AND_ATTRIBUTES(0),
                None,
            )
            .ok()?;
            let _ = SetConsoleScreenBufferSize(handle, COORD { X: 200, Y: 9000 });
            let _ = SetConsoleCtrlHandler(Some(ctrl_handler), true);
            let hwnd = GetConsoleWindow();
            if !hwnd.is_invalid() {
                let menu = GetSystemMenu(hwnd, false);
                if !menu.is_invalid() {
                    let _ = DeleteMenu(menu, SC_CLOSE, MF_BYCOMMAND);
                }
            }
            Some(handle)
        }
    }

    fn close_console(handle: HANDLE) {
        unsafe {
            let _ = SetConsoleCtrlHandler(Some(ctrl_handler), false);
            let _ = CloseHandle(handle);
            let _ = FreeConsole();
        }
    }

    unsafe extern "system" fn ctrl_handler(ctrl_type: u32) -> windows::core::BOOL {
        let name = match ctrl_type {
            CTRL_C_EVENT => "ctrl-c",
            CTRL_BREAK_EVENT => "ctrl-break",
            CTRL_CLOSE_EVENT => "close",
            _ => "other",
        };
        record(
            Level::Warn,
            "RVCI::diag",
            &format!("console: ignoring {name} event, RVCI keeps running"),
        );
        matches!(ctrl_type, CTRL_C_EVENT | CTRL_BREAK_EVENT | CTRL_CLOSE_EVENT).into()
    }

    pub fn shutdown(reason: &str) {
        set_phase("shutdown");
        record(
            Level::Info,
            "RVCI::diag",
            &format!("app: shutting down ({reason}), uptime {:.1}s", uptime_secs()),
        );
        if let Some(cell) = state() {
            let handle = {
                let mut st = lock(cell);
                if let Some(file) = st.file.as_mut() {
                    let _ = file.flush();
                }
                st.console.take()
            };
            CONSOLE_OPEN.store(false, Ordering::SeqCst);
            if let Some(raw) = handle {
                close_console(HANDLE(raw as *mut core::ffi::c_void));
            }
        }
    }

    fn gather_environment() -> String {
        let mut out = String::new();
        out.push_str(&format!("app version : {}\r\n", env!("CARGO_PKG_VERSION")));
        out.push_str(&format!(
            "executable  : {}\r\n",
            std::env::current_exe()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| "unknown".into())
        ));
        out.push_str(&format!("process id  : {}\r\n", std::process::id()));
        out.push_str(&format!("target arch : {}\r\n", std::env::consts::ARCH));
        out.push_str(&format!(
            "logical cpus: {}\r\n",
            std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(0)
        ));

        let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
        if let Ok(key) = hklm.open_subkey(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion") {
            let mut product: String = key.get_value("ProductName").unwrap_or_default();
            let display: String = key.get_value("DisplayVersion").unwrap_or_default();
            let build: String = key.get_value("CurrentBuild").unwrap_or_default();
            let ubr: u32 = key.get_value("UBR").unwrap_or(0);
            let build_num: u32 = build.parse().unwrap_or(0);
            if build_num >= 22000 && product.contains("Windows 10") {
                product = product.replace("Windows 10", "Windows 11");
            }
            out.push_str(&format!(
                "os          : {product} {display} (build {build}.{ubr})\r\n"
            ));
        } else {
            out.push_str("os          : unknown\r\n");
        }

        unsafe {
            let mut mem = MEMORYSTATUSEX {
                dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32,
                ..Default::default()
            };
            if GlobalMemoryStatusEx(&mut mem).is_ok() {
                out.push_str(&format!(
                    "memory      : {} MB total, {} MB available ({}% in use)\r\n",
                    mem.ullTotalPhys / (1024 * 1024),
                    mem.ullAvailPhys / (1024 * 1024),
                    mem.dwMemoryLoad
                ));
            }
        }
        out
    }

    pub fn environment() -> &'static str {
        ENVIRONMENT.get().map(|s| s.as_str()).unwrap_or("unavailable")
    }

    fn prune_crashes(dir: &Path) {
        let Ok(entries) = fs::read_dir(dir) else { return };
        let mut files: Vec<PathBuf> = entries
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.is_file())
            .collect();
        if files.len() <= KEPT_CRASHES {
            return;
        }
        files.sort();
        let excess = files.len() - KEPT_CRASHES;
        for path in files.into_iter().take(excess) {
            let _ = fs::remove_file(path);
        }
    }

    pub fn write_crash(kind: &str, summary: &str, detail: &str) -> Option<PathBuf> {
        let cell = state()?;
        let (dir, recent) = {
            let st = lock(cell);
            (
                crash_dir_for(&st.log_dir),
                st.recent.iter().cloned().collect::<Vec<_>>(),
            )
        };
        let _ = fs::create_dir_all(&dir);
        let t = now();
        let path = dir.join(format!(
            "crash-{}-{}.log",
            file_stamp(&t),
            std::process::id()
        ));

        let mut report = String::with_capacity(8192);
        report.push_str("RVCI crash report\r\n");
        report.push_str("=================\r\n");
        report.push_str(&format!("when        : {}\r\n", stamp(&t)));
        report.push_str(&format!("kind        : {kind}\r\n"));
        report.push_str(&format!("phase       : {}\r\n", phase()));
        report.push_str(&format!("thread      : {}\r\n", thread_label()));
        report.push_str(&format!("uptime      : {:.1}s\r\n", uptime_secs()));
        report.push_str(&format!("debug console: {}\r\n", console_is_open()));
        report.push_str(&format!("summary     : {summary}\r\n"));
        report.push_str("\r\n--- environment ---\r\n");
        report.push_str(environment());
        report.push_str("\r\n--- detail ---\r\n");
        report.push_str(detail);
        report.push_str("\r\n\r\n--- recent log ---\r\n");
        for line in &recent {
            report.push_str(line);
            report.push_str("\r\n");
        }

        if fs::write(&path, report.as_bytes()).is_err() {
            return None;
        }
        prune_crashes(&dir);
        Some(path)
    }

    fn install_crash_handlers() {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if !IN_CRASH.swap(true, Ordering::SeqCst) {
                let payload = if let Some(s) = info.payload().downcast_ref::<&str>() {
                    (*s).to_string()
                } else if let Some(s) = info.payload().downcast_ref::<String>() {
                    s.clone()
                } else {
                    "unknown panic payload".to_string()
                };
                let location = info
                    .location()
                    .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
                    .unwrap_or_else(|| "unknown location".to_string());
                let summary = format!("panic at {location}: {payload}");
                let backtrace = std::backtrace::Backtrace::force_capture();
                let detail = format!("panic location: {location}\r\nbacktrace:\r\n{backtrace}");
                let written = write_crash("panic", &summary, &detail);
                record(
                    Level::Error,
                    "RVCI::diag",
                    &format!(
                        "crash: {summary} (report: {})",
                        written
                            .map(|p| p.display().to_string())
                            .unwrap_or_else(|| "not written".into())
                    ),
                );
            }
            previous(info);
        }));

        unsafe {
            SetUnhandledExceptionFilter(Some(exception_filter));
        }
    }

    fn exception_name(code: u32) -> &'static str {
        match code {
            0xC000_0005 => "ACCESS_VIOLATION",
            0xC000_001D => "ILLEGAL_INSTRUCTION",
            0xC000_008C => "ARRAY_BOUNDS_EXCEEDED",
            0xC000_008E => "FLT_DIVIDE_BY_ZERO",
            0xC000_0094 => "INT_DIVIDE_BY_ZERO",
            0xC000_0096 => "PRIV_INSTRUCTION",
            0xC000_00FD => "STACK_OVERFLOW",
            0xC000_0409 => "STACK_BUFFER_OVERRUN",
            0xC000_0374 => "HEAP_CORRUPTION",
            0x8000_0003 => "BREAKPOINT",
            _ => "UNKNOWN",
        }
    }

    unsafe extern "system" fn exception_filter(info: *const EXCEPTION_POINTERS) -> i32 {
        const EXCEPTION_CONTINUE_SEARCH: i32 = 0;
        if IN_CRASH.swap(true, Ordering::SeqCst) || info.is_null() {
            return EXCEPTION_CONTINUE_SEARCH;
        }

        let pointers = unsafe { &*info };
        if pointers.ExceptionRecord.is_null() {
            return EXCEPTION_CONTINUE_SEARCH;
        }
        let record_ref = unsafe { &*pointers.ExceptionRecord };
        let code = record_ref.ExceptionCode.0 as u32;
        let address = record_ref.ExceptionAddress as usize;
        let base = unsafe { GetModuleHandleW(None) }
            .map(|h| h.0 as usize)
            .unwrap_or(0);

        let mut detail = String::with_capacity(1024);
        detail.push_str(&format!(
            "exception code : 0x{code:08X} ({})\r\n",
            exception_name(code)
        ));
        detail.push_str(&format!("exception addr : 0x{address:016X}\r\n"));
        if base != 0 && address >= base {
            detail.push_str(&format!(
                "module offset  : RVCI.exe+0x{:X} (base 0x{:016X})\r\n",
                address - base,
                base
            ));
        }
        detail.push_str(&format!(
            "exception flags: 0x{:08X}\r\n",
            record_ref.ExceptionFlags
        ));
        if code == 0xC000_0005 && record_ref.NumberParameters >= 2 {
            let op = match record_ref.ExceptionInformation[0] {
                0 => "read",
                1 => "write",
                8 => "execute",
                _ => "unknown",
            };
            detail.push_str(&format!(
                "access violation: {op} at 0x{:016X}\r\n",
                record_ref.ExceptionInformation[1]
            ));
        }

        #[cfg(target_arch = "x86_64")]
        if !pointers.ContextRecord.is_null() {
            let ctx = unsafe { &*pointers.ContextRecord };
            detail.push_str(&format!(
                "rip 0x{:016X}  rsp 0x{:016X}  rbp 0x{:016X}\r\n",
                ctx.Rip, ctx.Rsp, ctx.Rbp
            ));
        }

        detail.push_str(
            "\r\nNote: addresses resolve against the RVCI.exe PDB for this exact build.\r\n",
        );

        let summary = format!(
            "0x{code:08X} {} at 0x{address:016X}",
            exception_name(code)
        );
        let written = write_crash("exception", &summary, &detail);
        record(
            Level::Error,
            "RVCI::diag",
            &format!(
                "crash: unhandled {summary} (report: {})",
                written
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "not written".into())
            ),
        );
        EXCEPTION_CONTINUE_SEARCH
    }
}

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
    #[serde(default = "default_osd_style")]
    osd_style: String,
    dials: Vec<DialConfig>,
    #[serde(default)]
    buttons: Vec<ButtonConfig>,
}

fn default_theme() -> String { "Pink".to_string() }

fn default_osd_style() -> String { OSD_STYLES[0].to_string() }

const OSD_STYLES: [&str; 2] = ["themed", "mono"];
const OSD_STYLE_LABELS: [&str; 2] = ["Themed", "Black and white"];

static OSD_MONO: AtomicBool = AtomicBool::new(false);

fn set_osd_style(name: &str) {
    OSD_MONO.store(name == OSD_STYLES[1], Ordering::Relaxed);
}

fn osd_is_mono() -> bool {
    OSD_MONO.load(Ordering::Relaxed)
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
            theme: default_theme(),
            osd_style: default_osd_style(),
            dials: vec![],
            buttons: vec![],
        }
    }
}

fn get_exe_dir() -> PathBuf {
    std::env::current_exe().map(|p| p.parent().map(|p| p.to_path_buf()).unwrap_or(p)).unwrap_or_else(|_| PathBuf::from("."))
}

fn get_log_dir() -> PathBuf {
    let mut path = dirs::config_dir().unwrap_or_else(|| PathBuf::from("."));
    path.push("RVCI");
    path.push("logs");
    path
}

fn get_config_path() -> PathBuf {
    let mut path = dirs::config_dir().unwrap_or_else(|| PathBuf::from("."));
    path.push("RVCI");
    if !path.exists() { let _ = std::fs::create_dir_all(&path); }
    path.join("mapping.json")
}

const MAX_CONFIG_BYTES: u64 = 1024 * 1024;
const MAX_DIALS: usize = 32;
const MAX_BUTTONS: usize = 32;
const MAX_NAME_CHARS: usize = 128;
const DEFAULT_VALUE_MAX: f32 = 1024.0;
const MAX_VALUE_MAX: f32 = 65535.0;

fn clip_chars(s: &mut String, max: usize) {
    if s.chars().count() > max {
        let end = s.char_indices().nth(max).map(|(i, _)| i).unwrap_or(s.len());
        s.truncate(end);
    }
}

fn strip_exe(name: &str) -> &str {
    let n = name.len();
    if n >= 4 && name.is_char_boundary(n - 4) && name[n - 4..].eq_ignore_ascii_case(".exe") {
        &name[..n - 4]
    } else {
        name
    }
}

fn sanitize_config(cfg: &mut AppConfig) {
    if !cfg.value_max.is_finite() || cfg.value_max < 1.0 {
        cfg.value_max = DEFAULT_VALUE_MAX;
    }
    cfg.value_max = cfg.value_max.min(MAX_VALUE_MAX);

    if cfg.serial.baud == 0 {
        cfg.serial.baud = 115200;
    }
    cfg.serial.timeout = cfg.serial.timeout.clamp(1, 5000);
    clip_chars(&mut cfg.serial.port, MAX_NAME_CHARS);
    clip_chars(&mut cfg.work_device_1, MAX_NAME_CHARS);
    clip_chars(&mut cfg.work_device_2, MAX_NAME_CHARS);
    clip_chars(&mut cfg.theme, MAX_NAME_CHARS);
    if !OSD_STYLES.contains(&cfg.osd_style.as_str()) {
        cfg.osd_style = default_osd_style();
    }

    cfg.dials.truncate(MAX_DIALS);
    cfg.buttons.truncate(MAX_BUTTONS);

    for d in &mut cfg.dials {
        clip_chars(&mut d.dial_type, MAX_NAME_CHARS);
        if let Some(n) = &mut d.process_name {
            clip_chars(n, MAX_NAME_CHARS);
        }
    }

    let dial_count = cfg.dials.len();
    for b in &mut cfg.buttons {
        clip_chars(&mut b.action, MAX_NAME_CHARS);
        if dial_count == 0 {
            b.dial_index = 0;
        } else {
            b.dial_index = b.dial_index.min(dial_count - 1);
        }
        if !MEDIA_TOKENS.contains(&b.media_key.as_str()) {
            b.media_key = MEDIA_TOKENS[0].to_string();
        }
        b.modifiers.truncate(4);
        b.modifiers.retain(|m| token_to_vk(m).is_some());
        if !b.key_combo.is_empty() && token_to_vk(&b.key_combo).is_none() {
            b.key_combo.clear();
        }
    }
}

enum ConfigLoad {
    Loaded(AppConfig),
    Missing,
    Unreadable,
}

fn load_config(path: &Path) -> ConfigLoad {
    let Ok(file) = File::open(path) else {
        return ConfigLoad::Missing;
    };
    let reader = BufReader::new(file.take(MAX_CONFIG_BYTES));
    match serde_json::from_reader::<_, AppConfig>(reader) {
        Ok(mut cfg) => {
            sanitize_config(&mut cfg);
            ConfigLoad::Loaded(cfg)
        }
        Err(e) => {
            log_error!("config: {} could not be parsed: {e}", path.display());
            ConfigLoad::Unreadable
        }
    }
}

fn save_config(path: &Path, cfg: &AppConfig) -> bool {
    let tmp = path.with_extension("json.tmp");
    let written = File::create(&tmp)
        .ok()
        .and_then(|file| {
            serde_json::to_writer_pretty(&file, cfg).ok()?;
            file.sync_all().ok()
        })
        .is_some();
    if !written {
        log_error!("config: could not write {}", tmp.display());
        let _ = std::fs::remove_file(&tmp);
        return false;
    }
    if std::fs::rename(&tmp, path).is_ok() {
        true
    } else {
        log_error!("config: could not replace {} with {}", path.display(), tmp.display());
        let _ = std::fs::remove_file(&tmp);
        false
    }
}

fn acquire_single_instance() -> bool {
    use windows::core::w;
    use windows::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS};
    use windows::Win32::System::Threading::CreateMutexW;

    unsafe {
        match CreateMutexW(None, true, w!("RVCI_SINGLE_INSTANCE")) {
            Ok(handle) => {
                if GetLastError() == ERROR_ALREADY_EXISTS {
                    let _ = CloseHandle(handle);
                    false
                } else {
                    true
                }
            }
            Err(_) => true,
        }
    }
}

const STARTUP_VALUE: &str = "RVCI";

const STARTUP_VALUE_LEGACY: &str = "RVSC";
const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";

fn set_startup_launch(enable: bool) -> Result<()> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let path = hkcu.open_subkey_with_flags(RUN_KEY, KEY_SET_VALUE | KEY_QUERY_VALUE)?;

    let _ = path.delete_value(STARTUP_VALUE_LEGACY);
    if enable {
        let exe_path = std::env::current_exe()?;
        let quoted = format!("\"{}\"", exe_path.to_string_lossy());
        path.set_value(STARTUP_VALUE, &quoted)?;
        log_info!("startup: registered the HKCU Run entry as {quoted}");
    } else {
        let _ = path.delete_value(STARTUP_VALUE);
        log_info!("startup: removed the HKCU Run entry");
    }
    Ok(())
}

fn check_startup_enabled() -> bool {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    if let Ok(path) = hkcu.open_subkey(RUN_KEY) {
        if let Ok(exe_path) = std::env::current_exe() {
            let exe = exe_path.to_string_lossy();

            for name in [STARTUP_VALUE, STARTUP_VALUE_LEGACY] {
                if let Ok(val) = path.get_value::<String, _>(name) {
                    if val.trim().trim_matches('"') == exe { return true; }
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
        let store = item.OpenPropertyStore(STGM_READ).ok()?;
        let mut prop = store.GetValue(&PKEY_Device_FriendlyName).ok()?;
        let name = if prop.Anonymous.Anonymous.vt == VT_LPWSTR {
            let pwsz = prop.Anonymous.Anonymous.Anonymous.pwszVal;
            if pwsz.is_null() { None } else { pwsz.to_string().ok() }
        } else {
            None
        };
        let _ = PropVariantClear(&mut prop);
        name
    }

    unsafe fn get_mic_volume(mic_name: &str) -> Result<IAudioEndpointVolume> {
        Self::get_endpoint_volume(mic_name, eCapture)
    }

    unsafe fn get_output_device_volume(device_name: &str) -> Result<IAudioEndpointVolume> {
        Self::get_endpoint_volume(device_name, eRender)
    }

    fn get_process_name(pid: u32) -> String {
        unsafe {
            let Ok(handle) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
                return String::new();
            };
            let mut buffer = [0u16; 512];
            let mut len = buffer.len() as u32;
            let ok = QueryFullProcessImageNameW(
                handle,
                PROCESS_NAME_WIN32,
                PWSTR(buffer.as_mut_ptr()),
                &mut len,
            )
            .is_ok();
            let _ = CloseHandle(handle);
            if !ok || len == 0 {
                return String::new();
            }
            let full = String::from_utf16_lossy(&buffer[..len.min(buffer.len() as u32) as usize]);
            let base = full.rsplit(['\\', '/']).next().unwrap_or_default();
            strip_exe(base).to_string()
        }
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
                                let id_string = match item.GetId() {
                                    Ok(p) => {
                                        let s = p.to_string().unwrap_or_default();
                                        CoTaskMemFree(Some(p.as_ptr() as *const c_void));
                                        s
                                    }
                                    Err(_) => String::new(),
                                };
                                let name_string =
                                    AudioController::device_friendly_name(&item).unwrap_or_default();
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

    if match_result.is_none() {
        log_warn!("audio: no playback device matches \"{clean_name}\", output not switched");
    }
    if let Some((matched, real_id)) = match_result {
        log_info!("audio: switching the default output to {matched}");
        unsafe {
            if let Ok(policy) = CoCreateInstance::<_, IPolicyConfig>(&CLSID_PolicyConfigClient, None, CLSCTX_ALL) {
                let mut id_utf16: Vec<u16> = real_id.encode_utf16().collect();
                id_utf16.push(0);
                let pcwstr_id = PCWSTR(id_utf16.as_ptr());

                if let Err(e) = policy.SetDefaultEndpoint(pcwstr_id, eConsole) {
                    log_warn!("audio: SetDefaultEndpoint(eConsole) failed: {e}");
                }
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

#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct DialLevel {
    level: f32,
    muted: bool,
}

#[derive(Clone)]
struct UiLink {
    serial: Arc<Mutex<SerialStatus>>,
    open: Arc<AtomicBool>,
    levels: Arc<Mutex<Vec<DialLevel>>>,
}

impl UiLink {
    fn new(open: Arc<AtomicBool>) -> Self {
        Self {
            serial: Arc::new(Mutex::new(SerialStatus::Idle)),
            open,
            levels: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn status(&self) -> SerialStatus {
        self.serial.lock().map(|g| *g).unwrap_or(SerialStatus::Idle)
    }

    fn set_status(&self, new: SerialStatus) {
        if let Ok(mut s) = self.serial.lock() {
            *s = new;
        }
    }

    fn publish(&self, levels: &[f32], muted: &[bool]) {
        if !self.open.load(Ordering::Relaxed) {
            return;
        }
        if let Ok(mut g) = self.levels.lock() {
            g.clear();
            g.extend(
                levels
                    .iter()
                    .zip(muted.iter())
                    .map(|(l, m)| DialLevel { level: *l, muted: *m }),
            );
        }
    }

    fn clear_levels(&self) {
        if let Ok(mut g) = self.levels.lock() {
            g.clear();
        }
    }

    fn levels(&self) -> Vec<DialLevel> {
        self.levels.lock().map(|g| g.clone()).unwrap_or_default()
    }
}

#[derive(Clone, Debug, PartialEq)]
struct OsdMsg {
    label: String,
    level: f32,
    muted: bool,
}

const OSD_RAW_STEP: f32 = 15.0;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct GateSlot {
    raw: Option<f32>,
    muted: bool,
}

#[derive(Default)]
struct OsdGate {
    slots: Vec<GateSlot>,
}

impl OsdGate {
    fn at(&mut self, i: usize) -> &mut GateSlot {
        if self.slots.len() <= i {
            self.slots.resize(i + 1, GateSlot::default());
        }
        &mut self.slots[i]
    }

    fn sample(&mut self, i: usize, raw: f32, muted: bool) -> bool {
        let slot = self.at(i);
        let first = slot.raw.is_none();
        let moved = slot.raw.map(|p| (raw - p).abs() > OSD_RAW_STEP).unwrap_or(false);
        let mute_flipped = slot.muted != muted;
        slot.muted = muted;
        if first || moved {
            slot.raw = Some(raw);
        }
        !first && (moved || mute_flipped)
    }

    fn mute_changed(&mut self, i: usize, muted: bool) -> bool {
        let slot = self.at(i);
        if slot.muted == muted {
            return false;
        }
        slot.muted = muted;
        true
    }
}

const LEVEL_DEADBAND: f32 = 0.0074;

#[derive(Clone, Copy, Debug, PartialEq)]
struct ApplyDecision {
    level_changed: bool,
    mute_pending: bool,
}

impl ApplyDecision {
    fn any(&self) -> bool {
        self.level_changed || self.mute_pending
    }
}

fn apply_decision(
    quantized: f32,
    last_applied: f32,
    has_mute_button: bool,
    applied_mute: Option<bool>,
    want_mute: bool,
) -> ApplyDecision {
    ApplyDecision {
        level_changed: (quantized - last_applied).abs() >= LEVEL_DEADBAND,
        mute_pending: has_mute_button && applied_mute != Some(want_mute),
    }
}

fn serial_settings_changed(a: &AppConfig, b: &AppConfig) -> bool {
    a.serial != b.serial
        || a.value_max != b.value_max
        || a.use_logarithmic_scale != b.use_logarithmic_scale
        || a.enable_osd != b.enable_osd
        || a.work_device_1 != b.work_device_1
        || a.work_device_2 != b.work_device_2
        || a.dials != b.dials
        || a.buttons != b.buttons
}

fn dial_is_muted(cfg: &AppConfig, button_states: &[bool], dial: usize) -> bool {
    cfg.buttons.iter().enumerate().any(|(bid, b)| {
        b.action == "mute_dial"
            && b.dial_index == dial
            && button_states.get(bid).copied().unwrap_or(false)
    })
}

fn dial_has_mute_button(cfg: &AppConfig, dial: usize) -> bool {
    cfg.buttons
        .iter()
        .any(|b| b.action == "mute_dial" && b.dial_index == dial)
}

fn osd_label(d: &DialConfig) -> Option<String> {
    match d.dial_type.as_str() {
        "system" => Some("Master Volume".to_string()),
        "all_others" => Some("Other Apps".to_string()),
        _ => {
            let target = d.process_name.as_deref().unwrap_or("");
            if target.is_empty() || target == "None" {
                return None;
            }
            Some(strip_exe(target).to_string())
        }
    }
}

fn is_system_volume_key(token: &str) -> bool {
    matches!(
        token,
        "vol_mute" | "volume_mute" | "mute" | "vol_up" | "volume_up" | "vol_down" | "volume_down"
    )
}

fn spawn_system_volume_osd(tx: Sender<OsdMsg>) {
    std::thread::spawn(move || unsafe {
        std::thread::sleep(Duration::from_millis(90));
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        if let Ok(vol) = AudioController::get_system_volume() {
            let level = vol.GetMasterVolumeLevelScalar().unwrap_or(0.0);
            let muted = vol.GetMute().map(|b| b.as_bool()).unwrap_or(false);
            let _ = tx.send(OsdMsg {
                label: "Master Volume".to_string(),
                level: level.clamp(0.0, 1.0),
                muted,
            });
        }
        CoUninitialize();
    });
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

fn run_volume_logic_loop(config_path: PathBuf, osd_tx: Sender<OsdMsg>, ui: UiLink) {
    let mut current_config_sig = String::new();
    let mut smoothers: Vec<Smoother> = Vec::new();
    let mut last_seen_status = SerialStatus::Idle;
    let mut last_inuse_notify: Option<Instant> = None;
    loop {
        if let ConfigLoad::Loaded(config) = load_config(&config_path) {
            set_osd_style(&config.osd_style);
            diag::set_console(config.debug_mode, "config on disk");
            let new_sig = format!("{}{}", config.serial.port, config.serial.baud);
            if new_sig != current_config_sig {
                log_info!(
                    "serial: target is {} @{} ({} dials, {} buttons)",
                    config.serial.port,
                    config.serial.baud,
                    config.dials.len(),
                    config.buttons.len()
                );
                current_config_sig = new_sig;
                smoothers = (0..config.dials.len()).map(|_| Smoother::new()).collect();
            }
            if run_serial_processing(&config, &config_path, &mut smoothers, &osd_tx, &ui).is_ok() {
                last_seen_status = SerialStatus::Connected;
            } else {
                let cur = ui.status();
                if cur != last_seen_status && cur == SerialStatus::InUse {
                    let debounced = last_inuse_notify
                        .map(|t| t.elapsed() < Duration::from_secs(20))
                        .unwrap_or(false);
                    if !debounced {
                        last_inuse_notify = Some(Instant::now());
                        log_warn!(
                            "serial: {} is held by another program, notifying the user",
                            config.serial.port
                        );
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

const MAX_SERIAL_LINE: u64 = 512;
const BUTTON_MIN_GAP: Duration = Duration::from_millis(40);

fn button_ready(fired: &mut Vec<Option<Instant>>, id: usize) -> bool {
    while fired.len() <= id {
        fired.push(None);
    }
    let now = Instant::now();
    if let Some(prev) = fired[id] {
        if now.duration_since(prev) < BUTTON_MIN_GAP {
            return false;
        }
    }
    fired[id] = Some(now);
    true
}

fn run_serial_processing(
    config: &AppConfig,
    config_path: &Path,
    smoothers: &mut Vec<Smoother>,
    osd_tx: &Sender<OsdMsg>,
    ui: &UiLink,
) -> Result<()> {
    let port = match serialport::new(&config.serial.port, config.serial.baud)
        .timeout(Duration::from_millis(config.serial.timeout))
        .open()
    {
        Ok(p) => p,
        Err(e) => {
            let status = classify_serial_error(&e);
            log_warn!(
                "serial: could not open {} @{}: {e} (status {status:?})",
                config.serial.port,
                config.serial.baud
            );
            ui.set_status(status);
            return Err(anyhow::anyhow!("Failed to open serial port: {e}"));
        }
    };
    log_info!(
        "serial: connected to {} @{} (timeout {}ms)",
        config.serial.port,
        config.serial.baud,
        config.serial.timeout
    );
    ui.set_status(SerialStatus::Connected);

    let dial_count = config.dials.len();
    let mut reader = BufReader::new(port);
    let mut raw_line: Vec<u8> = Vec::with_capacity(MAX_SERIAL_LINE as usize);
    let mut discarding = false;
    let mut last_update = Instant::now();

    let mut last_applied: Vec<f32> = vec![-1.0; dial_count];
    let mut level_now: Vec<f32> = vec![0.0; dial_count];
    let mut muted_now: Vec<bool> = vec![false; dial_count];
    let has_mute_button: Vec<bool> = (0..dial_count).map(|i| dial_has_mute_button(config, i)).collect();
    let mut applied_mute: Vec<Option<bool>> = has_mute_button
        .iter()
        .map(|has| if *has { Some(false) } else { None })
        .collect();
    let mut gate = OsdGate::default();

    let mut button_states: Vec<bool> = vec![false; config.buttons.len()];
    let mut button_fired: Vec<Option<Instant>> = vec![None; config.buttons.len()];

    let mut mic_device_cache: HashMap<String, IAudioEndpointVolume> = HashMap::new();
    let mut output_device_cache: HashMap<String, IAudioEndpointVolume> = HashMap::new();
    let mut system_volume: Option<(IAudioEndpointVolume, Instant)> = None;
    let mut sessions = SessionCache::new();
    let mut last_value_line = String::new();
    let mut settle: u32 = 0;

    let mut process_map: HashSet<String> = HashSet::new();
    for dial in &config.dials {
        if let Some(name) = &dial.process_name {
            process_map.insert(strip_exe(name).to_lowercase());
        }
    }

    let dial_targets: Vec<Option<String>> = config
        .dials
        .iter()
        .map(|d| {
            d.process_name.as_ref().and_then(|n| {
                if n == "None" {
                    return None;
                }
                let clean = strip_exe(n).to_lowercase();
                if clean.is_empty() { None } else { Some(clean) }
            })
        })
        .collect();

    unsafe { let _ = CoInitializeEx(None, COINIT_MULTITHREADED); }
    let mut last_file_mod = std::fs::metadata(config_path).and_then(|m| m.modified()).ok();

    log_debug!(
        "serial: {dial_count} dials live, mute buttons bound to knobs {:?}",
        has_mute_button
            .iter()
            .enumerate()
            .filter(|(_, has)| **has)
            .map(|(i, _)| i + 1)
            .collect::<Vec<_>>()
    );

    let mut last_cfg_check = Instant::now();

    loop {
        if last_cfg_check.elapsed() >= Duration::from_millis(1000) {
            last_cfg_check = Instant::now();
            if let Ok(meta) = std::fs::metadata(config_path) {
                if let Ok(mod_time) = meta.modified() {
                    if Some(mod_time) != last_file_mod {
                        last_file_mod = Some(mod_time);
                        match load_config(config_path) {
                            ConfigLoad::Loaded(next) => {
                                set_osd_style(&next.osd_style);
                                diag::set_console(next.debug_mode, "config file changed");
                                if serial_settings_changed(config, &next) {
                                    log_info!("serial: settings changed on disk, restarting the loop");
                                    ui.clear_levels();
                                    return Ok(());
                                }
                                log_debug!("serial: config file changed, serial settings identical, staying connected");
                            }
                            _ => {
                                log_warn!("serial: config file changed but could not be read, restarting the loop");
                                ui.clear_levels();
                                return Ok(());
                            }
                        }
                    }
                }
            }
        }

        raw_line.clear();
        let read = (&mut reader).take(MAX_SERIAL_LINE).read_until(b'\n', &mut raw_line);
        let bytes = match read {
            Ok(0) => {
                std::thread::sleep(Duration::from_millis(10));
                continue;
            }
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {
                std::thread::sleep(Duration::from_millis(10));
                continue;
            }
            Err(e) => {
                log_warn!("serial: read failed on {}: {e}, reconnecting", config.serial.port);
                ui.clear_levels();
                return Err(anyhow::anyhow!("Serial error"));
            }
        };

        let terminated = raw_line.last() == Some(&b'\n');
        if discarding {
            discarding = !terminated;
            continue;
        }
        if !terminated && bytes as u64 >= MAX_SERIAL_LINE {
            log_warn!(
                "serial: oversized line from {} (over {MAX_SERIAL_LINE} bytes), discarding to the next newline",
                config.serial.port
            );
            discarding = true;
            continue;
        }

        let decoded = String::from_utf8_lossy(&raw_line);
        let line = decoded.trim();
        if line.is_empty() {
            continue;
        }

        if line == "WORKS 1" || line == "WORKS 2" {
            let target = if line == "WORKS 1" { &config.work_device_1 } else { &config.work_device_2 };
            log_info!("input: {line} switch, moving the default output to {target}");
            switch_device(target);
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
                        let prev = button_states.get(id).copied().unwrap_or(false);
                        while button_states.len() <= id {
                            button_states.push(false);
                        }
                        button_states[id] = new_state;
                        let rising = !prev && new_state;

                        let btn = &config.buttons[id];
                        match btn.action.as_str() {
                            "mute_dial" => {
                                let di = btn.dial_index;
                                if di < dial_count {
                                    let want = dial_is_muted(config, &button_states, di);
                                    if muted_now[di] != want {
                                        log_info!(
                                            "input: button {} {} knob {}",
                                            id + 1,
                                            if want { "muted" } else { "unmuted" },
                                            di + 1
                                        );
                                        muted_now[di] = want;
                                        if config.enable_osd && gate.mute_changed(di, want) {
                                            if let Some(label) = osd_label(&config.dials[di]) {
                                                let _ = osd_tx.send(OsdMsg {
                                                    label,
                                                    level: level_now[di],
                                                    muted: want,
                                                });
                                            }
                                        }
                                        ui.publish(&level_now, &muted_now);
                                    }
                                }
                                settle = settle.max(4);
                            }
                            "media" => {
                                if rising && button_ready(&mut button_fired, id) {
                                    log_info!("input: button {} sent media key {}", id + 1, btn.media_key);
                                    KeyEmu::tap(&btn.media_key);
                                    if config.enable_osd && is_system_volume_key(&btn.media_key) {
                                        spawn_system_volume_osd(osd_tx.clone());
                                    }
                                }
                            }
                            "keys" => {
                                if rising && button_ready(&mut button_fired, id) {
                                    log_info!(
                                        "input: button {} sent {}",
                                        id + 1,
                                        format_combo(&btn.modifiers, &btn.key_combo)
                                    );
                                    KeyEmu::send_combo(&btn.modifiers, &btn.key_combo);
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
            continue;
        }

        if line == last_value_line {
            if settle == 0 {
                continue;
            }
        } else {
            last_value_line.clear();
            last_value_line.push_str(line);
            settle = 14;
        }

        if last_update.elapsed() < Duration::from_millis(25) {
            continue;
        }
        last_update = Instant::now();
        if settle > 0 {
            settle -= 1;
        }

        let parts: Vec<&str> = line.split('|').collect();
        if dial_count == 0 || parts.len() < dial_count {
            continue;
        }

        for (i, dial_cfg) in config.dials.iter().enumerate() {
            let Ok(raw_val) = parts[i].parse::<f32>() else { continue };
            if !raw_val.is_finite() {
                continue;
            }

            let mut normalized = raw_val.clamp(0.0, config.value_max) / config.value_max;
            if dial_cfg.inverted {
                normalized = 1.0 - normalized;
            }
            if config.use_logarithmic_scale {
                normalized = normalized.powf(3.0);
            }

            if i >= smoothers.len() {
                smoothers.push(Smoother::new());
            }
            let smoothed = smoothers[i].process(normalized);
            let quantized = ((smoothed * 200.0).round() / 200.0).clamp(0.0, 1.0);

            let want_mute = dial_is_muted(config, &button_states, i);
            muted_now[i] = want_mute;
            level_now[i] = quantized;

            let show_osd = gate.sample(i, raw_val, want_mute);
            let decision = apply_decision(
                quantized,
                last_applied[i],
                has_mute_button[i],
                applied_mute[i],
                want_mute,
            );
            let ApplyDecision { level_changed, mute_pending } = decision;
            let mut observed_mute: Option<bool> = None;

            if decision.any() {
                if level_changed {
                    last_applied[i] = quantized;
                }
                if mute_pending {
                    applied_mute[i] = Some(want_mute);
                }
                let write_mute = mute_pending || want_mute;
                let read_mute = config.enable_osd && show_osd;
                let level = quantized;

                unsafe {
                    match dial_cfg.dial_type.as_str() {
                        "system" => {
                            let refetch = system_volume
                                .as_ref()
                                .map(|(_, t)| t.elapsed() > Duration::from_secs(3))
                                .unwrap_or(true);
                            if refetch {
                                system_volume = AudioController::get_system_volume()
                                    .ok()
                                    .map(|v| (v, Instant::now()));
                            }
                            if let Some((vol, _)) = &system_volume {
                                let mut ok = vol
                                    .SetMasterVolumeLevelScalar(level, std::ptr::null())
                                    .is_ok();
                                if ok && write_mute {
                                    ok = vol.SetMute(want_mute, std::ptr::null()).is_ok();
                                }
                                if ok && read_mute {
                                    observed_mute = vol.GetMute().ok().map(|b| b.as_bool());
                                }
                                if !ok {
                                    system_volume = None;
                                }
                            }
                        }
                        "microphone" | "output_device" => {
                            let is_mic = dial_cfg.dial_type == "microphone";
                            let Some(target) = dial_cfg.process_name.as_ref() else { continue };
                            if target == "None" {
                                continue;
                            }
                            let cache = if is_mic {
                                &mut mic_device_cache
                            } else {
                                &mut output_device_cache
                            };
                            let vol_opt = cache.get(target).cloned().or_else(|| {
                                let fetched = if is_mic {
                                    AudioController::get_mic_volume(target)
                                } else {
                                    AudioController::get_output_device_volume(target)
                                };
                                fetched.ok().map(|v| {
                                    cache.insert(target.clone(), v.clone());
                                    v
                                })
                            });
                            if let Some(vol) = vol_opt {
                                let mut ok = vol
                                    .SetMasterVolumeLevelScalar(level, std::ptr::null())
                                    .is_ok();
                                if ok && write_mute {
                                    ok = vol.SetMute(want_mute, std::ptr::null()).is_ok();
                                }
                                if ok && read_mute {
                                    observed_mute = vol.GetMute().ok().map(|b| b.as_bool());
                                }
                                if !ok {
                                    cache.remove(target);
                                }
                            }
                        }
                        "process" | "all_others" => {
                            let target = dial_targets.get(i).and_then(|t| t.as_ref());
                            if dial_cfg.dial_type == "process" && target.is_none() {
                                continue;
                            }
                            if sessions.stale() {
                                sessions.refresh();
                            }
                            for (pname, vol) in &sessions.list {
                                let should_change = if dial_cfg.dial_type == "all_others" {
                                    !process_map.contains(pname)
                                } else {
                                    Some(pname) == target
                                };
                                if should_change {
                                    let _ = vol.SetMasterVolume(level, std::ptr::null());
                                    if write_mute {
                                        let _ = vol.SetMute(want_mute, std::ptr::null());
                                    }
                                    if read_mute && observed_mute.is_none() {
                                        observed_mute = vol.GetMute().ok().map(|b| b.as_bool());
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }

            if config.enable_osd && show_osd {
                if let Some(label) = osd_label(dial_cfg) {
                    let _ = osd_tx.send(OsdMsg {
                        label,
                        level: quantized,
                        muted: want_mute || observed_mute.unwrap_or(false),
                    });
                }
            }
        }

        ui.publish(&level_now, &muted_now);
    }
}

fn load_icon_image(path: &Path) -> Option<image::RgbaImage> {
    let mut reader = image::ImageReader::open(path).ok()?.with_guessed_format().ok()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(1024);
    limits.max_image_height = Some(1024);
    limits.max_alloc = Some(16 * 1024 * 1024);
    reader.limits(limits);
    Some(reader.decode().ok()?.into_rgba8())
}

fn load_tray_icon(filename: &str) -> Icon {
    if let Some(rgba) = load_icon_image(&get_exe_dir().join(filename)) {
        let (w, h) = rgba.dimensions();
        if let Ok(icon) = Icon::from_rgba(rgba.into_raw(), w, h) { return icon; }
    }
    let (width, height) = (32, 32);
    let mut rgba = Vec::with_capacity((width * height * 4) as usize);
    for _ in 0..height { for _ in 0..width { rgba.extend_from_slice(&[255, 0, 0, 255]); } }
    Icon::from_rgba(rgba, width, height).unwrap_or_else(|_| panic!("Icon error"))
}

fn load_window_icon(filename: &str) -> Option<egui::IconData> {
    let rgba = load_icon_image(&get_exe_dir().join(filename))?;
    let (w, h) = rgba.dimensions();
    Some(egui::IconData { rgba: rgba.into_raw(), width: w, height: h })
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
    separator: Color32,
    widget_bg: Color32,
    row_hover: Color32,
    track: Color32,
    text: Color32,
    text_muted: Color32,
    text_faint: Color32,
    accent: Color32,
    accent2: Color32,
    accent_hover: Color32,
    destructive: Color32,
    destructive_hover: Color32,
    success: Color32,
    warning: Color32,
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
        bg:          mix(rgb(11, 12, 16),    accent, tint * 0.40),
        card_bg:     mix(rgb(24, 26, 32),    accent, tint * 0.70),
        card_border: mix(rgb(36, 39, 47),    accent, tint * 1.10),
        separator:   mix(rgb(44, 48, 57),    accent, tint * 1.20),
        widget_bg:   mix(rgb(39, 42, 51),    accent, tint * 0.90),
        row_hover:   mix(rgb(52, 56, 67),    accent, tint * 1.45),
        track:       mix(rgb(58, 62, 74),    accent, tint * 1.10),
        text:        rgb(238, 241, 246),
        text_muted:  mix(rgb(146, 154, 168), accent, tint * 0.50),
        text_faint:  mix(rgb(105, 113, 127), accent, tint * 0.45),
        accent,
        accent2,
        accent_hover: hover,
        destructive: rgb(248, 81, 73),
        destructive_hover: rgb(255, 120, 112),
        success: rgb(52, 199, 89),
        warning: rgb(240, 168, 48),
        extreme_bg:  mix(rgb(16, 18, 22),    accent, tint * 0.35),
        faint_bg:    mix(rgb(31, 34, 42),    accent, tint * 0.80),
        dark: true,
    }
}

fn light_theme(accent: Color32, accent2: Color32, hover: Color32, tint: f32) -> Palette {
    Palette {
        bg:          mix(rgb(238, 240, 245), accent, tint * 0.35),
        card_bg:     mix(rgb(255, 255, 255), accent, tint * 0.14),
        card_border: mix(rgb(223, 227, 234), accent, tint * 0.80),
        separator:   mix(rgb(226, 230, 237), accent, tint * 0.70),
        widget_bg:   mix(rgb(240, 242, 247), accent, tint * 0.30),
        row_hover:   mix(rgb(231, 235, 242), accent, tint * 1.00),
        track:       mix(rgb(213, 217, 225), accent, tint * 0.60),
        text:        rgb(20, 23, 30),
        text_muted:  mix(rgb(94, 102, 115), accent, tint * 0.40),
        text_faint:  mix(rgb(139, 147, 160), accent, tint * 0.35),
        accent,
        accent2,
        accent_hover: hover,
        destructive: rgb(205, 45, 42),
        destructive_hover: rgb(224, 70, 66),
        success: rgb(28, 152, 68),
        warning: rgb(176, 112, 12),
        extreme_bg:  rgb(255, 255, 255),
        faint_bg:    mix(rgb(233, 236, 242), accent, tint * 0.55),
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
fn warning() -> Color32 { pal().warning }
fn bg() -> Color32 { pal().bg }
fn card_bg() -> Color32 { pal().card_bg }
fn card_border() -> Color32 { pal().card_border }
fn separator() -> Color32 { pal().separator }
fn widget_bg() -> Color32 { pal().widget_bg }
fn row_hover() -> Color32 { pal().row_hover }
fn track() -> Color32 { pal().track }
fn text() -> Color32 { pal().text }
fn text_muted() -> Color32 { pal().text_muted }
fn text_faint() -> Color32 { pal().text_faint }

const FAMILY_SEMIBOLD: &str = "rvci-semibold";

fn semibold(size: f32) -> egui::FontId {
    static NAME: std::sync::OnceLock<std::sync::Arc<str>> = std::sync::OnceLock::new();
    let name = NAME.get_or_init(|| FAMILY_SEMIBOLD.into());
    egui::FontId::new(size, egui::FontFamily::Name(name.clone()))
}

fn regular(size: f32) -> egui::FontId {
    egui::FontId::proportional(size)
}

fn is_sfnt(bytes: &[u8]) -> bool {
    if bytes.len() < 4096 {
        return false;
    }
    matches!(
        &bytes[..4],
        [0x00, 0x01, 0x00, 0x00] | b"true" | b"OTTO" | b"ttcf"
    )
}

fn install_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    let dir = std::env::var_os("WINDIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("C:\\Windows"));
    let fonts_dir = dir.join("Fonts");

    let mut load = |file: &str, key: &str| -> bool {
        match std::fs::read(fonts_dir.join(file)) {
            Ok(bytes) if is_sfnt(&bytes) => {
                fonts.font_data.insert(key.to_owned(), egui::FontData::from_owned(bytes));
                true
            }
            _ => false,
        }
    };

    let has_regular = load("segoeui.ttf", "rvci-ui");
    let has_semibold = load("seguisb.ttf", "rvci-ui-semibold");

    if has_regular {
        if let Some(list) = fonts.families.get_mut(&egui::FontFamily::Proportional) {
            list.insert(0, "rvci-ui".to_owned());
        }
    }

    let semibold_stack: Vec<String> = if has_semibold {
        let mut v = vec!["rvci-ui-semibold".to_owned()];
        v.extend(
            fonts
                .families
                .get(&egui::FontFamily::Proportional)
                .cloned()
                .unwrap_or_default(),
        );
        v
    } else {
        fonts
            .families
            .get(&egui::FontFamily::Proportional)
            .cloned()
            .unwrap_or_default()
    };

    fonts.families.insert(
        egui::FontFamily::Name(FAMILY_SEMIBOLD.into()),
        semibold_stack,
    );

    log_info!(
        "ui: fonts installed (segoeui={has_regular}, seguisb={has_semibold}), from {}",
        fonts_dir.display()
    );
    ctx.set_fonts(fonts);
}

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
    config_unreadable: bool,

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

    link: UiLink,
    user_opened: Arc<AtomicBool>,
    hwnd: isize,

    proc_rx: std::sync::mpsc::Receiver<Vec<String>>,

    github_tex: Option<egui::TextureHandle>,

    show_themes: bool,
}

impl RvciApp {
    fn new(cc: &eframe::CreationContext<'_>, config_path: PathBuf, link: UiLink) -> Self {
        install_fonts(&cc.egui_ctx);
        configure_visuals(&cc.egui_ctx);

        let (mut cfg, config_unreadable) = match load_config(&config_path) {
            ConfigLoad::Loaded(cfg) => (cfg, false),
            ConfigLoad::Missing => (AppConfig::default(), false),
            ConfigLoad::Unreadable => (AppConfig::default(), true),
        };
        if config_unreadable {
            log_error!("ui: showing defaults because the config could not be read");
        }

        if cfg.theme == "RVCI Pink" { cfg.theme = "Pink".to_string(); }
        set_theme_by_name(&cfg.theme);
        set_osd_style(&cfg.osd_style);

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
        if tray.is_none() {
            log_error!("ui: the tray icon could not be created, RVCI has no visible entry point");
        } else {
            log_info!("ui: tray icon created");
        }

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

        log_info!("ui: settings window created, hwnd 0x{hwnd:X}");

        let want_show = Arc::new(AtomicBool::new(false));
        let user_opened = link.open.clone();

        {
            let ws = want_show.clone();
            let uo = user_opened.clone();
            let ctx = cc.egui_ctx.clone();
            MenuEvent::set_event_handler(Some(move |ev: MenuEvent| {
                if ev.id == open_id {
                    log_info!("ui: settings opened from the tray menu");
                    uo.store(true, Ordering::SeqCst);
                    show_window_native(hwnd);
                    ws.store(true, Ordering::SeqCst);
                    ctx.request_repaint();
                } else if ev.id == quit_id {
                    log_info!("ui: quit chosen from the tray menu");
                    diag::shutdown("tray quit");
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
                    log_info!("ui: settings opened from the tray icon");
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
            let scanner = std::thread::Builder::new().name("scanner".to_string()).spawn(move || {
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
            if scanner.is_err() {
                log_error!("ui: could not start the audio session scanner thread");
            }
        }

        let mut app = Self {
            config_path,
            cfg,
            config_unreadable,
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
            link,
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
        log_info!(
            "ui: rescan found {} COM ports, {} playback, {} capture, {} audio apps",
            self.com_ports.len(),
            self.playback_devices.len(),
            self.capture_devices.len(),
            self.active_processes.len()
        );
    }

    fn persist_debug_flag(&mut self, enabled: bool) {
        match load_config(&self.config_path) {
            ConfigLoad::Loaded(mut on_disk) => {
                if on_disk.debug_mode == enabled {
                    return;
                }
                on_disk.debug_mode = enabled;
                if save_config(&self.config_path, &on_disk) {
                    log_info!("config: debug console setting persisted as {enabled}");
                } else {
                    log_error!("config: could not persist the debug console setting");
                }
            }
            ConfigLoad::Missing => log_warn!(
                "config: no mapping.json yet, the debug console setting will persist on the next save"
            ),
            ConfigLoad::Unreadable => log_warn!(
                "config: mapping.json is unreadable, the debug console setting will persist on the next save"
            ),
        }
    }

    fn save(&mut self) {
        let _ = set_startup_launch(self.startup_enabled);

        self.cfg.work_device_1 = extract_clean_name(&self.cfg.work_device_1);
        self.cfg.work_device_2 = extract_clean_name(&self.cfg.work_device_2);
        sanitize_config(&mut self.cfg);

        if self.config_unreadable {
            let quarantine = self.config_path.with_extension("json.invalid");
            let _ = std::fs::remove_file(&quarantine);
            if std::fs::rename(&self.config_path, &quarantine).is_ok() {
                log_warn!("config: moved the unreadable file to {}", quarantine.display());
                self.config_unreadable = false;
            } else {
                log_error!("config: could not quarantine the unreadable file");
            }
        }

        let ok = save_config(&self.config_path, &self.cfg);
        if ok {
            log_info!(
                "config: saved, port={} baud={} dials={} buttons={} theme={} osd={} debug_console={} startup={}",
                self.cfg.serial.port,
                self.cfg.serial.baud,
                self.cfg.dials.len(),
                self.cfg.buttons.len(),
                self.cfg.theme,
                self.cfg.enable_osd,
                self.cfg.debug_mode,
                self.startup_enabled
            );
        } else {
            log_error!("config: save failed, the settings on disk are unchanged");
        }
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
                    log_info!(
                        "ui: button {} bound to {}",
                        idx + 1,
                        format_combo(&mods, &key)
                    );
                    self.cfg.buttons[idx].modifiers = mods;
                    self.cfg.buttons[idx].key_combo = key;
                }
            } else {
                log_debug!("ui: key capture for button {} cancelled", idx + 1);
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
                left: 20.0,
                right: 20.0,
                top: 18.0,
                bottom: 10.0,
            }))
            .show(ctx, |ui| {
                self.section_header(ui);
                ui.add_space(16.0);

                let content_w = (ui.available_width() - 16.0).max(300.0);
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.set_max_width(content_w);
                        ui.spacing_mut().item_spacing.y = 18.0;

                        let two_col = content_w >= 1000.0;
                        if two_col {

                            let gutter = 20.0;
                            ui.spacing_mut().item_spacing.x = gutter;

                            let col_w = ((content_w - gutter) / 2.0).floor();
                            set_layout_w(col_w);
                            ui.columns(2, |cols| {
                                cols[0].spacing_mut().item_spacing.y = 18.0;
                                cols[1].spacing_mut().item_spacing.y = 18.0;

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
                        ui.add_space(6.0);
                    });
            });

        self.themes_window(ctx);
    }

    fn section_header(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(RichText::new("Configuration").font(semibold(23.0)).color(text()));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ghost_button(ui, "Rescan COM", egui::vec2(0.0, 30.0))
                    .on_hover_text("Rescan COM ports, audio devices and running apps")
                    .clicked()
                {
                    self.rescan();
                }
                ui.add_space(10.0);
                status_pill(ui, self.link.status());
            });
        });
        if self.config_unreadable {
            ui.add_space(10.0);
            notice(
                ui,
                "mapping.json could not be read, so these are defaults. Saving keeps the old file as mapping.json.invalid.",
            );
        }
    }

    fn section_serial(&mut self, ui: &mut egui::Ui) {
        let ports = self.com_ports.clone();
        section(ui, "Connection", |ui| {
            let mut rows = Rows::new();
            rows.item(ui, |ui| {
                ui.horizontal(|ui| {
                    label_cell(ui, "Serial port");
                    let cur = if self.cfg.serial.port.is_empty() {
                        "None".to_string()
                    } else {
                        self.cfg.serial.port.clone()
                    };
                    let options: Vec<String> = if ports.is_empty() {
                        vec!["No COM ports found".to_string()]
                    } else {
                        ports.clone()
                    };
                    let w = ui.available_width();
                    if let Some(i) = select(ui, "serial_port", &cur, w, SelectStyle::Row, &options) {
                        if !ports.is_empty() {
                            self.cfg.serial.port = ports[i].clone();
                        }
                    }
                });
            });
            rows.item(ui, |ui| {
                ui.horizontal(|ui| {
                    label_cell(ui, "Baud rate");
                    let cur = self.cfg.serial.baud.to_string();
                    let options: Vec<String> = BAUD_RATES.iter().map(|b| b.to_string()).collect();
                    let w = ui.available_width();
                    if let Some(i) = select(ui, "baud", &cur, w, SelectStyle::Row, &options) {
                        self.cfg.serial.baud = BAUD_RATES[i];
                    }
                });
            });
        });
    }

    fn section_general(&mut self, ui: &mut egui::Ui) {
        section(ui, "Behavior", |ui| {
            let mut rows = Rows::new();
            rows.item(ui, |ui| {
                ui.horizontal(|ui| {
                    label_cell(ui, "Max pot value");
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let mut v = self.cfg.value_max;
                        if ui
                            .add_sized(
                                [74.0, 26.0],
                                egui::DragValue::new(&mut v).speed(1.0).range(1.0..=8192.0),
                            )
                            .on_hover_text("Highest value your board sends for a knob")
                            .changed()
                        {
                            self.cfg.value_max = v;
                        }
                    });
                });
            });
            rows.item(ui, |ui| {
                ui.horizontal(|ui| {
                    label_cell(ui, "Volume curve");
                    let cur = if self.cfg.use_logarithmic_scale { "Logarithmic" } else { "Linear" };
                    let options = vec!["Linear".to_string(), "Logarithmic".to_string()];
                    let w = ui.available_width();
                    if let Some(i) = select(ui, "curve", cur, w, SelectStyle::Row, &options) {
                        self.cfg.use_logarithmic_scale = i == 1;
                    }
                });
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

        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.show_themes = false;
            return;
        }

        let mut open = self.show_themes;
        let frame = egui::Frame::window(&ctx.style())
            .fill(card_bg())
            .stroke(egui::Stroke::new(1.0, card_border()))
            .rounding(egui::Rounding::same(18.0))
            .inner_margin(egui::Margin::same(22.0))
            .shadow(egui::epaint::Shadow {
                offset: egui::vec2(0.0, 16.0),
                blur: 44.0,
                spread: 0.0,
                color: Color32::from_black_alpha(140),
            });
        egui::Window::new(RichText::new("Themes").font(semibold(16.0)))
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
                let max_h = (screen.height() - 160.0).max(240.0);
                egui::ScrollArea::vertical()
                    .max_height(max_h)
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        ui.horizontal_wrapped(|ui| {
                            ui.spacing_mut().item_spacing = egui::vec2(12.0, 12.0);
                            for (name, p) in themes() {
                                let name = *name;
                                if theme_tile(ui, name, p, cur.as_str() == name) {
                                    pick = Some(name.to_string());
                                }
                            }
                        });
                    });
                ui.add_space(4.0);
                if let Some(name) = pick {
                    log_info!("ui: theme changed to {name}");
                    set_theme_by_name(&name);
                    self.cfg.theme = name;
                }
            });
        self.show_themes = open;
    }

    fn section_routing(&mut self, ui: &mut egui::Ui) {
        let devices = self.playback_devices.clone();
        let mut options: Vec<String> = vec!["None".to_string()];
        options.extend(devices.iter().cloned());

        section(ui, "Output switcher", |ui| {
            let mut rows = Rows::new();
            for (n, label) in [(1u8, "Output 1"), (2u8, "Output 2")] {
                rows.item(ui, |ui| {
                    ui.horizontal(|ui| {
                        label_cell(ui, label);
                        let stored = if n == 1 {
                            self.cfg.work_device_1.clone()
                        } else {
                            self.cfg.work_device_2.clone()
                        };
                        let display = routing_display(&stored, &devices);
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if text_button(ui, "Test")
                                .on_hover_text("Switch to this output now")
                                .clicked()
                            {
                                let name = extract_clean_name(&stored);
                                std::thread::spawn(move || switch_device(&name));
                            }
                            let w = ui.available_width();
                            if let Some(i) = select(
                                ui,
                                ("wd", n),
                                &display,
                                w,
                                SelectStyle::Row,
                                &options,
                            ) {
                                let picked = options[i].clone();
                                if n == 1 {
                                    self.cfg.work_device_1 = picked;
                                } else {
                                    self.cfg.work_device_2 = picked;
                                }
                            }
                        });
                    });
                });
            }
        });
    }

    fn section_knobs(&mut self, ui: &mut egui::Ui) {
        let processes = self.active_processes.clone();
        let captures = self.capture_devices.clone();
        let playbacks = self.playback_devices.clone();
        let live = self.link.levels();

        let (add_clicked, _) = section_with_action(ui, "Knobs", "Add knob", |ui| {
            if self.cfg.dials.is_empty() {
                empty_hint(ui, "No knobs yet. Add one for each potentiometer on your box.");
                return;
            }
            let mut remove: Option<usize> = None;
            let len = self.cfg.dials.len();
            let mut rows = Rows::new();
            for i in 0..len {
                rows.item(ui, |ui| {
                    ui.push_id(("knob", i), |ui| {
                        ui.horizontal(|ui| {
                            index_badge(ui, i + 1);
                            ui.add_space(2.0);

                            let cur_type = self.cfg.dials[i].dial_type.clone();
                            if let Some(pick) = select(
                                ui,
                                "ktype",
                                dial_type_label(&cur_type),
                                122.0,
                                SelectStyle::Inline,
                                &DIAL_TYPE_LABELS,
                            ) {
                                self.cfg.dials[i].dial_type = DIAL_TYPES[pick].to_string();
                            }

                            let dtype = self.cfg.dials[i].dial_type.clone();
                            if dtype == "system" || dtype == "all_others" {
                                self.cfg.dials[i].process_name = None;
                            }

                            let source: &[String] = match dtype.as_str() {
                                "process" => &processes,
                                "microphone" => &captures,
                                "output_device" => &playbacks,
                                _ => &[],
                            };
                            let has_target =
                                matches!(dtype.as_str(), "process" | "microphone" | "output_device");

                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if delete_button(ui).clicked() {
                                    remove = Some(i);
                                }
                                ui.add_space(4.0);
                                switch(ui, &mut self.cfg.dials[i].inverted)
                                    .on_hover_text("Invert this knob");
                                ui.label(RichText::new("Invert").font(regular(13.0)).color(text_muted()));
                                ui.add_space(6.0);

                                let cur = self.cfg.dials[i]
                                    .process_name
                                    .clone()
                                    .unwrap_or_else(|| "None".to_string());
                                let mut options: Vec<String> = vec!["None".to_string()];
                                options.extend(source.iter().cloned());
                                let w = ui.available_width();
                                if has_target {
                                    if let Some(pick) =
                                        select(ui, "ktarget", &cur, w, SelectStyle::Inline, &options)
                                    {
                                        self.cfg.dials[i].process_name = if pick == 0 {
                                            None
                                        } else {
                                            Some(options[pick].clone())
                                        };
                                    }
                                } else {
                                    quiet_note(ui, "Whole system", w);
                                }
                            });
                        });

                        let state = live.get(i).copied().unwrap_or_default();
                        let connected = i < live.len();
                        knob_meter(ui, state, connected);
                    });
                });
            }
            if let Some(idx) = remove {
                log_info!("ui: removed knob {}", idx + 1);
                self.cfg.dials.remove(idx);
            }
        });

        if add_clicked {
            log_info!("ui: added knob {}", self.cfg.dials.len() + 1);
            self.cfg.dials.push(DialConfig {
                dial_type: "system".to_string(),
                process_name: None,
                inverted: false,
            });
        }
    }

    fn section_buttons(&mut self, ui: &mut egui::Ui) {
        let dial_count = self.cfg.dials.len();

        let (add_clicked, _) = section_with_action(ui, "Buttons", "Add button", |ui| {
            if self.cfg.buttons.is_empty() {
                empty_hint(ui, "No buttons yet. Add one for each button, in wiring order.");
                return;
            }
            let mut remove: Option<usize> = None;
            let len = self.cfg.buttons.len();
            let mut rows = Rows::new();
            for i in 0..len {
                rows.item(ui, |ui| {
                    ui.push_id(("btn", i), |ui| {
                        ui.horizontal(|ui| {
                            index_badge(ui, i + 1);
                            ui.add_space(2.0);

                            let cur_action = self.cfg.buttons[i].action.clone();
                            if let Some(pick) = select(
                                ui,
                                "baction",
                                action_label(&cur_action),
                                122.0,
                                SelectStyle::Inline,
                                &ACTION_LABELS,
                            ) {
                                self.cfg.buttons[i].action = ACTIONS[pick].to_string();
                            }

                            let action = self.cfg.buttons[i].action.clone();

                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if delete_button(ui).clicked() {
                                    remove = Some(i);
                                }
                                ui.add_space(6.0);
                                let mid_w = ui.available_width();

                                match action.as_str() {
                                    "mute_dial" => {
                                        if dial_count == 0 {
                                            quiet_note(ui, "Add a knob first", mid_w);
                                        } else {
                                            let di = self.cfg.buttons[i].dial_index.min(dial_count - 1);
                                            self.cfg.buttons[i].dial_index = di;
                                            let options: Vec<String> =
                                                (0..dial_count).map(|k| format!("Knob {}", k + 1)).collect();
                                            if let Some(pick) = select(
                                                ui,
                                                "bknob",
                                                &format!("Knob {}", di + 1),
                                                mid_w,
                                                SelectStyle::Inline,
                                                &options,
                                            ) {
                                                self.cfg.buttons[i].dial_index = pick;
                                            }
                                        }
                                    }
                                    "media" => {
                                        let cur_tok = self.cfg.buttons[i].media_key.clone();
                                        let cur_idx =
                                            MEDIA_TOKENS.iter().position(|t| *t == cur_tok).unwrap_or(0);
                                        if let Some(pick) = select(
                                            ui,
                                            "bmedia",
                                            MEDIA_LABELS[cur_idx],
                                            mid_w,
                                            SelectStyle::Inline,
                                            &MEDIA_LABELS,
                                        ) {
                                            self.cfg.buttons[i].media_key = MEDIA_TOKENS[pick].to_string();
                                        }
                                    }
                                    "keys" => {
                                        let capturing = self.key_capture == Some(i);
                                        let label = if capturing {
                                            "Press keys".to_string()
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
                                        if filled_button(ui, &label, base, hover, egui::vec2(mid_w, 30.0))
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
                                        quiet_note(ui, "Nothing bound", mid_w);
                                    }
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
                log_info!("ui: removed button {}", idx + 1);
                self.cfg.buttons.remove(idx);
            }
        });

        if add_clicked {
            log_info!("ui: added button {}", self.cfg.buttons.len() + 1);
            self.cfg.buttons.push(ButtonConfig::default());
        }
    }

    fn section_options(&mut self, ui: &mut egui::Ui) {
        section(ui, "Options", |ui| {
            let mut rows = Rows::new();
            let mut startup = self.startup_enabled;
            if switch_row(&mut rows, ui, "Launch at startup", &mut startup) {
                self.startup_enabled = startup;
            }
            let mut osd = self.cfg.enable_osd;
            if switch_row(&mut rows, ui, "On-screen display", &mut osd) {
                self.cfg.enable_osd = osd;
            }
            let style_idx = OSD_STYLES
                .iter()
                .position(|s| *s == self.cfg.osd_style)
                .unwrap_or(0);
            let osd_on = self.cfg.enable_osd;
            rows.item(ui, |ui| {
                ui.horizontal(|ui| {
                    label_cell_enabled(ui, "OSD style", osd_on);
                    let w = ui.available_width();
                    ui.add_enabled_ui(osd_on, |ui| {
                        if let Some(pick) = select(
                            ui,
                            "osd_style",
                            OSD_STYLE_LABELS[style_idx],
                            w,
                            SelectStyle::Row,
                            &OSD_STYLE_LABELS,
                        ) {
                            log_info!("ui: OSD style changed to {}", OSD_STYLES[pick]);
                            self.cfg.osd_style = OSD_STYLES[pick].to_string();
                            set_osd_style(&self.cfg.osd_style);
                        }
                    });
                });
            });
            let mut debug = self.cfg.debug_mode;
            if switch_row(&mut rows, ui, "Debug console", &mut debug) {
                self.cfg.debug_mode = debug;
                log_info!(
                    "ui: debug console switched {}",
                    if debug { "on" } else { "off" }
                );
                diag::set_console(debug, "settings switch");
                self.persist_debug_flag(debug);
            }
        });
    }

    fn section_actionbar(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::bottom("actionbar")
            .show_separator_line(false)
            .frame(
                egui::Frame::none()
                    .fill(card_bg())
                    .inner_margin(egui::Margin::symmetric(20.0, 13.0)),
            )
            .show(ctx, |ui| {
                let r = ui.max_rect();
                ui.painter().hline(
                    r.left() - 20.0..=r.right() + 20.0,
                    r.top() - 13.0,
                    egui::Stroke::new(1.0, separator()),
                );
                ui.horizontal(|ui| {
                    let (label, base, hover) = match self.save_flash {
                        Some((t, true)) if t.elapsed() < Duration::from_millis(1200) => {
                            ("Saved", success(), success())
                        }
                        Some((t, false)) if t.elapsed() < Duration::from_millis(1800) => {
                            ("Save failed", destructive(), destructive_hover())
                        }
                        _ => ("Save changes", accent(), accent_hover()),
                    };
                    if filled_button(ui, label, base, hover, egui::vec2(132.0, 34.0)).clicked() {
                        self.save();
                    }

                    if ghost_button(ui, "Close", egui::vec2(0.0, 34.0)).clicked() {
                        log_info!("ui: settings window closed from the Close button, parking to the tray");
                        self.user_opened.store(false, Ordering::SeqCst);
                        park_window_native(self.hwnd);
                    }

                    if ghost_button(ui, "Themes", egui::vec2(0.0, 34.0)).clicked() {
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
        ui.label(RichText::new("Made by TZey").font(regular(12.5)).color(text_faint()));
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
    let (slot, resp) = ui.allocate_exact_size(egui::vec2(154.0, 104.0), egui::Sense::click());
    let lift = ui.ctx().animate_bool(resp.id, resp.hovered()) * 2.5;
    let rect = slot.translate(egui::vec2(0.0, -lift));
    let painter = ui.painter();
    let round = egui::Rounding::same(14.0);

    if lift > 0.1 {
        painter.rect_filled(
            rect.translate(egui::vec2(0.0, 4.0)),
            round,
            Color32::from_black_alpha((lift * 22.0) as u8),
        );
    }
    painter.rect_filled(rect, round, p.bg);

    let card_r = egui::Rect::from_min_max(
        rect.min + egui::vec2(11.0, 11.0),
        egui::pos2(rect.right() - 11.0, rect.bottom() - 31.0),
    );
    painter.rect_filled(card_r, egui::Rounding::same(9.0), p.card_bg);
    painter.rect_stroke(card_r, egui::Rounding::same(9.0), egui::Stroke::new(1.0, p.card_border));

    let mut y = card_r.top() + 11.0;
    for w in [0.62_f32, 0.44] {
        let row = egui::Rect::from_min_size(
            egui::pos2(card_r.left() + 10.0, y),
            egui::vec2((card_r.width() - 20.0) * w, 6.0),
        );
        painter.rect_filled(row, egui::Rounding::same(3.0), p.widget_bg);
        y += 11.0;
    }

    let bar = egui::Rect::from_min_size(
        egui::pos2(card_r.left() + 10.0, card_r.bottom() - 14.0),
        egui::vec2(card_r.width() - 20.0, 5.0),
    );
    painter.rect_filled(bar, egui::Rounding::same(2.5), p.track);
    paint_vgrad(
        painter,
        egui::Rect::from_min_size(bar.min, egui::vec2(bar.width() * 0.66, bar.height())),
        p.accent2,
        p.accent,
        2.5,
    );

    painter.text(
        egui::pos2(rect.left() + 13.0, rect.bottom() - 16.0),
        egui::Align2::LEFT_CENTER,
        name,
        regular(13.0),
        p.text,
    );

    if selected {
        let c = egui::pos2(rect.right() - 20.0, rect.bottom() - 16.0);
        painter.circle_filled(c, 8.0, p.accent);
        let s = egui::Stroke::new(1.8, contrast_text(p.accent));
        painter.line_segment([c + egui::vec2(-3.5, 0.2), c + egui::vec2(-1.0, 2.8)], s);
        painter.line_segment([c + egui::vec2(-1.0, 2.8), c + egui::vec2(3.8, -3.0)], s);
    }

    let (bw, bc) = if selected {
        (2.0, accent())
    } else if resp.hovered() {
        (1.0, text_faint())
    } else {
        (1.0, card_border())
    };
    painter.rect_stroke(rect, round, egui::Stroke::new(bw, bc));

    resp.clicked()
}

fn index_badge(ui: &mut egui::Ui, n: usize) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(22.0, 22.0), egui::Sense::hover());
    ui.painter().rect_filled(rect, egui::Rounding::same(7.0), pal().faint_bg);
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        n.to_string(),
        semibold(12.0),
        text_muted(),
    );
}

const ROW_PAD: f32 = 15.0;

struct Rows {
    first: bool,
}

impl Rows {
    fn new() -> Self {
        Self { first: true }
    }

    fn item<R>(&mut self, ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
        if !self.first {
            hairline(ui);
        }
        self.first = false;
        row(ui, add)
    }
}

fn hairline(ui: &mut egui::Ui) {
    let w = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(egui::vec2(w, 1.0), egui::Sense::hover());
    ui.painter().hline(
        (rect.left() + ROW_PAD)..=rect.right(),
        rect.top() + 0.5,
        egui::Stroke::new(1.0, separator()),
    );
}

fn row<R>(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    let content_w = (ui.available_width() - ROW_PAD * 2.0).max(40.0);
    egui::Frame::none()
        .inner_margin(egui::Margin::symmetric(ROW_PAD, 9.0))
        .show(ui, |ui| {
            ui.set_width(content_w);
            ui.set_min_height(28.0);
            ui.spacing_mut().item_spacing.y = 8.0;
            add(ui)
        })
        .inner
}

fn group<R>(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    let width = layout_w().min(ui.available_width());
    egui::Frame::none()
        .fill(card_bg())
        .stroke(egui::Stroke::new(1.0, card_border()))
        .rounding(egui::Rounding::same(14.0))
        .inner_margin(egui::Margin::symmetric(0.0, 6.0))
        .show(ui, |ui| {
            ui.set_width(width);
            ui.spacing_mut().item_spacing.y = 0.0;
            add(ui)
        })
        .inner
}

fn section_title(ui: &mut egui::Ui, title: &str) {
    ui.label(RichText::new(title).font(semibold(13.0)).color(text_muted()));
}

fn section<R>(ui: &mut egui::Ui, title: &str, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    ui.vertical(|ui| {
        ui.spacing_mut().item_spacing.y = 0.0;
        ui.horizontal(|ui| {
            ui.add_space(5.0);
            section_title(ui, title);
        });
        ui.add_space(9.0);
        group(ui, add)
    })
    .inner
}

fn section_with_action<R>(
    ui: &mut egui::Ui,
    title: &str,
    action: &str,
    add: impl FnOnce(&mut egui::Ui) -> R,
) -> (bool, R) {
    let mut clicked = false;
    let width = layout_w().min(ui.available_width());
    let inner = ui
        .vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            ui.horizontal(|ui| {
                ui.set_width(width);
                ui.add_space(5.0);
                section_title(ui, title);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    clicked = text_button(ui, action).clicked();
                });
            });
            ui.add_space(5.0);
            group(ui, add)
        })
        .inner;
    (clicked, inner)
}

fn quiet_note(ui: &mut egui::Ui, message: &str, width: f32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width.max(20.0), 30.0), egui::Sense::hover());
    let shown = elide(ui, message, regular(13.5), rect.width() - 12.0);
    ui.painter().text(
        egui::pos2(rect.right() - 6.0, rect.center().y),
        egui::Align2::RIGHT_CENTER,
        shown,
        regular(13.5),
        text_faint(),
    );
}

fn label_cell(ui: &mut egui::Ui, label: &str) {
    label_cell_enabled(ui, label, true);
}

fn label_cell_enabled(ui: &mut egui::Ui, label: &str, enabled: bool) {
    let col = if enabled { text() } else { mix(text(), card_bg(), 0.55) };
    ui.label(RichText::new(label).font(regular(14.5)).color(col));
}

fn notice(ui: &mut egui::Ui, message: &str) {
    egui::Frame::none()
        .fill(pal().faint_bg)
        .stroke(egui::Stroke::new(1.0, mix(card_border(), warning(), 0.5)))
        .rounding(egui::Rounding::same(10.0))
        .inner_margin(egui::Margin::symmetric(13.0, 10.0))
        .show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                ui.label(RichText::new("!").font(semibold(13.0)).color(warning()));
                ui.label(RichText::new(message).font(regular(13.0)).color(text_muted()));
            });
        });
}

fn knob_meter(ui: &mut egui::Ui, state: DialLevel, connected: bool) {
    let w = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(egui::vec2(w, 10.0), egui::Sense::hover());
    let readout_w = 44.0;
    let bar = egui::Rect::from_min_max(
        egui::pos2(rect.left(), rect.center().y - 2.0),
        egui::pos2((rect.right() - readout_w).max(rect.left() + 10.0), rect.center().y + 2.0),
    );
    let painter = ui.painter();
    let round = egui::Rounding::same(2.0);
    painter.rect_filled(bar, round, track());

    let (label, label_col) = if !connected {
        ("--".to_string(), text_faint())
    } else if state.muted {
        ("Muted".to_string(), text_muted())
    } else {
        (format!("{}%", (state.level * 100.0).round() as i32), text_muted())
    };

    if connected {
        let filled = bar.width() * state.level.clamp(0.0, 1.0);
        if filled > 1.0 {
            let fill_col = if state.muted { text_faint() } else { accent() };
            painter.rect_filled(
                egui::Rect::from_min_size(bar.min, egui::vec2(filled.max(4.0), bar.height())),
                round,
                fill_col,
            );
        }
    }

    painter.text(
        egui::pos2(rect.right(), rect.center().y),
        egui::Align2::RIGHT_CENTER,
        label,
        regular(11.5),
        label_col,
    );
}

fn status_pill(ui: &mut egui::Ui, status: SerialStatus) {
    let (label, tone, hint) = match status {
        SerialStatus::Connected => ("Connected", text_muted(), "RVCI is reading your board"),
        SerialStatus::InUse => ("Port in use", warning(), "Another program has the COM port open"),
        SerialStatus::NotFound => ("No device", text_faint(), "Nothing answered on that COM port"),
        SerialStatus::Error => ("Serial error", destructive(), "The port opened but stopped responding"),
        SerialStatus::Idle => ("Searching", text_faint(), "Looking for your board"),
    };
    let galley = ui
        .painter()
        .layout_no_wrap(label.to_string(), regular(12.5), tone);
    let (rect, resp) =
        ui.allocate_exact_size(egui::vec2(galley.size().x + 28.0, 27.0), egui::Sense::hover());
    ui.painter()
        .rect_filled(rect, egui::Rounding::same(13.5), pal().faint_bg);
    let pos = rect.center() - galley.size() / 2.0;
    ui.painter().galley(pos, galley, tone);
    resp.on_hover_text(hint);
}

fn chevron(painter: &egui::Painter, center: egui::Pos2, color: Color32, up: bool) {
    let half_w = 4.2;
    let half_h = if up { -2.6 } else { 2.6 };
    let stroke = egui::Stroke::new(1.7, color);
    let a = egui::pos2(center.x - half_w, center.y - half_h * 0.5);
    let b = egui::pos2(center.x, center.y + half_h * 0.5);
    let c = egui::pos2(center.x + half_w, center.y - half_h * 0.5);
    painter.line_segment([a, b], stroke);
    painter.line_segment([b, c], stroke);
}

#[derive(Clone, Copy, PartialEq)]
enum SelectStyle {
    Row,
    Inline,
}

fn select<S: AsRef<str>>(
    ui: &mut egui::Ui,
    id_src: impl std::hash::Hash,
    current: &str,
    width: f32,
    style: SelectStyle,
    options: &[S],
) -> Option<usize> {
    let height = if style == SelectStyle::Inline { 30.0 } else { 26.0 };
    let (rect, resp) =
        ui.allocate_exact_size(egui::vec2(width.max(56.0), height), egui::Sense::click());
    let popup_id = ui.make_persistent_id(id_src);
    let enabled = ui.is_enabled() && !options.is_empty();
    if resp.clicked() && enabled {
        ui.memory_mut(|m| m.toggle_popup(popup_id));
    }
    let open = enabled && ui.memory(|m| m.is_popup_open(popup_id));
    let hot = ui
        .ctx()
        .animate_bool(resp.id, (resp.hovered() && enabled) || open);

    let dim = |c: Color32| if enabled { c } else { mix(c, card_bg(), 0.55) };
    let rounding = egui::Rounding::same(9.0);
    let pad = if style == SelectStyle::Inline { 11.0 } else { 4.0 };
    let chev_w = 20.0;

    {
        let painter = ui.painter();
        match style {
            SelectStyle::Inline => {
                painter.rect_filled(rect, rounding, dim(lerp_color(widget_bg(), row_hover(), hot)));
            }
            SelectStyle::Row => {
                if hot > 0.0 {
                    let c = row_hover();
                    painter.rect_filled(
                        rect,
                        rounding,
                        Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), (hot * 80.0) as u8),
                    );
                }
            }
        }
        if resp.has_focus() {
            painter.rect_stroke(rect, rounding, egui::Stroke::new(2.0, accent()));
        }
    }

    let text_col = if style == SelectStyle::Inline { dim(text()) } else { dim(text_muted()) };
    let avail = (rect.width() - pad * 2.0 - chev_w - 4.0).max(10.0);
    let shown = elide(ui, current, regular(14.0), avail);
    let galley = ui
        .painter()
        .layout_no_wrap(shown, regular(14.0), text_col);
    let ty = rect.center().y - galley.size().y / 2.0;
    let tx = match style {
        SelectStyle::Inline => rect.left() + pad,
        SelectStyle::Row => rect.right() - pad - chev_w - galley.size().x,
    };
    ui.painter().galley(egui::pos2(tx, ty), galley, text_col);
    chevron(
        ui.painter(),
        egui::pos2(rect.right() - pad - chev_w * 0.5, rect.center().y),
        dim(text_faint()),
        open,
    );

    let mut picked = None;
    if open {
        egui::popup_below_widget(
            ui,
            popup_id,
            &resp,
            egui::PopupCloseBehavior::CloseOnClickOutside,
            |ui| {
                ui.spacing_mut().item_spacing.y = 2.0;
                egui::ScrollArea::vertical()
                    .max_height(260.0)
                    .show(ui, |ui| {
                        for (i, option) in options.iter().enumerate() {
                            let option = option.as_ref();
                            let selected = option == current;
                            if ui
                                .selectable_label(
                                    selected,
                                    RichText::new(option).font(regular(14.0)),
                                )
                                .clicked()
                            {
                                picked = Some(i);
                            }
                        }
                    });
            },
        );
        if picked.is_some() {
            ui.memory_mut(|m| m.close_popup());
        }
    }
    picked
}

fn switch(ui: &mut egui::Ui, on: &mut bool) -> egui::Response {
    let (rect, mut resp) = ui.allocate_exact_size(egui::vec2(40.0, 24.0), egui::Sense::click());
    if resp.clicked() {
        *on = !*on;
        resp.mark_changed();
    }
    let t = ui.ctx().animate_bool(resp.id, *on);
    let radius = rect.height() / 2.0;
    let rounding = egui::Rounding::same(radius);
    let fill = lerp_color(track(), accent(), t);
    let painter = ui.painter();
    painter.rect_filled(rect, rounding, fill);
    if resp.hovered() {
        painter.rect_stroke(rect, rounding, egui::Stroke::new(1.0, mix(fill, text(), 0.3)));
    }
    if resp.has_focus() {
        painter.rect_stroke(rect.expand(2.0), egui::Rounding::same(radius + 2.0), egui::Stroke::new(2.0, accent()));
    }
    let knob_r = radius - 2.5;
    let cx = egui::lerp((rect.left() + radius)..=(rect.right() - radius), t);
    let center = egui::pos2(cx, rect.center().y);
    painter.circle_filled(center + egui::vec2(0.0, 0.7), knob_r, Color32::from_black_alpha(50));
    painter.circle_filled(center, knob_r, Color32::WHITE);
    resp
}

fn switch_row(rows: &mut Rows, ui: &mut egui::Ui, label: &str, on: &mut bool) -> bool {
    rows.item(ui, |ui| {
        ui.horizontal(|ui| {
            label_cell(ui, label);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                switch(ui, on).changed()
            })
            .inner
        })
        .inner
    })
}

const APP_AUMID: &str = "TZey.RVCI";

fn register_toast_identity() {
    let key_path = format!("Software\\Classes\\AppUserModelId\\{}", APP_AUMID);
    if RegKey::predef(HKEY_CURRENT_USER).create_subkey(&key_path).is_err() {
        log_warn!("toast: could not register the AUMID, notifications may not appear");
    }
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
    let spawned = std::thread::Builder::new()
        .name("toast".to_string())
        .spawn(move || {
            log_info!("toast: showing \"{title}\"");
            if let Err(e) = show_toast(&title, &body) {
                log_warn!("toast: could not show \"{title}\": {e}");
            }
        });
    if spawned.is_err() {
        log_error!("toast: could not start the notification thread");
    }
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

fn empty_hint(ui: &mut egui::Ui, message: &str) {
    ui.vertical_centered(|ui| {
        ui.add_space(12.0);
        ui.label(RichText::new(message).font(regular(13.0)).color(text_faint()));
        ui.add_space(12.0);
    });
}

impl eframe::App for RvciApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        diag::set_console(self.cfg.debug_mode, "settings state");

        if self.want_show.swap(false, Ordering::SeqCst) {
            self.rescan();
        }

        if !self.user_opened.load(Ordering::SeqCst) {
            park_window_native(self.hwnd);
            return;
        }

        while let Ok(procs) = self.proc_rx.try_recv() {
            self.active_processes = procs;
        }

        if ctx.input(|i| i.viewport().close_requested()) {
            log_info!("ui: settings window closed from the title bar, parking to the tray");
            ctx.send_viewport_cmd(ViewportCommand::CancelClose);
            self.user_opened.store(false, Ordering::SeqCst);
            park_window_native(self.hwnd);
        }

        self.handle_key_capture(ctx);
        self.show_main_panel(ctx);

        if self.save_flash.is_some() {
            ctx.request_repaint_after(Duration::from_millis(50));
        } else if !self.cfg.dials.is_empty() {
            ctx.request_repaint_after(Duration::from_millis(66));
        }
    }
}

const PARK_POS: i32 = -32000;

fn show_window_native(hwnd: isize) {
    if hwnd == 0 {
        return;
    }
    use windows::Win32::Foundation::{HWND, RECT};
    use windows::Win32::UI::WindowsAndMessaging::{
        GetWindowLongPtrW, GetWindowRect, IsIconic, SetForegroundWindow, SetWindowLongPtrW,
        SetWindowPos, ShowWindow, SystemParametersInfoW, GWL_EXSTYLE, SPI_GETWORKAREA,
        SWP_FRAMECHANGED, SWP_NOSIZE, SWP_NOZORDER, SW_HIDE, SW_RESTORE, SW_SHOW,
        SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, WS_EX_APPWINDOW, WS_EX_TOOLWINDOW,
    };
    let hwnd = HWND(hwnd as *mut c_void);
    unsafe {
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
        if ex & WS_EX_TOOLWINDOW.0 != 0 {
            let _ = ShowWindow(hwnd, SW_HIDE);
            let restored = (ex & !WS_EX_TOOLWINDOW.0) | WS_EX_APPWINDOW.0;
            SetWindowLongPtrW(hwnd, GWL_EXSTYLE, restored as isize);
        }

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
            let _ = SetWindowPos(hwnd, None, x, y, 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_FRAMECHANGED);
        }
        if IsIconic(hwnd).as_bool() {
            let _ = ShowWindow(hwnd, SW_RESTORE);
        } else {
            let _ = ShowWindow(hwnd, SW_SHOW);
        }
        let _ = SetForegroundWindow(hwnd);
    }
}

fn park_window_native(hwnd: isize) {
    if hwnd == 0 {
        return;
    }
    use windows::Win32::Foundation::{HWND, RECT};
    use windows::Win32::UI::WindowsAndMessaging::{
        GetWindowLongPtrW, GetWindowRect, SetWindowLongPtrW, SetWindowPos, ShowWindow,
        GWL_EXSTYLE, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER, SW_HIDE,
        SW_SHOWNOACTIVATE, WS_EX_APPWINDOW, WS_EX_TOOLWINDOW,
    };
    let hwnd = HWND(hwnd as *mut c_void);
    unsafe {
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
        let mut rc = RECT::default();
        let off_screen = GetWindowRect(hwnd, &mut rc).is_ok() && rc.left <= -20000;
        if off_screen && ex & WS_EX_TOOLWINDOW.0 != 0 {
            return;
        }

        let _ = ShowWindow(hwnd, SW_HIDE);
        let parked = (ex & !WS_EX_APPWINDOW.0) | WS_EX_TOOLWINDOW.0;
        SetWindowLongPtrW(hwnd, GWL_EXSTYLE, parked as isize);
        let _ = SetWindowPos(
            hwnd,
            None,
            PARK_POS,
            PARK_POS,
            0,
            0,
            SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
        );
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
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

    let rounding = egui::Rounding::same(9.0);
    for w in [
        &mut visuals.widgets.noninteractive,
        &mut visuals.widgets.inactive,
        &mut visuals.widgets.hovered,
        &mut visuals.widgets.active,
        &mut visuals.widgets.open,
    ] {
        w.rounding = rounding;
    }
    visuals.window_rounding = egui::Rounding::same(16.0);
    visuals.menu_rounding = egui::Rounding::same(12.0);
    ctx.set_visuals(visuals);

    let mut style = (*ctx.style()).clone();
    use egui::TextStyle;
    style.text_styles = [
        (TextStyle::Heading, semibold(22.0)),
        (TextStyle::Body, regular(14.5)),
        (TextStyle::Button, regular(14.5)),
        (TextStyle::Monospace, egui::FontId::new(13.0, egui::FontFamily::Monospace)),
        (TextStyle::Small, regular(12.0)),
    ]
    .into();
    style.spacing.item_spacing = egui::vec2(9.0, 9.0);
    style.spacing.button_padding = egui::vec2(11.0, 6.0);
    style.spacing.interact_size.y = 28.0;
    style.spacing.combo_width = 180.0;
    style.spacing.window_margin = egui::Margin::same(0.0);
    style.spacing.menu_margin = egui::Margin::same(6.0);
    style.visuals.clip_rect_margin = 3.0;
    ctx.set_style(style);
}

fn elide(ui: &egui::Ui, text: &str, font: egui::FontId, max_w: f32) -> String {
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

fn filled_button(
    ui: &mut egui::Ui,
    text: &str,
    base: Color32,
    hover: Color32,
    min: egui::Vec2,
) -> egui::Response {
    let galley = ui
        .painter()
        .layout_no_wrap(text.to_string(), regular(14.5), Color32::PLACEHOLDER);
    let pad = egui::vec2(16.0, 8.0);
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
    let rounding = egui::Rounding::same(rect.height() * 0.5);
    ui.painter().rect_filled(rect, rounding, fill);
    if resp.has_focus() {
        ui.painter().rect_stroke(
            rect.expand(2.0),
            egui::Rounding::same(rect.height() * 0.5 + 2.0),
            egui::Stroke::new(2.0, accent()),
        );
    }
    let text_pos = rect.center() - galley.size() / 2.0;
    ui.painter().galley(text_pos, galley, contrast_text(fill));
    resp
}

fn ghost_button(ui: &mut egui::Ui, label: &str, min: egui::Vec2) -> egui::Response {
    let galley = ui
        .painter()
        .layout_no_wrap(label.to_string(), regular(14.0), Color32::PLACEHOLDER);
    let pad = egui::vec2(15.0, 7.0);
    let desired = egui::vec2(
        (galley.size().x + pad.x * 2.0).max(min.x),
        (galley.size().y + pad.y * 2.0).max(min.y),
    );
    let (rect, resp) = ui.allocate_exact_size(desired, egui::Sense::click());
    let rounding = egui::Rounding::same(rect.height() * 0.5);
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
            rounding,
            Color32::from_rgba_unmultiplied(fill.r(), fill.g(), fill.b(), a),
        );
    }
    ui.painter()
        .rect_stroke(rect, rounding, egui::Stroke::new(1.0, card_border()));
    if resp.has_focus() {
        ui.painter().rect_stroke(
            rect.expand(2.0),
            egui::Rounding::same(rect.height() * 0.5 + 2.0),
            egui::Stroke::new(2.0, accent()),
        );
    }
    let pos = rect.center() - galley.size() / 2.0;
    ui.painter().galley(pos, galley, text());
    resp
}

fn text_button(ui: &mut egui::Ui, label: &str) -> egui::Response {
    let galley = ui
        .painter()
        .layout_no_wrap(label.to_string(), regular(13.5), Color32::PLACEHOLDER);
    let pad = egui::vec2(10.0, 5.0);
    let desired = galley.size() + pad * 2.0;
    let (rect, resp) = ui.allocate_exact_size(desired, egui::Sense::click());
    let t = ui.ctx().animate_bool(resp.id, resp.hovered());
    if t > 0.0 {
        let a = accent();
        ui.painter().rect_filled(
            rect,
            egui::Rounding::same(rect.height() * 0.5),
            Color32::from_rgba_unmultiplied(a.r(), a.g(), a.b(), (t * 30.0) as u8),
        );
    }
    if resp.has_focus() {
        ui.painter().rect_stroke(
            rect,
            egui::Rounding::same(rect.height() * 0.5),
            egui::Stroke::new(2.0, accent()),
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
            egui::Rounding::same(13.0),
            Color32::from_rgba_unmultiplied(d.r(), d.g(), d.b(), (t * 45.0) as u8),
        );
    }
    if resp.has_focus() {
        ui.painter()
            .rect_stroke(rect, egui::Rounding::same(13.0), egui::Stroke::new(2.0, accent()));
    }
    let col = lerp_color(text_faint(), destructive(), t);
    let c = rect.center();
    let s = egui::Stroke::new(1.6, col);
    let d = 4.6;
    ui.painter()
        .line_segment([c + egui::vec2(-d, -d), c + egui::vec2(d, d)], s);
    ui.painter()
        .line_segment([c + egui::vec2(d, -d), c + egui::vec2(-d, d)], s);
    resp.on_hover_text("Remove")
}

fn lerp_color(a: Color32, b: Color32, t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let l = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t) as u8;
    Color32::from_rgb(l(a.r(), b.r()), l(a.g(), b.g()), l(a.b(), b.b()))
}

const DIAL_TYPES: [&str; 5] = ["system", "process", "all_others", "microphone", "output_device"];
const DIAL_TYPE_LABELS: [&str; 5] = ["System", "Process", "Others", "Microphone", "Output device"];
const ACTIONS: [&str; 4] = ["none", "mute_dial", "media", "keys"];
const ACTION_LABELS: [&str; 4] = ["None", "Mute knob", "Media", "Keys"];

fn dial_type_label(t: &str) -> &'static str {
    DIAL_TYPES
        .iter()
        .position(|d| *d == t)
        .map(|i| DIAL_TYPE_LABELS[i])
        .unwrap_or(DIAL_TYPE_LABELS[0])
}

fn action_label(a: &str) -> &'static str {
    ACTIONS
        .iter()
        .position(|x| *x == a)
        .map(|i| ACTION_LABELS[i])
        .unwrap_or(ACTION_LABELS[0])
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
    use super::{accent, is_light_color, mix, pal, OsdMsg};
    use std::sync::mpsc::Receiver;
    use std::time::{Duration, Instant};

    use egui::Color32;
    use windows::core::w;
    use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM};
    use windows::Win32::Graphics::Gdi::{
        CreateCompatibleDC, CreateDIBSection, CreateFontW, DeleteDC, DeleteObject, DrawTextW,
        SelectObject, SetBkMode, SetTextColor, AC_SRC_ALPHA, AC_SRC_OVER, ANTIALIASED_QUALITY,
        BITMAPINFO, BITMAPINFOHEADER, BI_RGB, BLENDFUNCTION, DEFAULT_CHARSET, DEFAULT_PITCH,
        DIB_RGB_COLORS, DT_LEFT, DT_RIGHT, DT_SINGLELINE, DT_VCENTER, FF_DONTCARE,
        FONT_CLIP_PRECISION, FW_NORMAL, FW_SEMIBOLD, HBITMAP, HDC, HFONT, HGDIOBJ, OUT_TT_PRECIS,
        TRANSPARENT,
    };
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::HiDpi::GetDpiForWindow;
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DispatchMessageW, LoadCursorW, PeekMessageW,
        RegisterClassExW, SetWindowPos, ShowWindow, SystemParametersInfoW, TranslateMessage,
        UpdateLayeredWindow, HWND_TOPMOST, IDC_ARROW, MSG, PM_REMOVE, SPI_GETWORKAREA,
        SWP_NOACTIVATE, SW_HIDE, SW_SHOWNOACTIVATE, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, ULW_ALPHA,
        WINDOW_EX_STYLE, WNDCLASSEXW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
        WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
    };

    const BASE_W: f32 = 320.0;
    const BASE_H: f32 = 90.0;
    const BASE_MARGIN: f32 = 28.0;
    const CORNER: f32 = 20.0;
    const SHADOW: f32 = 14.0;
    const BORDER_W: f32 = 1.3;

    const FADE_IN_MS: f32 = 120.0;
    const FADE_OUT_MS: f32 = 200.0;
    const HOLD_MS: u64 = 1500;
    const FRAME_MS: u64 = 16;
    const INV255: f32 = 1.0 / 255.0;

    struct Layer {
        dc: HDC,
        bitmap: HBITMAP,
        old: HGDIOBJ,
        bits: *mut u32,
    }

    impl Layer {
        unsafe fn new(w: i32, h: i32) -> Option<Self> {
            let dc = CreateCompatibleDC(None);
            if dc.is_invalid() {
                return None;
            }
            let mut info = BITMAPINFO::default();
            info.bmiHeader = BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            };
            let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
            let bitmap =
                match CreateDIBSection(Some(dc), &info, DIB_RGB_COLORS, &mut bits, None, 0) {
                    Ok(b) if !bits.is_null() => b,
                    _ => {
                        let _ = DeleteDC(dc);
                        return None;
                    }
                };
            let old = SelectObject(dc, bitmap.into());
            Some(Self { dc, bitmap, old, bits: bits as *mut u32 })
        }

        unsafe fn destroy(self) {
            SelectObject(self.dc, self.old);
            let _ = DeleteObject(self.bitmap.into());
            let _ = DeleteDC(self.dc);
        }
    }

    struct Canvas {
        surface: Layer,
        glyphs: Layer,
        w: i32,
        h: i32,
        inner: Vec<u8>,
        edge: Vec<u8>,
        halo: Vec<u8>,
        bar: Vec<u8>,
    }

    impl Canvas {
        unsafe fn new(w: i32, h: i32, scale: f32) -> Option<Self> {
            let surface = Layer::new(w, h)?;
            let glyphs = match Layer::new(w, h) {
                Some(l) => l,
                None => {
                    surface.destroy();
                    return None;
                }
            };
            let (inner, edge, halo) = build_masks(w, h, scale);
            Some(Self {
                surface,
                glyphs,
                w,
                h,
                inner,
                edge,
                halo,
                bar: vec![0u8; (w * h) as usize],
            })
        }

        unsafe fn destroy(self) {
            self.surface.destroy();
            self.glyphs.destroy();
        }
    }

    fn panel_rect(w: i32, h: i32, scale: f32) -> (f32, f32, f32, f32) {
        let side = SHADOW * scale * 0.5;
        (side, side * 0.6, w as f32 - side, h as f32 - side * 1.5)
    }

    fn corner_distance(x: f32, y: f32, rect: (f32, f32, f32, f32), radius: f32) -> f32 {
        let (l, t, r, b) = rect;
        let cx = (l + radius - x).max(x - (r - radius)).max(0.0);
        let cy = (t + radius - y).max(y - (b - radius)).max(0.0);
        (cx * cx + cy * cy).sqrt() - radius
    }

    fn coverage(x: f32, y: f32, rect: (f32, f32, f32, f32), radius: f32) -> f32 {
        (0.5 - corner_distance(x, y, rect, radius)).clamp(0.0, 1.0)
    }

    fn build_masks(w: i32, h: i32, scale: f32) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
        let outer = panel_rect(w, h, scale);
        let radius = CORNER * scale;
        let bw = BORDER_W * scale;
        let inner_rect = (outer.0 + bw, outer.1 + bw, outer.2 - bw, outer.3 - bw);
        let inner_radius = (radius - bw).max(1.0);
        let glow = (
            outer.0 - 1.0 * scale,
            outer.1 + 3.0 * scale,
            outer.2 + 1.0 * scale,
            outer.3 + 5.0 * scale,
        );
        let reach = SHADOW * scale;

        let mut inner = vec![0u8; (w * h) as usize];
        let mut edge = vec![0u8; (w * h) as usize];
        let mut halo = vec![0u8; (w * h) as usize];
        for y in 0..h {
            for x in 0..w {
                let fx = x as f32 + 0.5;
                let fy = y as f32 + 0.5;
                let idx = (y * w + x) as usize;
                let out_cov = coverage(fx, fy, outer, radius);
                let in_cov = coverage(fx, fy, inner_rect, inner_radius);
                inner[idx] = (in_cov * 255.0) as u8;
                edge[idx] = ((out_cov - in_cov).clamp(0.0, 1.0) * 255.0) as u8;
                if out_cov < 1.0 {
                    let d = corner_distance(fx, fy, glow, radius + 1.0 * scale).max(0.0);
                    let s = (1.0 - (d / reach).clamp(0.0, 1.0)).powi(2);
                    halo[idx] = ((1.0 - out_cov) * s * 255.0) as u8;
                }
            }
        }
        (inner, edge, halo)
    }

    struct Fonts {
        title: HFONT,
        value: HFONT,
    }

    impl Fonts {
        unsafe fn new(scale: f32) -> Self {
            let make = |px: f32, weight: i32, face: windows::core::PCWSTR| {
                CreateFontW(
                    -((px * scale).round() as i32),
                    0,
                    0,
                    0,
                    weight,
                    0,
                    0,
                    0,
                    DEFAULT_CHARSET,
                    OUT_TT_PRECIS,
                    FONT_CLIP_PRECISION(0),
                    ANTIALIASED_QUALITY,
                    (DEFAULT_PITCH.0 | FF_DONTCARE.0) as u32,
                    face,
                )
            };
            Self {
                title: make(15.5, FW_SEMIBOLD.0 as i32, w!("Segoe UI Semibold")),
                value: make(13.0, FW_NORMAL.0 as i32, w!("Segoe UI")),
            }
        }

        unsafe fn destroy(self) {
            let _ = DeleteObject(self.title.into());
            let _ = DeleteObject(self.value.into());
        }
    }

    fn to_utf16(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }

    fn bgr(c: Color32) -> u32 {
        ((c.r() as u32) << 16) | ((c.g() as u32) << 8) | c.b() as u32
    }

    struct Skin {
        panel: Color32,
        panel_alpha: f32,
        border: Color32,
        border_alpha: f32,
        ink: Color32,
        sub: Color32,
        track: Color32,
        fill: Color32,
        shadow: f32,
    }

    fn skin(muted: bool) -> Skin {
        let p = pal();
        let light = !p.dark;
        let mono = super::osd_is_mono();

        let mut fill = if mono {
            if light {
                Color32::from_rgb(34, 36, 42)
            } else {
                Color32::from_rgb(240, 242, 246)
            }
        } else {
            accent()
        };
        if muted {
            fill = if light {
                Color32::from_rgb(150, 155, 165)
            } else {
                Color32::from_rgb(124, 130, 142)
            };
        } else if light && !mono && is_light_color(fill) {
            fill = mix(fill, Color32::BLACK, 0.22);
        }

        if light {
            let base = Color32::from_rgb(252, 252, 253);
            Skin {
                panel: if mono { base } else { mix(base, p.accent, 0.04) },
                panel_alpha: 0.80,
                border: Color32::from_rgb(0, 0, 0),
                border_alpha: 0.22,
                ink: Color32::from_rgb(18, 20, 26),
                sub: Color32::from_rgb(96, 102, 114),
                track: Color32::from_rgb(206, 210, 218),
                fill,
                shadow: 0.26,
            }
        } else {
            let base = Color32::from_rgb(21, 22, 27);
            Skin {
                panel: if mono { base } else { mix(base, p.accent, 0.12) },
                panel_alpha: 0.74,
                border: Color32::from_rgb(255, 255, 255),
                border_alpha: 0.30,
                ink: Color32::from_rgb(244, 246, 250),
                sub: Color32::from_rgb(168, 175, 188),
                track: Color32::from_rgb(96, 101, 114),
                fill,
                shadow: 0.50,
            }
        }
    }

    pub fn spawn(rx: Receiver<OsdMsg>) {
        let started = std::thread::Builder::new()
            .name("osd".to_string())
            .spawn(move || unsafe { run(rx) });
        if started.is_err() {
            log_error!("osd: could not start the OSD thread, the overlay is unavailable");
        }
    }

    unsafe fn run(rx: Receiver<OsdMsg>) {
        let Ok(hinstance) = GetModuleHandleW(None) else {
            log_error!("osd: GetModuleHandleW failed, the overlay cannot start");
            return;
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

        let ex_style: WINDOW_EX_STYLE = WS_EX_LAYERED
            | WS_EX_TRANSPARENT
            | WS_EX_TOPMOST
            | WS_EX_TOOLWINDOW
            | WS_EX_NOACTIVATE;

        let Ok(hwnd) = CreateWindowExW(
            ex_style,
            class_name,
            w!("RVCI OSD"),
            WS_POPUP,
            0,
            0,
            1,
            1,
            None,
            None,
            Some(hinstance.into()),
            None,
        ) else {
            log_error!("osd: CreateWindowExW failed, the overlay cannot start");
            return;
        };
        log_info!("osd: overlay window created");

        let mut scale = 0.0f32;
        let mut canvas: Option<Canvas> = None;
        let mut fonts: Option<Fonts> = None;

        let mut label = String::new();
        let mut target = 0.0f32;
        let mut shown = 0.0f32;
        let mut muted = false;

        let mut visible = false;
        let mut pending_show = false;
        let mut holding = false;
        let mut alpha = 0.0f32;
        let mut deadline = Instant::now();
        let mut msg = MSG::default();

        loop {
            let timeout = if visible {
                Duration::from_millis(FRAME_MS)
            } else {
                Duration::from_millis(250)
            };
            let mut got = match rx.recv_timeout(timeout) {
                Ok(m) => Some(m),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => None,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            };
            while let Ok(m) = rx.try_recv() {
                got = Some(m);
            }

            if let Some(m) = got {
                let fresh = !visible;
                if fresh {
                    shown = m.level.clamp(0.0, 1.0);
                    alpha = 0.0;
                }
                label = m.label;
                target = m.level.clamp(0.0, 1.0);
                muted = m.muted;
                deadline = Instant::now() + Duration::from_millis(HOLD_MS);
                holding = true;

                if fresh {
                    visible = true;
                    pending_show = true;
                    let dpi = place_window(hwnd);
                    let want = (dpi as f32 / 96.0).clamp(1.0, 4.0);
                    if canvas.is_none() || (want - scale).abs() > 0.01 {
                        scale = want;
                        if let Some(c) = canvas.take() {
                            c.destroy();
                        }
                        if let Some(f) = fonts.take() {
                            f.destroy();
                        }
                        let w = (BASE_W * scale).round() as i32;
                        let h = (BASE_H * scale).round() as i32;
                        canvas = Canvas::new(w, h, scale);
                        fonts = Some(Fonts::new(scale));
                        if canvas.is_none() {
                            log_error!("osd: could not allocate a {w}x{h} surface, the overlay is disabled");
                        } else {
                            log_debug!("osd: surface {w}x{h} at {dpi} dpi (scale {scale:.2})");
                        }
                    }
                }
            }

            while PeekMessageW(&mut msg, Some(hwnd), 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }

            if !visible {
                continue;
            }

            let step = FRAME_MS as f32;
            if holding {
                alpha = (alpha + step / FADE_IN_MS).min(1.0);
                if Instant::now() >= deadline {
                    holding = false;
                }
            } else {
                alpha -= step / FADE_OUT_MS;
                if alpha <= 0.0 {
                    alpha = 0.0;
                    visible = false;
                    let _ = ShowWindow(hwnd, SW_HIDE);
                    continue;
                }
            }

            shown += (target - shown) * 0.3;
            if (target - shown).abs() < 0.002 {
                shown = target;
            }

            if let (Some(c), Some(f)) = (canvas.as_mut(), fonts.as_ref()) {
                draw(c, f, scale, &label, shown, muted, alpha);
                present(hwnd, c);
                if pending_show {
                    pending_show = false;
                    let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
                }
            } else {
                visible = false;
            }
        }

        log_info!("osd: channel closed, overlay thread exiting");
        if let Some(c) = canvas {
            c.destroy();
        }
        if let Some(f) = fonts {
            f.destroy();
        }
    }

    unsafe fn place_window(hwnd: HWND) -> u32 {
        let dpi = match GetDpiForWindow(hwnd) {
            0 => 96,
            d => d,
        };
        let scale = (dpi as f32 / 96.0).clamp(1.0, 4.0);
        let w = (BASE_W * scale).round() as i32;
        let h = (BASE_H * scale).round() as i32;
        let margin = (BASE_MARGIN * scale).round() as i32;

        let mut work = RECT::default();
        let ok = SystemParametersInfoW(
            SPI_GETWORKAREA,
            0,
            Some(&mut work as *mut RECT as *mut core::ffi::c_void),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
        .is_ok();
        let (x, y) = if ok && work.right > work.left {
            (
                work.left + (work.right - work.left - w) / 2,
                work.bottom - h - margin,
            )
        } else {
            ((1920 - w) / 2, 1040 - h - margin)
        };
        let _ = SetWindowPos(hwnd, Some(HWND_TOPMOST), x, y, w, h, SWP_NOACTIVATE);
        dpi
    }

    unsafe fn present(hwnd: HWND, canvas: &Canvas) {
        let size = SIZE { cx: canvas.w, cy: canvas.h };
        let src = POINT { x: 0, y: 0 };
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        let _ = UpdateLayeredWindow(
            hwnd,
            None,
            None,
            Some(&size),
            Some(canvas.surface.dc),
            Some(&src),
            COLORREF(0),
            Some(&blend),
            ULW_ALPHA,
        );
    }

    unsafe fn draw(
        canvas: &mut Canvas,
        fonts: &Fonts,
        scale: f32,
        label: &str,
        level: f32,
        muted: bool,
        fade: f32,
    ) {
        let s = skin(muted);
        let (w, h) = (canvas.w, canvas.h);
        let total = (w * h) as usize;
        let rect = panel_rect(w, h, scale);

        let surface = std::slice::from_raw_parts_mut(canvas.surface.bits, total);
        surface.fill(bgr(s.panel));
        canvas.bar.fill(0);

        let inner_l = rect.0 + 20.0 * scale;
        let inner_r = rect.2 - 20.0 * scale;
        let bar_h = (5.0 * scale).max(3.0);
        let bar_t = rect.3 - 22.0 * scale;
        let bar_b = bar_t + bar_h;
        let bar_radius = bar_h * 0.5;
        let fill_edge = if level > 0.001 {
            (inner_l + (inner_r - inner_l) * level.clamp(0.0, 1.0)).max(inner_l + bar_h)
        } else {
            inner_l - 1.0
        };

        {
            let track_bgr = bgr(s.track);
            let fill_bgr = bgr(s.fill);
            let y0 = (bar_t.floor() as i32).max(0);
            let y1 = (bar_b.ceil() as i32).min(h - 1);
            let x0 = (inner_l.floor() as i32).max(0);
            let x1 = (inner_r.ceil() as i32).min(w - 1);
            for y in y0..=y1 {
                for x in x0..=x1 {
                    let fx = x as f32 + 0.5;
                    let fy = y as f32 + 0.5;
                    let cov = coverage(fx, fy, (inner_l, bar_t, inner_r, bar_b), bar_radius);
                    if cov <= 0.0 {
                        continue;
                    }
                    let want = if fx <= fill_edge { fill_bgr } else { track_bgr };
                    let idx = (y * w + x) as usize;
                    surface[idx] = blend_bgr(surface[idx], want, cov);
                    canvas.bar[idx] = (cov * 255.0) as u8;
                }
            }
        }

        let glyph_bits = std::slice::from_raw_parts_mut(canvas.glyphs.bits, total);
        glyph_bits.fill(0);
        let gdc = canvas.glyphs.dc;
        SetBkMode(gdc, TRANSPARENT);
        let old_font = SelectObject(gdc, fonts.title.into());
        SetTextColor(gdc, COLORREF(0x00FF_FFFF));
        let mut title_rc = RECT {
            left: inner_l as i32,
            top: (rect.1 + 13.0 * scale) as i32,
            right: (inner_r - 66.0 * scale) as i32,
            bottom: (rect.1 + 41.0 * scale) as i32,
        };
        let mut title = to_utf16(label);
        if !title.is_empty() {
            DrawTextW(gdc, &mut title, &mut title_rc, DT_LEFT | DT_VCENTER | DT_SINGLELINE);
        }

        SelectObject(gdc, fonts.value.into());
        let readout = if muted {
            "Muted".to_string()
        } else {
            format!("{}%", (level * 100.0).round() as i32)
        };
        let mut readout_rc = RECT {
            left: (inner_r - 90.0 * scale) as i32,
            top: title_rc.top,
            right: inner_r as i32,
            bottom: title_rc.bottom,
        };
        let mut readout_w = to_utf16(&readout);
        DrawTextW(gdc, &mut readout_w, &mut readout_rc, DT_RIGHT | DT_VCENTER | DT_SINGLELINE);
        SelectObject(gdc, old_font);

        let title_split = title_rc.right.max(0) as usize;
        let ink = (s.ink.r() as f32, s.ink.g() as f32, s.ink.b() as f32);
        let sub = (s.sub.r() as f32, s.sub.g() as f32, s.sub.b() as f32);
        let border = (
            s.border.r() as f32,
            s.border.g() as f32,
            s.border.b() as f32,
        );

        for y in 0..h {
            for x in 0..w {
                let idx = (y * w + x) as usize;
                let in_cov = canvas.inner[idx] as f32 * INV255;
                let edge_cov = canvas.edge[idx] as f32 * INV255;
                let glow = canvas.halo[idx] as f32 * INV255 * s.shadow;
                let bar_cov = canvas.bar[idx] as f32 * INV255;
                let glyph = (glyph_bits[idx] & 0xFF) as f32 * INV255;

                if in_cov <= 0.0 && edge_cov <= 0.0 && glow <= 0.0 {
                    surface[idx] = 0;
                    continue;
                }

                let body = surface[idx];
                let br = ((body >> 16) & 0xFF) as f32;
                let bg_ = ((body >> 8) & 0xFF) as f32;
                let bb = (body & 0xFF) as f32;

                let fill_a = in_cov * s.panel_alpha;
                let border_a = edge_cov * s.border_alpha;
                let mut a = fill_a + border_a;
                let mut r = br * fill_a + border.0 * border_a;
                let mut g = bg_ * fill_a + border.1 * border_a;
                let mut b = bb * fill_a + border.2 * border_a;

                let solid = (bar_cov * in_cov).clamp(0.0, 1.0);
                if solid > 0.0 && solid > a {
                    let boost = solid - a;
                    a += boost;
                    r += br * boost;
                    g += bg_ * boost;
                    b += bb * boost;
                }

                if glyph > 0.0 {
                    let text_col = if (x as usize) >= title_split { sub } else { ink };
                    let keep = 1.0 - glyph;
                    a = glyph + a * keep;
                    r = text_col.0 * glyph + r * keep;
                    g = text_col.1 * glyph + g * keep;
                    b = text_col.2 * glyph + b * keep;
                }

                if glow > 0.0 {
                    a += glow * (1.0 - a);
                }

                let a = (a * fade).clamp(0.0, 1.0);
                let scale_rgb = fade;
                let pack = |v: f32| ((v * scale_rgb) as u32).min(255);
                surface[idx] = (((a * 255.0) as u32) << 24) | (pack(r) << 16) | (pack(g) << 8) | pack(b);
            }
        }
    }

    fn blend_bgr(dst: u32, src: u32, t: f32) -> u32 {
        let t = t.clamp(0.0, 1.0);
        let channel = |shift: u32| {
            let a = ((dst >> shift) & 0xFF) as f32;
            let b = ((src >> shift) & 0xFF) as f32;
            ((a + (b - a) * t) as u32).min(255) << shift
        };
        channel(16) | channel(8) | channel(0)
    }

    unsafe extern "system" fn wndproc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        DefWindowProcW(hwnd, msg, wparam, lparam)
    }
}


fn main() -> Result<()> {
    diag::init(get_log_dir());
    log_info!("app: RVCI {} starting", env!("CARGO_PKG_VERSION"));
    for line in diag::environment().lines() {
        let line = line.trim_end();
        if !line.is_empty() {
            log_info!("env: {line}");
        }
    }

    if !acquire_single_instance() {
        log_warn!("app: another RVCI instance already holds the single-instance mutex, exiting");
        diag::shutdown("duplicate instance");
        return Ok(());
    }
    log_info!("app: single instance mutex acquired");

    register_toast_identity();

    let path = get_config_path();
    log_info!("config: using {}", path.display());

    match load_config(&path) {
        ConfigLoad::Loaded(cfg) => {
            log_info!(
                "config: loaded, port={} baud={} dials={} buttons={} osd={} style={} theme={} debug_console={}",
                cfg.serial.port,
                cfg.serial.baud,
                cfg.dials.len(),
                cfg.buttons.len(),
                cfg.enable_osd,
                cfg.osd_style,
                cfg.theme,
                cfg.debug_mode
            );
            diag::set_console(cfg.debug_mode, "startup, persisted setting");
        }
        ConfigLoad::Missing => {
            log_warn!("config: no file yet, starting from defaults");
            diag::set_console(false, "startup, no config file");
        }
        ConfigLoad::Unreadable => {
            log_error!("config: unreadable, starting from defaults");
            diag::set_console(false, "startup, unreadable config");
        }
    }

    let path_clone = path.clone();
    let (osd_tx, osd_rx) = std::sync::mpsc::channel::<OsdMsg>();

    let link = UiLink::new(Arc::new(AtomicBool::new(false)));
    let link_worker = link.clone();

    osd::spawn(osd_rx);

    if std::thread::Builder::new()
        .name("serial".to_string())
        .spawn(move || run_volume_logic_loop(path_clone, osd_tx, link_worker))
        .is_err()
    {
        log_error!("app: could not start the serial thread, volume control is dead");
    }

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

    log_info!("ui: handing control to eframe");
    diag::set_phase("running");
    let run = eframe::run_native(
        "RVCI",
        native_options,
        Box::new(move |cc| Ok(Box::new(RvciApp::new(cc, path, link)))),
    );

    if let Err(e) = run {
        log_error!("ui: eframe exited with an error: {e}");
        diag::shutdown("eframe error");
        return Err(anyhow::anyhow!("eframe error: {e}"));
    }
    diag::shutdown("event loop ended");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_with(dials: usize, buttons: Vec<ButtonConfig>) -> AppConfig {
        let mut cfg = AppConfig::default();
        cfg.dials = (0..dials)
            .map(|_| DialConfig {
                dial_type: "system".to_string(),
                process_name: None,
                inverted: false,
            })
            .collect();
        cfg.buttons = buttons;
        cfg
    }

    fn mute_button(dial_index: usize) -> ButtonConfig {
        ButtonConfig {
            action: "mute_dial".to_string(),
            dial_index,
            ..ButtonConfig::default()
        }
    }

    #[test]
    fn gate_seeds_on_first_sample_without_showing() {
        let mut gate = OsdGate::default();
        assert!(!gate.sample(0, 400.0, false));
        assert!(!gate.sample(0, 405.0, false));
    }

    #[test]
    fn gate_shows_only_after_a_real_move() {
        let mut gate = OsdGate::default();
        gate.sample(0, 400.0, false);
        assert!(!gate.sample(0, 410.0, false), "10 counts is inside the dead band");
        assert!(gate.sample(0, 440.0, false), "40 counts should show the OSD");
    }

    #[test]
    fn gate_shows_immediately_when_mute_flips() {
        let mut gate = OsdGate::default();
        gate.sample(0, 400.0, false);
        assert!(gate.mute_changed(0, true), "muting must push an OSD right away");
        assert!(!gate.mute_changed(0, true), "no repeat for the same state");
        assert!(gate.mute_changed(0, false), "unmuting must push an OSD too");
    }

    #[test]
    fn gate_does_not_double_fire_after_a_mute_press() {
        let mut gate = OsdGate::default();
        gate.sample(0, 400.0, false);
        assert!(gate.mute_changed(0, true));
        assert!(
            !gate.sample(0, 402.0, true),
            "the value line right after the button must not repeat the OSD"
        );
    }

    #[test]
    fn gate_keeps_mute_state_while_the_knob_moves() {
        let mut gate = OsdGate::default();
        gate.sample(0, 100.0, false);
        assert!(gate.mute_changed(0, true));
        assert!(gate.sample(0, 300.0, true), "moving while muted still shows the OSD");
        assert!(gate.sample(0, 300.0, false), "releasing mute shows it again");
    }

    #[test]
    fn gate_tracks_dials_independently() {
        let mut gate = OsdGate::default();
        gate.sample(0, 100.0, false);
        gate.sample(3, 900.0, false);
        assert!(gate.mute_changed(3, true));
        assert!(!gate.sample(0, 100.0, false), "dial 0 is untouched");
    }

    #[test]
    fn mute_follows_the_button_bound_to_that_dial() {
        let cfg = cfg_with(3, vec![mute_button(1), mute_button(2)]);
        assert!(!dial_is_muted(&cfg, &[false, false], 1));
        assert!(dial_is_muted(&cfg, &[true, false], 1));
        assert!(!dial_is_muted(&cfg, &[true, false], 2));
        assert!(dial_is_muted(&cfg, &[true, true], 2));
        assert!(!dial_is_muted(&cfg, &[true, true], 0));
    }

    #[test]
    fn only_dials_with_a_mute_button_are_ever_written() {
        let cfg = cfg_with(3, vec![mute_button(1)]);
        assert!(!dial_has_mute_button(&cfg, 0));
        assert!(dial_has_mute_button(&cfg, 1));
        assert!(!dial_has_mute_button(&cfg, 2));
    }

    #[test]
    fn unbound_dials_never_touch_the_device_mute() {
        let d = apply_decision(0.5, 0.5, false, None, false);
        assert!(!d.mute_pending, "a dial with no mute button must not call SetMute");
        assert!(!d.any());
    }

    #[test]
    fn mute_transition_forces_an_apply_even_when_the_knob_is_still() {
        let d = apply_decision(0.5, 0.5, true, Some(false), true);
        assert!(!d.level_changed);
        assert!(d.mute_pending);
        assert!(d.any());
    }

    #[test]
    fn mute_is_written_once_per_transition() {
        let d = apply_decision(0.5, 0.5, true, Some(true), true);
        assert!(!d.mute_pending, "already applied, must not retry every frame");
    }

    #[test]
    fn level_dead_band_still_holds() {
        assert!(!apply_decision(0.500, 0.497, false, None, false).level_changed);
        assert!(apply_decision(0.520, 0.497, false, None, false).level_changed);
        assert!(apply_decision(0.0, -1.0, false, None, false).level_changed);
    }

    #[test]
    fn osd_label_skips_unassigned_dials() {
        let sys = DialConfig { dial_type: "system".into(), process_name: None, inverted: false };
        let others = DialConfig { dial_type: "all_others".into(), process_name: None, inverted: false };
        let none = DialConfig {
            dial_type: "process".into(),
            process_name: Some("None".into()),
            inverted: false,
        };
        let empty = DialConfig { dial_type: "process".into(), process_name: None, inverted: false };
        let app = DialConfig {
            dial_type: "process".into(),
            process_name: Some("Spotify.EXE".into()),
            inverted: false,
        };
        assert_eq!(osd_label(&sys).as_deref(), Some("Master Volume"));
        assert_eq!(osd_label(&others).as_deref(), Some("Other Apps"));
        assert_eq!(osd_label(&none), None);
        assert_eq!(osd_label(&empty), None);
        assert_eq!(osd_label(&app).as_deref(), Some("Spotify"));
    }

    #[test]
    fn media_keys_that_move_system_volume_are_recognised() {
        assert!(is_system_volume_key("vol_mute"));
        assert!(is_system_volume_key("vol_up"));
        assert!(is_system_volume_key("vol_down"));
        assert!(!is_system_volume_key("play_pause"));
        assert!(!is_system_volume_key("next"));
    }

    #[test]
    fn negative_or_nan_pot_range_cannot_panic_the_clamp() {
        for bad in [-5.0f32, 0.0, f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let mut cfg = AppConfig::default();
            cfg.value_max = bad;
            sanitize_config(&mut cfg);
            assert!(cfg.value_max.is_finite() && cfg.value_max >= 1.0, "bad input {bad}");
            let _ = 512.0f32.clamp(0.0, cfg.value_max);
        }
    }

    #[test]
    fn config_limits_are_enforced() {
        let mut cfg = AppConfig::default();
        cfg.serial.baud = 0;
        cfg.serial.timeout = 10_000_000;
        cfg.serial.port = "C".repeat(4096);
        cfg.dials = (0..500)
            .map(|_| DialConfig {
                dial_type: "system".into(),
                process_name: None,
                inverted: false,
            })
            .collect();
        cfg.buttons = (0..500).map(|_| ButtonConfig::default()).collect();
        sanitize_config(&mut cfg);
        assert_eq!(cfg.serial.baud, 115200);
        assert_eq!(cfg.serial.timeout, 5000);
        assert_eq!(cfg.serial.port.chars().count(), MAX_NAME_CHARS);
        assert_eq!(cfg.dials.len(), MAX_DIALS);
        assert_eq!(cfg.buttons.len(), MAX_BUTTONS);
    }

    #[test]
    fn config_drops_key_tokens_that_cannot_be_sent() {
        let mut cfg = cfg_with(2, vec![ButtonConfig {
            action: "keys".into(),
            modifiers: vec![
                "ctrl".into(),
                "not-a-key".into(),
                "shift".into(),
                "also-bogus".into(),
                "alt".into(),
                "win".into(),
            ],
            key_combo: "definitely-not-a-key".into(),
            media_key: "bogus".into(),
            dial_index: 99,
        }]);
        sanitize_config(&mut cfg);
        let b = &cfg.buttons[0];
        assert!(b.modifiers.iter().all(|m| token_to_vk(m).is_some()));
        assert!(b.key_combo.is_empty());
        assert_eq!(b.media_key, MEDIA_TOKENS[0]);
        assert_eq!(b.dial_index, 1, "dial_index must be clamped into range");
    }

    #[test]
    fn dial_index_is_zero_when_there_are_no_dials() {
        let mut cfg = cfg_with(0, vec![mute_button(7)]);
        sanitize_config(&mut cfg);
        assert_eq!(cfg.buttons[0].dial_index, 0);
    }

    #[test]
    fn strip_exe_is_char_boundary_safe() {
        assert_eq!(strip_exe("Spotify.exe"), "Spotify");
        assert_eq!(strip_exe("Spotify.EXE"), "Spotify");
        assert_eq!(strip_exe("Spotify"), "Spotify");
        assert_eq!(strip_exe(".exe"), "");
        assert_eq!(strip_exe("exe"), "exe");
        assert_eq!(strip_exe("bcde"), "bcde");
    }

    #[test]
    fn clip_chars_never_splits_a_character() {
        let mut s = "abcdef".to_string();
        clip_chars(&mut s, 3);
        assert_eq!(s, "abc");
        let mut t = "abc".to_string();
        clip_chars(&mut t, 10);
        assert_eq!(t, "abc");
        let mut wide = String::from_utf8(vec![0xE6, 0x97, 0xA5, 0xE6, 0x9C, 0xAC, 0xE8, 0xAA, 0x9E]).unwrap();
        clip_chars(&mut wide, 2);
        assert_eq!(wide.chars().count(), 2);
        assert_eq!(wide.len(), 6);
    }

    #[test]
    fn key_tokens_map_to_a_fixed_whitelist() {
        assert_eq!(token_to_vk("a"), Some((0x41, false)));
        assert_eq!(token_to_vk("ctrl"), Some((0xA2, false)));
        assert_eq!(token_to_vk("vol_mute"), Some((0xAD, true)));
        assert_eq!(token_to_vk(""), None);
        assert_eq!(token_to_vk("rm -rf /"), None);
        assert_eq!(token_to_vk("0x41"), None);
    }

    #[test]
    fn toast_text_is_xml_escaped() {
        let out = xml_escape("COM1 <b>&x</b>");
        assert!(!out.contains('<'));
        assert!(!out.contains('>'));
        assert_eq!(out, "COM1 &lt;b&gt;&amp;x&lt;/b&gt;");
    }

    #[test]
    fn device_names_in_parentheses_are_unwrapped() {
        assert_eq!(extract_clean_name("Speakers (Realtek Audio)"), "Realtek Audio");
        assert_eq!(extract_clean_name("None"), "None");
        assert_eq!(extract_clean_name("Plain Name"), "Plain Name");
    }

    #[test]
    fn every_dial_type_and_action_has_a_label() {
        assert_eq!(DIAL_TYPES.len(), DIAL_TYPE_LABELS.len());
        assert_eq!(ACTIONS.len(), ACTION_LABELS.len());
        assert_eq!(MEDIA_TOKENS.len(), MEDIA_LABELS.len());
        for t in DIAL_TYPES {
            assert!(!dial_type_label(t).is_empty(), "{t}");
        }
        for a in ACTIONS {
            assert!(!action_label(a).is_empty(), "{a}");
        }
        for m in MEDIA_TOKENS {
            assert!(token_to_vk(m).is_some(), "{m} is not sendable");
        }
    }

    #[test]
    fn saved_config_round_trips_through_disk() {
        let dir = std::env::temp_dir().join("rvci-test-roundtrip");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("mapping.json");
        let _ = std::fs::remove_file(&path);

        let mut cfg = cfg_with(2, vec![mute_button(1)]);
        cfg.serial.port = "COM7".into();
        cfg.theme = "Emerald".into();
        assert!(save_config(&path, &cfg));

        match load_config(&path) {
            ConfigLoad::Loaded(back) => {
                assert_eq!(back.serial.port, "COM7");
                assert_eq!(back.theme, "Emerald");
                assert_eq!(back.dials.len(), 2);
                assert_eq!(back.buttons.len(), 1);
            }
            _ => panic!("config did not load back"),
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_corrupt_config_is_reported_not_silently_replaced() {
        let dir = std::env::temp_dir().join("rvci-test-corrupt");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("mapping.json");
        std::fs::write(&path, b"{ this is not json").unwrap();
        assert!(matches!(load_config(&path), ConfigLoad::Unreadable));
        let _ = std::fs::remove_file(&path);
        assert!(matches!(load_config(&path), ConfigLoad::Missing));
    }

    #[test]
    fn an_oversized_config_is_rejected_instead_of_loaded() {
        let dir = std::env::temp_dir().join("rvci-test-huge");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("mapping.json");
        let mut blob = String::from("{\"serial\":{\"port\":\"");
        blob.push_str(&"A".repeat((MAX_CONFIG_BYTES as usize) + 4096));
        blob.push_str("\",\"baud\":9600,\"timeout\":50},\"value_max\":1.0,\"work_device_1\":\"None\",\"work_device_2\":\"None\",\"dials\":[]}");
        std::fs::write(&path, blob).unwrap();
        assert!(matches!(load_config(&path), ConfigLoad::Unreadable));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_flood_of_button_edges_is_rate_limited() {
        let mut fired: Vec<Option<Instant>> = Vec::new();
        assert!(button_ready(&mut fired, 0), "first press must act");
        let mut allowed = 1;
        for _ in 0..5000 {
            if button_ready(&mut fired, 0) {
                allowed += 1;
            }
        }
        assert!(
            allowed < 20,
            "a serial flood should not turn into thousands of keystrokes, got {allowed}"
        );
        assert!(button_ready(&mut fired, 1), "other buttons stay independent");
    }

    #[test]
    fn only_real_font_files_are_installed() {
        assert!(!is_sfnt(b"not a font"));
        assert!(!is_sfnt(&vec![0u8; 8192]));
        let mut ok = vec![0x00, 0x01, 0x00, 0x00];
        ok.extend(std::iter::repeat(0u8).take(8192));
        assert!(is_sfnt(&ok));
        let mut otto = b"OTTO".to_vec();
        otto.extend(std::iter::repeat(0u8).take(8192));
        assert!(is_sfnt(&otto));
    }

    #[test]
    fn osd_style_is_normalised_and_drives_the_renderer() {
        let mut cfg = AppConfig::default();
        assert_eq!(cfg.osd_style, "themed");

        cfg.osd_style = "rainbow".into();
        sanitize_config(&mut cfg);
        assert_eq!(cfg.osd_style, "themed", "unknown styles fall back");

        set_osd_style("mono");
        assert!(osd_is_mono());
        set_osd_style("themed");
        assert!(!osd_is_mono());

        assert_eq!(OSD_STYLES.len(), OSD_STYLE_LABELS.len());
    }

    #[test]
    fn console_reconciler_only_acts_on_a_real_difference() {
        use diag::{reconcile, ConsoleAction};
        assert_eq!(reconcile(true, false), Some(ConsoleAction::Open));
        assert_eq!(reconcile(false, true), Some(ConsoleAction::Close));
        assert_eq!(reconcile(true, true), None, "already open, must not reopen");
        assert_eq!(reconcile(false, false), None, "already closed, must not churn");
    }

    #[test]
    fn debug_console_setting_survives_a_round_trip() {
        let dir = std::env::temp_dir().join("rvci-test-debugflag");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("mapping.json");
        let _ = std::fs::remove_file(&path);

        let mut cfg = AppConfig::default();
        assert!(!cfg.debug_mode, "the console is off by default");
        cfg.debug_mode = true;
        assert!(save_config(&path, &cfg));

        match load_config(&path) {
            ConfigLoad::Loaded(back) => assert!(
                back.debug_mode,
                "an enabled debug console must come back enabled on the next launch"
            ),
            _ => panic!("config did not load back"),
        }

        let mut off = load_config(&path);
        if let ConfigLoad::Loaded(ref mut c) = off {
            c.debug_mode = false;
            assert!(save_config(&path, c));
        }
        match load_config(&path) {
            ConfigLoad::Loaded(back) => assert!(!back.debug_mode),
            _ => panic!("config did not load back"),
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_config_without_the_debug_key_defaults_to_console_off() {
        let dir = std::env::temp_dir().join("rvci-test-legacycfg");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("mapping.json");
        std::fs::write(
            &path,
            br#"{"serial":{"port":"COM3","baud":115200,"timeout":50},"value_max":1024.0,
                 "work_device_1":"None","work_device_2":"None","dials":[]}"#,
        )
        .unwrap();
        match load_config(&path) {
            ConfigLoad::Loaded(cfg) => assert!(
                !cfg.debug_mode,
                "an older config must not silently open a console"
            ),
            _ => panic!("legacy config should still load"),
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn log_rotation_keeps_a_bounded_history() {
        let dir = std::env::temp_dir().join("rvci-test-rotate");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let current = dir.join("rvci.log");

        std::fs::write(&current, b"small").unwrap();
        diag::rotate(&dir);
        assert!(current.exists(), "a small log must not rotate");
        assert!(!dir.join("rvci.1.log").exists());

        std::fs::write(&current, vec![b'x'; (diag::MAX_LOG_BYTES + 16) as usize]).unwrap();
        diag::rotate(&dir);
        assert!(!current.exists(), "the oversized log is moved aside");
        assert!(dir.join("rvci.1.log").exists());

        for _ in 0..5 {
            std::fs::write(&current, vec![b'x'; (diag::MAX_LOG_BYTES + 16) as usize]).unwrap();
            diag::rotate(&dir);
        }
        assert!(dir.join("rvci.1.log").exists());
        assert!(dir.join("rvci.3.log").exists());
        assert!(
            !dir.join("rvci.4.log").exists(),
            "history must stay bounded"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn diagnostics_create_their_directories_and_write_a_crash_report() {
        let dir = std::env::temp_dir().join("rvci-test-diag");
        let _ = std::fs::remove_dir_all(&dir);
        assert!(!dir.exists());

        diag::init(dir.clone());
        assert!(dir.is_dir(), "the log directory must be created automatically");
        assert!(
            diag::crash_dir_for(&dir).is_dir(),
            "the crash directory must be created automatically"
        );

        log_info!("test: a log line that should reach the file");
        log_error!("test: an error line with a marker 4711");

        let log = dir.join("rvci.log");
        assert!(log.is_file(), "rvci.log must exist after logging");
        let body = std::fs::read_to_string(&log).unwrap();
        assert!(body.contains("a log line that should reach the file"));
        assert!(body.contains("marker 4711"));
        assert!(body.contains("INFO"));
        assert!(body.contains("ERROR"));

        let report = diag::write_crash(
            "selftest",
            "synthetic failure for the test suite",
            "detail block with a stack trace stand-in",
        )
        .expect("a crash report must be written");
        assert!(report.is_file());
        assert!(report.starts_with(diag::crash_dir_for(&dir)));

        let crash = std::fs::read_to_string(&report).unwrap();
        for expected in [
            "RVCI crash report",
            "when        :",
            "kind        : selftest",
            "synthetic failure for the test suite",
            "app version : ",
            "os          : ",
            "--- recent log ---",
            "marker 4711",
        ] {
            assert!(crash.contains(expected), "crash report is missing {expected:?}");
        }
        assert!(crash.contains(env!("CARGO_PKG_VERSION")));

        assert!(
            !diag::console_is_open(),
            "no console should be open in a test run"
        );
    }

    #[test]
    fn the_log_directory_lives_under_appdata_next_to_the_config() {
        let logs = get_log_dir();
        let config = get_config_path();
        assert_eq!(
            logs.parent(),
            config.parent(),
            "logs belong beside mapping.json in AppData, not next to the exe"
        );
        assert!(logs.ends_with("RVCI\\logs"), "unexpected log path {logs:?}");
        let exe_dir = get_exe_dir();
        assert!(
            !logs.starts_with(&exe_dir),
            "logs must not be written into the install directory"
        );
    }

    #[test]
    fn a_debug_console_toggle_alone_does_not_bounce_the_serial_port() {
        let mut a = AppConfig::default();
        a.serial.port = "COM11".into();
        let mut b = a.clone();
        b.debug_mode = !a.debug_mode;
        assert!(
            !serial_settings_changed(&a, &b),
            "toggling the console must not reopen the COM port"
        );

        let mut c = a.clone();
        c.theme = "Emerald".into();
        assert!(!serial_settings_changed(&a, &c));

        let mut d = a.clone();
        d.serial.baud = 9600;
        assert!(serial_settings_changed(&a, &d));

        let mut e = a.clone();
        e.dials.push(DialConfig {
            dial_type: "system".into(),
            process_name: None,
            inverted: false,
        });
        assert!(serial_settings_changed(&a, &e));
    }

    #[test]
    fn saving_leaves_no_temp_file_behind() {
        let dir = std::env::temp_dir().join("rvci-test-atomic");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("mapping.json");
        assert!(save_config(&path, &AppConfig::default()));
        assert!(!path.with_extension("json.tmp").exists());
        let _ = std::fs::remove_file(&path);
    }
}
