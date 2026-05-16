#![windows_subsystem = "windows"]

use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use ini::Ini;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, MAX_PATH, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Power::RegisterSuspendResumeNotification;
use windows::Win32::System::RemoteDesktop::{WTSRegisterSessionNotification, NOTIFY_FOR_THIS_SESSION};
use windows::Win32::System::Threading::{OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetForegroundWindow, GetMessageW,
    GetWindowThreadProcessId, PostQuitMessage, RegisterClassW, TranslateMessage,
    CW_USEDEFAULT, MSG, PBT_APMRESUMEAUTOMATIC, PBT_APMSUSPEND, WM_POWERBROADCAST,
    WM_WTSSESSION_CHANGE, WNDCLASSW, WTS_SESSION_LOCK, WTS_SESSION_LOGOFF, WTS_SESSION_LOGON,
    WTS_SESSION_UNLOCK,
};

const DEVICE_NOTIFY_WINDOW_HANDLE: u32 = 0;
const POLL_INTERVAL_SECS: u64 = 60; // 1 minute
const REPORT_INTERVAL_SECS: u64 = 3600; // 1 hour

#[derive(Debug, Clone)]
struct Config {
    bot_token: String,
    chat_id: String,
}

struct AppState {
    config: Config,
    stats: HashMap<String, u64>, // Executable name -> minutes focused
    last_report_time: Instant,
    last_login_msg_time: Option<Instant>,
    hostname: String,
    username: String,
    ip_address: String,
}

lazy_static::lazy_static! {
    static ref APP_STATE: Arc<Mutex<Option<AppState>>> = Arc::new(Mutex::new(None));
}

fn load_config() -> std::result::Result<Config, String> {
    let mut config_path: PathBuf = env::current_exe().map_err(|e| format!("Failed to get exe path: {}", e))?;
    config_path.pop();
    config_path.push("config.ini");

    if !config_path.exists() {
        return Err(format!("Config file not found at {:?}", config_path));
    }

    let conf = Ini::load_from_file(&config_path).map_err(|e| format!("Failed to load config: {}", e))?;
    let section = conf.section(Some("Telegram")).ok_or("Missing [Telegram]")?;
    let bot_token = section.get("bot_token").ok_or("Missing bot_token")?.to_string();
    let chat_id = section.get("chat_id").ok_or("Missing chat_id")?.to_string();

    Ok(Config { bot_token, chat_id })
}

fn send_telegram_message(config: &Config, message: &str) {
    let url = format!("https://api.telegram.org/bot{}/sendMessage", config.bot_token);
    let payload = serde_json::json!({
        "chat_id": config.chat_id,
        "text": message,
    });
    let _ = ureq::post(&url)
        .set("Content-Type", "application/json")
        .send_string(&payload.to_string());
}

fn get_foreground_app_exe() -> Option<String> {
    unsafe {
        let hwnd: HWND = GetForegroundWindow();
        if hwnd.0.is_null() {
            return None;
        }

        let mut process_id: u32 = 0;
        GetWindowThreadProcessId(hwnd, Some(&mut process_id as *mut _));
        if process_id == 0 {
            return None;
        }

        let process_handle_result = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, process_id);
        let process_handle = match process_handle_result {
            Ok(handle) => handle,
            Err(_) => return None,
        };

        let mut buffer = [0u16; MAX_PATH as usize];
        let mut len = MAX_PATH;
        let success = QueryFullProcessImageNameW(
            process_handle,
            PROCESS_NAME_WIN32,
            windows::core::PWSTR::from_raw(buffer.as_mut_ptr()),
            &mut len,
        );

        let _ = windows::Win32::Foundation::CloseHandle(process_handle);

        if success.is_ok() && len > 0 {
            let path_str = String::from_utf16_lossy(&buffer[..len as usize]);
            let path = std::path::Path::new(&path_str);
            if let Some(file_name) = path.file_name() {
                return Some(file_name.to_string_lossy().into_owned());
            }
        }
    }
    None
}

fn get_last_input_time() -> u32 {
    unsafe {
        let mut lii = LASTINPUTINFO {
            cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32,
            dwTime: 0,
        };
        let res = GetLastInputInfo(&mut lii);
        if res.as_bool() {
            return lii.dwTime;
        }
        0
    }
}

fn send_usage_report(state: &mut AppState, reason: &str) {
    if state.stats.is_empty() {
        return; // nothing to report
    }

    let mut msg = format!("{}/{}/{} - usage report ({})\n", state.hostname, state.username, state.ip_address, reason);
    let mut entries: Vec<_> = state.stats.iter().collect();
    entries.sort_by(|a, b| b.1.cmp(a.1)); // Sort descending by usage

    for (app, mins) in entries {
        msg.push_str(&format!("{}: {}m\n", app, mins));
    }

    send_telegram_message(&state.config, &msg);
    state.stats.clear();
    state.last_report_time = Instant::now();
}

fn handle_login_wake(state: &mut AppState, reason: &str) {
    let now = Instant::now();
    let should_send = state.last_login_msg_time.is_none_or(|t| now.duration_since(t).as_secs() > 60);

    if should_send {
        let msg = format!("{}/{}/{} - {}", state.hostname, state.username, state.ip_address, reason);
        send_telegram_message(&state.config, &msg);
        state.last_login_msg_time = Some(now);
    }
}

unsafe extern "system" fn window_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_POWERBROADCAST => {
            if let Ok(mut state_guard) = APP_STATE.lock()
                && let Some(state) = state_guard.as_mut() {
                    match wparam.0 as u32 {
                        PBT_APMRESUMEAUTOMATIC => {
                            handle_login_wake(state, "login");
                        }
                        PBT_APMSUSPEND => {
                            send_usage_report(state, "sleep");
                        }
                        _ => {}
                    }
                }
            LRESULT(1)
        }
        WM_WTSSESSION_CHANGE => {
            if let Ok(mut state_guard) = APP_STATE.lock()
                && let Some(state) = state_guard.as_mut() {
                    match wparam.0 as u32 {
                        WTS_SESSION_LOGON | WTS_SESSION_UNLOCK => {
                            handle_login_wake(state, "login");
                        }
                        WTS_SESSION_LOGOFF | WTS_SESSION_LOCK => {
                            send_usage_report(state, "logout");
                        }
                        _ => {}
                    }
                }
            LRESULT(0)
        }
        windows::Win32::UI::WindowsAndMessaging::WM_DESTROY => {
            unsafe { PostQuitMessage(0); }
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

fn run_message_loop() {
    unsafe {
        let instance = GetModuleHandleW(None).unwrap();
        let class_name = windows::core::w!("WinMonitorHiddenClass");

        let wc = WNDCLASSW {
            lpfnWndProc: Some(window_proc),
            hInstance: windows::Win32::Foundation::HINSTANCE(instance.0),
            lpszClassName: class_name,
            ..Default::default()
        };

        if RegisterClassW(&wc) == 0 {
            return;
        }

        let hwnd_res = CreateWindowExW(
            Default::default(),
            class_name,
            windows::core::w!("WinMonitorHiddenWindow"),
            windows::Win32::UI::WindowsAndMessaging::WS_OVERLAPPEDWINDOW,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            None,
            None,
            Some(windows::Win32::Foundation::HINSTANCE(instance.0)),
            None,
        );

        let hwnd = match hwnd_res {
            Ok(h) => h,
            Err(_) => return,
        };

        if hwnd.0.is_null() {
            return;
        }

        let _ = WTSRegisterSessionNotification(hwnd, NOTIFY_FOR_THIS_SESSION);
        let _ = RegisterSuspendResumeNotification(windows::Win32::Foundation::HANDLE(hwnd.0), windows::Win32::UI::WindowsAndMessaging::REGISTER_NOTIFICATION_FLAGS(DEVICE_NOTIFY_WINDOW_HANDLE));

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).into() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

fn main() {
    let config = match load_config() {
        Ok(c) => c,
        Err(e) => {
            let _ = fs::write("error.log", format!("Startup error: {}\n", e));
            return;
        }
    };

    let hostname = env::var("COMPUTERNAME").unwrap_or_else(|_| "UnknownHost".to_string());
    let username = env::var("USERNAME").unwrap_or_else(|_| "UnknownUser".to_string());
    let ip_address = match ureq::get("https://api.ipify.org").call() {
        Ok(response) => response.into_string().unwrap_or_else(|_| "UnknownIP".to_string()),
        Err(_) => "UnknownIP".to_string(),
    };

    {
        let mut state_guard = APP_STATE.lock().unwrap();
        *state_guard = Some(AppState {
            config: config.clone(),
            stats: HashMap::new(),
            last_report_time: Instant::now(),
            last_login_msg_time: None,
            hostname,
            username,
            ip_address,
        });
    }

    // Send startup message (as "login" as per requirement)
    {
        let mut state_guard = APP_STATE.lock().unwrap();
        if let Some(state) = state_guard.as_mut() {
            handle_login_wake(state, "login");
        }
    }

    // Start message loop thread
    thread::spawn(|| {
        run_message_loop();
    });

    // Main loop for tracking focus
    let _last_tick = unsafe { windows::Win32::System::SystemInformation::GetTickCount() };

    loop {
        thread::sleep(Duration::from_secs(POLL_INTERVAL_SECS));

        let current_tick = unsafe { windows::Win32::System::SystemInformation::GetTickCount() };
        let last_input = get_last_input_time();

        // Handle tick wrap around if needed (rare, roughly 49 days)
        let elapsed_since_input = if current_tick >= last_input {
            current_tick - last_input
        } else {
            (u32::MAX - last_input) + current_tick
        };

        // If elapsed since input is less than a minute (60_000 ms), record usage
        let active_minute = elapsed_since_input <= 60_000;

        let mut app_to_credit = None;
        if active_minute {
            app_to_credit = get_foreground_app_exe();
        }

        if let Ok(mut state_guard) = APP_STATE.lock()
            && let Some(state) = state_guard.as_mut() {
                // Record usage
                if let Some(app) = app_to_credit {
                    *state.stats.entry(app).or_insert(0) += 1;
                }

                // Check for hourly report
                if state.last_report_time.elapsed().as_secs() >= REPORT_INTERVAL_SECS {
                    send_usage_report(state, "hourly");
                }
            }
    }
}
