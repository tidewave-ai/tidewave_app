//! Watch demo page for testing Phoenix-based file watching.

use axum::response::Html;

pub async fn watch_demo_handler() -> Html<&'static str> {
    Html(WATCH_DEMO_HTML)
}

const WATCH_DEMO_HTML: &str = r#"<!DOCTYPE html>
<html>
<head>
    <meta charset="utf-8">
    <title>File Watch Demo</title>
    <script src="https://cdn.jsdelivr.net/npm/phoenix@1.7.14/priv/static/phoenix.min.js"></script>
    <style>
        * { box-sizing: border-box; }
        body {
            font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, sans-serif;
            max-width: 900px;
            margin: 40px auto;
            padding: 20px;
            background: #f5f5f5;
        }
        h1 { color: #333; margin-bottom: 10px; }
        .subtitle { color: #666; margin-bottom: 20px; }
        #status {
            padding: 8px 12px;
            border-radius: 4px;
            margin-bottom: 20px;
            font-weight: 500;
        }
        .connected { background: #d4edda; color: #155724; }
        .disconnected { background: #f8d7da; color: #721c24; }
        #events {
            background: white;
            border: 1px solid #ddd;
            border-radius: 8px;
            height: 350px;
            overflow-y: auto;
            padding: 15px;
            margin-bottom: 15px;
            font-family: monospace;
            font-size: 13px;
        }
        .event {
            padding: 6px 10px;
            margin: 4px 0;
            border-radius: 4px;
            border-left: 4px solid #ccc;
        }
        .event.created { border-left-color: #28a745; background: #d4edda; }
        .event.modified { border-left-color: #007bff; background: #cce5ff; }
        .event.deleted { border-left-color: #dc3545; background: #f8d7da; }
        .event.renamed { border-left-color: #6f42c1; background: #e2d9f3; }
        .event.warning { border-left-color: #ffc107; background: #fff3cd; }
        .event.error { border-left-color: #dc3545; background: #f8d7da; }
        .event.system { border-left-color: #6c757d; background: #e9ecef; }
        .event.joined { border-left-color: #17a2b8; background: #d1ecf1; }
        .event.left { border-left-color: #6c757d; background: #e9ecef; }
        .event-time { color: #666; margin-right: 10px; }
        .event-type { font-weight: bold; margin-right: 10px; text-transform: uppercase; }
        .event-path { color: #6c757d; font-size: 11px; }
        #add-watch-form {
            background: white;
            padding: 20px;
            border-radius: 8px;
            border: 1px solid #ddd;
            margin-bottom: 20px;
        }
        #add-watch-form input {
            flex: 1;
            padding: 12px;
            border: 1px solid #ddd;
            border-radius: 6px;
            font-size: 14px;
            font-family: monospace;
        }
        .input-row {
            display: flex;
            gap: 10px;
            margin-bottom: 10px;
        }
        button {
            padding: 12px 24px;
            background: #007bff;
            color: white;
            border: none;
            border-radius: 6px;
            cursor: pointer;
            font-size: 14px;
            white-space: nowrap;
        }
        button:hover { background: #0056b3; }
        button:disabled { background: #ccc; cursor: not-allowed; }
        button.secondary {
            background: #6c757d;
        }
        button.secondary:hover { background: #545b62; }
        button.danger {
            background: #dc3545;
            padding: 8px 16px;
            font-size: 12px;
        }
        button.danger:hover { background: #c82333; }
        .help-text {
            font-size: 12px;
            color: #666;
        }
        #watches {
            margin-bottom: 20px;
        }
        .watch-item {
            background: white;
            border: 1px solid #ddd;
            border-radius: 6px;
            padding: 12px 15px;
            margin-bottom: 8px;
            display: flex;
            align-items: center;
            justify-content: space-between;
        }
        .watch-item.joining {
            background: #fff3cd;
            border-color: #ffc107;
        }
        .watch-item.error {
            background: #f8d7da;
            border-color: #dc3545;
        }
        .watch-path {
            font-family: monospace;
            font-size: 14px;
            word-break: break-all;
            flex: 1;
            margin-right: 15px;
        }
        .watch-status {
            font-size: 11px;
            color: #666;
            margin-left: 10px;
        }
        .button-row {
            display: flex;
            gap: 10px;
        }
        #no-watches {
            color: #666;
            font-style: italic;
            padding: 20px;
            text-align: center;
            background: white;
            border: 1px dashed #ddd;
            border-radius: 6px;
        }
    </style>
</head>
<body>
    <h1>File Watch Demo</h1>
    <p class="subtitle">Test Phoenix-based file system watching</p>

    <div id="status" class="disconnected">Connecting...</div>

    <div id="add-watch-form">
        <div class="input-row">
            <input type="text" id="watch-path" placeholder="Enter absolute path to watch (e.g., /tmp/test)" value="/tmp">
            <button onclick="addWatch()" id="add-btn">Add Watch</button>
        </div>
        <p class="help-text">Enter an absolute path to a directory. You can watch multiple directories simultaneously.</p>
    </div>

    <h3>Active Watches</h3>
    <div id="watches">
        <div id="no-watches">No directories being watched. Add a path above to start.</div>
    </div>

    <div class="button-row" style="margin-bottom: 15px;">
        <button onclick="clearEvents()" class="secondary">Clear Events</button>
    </div>

    <div id="events"></div>

    <script>
        let socket = null;
        const channels = new Map(); // path -> channel

        function setStatus(text, className) {
            const status = document.getElementById('status');
            status.textContent = text;
            status.className = className;
        }

        function addEvent(type, message, path = null) {
            const events = document.getElementById('events');
            const div = document.createElement('div');
            div.className = 'event ' + type;

            const time = new Date().toLocaleTimeString();
            let html = `<span class="event-time">${time}</span><span class="event-type">${type}</span>${escapeHtml(message)}`;
            if (path) {
                html += `<br><span class="event-path">from: ${escapeHtml(path)}</span>`;
            }
            div.innerHTML = html;

            events.appendChild(div);
            events.scrollTop = events.scrollHeight;
        }

        function escapeHtml(text) {
            const div = document.createElement('div');
            div.textContent = text;
            return div.innerHTML;
        }

        function clearEvents() {
            document.getElementById('events').innerHTML = '';
            addEvent('system', 'Events cleared');
        }

        function updateWatchesList() {
            const container = document.getElementById('watches');
            const noWatches = document.getElementById('no-watches');

            if (channels.size === 0) {
                noWatches.style.display = 'block';
                // Remove all watch items except no-watches
                Array.from(container.querySelectorAll('.watch-item')).forEach(el => el.remove());
                return;
            }

            noWatches.style.display = 'none';

            // Update or create watch items
            channels.forEach((channelInfo, path) => {
                let item = container.querySelector(`[data-path="${CSS.escape(path)}"]`);
                if (!item) {
                    item = document.createElement('div');
                    item.className = 'watch-item';
                    item.setAttribute('data-path', path);
                    item.innerHTML = `
                        <div class="watch-path">${escapeHtml(path)}<span class="watch-status"></span></div>
                        <button class="danger" onclick="removeWatch('${escapeHtml(path).replace(/'/g, "\\'")}')">Stop</button>
                    `;
                    container.appendChild(item);
                }

                // Update status
                const statusSpan = item.querySelector('.watch-status');
                item.className = 'watch-item';
                if (channelInfo.status === 'joining') {
                    item.classList.add('joining');
                    statusSpan.textContent = ' (joining...)';
                } else if (channelInfo.status === 'error') {
                    item.classList.add('error');
                    statusSpan.textContent = ` (error: ${channelInfo.error})`;
                } else {
                    statusSpan.textContent = '';
                }
            });

            // Remove items for paths no longer watched
            Array.from(container.querySelectorAll('.watch-item')).forEach(item => {
                const path = item.getAttribute('data-path');
                if (!channels.has(path)) {
                    item.remove();
                }
            });
        }

        function addWatch() {
            const pathInput = document.getElementById('watch-path');
            const path = pathInput.value.trim();

            if (!path) {
                alert('Please enter a path to watch');
                return;
            }

            if (!path.startsWith('/')) {
                alert('Path must be absolute (start with /)');
                return;
            }

            if (channels.has(path)) {
                alert('Already watching this path');
                return;
            }

            if (!socket || socket.connectionState() !== 'open') {
                alert('Socket not connected. Please wait or refresh the page.');
                return;
            }

            // Join the watch channel with the path as the topic
            const topic = `watch:${path}`;
            const channel = socket.channel(topic, {});

            channels.set(path, { channel, status: 'joining', error: null });
            updateWatchesList();

            // Handle file events
            channel.on("created", (payload) => {
                addEvent('created', payload.path, path);
            });

            channel.on("modified", (payload) => {
                addEvent('modified', payload.path, path);
            });

            channel.on("deleted", (payload) => {
                addEvent('deleted', payload.path, path);
            });

            channel.on("renamed", (payload) => {
                addEvent('renamed', `${payload.from} → ${payload.to}`, path);
            });

            channel.on("warning", (payload) => {
                addEvent('warning', payload.message, path);
            });

            channel.on("error", (payload) => {
                addEvent('error', payload.reason, path);
            });

            // Handle join
            channel.join()
                .receive("ok", (resp) => {
                    console.log("Joined successfully", path, resp);
                    const info = channels.get(path);
                    if (info) {
                        info.status = 'watching';
                        info.canonicalPath = resp.path;
                        updateWatchesList();
                    }
                    addEvent('joined', `Started watching: ${resp.path || path}`);
                })
                .receive("error", (resp) => {
                    console.log("Unable to join", path, resp);
                    const info = channels.get(path);
                    if (info) {
                        info.status = 'error';
                        info.error = resp.reason || JSON.stringify(resp);
                        updateWatchesList();
                    }
                    addEvent('error', `Failed to watch ${path}: ${resp.reason || JSON.stringify(resp)}`);
                });

            pathInput.value = '';
        }

        function removeWatch(path) {
            const info = channels.get(path);
            if (!info) return;

            info.channel.leave()
                .receive("ok", () => {
                    addEvent('left', `Stopped watching: ${path}`);
                })
                .receive("error", (err) => {
                    addEvent('error', `Error leaving ${path}: ${JSON.stringify(err)}`);
                });

            channels.delete(path);
            updateWatchesList();
        }

        function connectSocket() {
            socket = new Phoenix.Socket("/socket", {});

            socket.onOpen(() => {
                console.log("Socket connected");
                setStatus("Connected - Ready to watch", "connected");
                addEvent('system', 'Socket connected');
                document.getElementById('add-btn').disabled = false;
            });

            socket.onClose(() => {
                console.log("Socket closed");
                setStatus("Disconnected", "disconnected");
                addEvent('system', 'Socket disconnected');
                document.getElementById('add-btn').disabled = true;
                // Clear all channels
                channels.clear();
                updateWatchesList();
            });

            socket.onError((err) => {
                console.log("Socket error", err);
                setStatus("Connection error", "disconnected");
                addEvent('error', 'Socket connection error');
            });

            socket.connect();
        }

        // Connect on page load
        document.getElementById('add-btn').disabled = true;
        addEvent('system', 'Connecting to server...');
        connectSocket();

        // Handle enter key in input
        document.getElementById('watch-path').addEventListener('keypress', (e) => {
            if (e.key === 'Enter') {
                addWatch();
            }
        });
    </script>
</body>
</html>
"#;
