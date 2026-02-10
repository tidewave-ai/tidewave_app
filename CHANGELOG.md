# Changelog

## v0.3.4 (2026-02-10)

* Add file system watching endpoint
* Add /delete, /mmkdir, and /listdir endpoints

## v0.3.3 (2026-01-26)

* Handle WSL for /download endpoint

## v0.3.2 (2026-01-23)

* Allow extracting in /download endpoint

## v0.3.1 (2026-01-21)

* Add /download and /check-origin endpoint
* Return sytem info on /about

## v0.3.0 (2026-01-14)

* Allow `allowed_origins` to be set in the toml config file
* Add support for ACP fork and resume
* Track spawned processes and terminate them on exit (this uses process groups on Unix-like systems and job objects on Windows)
* Properly buffer ACP prompt responses while the client is disconnected and fix buffer race condition

## v0.2.7 (2025-12-10)

* Return CORS headers at /about

## v0.2.6 (2025-12-07)

* Fix write and stat endpoints on WSL
* Reopening the app opens the browser
* Display error dialog when we can't open url

## v0.2.5 (2025-12-02)

* Also serve Tidewave at *.localhost if scheme+port matches

## v0.2.4 (2025-11-27)

* Add more detailed output when ACP fails to initialize
* Do not forward app-specific env variables downstream

## v0.2.3 (2025-11-25)

* Allow `*.localhost` access
* Fix code signing on Windows

## v0.2.2 (2025-11-19)

* Fix invalid version number on 0.2.1 release
* Prepare WSL detection for Tidewave client

## v0.2.1 (2025-11-19)

* Add ARM64 AppImage build for Linux
* Ensure origin check runs for all routes

## v0.2.0 (2025-11-11)

* Cancel ACP sessions when client disconnects but doesn't reconnect
* Add hotkeys for menu items on macOS
* Add TLS support (configurable in the config file)
* Ensure custom certificates can be used properly on all platforms
* Allow the client to restart ACP agents when necessary

## v0.1.0 (2025-10-31)

Initial release.
