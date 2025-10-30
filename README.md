# Tidewave

## Development

### App

    $ cargo install tauri-cli --version "=2.8.0" --locked
    $ cargo tauri dev -- -- --debug

To run as macOS app bundle (to use Info.plist, test tidewave:// handler, etc), run:

    $ ./run_app.sh

### CLI

    $ cargo run -p tidewave-cli [-- --help]

### Tests

Run all tests:

    $ cargo test

Run tests for a specific package:

    $ cargo test -p tidewave-core
