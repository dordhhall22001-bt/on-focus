#![windows_subsystem = "windows"]

use std::env;
use std::fs;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use ini::Ini;
use windows::Win32::Foundation::{HWND, MAX_PATH, HMODULE};
use windows::Win32::System::ProcessStatus::K32GetModuleFileNameExW;
use windows::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};

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

fn send_telegram_message(config: &Config, app_name: &str, duration_mins: u64) {
    let url = format!("https://api.telegram.org/bot{}/sendMessage", config.bot_token);
    let message = format!("Alert: You have been using {} for over {} minutes.", app_name, duration_mins);

    let payload = serde_json::json!({
        "chat_id": config.chat_id,
        "text": message,
    });

    match ureq::post(&url)
        .set("Content-Type", "application/json")
        .send_string(&payload.to_string())
    {
        Ok(_) => (),
        Err(_) => (),
    }
}

fn get_foreground_app_name() -> Option<String> {
    unsafe {
        let hwnd: HWND = GetForegroundWindow();
        if hwnd.0 == std::ptr::null_mut() {
            return None;
        }

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
        let len = K32GetModuleFileNameExW(Some(process_handle), Some(HMODULE::default()), &mut buffer);

        let _ = windows::Win32::Foundation::CloseHandle(process_handle);

        if len > 0 {
            let path_str = String::from_utf16_lossy(&buffer[..len as usize]);
            // Extract just the executable name from the path
            let path = std::path::Path::new(&path_str);
            if let Some(file_name) = path.file_name() {
                return Some(file_name.to_string_lossy().into_owned());
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

    let mut current_app: Option<String> = None;
    let mut app_focus_start: Instant = Instant::now();
    let mut notification_sent = false;

    loop {
        let active_app = get_foreground_app_name();

        if active_app != current_app {
            // App has changed
            current_app = active_app;
            app_focus_start = Instant::now();
            notification_sent = false;
        } else if let Some(ref app_name) = current_app {
            // App is the same
            let elapsed = app_focus_start.elapsed().as_secs();

            if elapsed >= TARGET_DURATION_SECS && !notification_sent {
                // Time exceeded, send notification
                send_telegram_message(&config, app_name, elapsed / 60);
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
