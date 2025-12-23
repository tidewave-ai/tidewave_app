# Tidewave

Tidewave's Desktop app and CLI.

[Latest release](https://github.com/tidewave-ai/tidewave_app/releases/latest).
[Nightly builds](https://github.com/tidewave-ai/tidewave_app/releases/tag/nightly).

## Development

### App

```bash
cargo install tauri-cli --version "=2.8.0" --locked
cargo tauri dev -- -- --debug --port 9878
```

To run as macOS app bundle (to use Info.plist, etc), run:

```bash
./run_macos.sh
```

Note the app will load your Tidewave settings file. The app
supports only a subset of the CLI flags, as they are only
used in development, and they override the values in settings.

### CLI

```bash
cargo run -p tidewave-cli [-- --help]
```

### HTTPS

**Generate a self-signed certificate for development:**

```bash
openssl req -x509 -newkey rsa:4096 -keyout key.pem -out cert.pem -sha256 -days 365 -nodes \
  -subj "/CN=localhost" \
  -addext "subjectAltName=DNS:localhost,IP:127.0.0.1"
```

**Run the server with HTTPS:**

```bash
# Run on default HTTP port (9832) and HTTPS port (9833)
cargo run -p tidewave-cli -- \
  --https-port 9833 \
  --https-cert-path ./cert.pem \
  --https-key-path ./key.pem
```

**Test the proxy configuration:**

```bash
curl -k "https://localhost:9833/proxy?url=https://httpbin.org/get"
```

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
