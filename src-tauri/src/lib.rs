use tauri::{
    menu::{Menu, MenuItem, PredefinedMenuItem},
    tray::TrayIconBuilder,
};
use tauri_plugin_cli::CliExt;
use tauri_plugin_deep_link::DeepLinkExt;
use tauri_plugin_dialog::{DialogExt, MessageDialogKind};
use tauri_plugin_opener::OpenerExt;
use tracing::{debug, info, error};
use std::fs;

const DEFAULT_CONFIG: &str = r#"# This file is used to configure Tidewave.
# If you change this file, you must restart Tidewave for your changes to take place.

# port = 9999
"#;

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_cli::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_single_instance::init(|_app, args, _cwd| {
            debug!("a new app instance was opened with {args:?} and the deep link event was already triggered");
        }))
        .plugin(tauri_plugin_deep_link::init())
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
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

            let mut serve_only = false;

            match app.cli().matches() {
                Ok(matches) => {
                    // Check for debug flag at root level
                    if let Some(debug_arg) = matches.args.get("debug") {
                        if let Some(value) = debug_arg.value.as_bool() {
                            if value {
                                config.debug = true;
                            }
                        }
                    }

                    // Check for port argument at root level
                    if let Some(port_arg) = matches.args.get("port") {
                        if let Some(port_str) = port_arg.value.as_str() {
                            if let Ok(p) = port_str.parse::<u16>() {
                                config.port = p;
                            }
                        }
                    }

                    // Check if serve subcommand was used
                    if let Some(subcommand) = matches.subcommand {
                        if subcommand.name == "serve" {
                            serve_only = true;

                            // Check for debug flag in subcommand args
                            if let Some(debug_arg) = subcommand.matches.args.get("debug") {
                                if let Some(value) = debug_arg.value.as_bool() {
                                    if value {
                                        config.debug = true;
                                    }
                                }
                            }

                            // Check for port argument in subcommand args
                            if let Some(port_arg) = subcommand.matches.args.get("port") {
                                if let Some(port_str) = port_arg.value.as_str() {
                                    if let Ok(p) = port_str.parse::<u16>() {
                                        config.port = p;
                                    }
                                }
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
                debug!("Serve only: {}", serve_only);
                debug!("{:?}", config);
            }

            let port = config.port;

            // If serve subcommand, run only the HTTP server
            if serve_only {
                info!("Starting in server-only mode on port {}", port);
                let rt = tokio::runtime::Runtime::new().expect("Failed to create runtime");
                let server_config = config.clone();
                rt.block_on(async move {
                    if let Err(e) = tidewave_core::start_http_server(server_config).await {
                        error!("HTTP server error: {}", e);
                        std::process::exit(1);
                    }
                });
                return Ok(());
            }

            // Normal GUI mode
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

            let app_handle_for_server = app.handle().clone();
            let server_config = config.clone();
            let _server_handle = tauri::async_runtime::spawn(async move {
                if let Err(e) = tidewave_core::start_http_server(server_config).await {
                    app_handle_for_server.dialog()
                        .message(format!("HTTP server error: {}", e))
                        .kind(MessageDialogKind::Error)
                        .title("Error")
                        .blocking_show();
                    app_handle_for_server.exit(1);
                }
            });

            let open_config_i = MenuItem::with_id(app, "open_config", "Open Configuration", true, None::<&str>)?;
            let reload_config_i = MenuItem::with_id(app, "reload_config", "Reload Configuration", true, None::<&str>)?;
            let separator = PredefinedMenuItem::separator(app)?;
            let quit_i = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&open_config_i, &reload_config_i, &separator, &quit_i])?;

            TrayIconBuilder::new()
            .menu(&menu)
            .icon(app.default_window_icon().unwrap().clone())
            .on_menu_event(|app, event| match event.id.as_ref() {
                "quit" => {
                    debug!("quit menu item was clicked");
                    app.exit(0);
                }
                "open_config" => {
                    debug!("Open Configuration menu item clicked");
                    if let Err(e) = open_config_file(app) {
                        error!("Failed to open config file: {}", e);
                        app.dialog()
                            .message(format!("Failed to open config file: {}", e))
                            .kind(MessageDialogKind::Error)
                            .title("Error")
                            .blocking_show();
                    }
                }
                "reload_config" => {
                    debug!("Reload Configuration menu item clicked");
                    match tidewave_core::load_config() {
                        Ok(config) => {
                            info!("Reloaded config: {:?}", config);
                            info!("Restarting application to apply new configuration...");
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
                _ => {
                    debug!("menu item {:?} not handled", event.id);
                }
            })
            .build(app)?;
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
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
        Command::new("notepad.exe")
            .arg(&config_path)
            .spawn()?;
    }

    #[cfg(not(target_os = "windows"))]
    {
        app.opener().open_path(config_path.to_str().unwrap(), None::<&str>)?;
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
