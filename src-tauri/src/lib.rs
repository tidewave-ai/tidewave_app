use std::fs;
use std::sync::{Arc, Mutex};
use tauri::{
    menu::{Menu, MenuItem, PredefinedMenuItem},
    tray::TrayIconBuilder,
    Manager,
};
use tauri_plugin_cli::CliExt;
use tauri_plugin_deep_link::DeepLinkExt;
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};
use tauri_plugin_opener::OpenerExt;
use tauri_plugin_updater::UpdaterExt;
use tracing::{debug, error, info};

struct ServerState {
    handle: Arc<Mutex<Option<tauri::async_runtime::JoinHandle<()>>>>,
    shutdown_tx: tokio::sync::watch::Sender<bool>,
}

const DEFAULT_CONFIG: &str = r#"# This file is used to configure the Tidewave app.
# If you change this file, you must restart Tidewave.

# port = 9832
"#;

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_cli::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_single_instance::init(|_app, args, _cwd| {
            debug!("a new app instance was opened with {args:?} and the deep link event was already triggered");
        }))
        .plugin(tauri_plugin_deep_link::init())
        .plugin(tauri_plugin_opener::init())
        .setup(|app| tauri::async_runtime::block_on(async move {
            let mut config = match tidewave_core::load_config() {
                Ok(config) => config,
                Err(e) => {
                    error!("Failed to load config: {}", e);
                    app.dialog()
                        .message(format!("Failed to load config file: {}", e))
                        .kind(MessageDialogKind::Error)
                        .title("Config Error")
                        .blocking_show();
                    std::process::exit(1);
                }
            };

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

            // Initialize tracing
            let filter = if config.debug {
                "debug"
            } else {
                "info"
            };
            tracing_subscriber::fmt()
                .with_env_filter(filter)
                .init();

            if config.debug {
                debug!("Debug logging enabled");
                debug!("{:?}", config);
            }

            let port = config.port;

            let listener = match tidewave_core::bind_http_server(config.clone()).await {
                Ok(listener) => listener,
                Err(e) => {
                    app.dialog()
                        .message(format!("Failed to bind HTTP server: {}", e))
                        .kind(MessageDialogKind::Error)
                        .title("Error")
                        .blocking_show();
                    std::process::exit(1);
                }
            };

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

                if let Err(e) = tidewave_core::serve_http_server_with_shutdown(server_config, listener, shutdown_signal).await {
                    app_handle_for_server.dialog()
                        .message(format!("HTTP server error: {}", e))
                        .kind(MessageDialogKind::Error)
                        .title("Error")
                        .blocking_show();
                    app_handle_for_server.exit(1);
                }
            });

            *handle_holder.lock().unwrap() = Some(server_handle);

            // Store server state for cleanup on restart
            app.manage(ServerState {
                handle: handle_holder_clone,
                shutdown_tx,
            });

            #[cfg(any(target_os = "windows", target_os = "linux"))]
            app.deep_link()
                .register("tidewave")
                .expect("failed to register tidewave:// handler");

            if let Some(urls) = app.deep_link().get_current()? {
                handle_urls(&app.handle(), port, &urls);
            }

            let app_handle_for_open = app.handle().clone();
            app.deep_link().on_open_url(move |event| {
                handle_urls(&app_handle_for_open, port, &event.urls());
            });

            open_tidewave(&app.handle(), port);

            let open_tidewave_i = MenuItem::with_id(app, "open_tidewave", "Open in Browser", true, None::<&str>)?;
            let open_config_i = MenuItem::with_id(app, "open_config", "Settings…", true, None::<&str>)?;
            let check_for_updates_i = MenuItem::with_id(app, "check_for_updates", "Check for Updates…", true, None::<&str>)?;
            let restart_i = MenuItem::with_id(app, "restart", "Restart", true, None::<&str>)?;
            let separator = PredefinedMenuItem::separator(app)?;
            let quit_i = MenuItem::with_id(app, "quit", "Quit Tidewave", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&open_tidewave_i, &separator, &open_config_i, &check_for_updates_i, &restart_i, &quit_i])?;

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
                    open_tidewave(app, port);
                }
                "open_config" => {
                    if let Err(e) = open_config_file(app) {
                        error!("Failed to open config file: {}", e);
                        app.dialog()
                            .message(format!("Failed to open config file: {}", e))
                            .kind(MessageDialogKind::Error)
                            .title("Error")
                            .blocking_show();
                    }
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
                            app.dialog()
                                .message(format!("Failed to reload config file: {}", e))
                                .kind(MessageDialogKind::Error)
                                .title("Config Error")
                                .blocking_show();
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
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

fn open_tidewave(app: &tauri::AppHandle, port: u16) {
    debug!("Opening Tidewave in browser");
    let url = format!("http://localhost:{}", port);
    if let Err(e) = app.opener().open_url(url, None::<&str>) {
        error!("Failed to open Tidewave: {}", e);
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
        use std::process::Command;
        Command::new("notepad.exe").arg(&config_path).spawn()?;
    }

    #[cfg(not(target_os = "windows"))]
    {
        app.opener()
            .open_path(config_path.to_str().unwrap(), None::<&str>)?;
    }

    Ok(())
}

fn handle_urls(app_handle: &tauri::AppHandle, port: u16, urls: &[tauri::Url]) {
    for url in urls.iter().filter(|u| u.scheme() == "tidewave") {
        let s = if url.host_str() == Some("localhost") {
            url.as_str().replacen("tidewave://", "http://", 1)
        } else {
            url.as_str().replacen("tidewave://", "https://", 1)
        };

        debug!("Handling tidewave URL: {}", s);

        app_handle
            .opener()
            .open_path(
                &format!("http://localhost:{}?path={}", port, s),
                None::<&str>,
            )
            .expect("could not open url");
    }
}

async fn check_for_updates_on_boot(app: tauri::AppHandle) -> tauri_plugin_updater::Result<()> {
    if let Some(update) = app.updater()?.check().await? {
        let should_install = app.dialog()
            .message(format!("Version {} is available!\n\nWould you like to download and install it now?", update.version))
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
                    app.dialog()
                        .message(format!("Failed to install update: {}", e))
                        .kind(MessageDialogKind::Error)
                        .title("Update Failed")
                        .blocking_show();
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
                app.dialog()
                    .message(format!("Failed to check for updates: {}", e))
                    .kind(MessageDialogKind::Error)
                    .title("Update Check Failed")
                    .blocking_show();
            }
        }
    });
}

async fn check_for_updates_async(app: tauri::AppHandle) -> tauri_plugin_updater::Result<()> {
    if let Some(update) = app.updater()?.check().await? {
        let should_install = app.dialog()
            .message(format!("Version {} is available!\n\nWould you like to download and install it now?", update.version))
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
                    app.dialog()
                        .message(format!("Failed to install update: {}", e))
                        .kind(MessageDialogKind::Error)
                        .title("Update Failed")
                        .blocking_show();
                }
            }
        }
    } else {
        app.dialog()
            .message(format!("You're running the latest version:\n\nv{}", app.package_info().version))
            .kind(MessageDialogKind::Info)
            .title("No Updates Available")
            .blocking_show();
    }

    Ok(())
}
