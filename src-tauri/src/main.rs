// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod media_server;

use std::path::PathBuf;
use tauri::{AppHandle, Manager, WebviewWindowBuilder};

// wry's defaults - passing our own browser args replaces them, so they are repeated here
const DEFAULT_BROWSER_ARGS: &str = "--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection";

#[tauri::command]
fn write_binary_file(path: String, bytes: Vec<u8>) -> Result<(), String> {
    if path.trim().is_empty() {
        return Err("missing file path".to_string());
    }

    std::fs::write(&path, bytes).map_err(|e| format!("write file failed: {e}"))
}

fn decode_settings_path(app: &AppHandle) -> Option<PathBuf> {
    app.path().app_config_dir().ok().map(|dir| dir.join("video-decode.json"))
}

// Software decoding is the default: some GPUs (e.g. AMD integrated graphics) stall for tens of
// seconds on the HW4 front camera stream when WebView2 decodes it in hardware.
fn hardware_decode_enabled(app: &AppHandle) -> bool {
    decode_settings_path(app)
        .and_then(|path| std::fs::read_to_string(path).ok())
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .and_then(|value| value.get("hardwareDecode").and_then(|v| v.as_bool()))
        .unwrap_or(false)
}

// Base URL of the local video server, or None if it could not start (the page then falls back
// to the asset protocol)
#[tauri::command]
fn get_media_base_url(server: tauri::State<'_, Option<media_server::MediaServer>>) -> Option<String> {
    server.as_ref().map(|s| s.base_url.clone())
}

#[tauri::command]
fn get_hardware_decode(app: AppHandle) -> bool {
    hardware_decode_enabled(&app)
}

// The decoder is picked when the webview starts, so a change takes effect through a restart
#[tauri::command]
fn set_hardware_decode(app: AppHandle, enabled: bool) -> Result<(), String> {
    let path = decode_settings_path(&app).ok_or("config directory unavailable")?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("create config dir failed: {e}"))?;
    }
    let body = serde_json::json!({ "hardwareDecode": enabled }).to_string();
    std::fs::write(&path, body).map_err(|e| format!("write setting failed: {e}"))?;
    app.restart();
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_fs::init())
        .plugin(tauri_plugin_shell::init())
        .invoke_handler(tauri::generate_handler![
            write_binary_file,
            get_media_base_url,
            get_hardware_decode,
            set_hardware_decode
        ])
        .setup(|app| {
            app.manage(media_server::start().ok());

            // The main window has `create: false` in tauri.conf.json so its browser args
            // can follow the saved decode setting
            let config = app
                .config()
                .app
                .windows
                .iter()
                .find(|w| w.label == "main")
                .cloned()
                .ok_or("main window config missing")?;
            let mut args = DEFAULT_BROWSER_ARGS.to_string();
            if !hardware_decode_enabled(app.handle()) {
                args.push_str(" --disable-accelerated-video-decode");
            }
            WebviewWindowBuilder::from_config(app.handle(), &config)?
                .additional_browser_args(&args)
                .build()?;
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
