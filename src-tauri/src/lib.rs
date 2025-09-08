use tauri::{
    menu::{Menu, MenuItem},
    tray::TrayIconBuilder,
};
use tauri_plugin_deep_link::DeepLinkExt;
use tauri_plugin_opener::OpenerExt;
use tauri_plugin_dialog::{DialogExt, MessageDialogKind};

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_single_instance::init(|_app, args, _cwd| {
            println!("a new app instance was opened with {args:?} and the deep link event was already triggered");
        }))
        .plugin(tauri_plugin_deep_link::init())
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            #[cfg(any(target_os = "windows", target_os = "linux"))]
            app.deep_link()
                .register("tidewave")
                .expect("failed to register tidewave:// handler");

            let port = 3000;

            if let Some(urls) = app.deep_link().get_current()? {
                handle_urls(&app.handle(), port, &urls);
            }

            let app_handle_for_open = app.handle().clone();
            app.deep_link().on_open_url(move |event| {
                handle_urls(&app_handle_for_open, port, &event.urls());
            });

            let app_handle_for_server = app.handle().clone();
            let _server_handle = tauri::async_runtime::spawn(async move {
                if let Err(e) = start_http_server(port).await {
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
                    dbg!("quit menu item was clicked");
                    app.exit(0);
                }
                "test" => {
                    println!("test menu item clicked");
                }
                _ => {
                    println!("menu item {:?} not handled", event.id);
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

        dbg!(&s);

        app_handle
            .opener()
            .open_path(&format!("http://localhost:{}?path={}", port, s), None::<&str>)
            .expect("could not open url");
    }
}

async fn start_http_server(port: u16) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let app = axum::Router::new().route("/", axum::routing::get(root));

    let listener = tokio::net::TcpListener::bind(&format!("0.0.0.0:{}", port))
        .await?;
    println!("HTTP server running on {}", port);
    axum::serve(listener, app).await?;
    Ok(())
}

async fn root() -> axum::response::Html<&'static str> {
    axum::response::Html(
        "<h1>Hello, World!</h1><p>This is a simple Axum web server running in Tauri.</p>",
    )
}
