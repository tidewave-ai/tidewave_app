# Tidewave

Tidewave's Desktop app and CLI.

[Latest release](https://github.com/tidewave-ai/tidewave_app/releases/latest).
[Nightly builds](https://github.com/tidewave-ai/tidewave_app/releases/tag/nightly).

## Development

### App

    $ cargo install tauri-cli --version "=2.8.0" --locked
    $ cargo tauri dev -- -- --debug

To run as macOS app bundle (to use Info.plist, etc), run:

    $ ./run_macos.sh

### CLI

    $ cargo run -p tidewave-cli [-- --help]

### Tests

Run all tests:

    $ cargo test

Run tests for a specific package:

    $ cargo test -p tidewave-core

## License

Copyright (c) 2025 Dashbit

Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at [http://www.apache.org/licenses/LICENSE-2.0](http://www.apache.org/licenses/LICENSE-2.0)

Unless required by applicable law or agreed to in writing, software
distributed under the License is distributed on an "AS IS" BASIS,
WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
See the License for the specific language governing permissions and
limitations under the License.
