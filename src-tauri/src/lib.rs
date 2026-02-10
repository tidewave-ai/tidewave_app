use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tauri::{
    menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem},
    tray::TrayIconBuilder,
    Manager,
};
use tauri_plugin_autostart::ManagerExt;
use tauri_plugin_cli::CliExt;
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};
use tauri_plugin_opener::OpenerExt;
use tauri_plugin_updater::UpdaterExt;
use tracing::{debug, error, info};
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::prelude::*;

struct ServerState {
    handle: Arc<Mutex<Option<tauri::async_runtime::JoinHandle<()>>>>,
    shutdown_tx: tokio::sync::watch::Sender<bool>,
}

struct PortState {
    port: u16,
    https_port: Option<u16>,
}

struct LogState {
    log_path: PathBuf,
    #[allow(dead_code)]
    log_guard: Option<tracing_appender::non_blocking::WorkerGuard>,
}

const DEFAULT_CONFIG: &str = r#"# This file is used to configure the Tidewave app.
# If you change this file, you must restart Tidewave.

# port = 9832
# https_port = 9833
# https_cert_path = "/path/to/cert.pem"
# https_key_path = "/path/to/key.pem"
# allow_remote_access = false

[env]
# SOME_API_KEY = "value"
"#;

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_cli::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_single_instance::init(|_app, args, _cwd| {
            debug!("a new app instance was opened with {args:?} and the deep link event was already triggered");
        }))
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .plugin(tauri_plugin_opener::init())
        .setup(|app| tauri::async_runtime::block_on(async move {
            let mut config = match tidewave_core::load_config() {
                Ok(config) => config,
                Err(e) => {
                    error!("Failed to load config: {}", e);
                    config_error_dialog(&app.handle(), e.to_string());
                    std::process::exit(1);
                }
            };

            // We only list some of the options here,
            // as this is mostly for development help.
            // All other options are read from the config file.
            match app.cli().matches() {
                Ok(matches) => {
                    if let Some(debug_arg) = matches.args.get("debug") {
                        if let Some(value) = debug_arg.value.as_bool() {
                            if value {
                                config.debug = true;
                            }
                        }
                    }

                    if let Some(port_arg) = matches.args.get("port") {
                        if let Some(port_str) = port_arg.value.as_str() {
                            if let Ok(p) = port_str.parse::<u16>() {
                                config.port = p;
                            }
                        }
                    }
                }
                Err(_) => {}
            }

            // Initialize logging to file and console
            let log_path = log_path();
            let _ = ensure_parent_dir(&log_path);
            let log_guard = init_tracing(&log_path, config.debug);

            if config.debug {
                debug!("Debug logging enabled");
                debug!("{:?}", config);
            }
            println!("Logging to file: {}", log_path.display());

            // Set environment variables from config before server initialization
            for (key, value) in &config.env {
                debug!("Setting env var: {}={}", key, value);
                std::env::set_var(key, value);
            }

            let port = config.port;
            let https_port = config.https_port;

            // Create shutdown signal channel
            let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);

            let handle_holder = Arc::new(Mutex::new(None));
            let handle_holder_clone = handle_holder.clone();

            let app_handle_for_server = app.handle().clone();
            let server_config = config.clone();
            let server_handle = tauri::async_runtime::spawn(async move {
                let shutdown_signal = async move {
                    let _ = shutdown_rx.changed().await;
                };

                if let Err(e) = tidewave_core::serve_http_server_with_shutdown(server_config, shutdown_signal).await {
                    error_dialog(&app_handle_for_server, "Error", format!("HTTP server error: {}", e));
                    app_handle_for_server.exit(1);
                }
            });

            *handle_holder.lock().unwrap() = Some(server_handle);

            // Store server state for cleanup on restart
            app.manage(ServerState {
                handle: handle_holder_clone,
                shutdown_tx,
            });

            app.manage(PortState {
                port,
                https_port,
            });

            // Store log state (log_path and guard to keep writer alive)
            app.manage(LogState {
                log_path,
                log_guard,
            });

            open_tidewave(&app.handle(), port, https_port);

            #[cfg(target_os = "macos")]
            let maybe_hotkey = Some;
            #[cfg(not(target_os = "macos"))]
            let maybe_hotkey = |_s: &str| None::<&str>;

            let open_tidewave_i = MenuItem::with_id(app, "open_tidewave", "Open in Browser", true, maybe_hotkey("Command+O"))?;
            let open_config_i = MenuItem::with_id(app, "open_config", "Settings…", true, maybe_hotkey("Command+,"))?;
            let is_autostart = app.autolaunch().is_enabled().unwrap_or(false);
            let launch_at_login_i = CheckMenuItem::with_id(app, "launch_at_login", "Launch at Login", true, is_autostart, None::<&str>)?;
            let view_logs_i = MenuItem::with_id(app, "view_logs", "View Logs", true, maybe_hotkey("Command+L"))?;
            let check_for_updates_i = MenuItem::with_id(app, "check_for_updates", "Check for Updates…", true, None::<&str>)?;
            let restart_i = MenuItem::with_id(app, "restart", "Restart", true, None::<&str>)?;
            let separator = PredefinedMenuItem::separator(app)?;
            let separator2 = PredefinedMenuItem::separator(app)?;
            let quit_i = MenuItem::with_id(app, "quit", "Quit Tidewave", true, maybe_hotkey("Command+Q"))?;
            let menu = Menu::with_items(app, &[&open_tidewave_i, &separator, &open_config_i, &view_logs_i, &separator2, &launch_at_login_i, &check_for_updates_i, &restart_i, &quit_i])?;

            let launch_at_login_item = launch_at_login_i.clone();
            TrayIconBuilder::new()
            .menu(&menu)
            .icon(app.default_window_icon().unwrap().clone())
            .icon_as_template(true)
            .on_menu_event(move |app, event| match event.id.as_ref() {
                "quit" => {
                    debug!("quit menu item was clicked");
                    app.exit(0);
                }
                "open_tidewave" => {
                    open_tidewave(app, port, https_port);
                }
                "open_config" => {
                    if let Err(e) = open_config_file(app) {
                        error!("Failed to open config file: {}", e);
                        error_dialog(app, "Error", format!("Failed to open config file: {}", e));
                    }
                }
                "view_logs" => {
                    open_log_file(app);
                }
                "restart" => {
                    match tidewave_core::load_config() {
                        Ok(config) => {
                            info!("Reloaded config: {:?}", config);
                            info!("Restarting application to apply new configuration...");

                            // Trigger graceful shutdown and wait for server to stop
                            if let Some(server_state) = app.try_state::<ServerState>() {
                                let _ = server_state.shutdown_tx.send(true);

                                // Wait for the server task to complete gracefully
                                if let Some(handle) = server_state.handle.lock().unwrap().take() {
                                    tauri::async_runtime::block_on(async {
                                        let _ = handle.await;
                                    });
                                    info!("Server shut down gracefully");
                                }
                            }

                            app.restart();
                        }
                        Err(e) => {
                            error!("Failed to reload config: {}", e);
                            config_error_dialog(app, e.to_string());
                        }
                    }
                }
                "launch_at_login" => {
                    let manager = app.autolaunch();
                    let enabled = manager.is_enabled().unwrap_or(false);
                    let result = if enabled { manager.disable() } else { manager.enable() };
                    match result {
                        Ok(()) => {
                            let _ = launch_at_login_item.set_checked(!enabled);
                        }
                        Err(e) => {
                            error!("Failed to toggle autostart: {}", e);
                            error_dialog(app, "Error", format!("Failed to toggle Launch at Login: {}", e));
                        }
                    }
                }
                "check_for_updates" => {
                    check_for_updates(app.clone());
                }
                _ => {
                    debug!("menu item {:?} not handled", event.id);
                }
            })
            .build(app)?;

            let app_handle_for_updates = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                if let Err(e) = check_for_updates_on_boot(app_handle_for_updates).await {
                    error!("Failed to check for updates on boot: {}", e);
                }
            });

            Ok(())
        }))
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app_handle, event| {
            #[cfg(target_os = "macos")]
            if let tauri::RunEvent::Reopen { .. } = event {
                if let Some(port_state) = app_handle.try_state::<PortState>() {
                    open_tidewave(app_handle, port_state.port, port_state.https_port);
                }
            }

            if let tauri::RunEvent::ExitRequested { .. } = event {
                // Trigger graceful shutdown and wait for server to stop
                if let Some(server_state) = app_handle.try_state::<ServerState>() {
                    let _ = server_state.shutdown_tx.send(true);

                    // Wait for the server task to complete gracefully
                    if let Some(handle) = server_state.handle.lock().unwrap().take() {
                        tauri::async_runtime::block_on(async {
                            let _ = handle.await;
                        });
                        info!("Server shut down gracefully");
                    }
                }
            }
        });
}

fn open_tidewave(app: &tauri::AppHandle, port: u16, https_port: Option<u16>) {
    debug!("Opening Tidewave in browser");
    // Prefer HTTPS if available
    let url = if let Some(https_port) = https_port {
        format!("https://localhost:{}", https_port)
    } else {
        format!("http://localhost:{}", port)
    };
    if let Err(e) = app.opener().open_url(&url, None::<&str>) {
        let message = format!(
            "Failed to open Tidewave: {}. Please open {} in your browser instead.",
            e, url
        );
        error!(message);
        error_dialog(app, "Error", message);
    }
}

fn open_config_file(app: &tauri::AppHandle) -> Result<(), Box<dyn std::error::Error>> {
    let config_path = tidewave_core::get_config_path();

    if !config_path.exists() {
        debug!("Creating config file: {:?}", config_path);
        if let Some(parent) = config_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&config_path, DEFAULT_CONFIG)?;
    }

    debug!("Opening config file: {:?}", config_path);

    #[cfg(target_os = "windows")]
    {
        // silence unused variable warning
        let _ = app;

        std::process::Command::new("notepad.exe")
            .arg(&config_path)
            .spawn()?;
    }

    #[cfg(not(target_os = "windows"))]
    {
        app.opener()
            .open_path(config_path.to_str().unwrap(), None::<&str>)?;
    }

    Ok(())
}

async fn check_for_updates_on_boot(app: tauri::AppHandle) -> tauri_plugin_updater::Result<()> {
    if let Some(update) = app.updater()?.check().await? {
        let should_install = app
            .dialog()
            .message(format!(
                "Version {} is available!\n\nWould you like to download and install it now?",
                update.version
            ))
            .kind(MessageDialogKind::Info)
            .title("Update Available")
            .buttons(MessageDialogButtons::OkCancel)
            .blocking_show();

        if should_install {
            match update.download_and_install(|_, _| {}, || {}).await {
                Ok(()) => {
                    app.restart();
                }
                Err(e) => {
                    error!("Failed to install update: {}", e);
                    error_dialog(
                        &app,
                        "Update Failed",
                        format!("Failed to install update: {}", e),
                    );
                }
            }
        }
    }

    Ok(())
}

fn check_for_updates(app: tauri::AppHandle) {
    tauri::async_runtime::spawn(async move {
        match check_for_updates_async(app.clone()).await {
            Ok(()) => {}
            Err(e) => {
                error!("Failed to check for updates: {}", e);
                error_dialog(
                    &app,
                    "Update Check Failed",
                    format!("Failed to check for updates: {}", e),
                );
            }
        }
    });
}

async fn check_for_updates_async(app: tauri::AppHandle) -> tauri_plugin_updater::Result<()> {
    if let Some(update) = app.updater()?.check().await? {
        let should_install = app
            .dialog()
            .message(format!(
                "Version {} is available!\n\nWould you like to download and install it now?",
                update.version
            ))
            .kind(MessageDialogKind::Info)
            .title("Update Available")
            .buttons(MessageDialogButtons::OkCancel)
            .blocking_show();

        if should_install {
            match update.download_and_install(|_, _| {}, || {}).await {
                Ok(()) => {
                    app.restart();
                }
                Err(e) => {
                    error_dialog(
                        &app,
                        "Update Failed",
                        format!("Failed to install update: {}", e),
                    );
                }
            }
        }
    } else {
        app.dialog()
            .message(format!(
                "You're running the latest version:\n\nv{}",
                app.package_info().version
            ))
            .kind(MessageDialogKind::Info)
            .title("No Updates Available")
            .blocking_show();
    }

    Ok(())
}

fn error_dialog(app: &tauri::AppHandle, title: impl Into<String>, message: impl Into<String>) {
    let app_handle = app.clone();
    let message = message.into();
    error!("{}", message);
    let result = app
        .dialog()
        .message(message)
        .kind(MessageDialogKind::Error)
        .title(title.into())
        .buttons(MessageDialogButtons::OkCancelCustom(
            "Dismiss".to_string(),
            "View Logs".to_string(),
        ))
        .blocking_show();

    if !result {
        open_log_file(&app_handle);
    }
}

fn config_error_dialog(app: &tauri::AppHandle, error_message: impl Into<String>) {
    let config_path = tidewave_core::get_config_path();
    let path_str = config_path.display().to_string();
    let error_msg = error_message.into();

    let message = format!("Invalid {}:\n\n{}", path_str, error_msg);

    let app_handle = app.clone();
    let result = app
        .dialog()
        .message(message)
        .kind(MessageDialogKind::Error)
        .title("Config Error")
        .buttons(MessageDialogButtons::OkCancelCustom(
            "Dismiss".to_string(),
            "Open app.toml".to_string(),
        ))
        .blocking_show();

    if !result {
        if let Err(e) = open_config_file(&app_handle) {
            error!("Failed to open config file: {}", e);
            error_dialog(
                &app_handle,
                "Error",
                format!("Failed to open config file: {}", e),
            );
        }
    }
}

fn log_path() -> PathBuf {
    let data_dir = dirs::data_local_dir().unwrap_or_else(|| std::env::temp_dir());
    data_dir.join("tidewave").join("logs").join("tidewave.log")
}

fn ensure_parent_dir(path: &PathBuf) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    Ok(())
}

fn init_tracing(
    log_path: &PathBuf,
    debug: bool,
) -> Option<tracing_appender::non_blocking::WorkerGuard> {
    let dir = log_path.parent()?;
    let file = log_path.file_name()?;

    let file_appender = tracing_appender::rolling::never(dir, file);
    let (file_writer, guard) = tracing_appender::non_blocking(file_appender);

    let console_filter = if debug {
        LevelFilter::DEBUG
    } else {
        LevelFilter::INFO
    };

    let console_layer = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stdout)
        .with_filter(console_filter);
    let file_layer = tracing_subscriber::fmt::layer()
        .with_writer(file_writer)
        .with_ansi(false)
        .with_target(false)
        .with_level(true)
        .with_filter(LevelFilter::INFO);

    let _ = tracing_subscriber::registry()
        .with(console_layer)
        .with(file_layer)
        .try_init();

    Some(guard)
}

fn open_log_file(app: &tauri::AppHandle) {
    if let Some(log_state) = app.try_state::<LogState>() {
        let _ = app
            .opener()
            .open_path(log_state.log_path.display().to_string(), None::<&str>);
    }
}
