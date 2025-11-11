// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    tidewave_lib::run()
}
