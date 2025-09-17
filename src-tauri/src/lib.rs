mod server;

use tauri::{
    menu::{Menu, MenuItem},
    tray::TrayIconBuilder,
};
use tauri_plugin_cli::CliExt;
use tauri_plugin_deep_link::DeepLinkExt;
use tauri_plugin_dialog::{DialogExt, MessageDialogKind};
use tauri_plugin_opener::OpenerExt;
use tracing::{debug, info, error};

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
            let mut port = 3000;
            let mut serve_only = false;
            let mut debug_mode = false;

            match app.cli().matches() {
                Ok(matches) => {
                    // Check for debug flag at root level
                    if let Some(debug_arg) = matches.args.get("debug") {
                        if let Some(value) = debug_arg.value.as_bool() {
                            debug_mode = value;
                        }
                    }

                    // Check for port argument at root level
                    if let Some(port_arg) = matches.args.get("port") {
                        if let Some(port_str) = port_arg.value.as_str() {
                            if let Ok(p) = port_str.parse::<u16>() {
                                port = p;
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
                                    debug_mode = value;
                                }
                            }

                            // Check for port argument in subcommand args
                            if let Some(port_arg) = subcommand.matches.args.get("port") {
                                if let Some(port_str) = port_arg.value.as_str() {
                                    if let Ok(p) = port_str.parse::<u16>() {
                                        port = p;
                                    }
                                }
                            }
                        }
                    }
                }
                Err(_) => {}
            }

            // Initialize tracing
            let filter = if debug_mode {
                "debug"
            } else {
                "info"
            };
            tracing_subscriber::fmt()
                .with_env_filter(filter)
                .init();

            if debug_mode {
                debug!("Debug logging enabled");
                debug!("Port: {}", port);
                debug!("Serve only: {}", serve_only);
            }

            // If serve subcommand, run only the HTTP server
            if serve_only {
                info!("Starting in server-only mode on port {}", port);
                let rt = tokio::runtime::Runtime::new().expect("Failed to create runtime");
                rt.block_on(async move {
                    if let Err(e) = server::start_http_server(port).await {
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
            let _server_handle = tauri::async_runtime::spawn(async move {
                if let Err(e) = server::start_http_server(port).await {
                    app_handle_for_server.dialog()
                        .message(format!("HTTP server error: {}", e))
                        .kind(MessageDialogKind::Error)
                        .title("Error")
                        .blocking_show();
                    app_handle_for_server.exit(1);
                }
            });

            let test_i = MenuItem::with_id(app, "test", "test", true, None::<&str>)?;
            let quit_i = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&test_i, &quit_i])?;

            TrayIconBuilder::new()
            .menu(&menu)
            .icon(app.default_window_icon().unwrap().clone())
            .on_menu_event(|app, event| match event.id.as_ref() {
                "quit" => {
                    debug!("quit menu item was clicked");
                    app.exit(0);
                }
                "test" => {
                    debug!("test menu item clicked");
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
