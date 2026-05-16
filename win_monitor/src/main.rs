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
use windows::Win32::System::Power::{RegisterSuspendResumeNotification, RegisterPowerSettingNotification, POWERBROADCAST_SETTING};
use windows::core::GUID;
use windows::Win32::System::RemoteDesktop::{WTSRegisterSessionNotification, NOTIFY_FOR_THIS_SESSION};
use windows::Win32::System::Threading::{OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetForegroundWindow, GetMessageW,
    GetWindowThreadProcessId, PostQuitMessage, RegisterClassW, TranslateMessage,
    CW_USEDEFAULT, MSG, PBT_APMRESUMEAUTOMATIC, PBT_APMSUSPEND, WM_POWERBROADCAST, PBT_POWERSETTINGCHANGE,
    WM_WTSSESSION_CHANGE, WNDCLASSW, WTS_SESSION_LOCK, WTS_SESSION_LOGOFF, WTS_SESSION_LOGON,
    WTS_SESSION_UNLOCK,
};

const DEVICE_NOTIFY_WINDOW_HANDLE: u32 = 0;

const GUID_CONSOLE_DISPLAY_STATE: GUID = GUID::from_values(
    0x271a8220, 0x40d2, 0x4d1e, [0xae, 0x4b, 0xef, 0xc1, 0x0a, 0x2d, 0x9b, 0x55]
);
const TARGET_DURATION_SECS: u64 = 60;
const POLL_INTERVAL_SECS: u64 = 5; // 1 minute
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

fn get_foreground_app_info() -> Option<(String, String)> {
    unsafe {
        let hwnd: HWND = GetForegroundWindow();
        if hwnd.0.is_null() {
            return None;
        }

        let mut title_buffer = [0u16; 512];
        let title_len = windows::Win32::UI::WindowsAndMessaging::GetWindowTextW(hwnd, &mut title_buffer);
        let title = if title_len > 0 {
            String::from_utf16_lossy(&title_buffer[..title_len as usize])
        } else {
            String::new()
        };

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
                return Some((file_name.to_string_lossy().into_owned(), title));
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
                && let Some(state) = state_guard.as_mut()
            {
                match wparam.0 as u32 {
                    PBT_APMRESUMEAUTOMATIC => {
                        handle_login_wake(state, "login");
                    }
                    PBT_APMSUSPEND => {
                        send_usage_report(state, "sleep");
                    }
                    PBT_POWERSETTINGCHANGE => {
                        let setting = unsafe { &*(lparam.0 as *const POWERBROADCAST_SETTING) };
                        if setting.PowerSetting == GUID_CONSOLE_DISPLAY_STATE && setting.DataLength == 4 {
                            let data = unsafe { *(setting.Data.as_ptr() as *const u32) };
                            if data == 0 {
                                // Display off (lid closed usually)
                                send_usage_report(state, "sleep");
                            } else if data == 1 {
                                // Display on
                                handle_login_wake(state, "login");
                            }
                        }
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
        let _ = RegisterSuspendResumeNotification(
            windows::Win32::Foundation::HANDLE(hwnd.0),
            windows::Win32::UI::WindowsAndMessaging::REGISTER_NOTIFICATION_FLAGS(DEVICE_NOTIFY_WINDOW_HANDLE),
        );
        let _ = RegisterPowerSettingNotification(
            windows::Win32::Foundation::HANDLE(hwnd.0),
            &GUID_CONSOLE_DISPLAY_STATE,
            windows::Win32::UI::WindowsAndMessaging::REGISTER_NOTIFICATION_FLAGS(DEVICE_NOTIFY_WINDOW_HANDLE),
        );

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


    let mut current_app_info: Option<(String, String)> = None;
    let mut app_focus_start: Instant = Instant::now();
    let mut notification_sent = false;
    let mut last_stat_minute_tick = Instant::now();

    loop {
        thread::sleep(Duration::from_secs(POLL_INTERVAL_SECS));

        let active_app_info = get_foreground_app_info();

        let app_changed = match (&active_app_info, &current_app_info) {
            (Some((active_app_name, _)), Some((current_app_name, _))) => active_app_name != current_app_name,
            (None, None) => false,
            _ => true,
        };

        if app_changed {
            current_app_info = active_app_info.clone();
            app_focus_start = Instant::now();
            notification_sent = false;
        } else if let Some((app_name, _)) = current_app_info.clone() {
            if let Some((_, ref new_title)) = active_app_info {
                current_app_info = Some((app_name.clone(), new_title.clone()));
            }

            let elapsed = app_focus_start.elapsed().as_secs();

            if elapsed >= TARGET_DURATION_SECS && !notification_sent {
                let title_to_send = current_app_info.as_ref().map(|(_, t)| t.clone()).unwrap_or_default();
                let alert_msg = format!("{} - {}", app_name, title_to_send);
                if let Ok(state_guard) = APP_STATE.lock()
                    && let Some(state) = state_guard.as_ref() {
                        send_telegram_message(&state.config, &alert_msg);
                    }
                notification_sent = true;
            }
        }

        // Check if 60 seconds have passed for usage statistics
        if last_stat_minute_tick.elapsed().as_secs() >= 60 {
            last_stat_minute_tick = Instant::now();

            let current_tick = unsafe { windows::Win32::System::SystemInformation::GetTickCount() };
            let last_input = get_last_input_time();

            let elapsed_since_input = if current_tick >= last_input {
                current_tick - last_input
            } else {
                (u32::MAX - last_input) + current_tick
            };

            let active_minute = elapsed_since_input <= 60_000;

            if active_minute
                && let Some((app, _)) = &active_app_info
                    && let Ok(mut state_guard) = APP_STATE.lock()
                        && let Some(state) = state_guard.as_mut() {
                            *state.stats.entry(app.clone()).or_insert(0) += 1;
                        }
        }

        // Hourly report
        if let Ok(mut state_guard) = APP_STATE.lock()
            && let Some(state) = state_guard.as_mut()
                && state.last_report_time.elapsed().as_secs() >= REPORT_INTERVAL_SECS {
                    send_usage_report(state, "hourly");
                    // Important: send_usage_report resets the timer. But if empty, it doesn't.
                    // We must update last_report_time even if empty, so it doesn't poll rapidly.
                    state.last_report_time = Instant::now();
                }
    }
}
