use std::any::Any;
use std::time::Duration;

use async_trait::async_trait;
use axum::{response::Html, routing::get, Router};
use phoenix_rs::{
    channel::{Channel, HandleResult, JoinResult, SocketRef},
    phoenix_router, CancellationToken, ChannelRegistry, InfoSender,
};
use serde_json::{json, Value};
use tokio::net::TcpListener;

/// Info messages sent from background tasks to the channel
enum ChatInfo {
    /// Periodic tick with current count
    Tick(u32),
}

/// A simple chat room channel with background counter
struct ChatChannel;

#[async_trait]
impl Channel for ChatChannel {
    async fn join(&self, topic: &str, payload: Value, socket: &mut SocketRef) -> JoinResult {
        let username = payload
            .get("username")
            .and_then(|v| v.as_str())
            .unwrap_or("anonymous")
            .to_string();

        println!("User '{}' joined {}", username, topic);

        // Store username in assigns for later use
        socket.assign("username", username.clone());
        socket.assign("message_count", 0u32);

        // Get a typed info sender and shutdown token for background tasks
        if let Some(info_sender) = socket.info_sender::<ChatInfo>() {
            let shutdown = socket.shutdown_token().unwrap();
            // Spawn a background task that sends periodic tick messages
            // It will automatically stop when the subscription terminates
            tokio::spawn(counter_task(info_sender, shutdown));
        }

        JoinResult::ok(json!({
            "status": "joined",
            "topic": topic,
            "username": username
        }))
    }

    async fn handle_in(&self, event: &str, payload: Value, socket: &mut SocketRef) -> HandleResult {
        // Get username from assigns (stored during join)
        let username = socket.get_assign::<String>("username")
            .cloned()
            .unwrap_or_else(|| "unknown".to_string());

        match event {
            "shout" => {
                // Increment message count
                if let Some(count) = socket.get_assign_mut::<u32>("message_count") {
                    *count += 1;
                    println!("User '{}' sent message #{}", username, count);
                }

                // Broadcast with username injected from assigns
                let body = payload.get("body").and_then(|v| v.as_str()).unwrap_or("");
                socket.broadcast("shout", json!({
                    "username": username,
                    "body": body
                }));
                HandleResult::ok(json!({}))
            }
            "typing" => {
                // Broadcast typing indicator with username from assigns
                socket.broadcast_from("typing", json!({
                    "username": username
                }));
                HandleResult::no_reply()
            }
            _ => HandleResult::no_reply(),
        }
    }

    async fn handle_info(&self, message: Box<dyn Any + Send>, socket: &mut SocketRef) {
        // Downcast the message to our expected type
        if let Ok(info) = message.downcast::<ChatInfo>() {
            match *info {
                ChatInfo::Tick(count) => {
                    // Push the counter update to the client
                    socket.push("tick", json!({ "count": count }));
                }
            }
        }
    }

    async fn terminate(&self, reason: &str, socket: &mut SocketRef) {
        let username = socket.get_assign::<String>("username")
            .map(|s| s.as_str())
            .unwrap_or("unknown");
        let message_count = socket.get_assign::<u32>("message_count").copied().unwrap_or(0);
        println!("User '{}' left ({}), sent {} messages", username, reason, message_count);
    }
}

/// Background task that sends periodic tick messages via InfoSender.
/// Automatically stops when the subscription terminates thanks to the shutdown token.
async fn counter_task(info_sender: InfoSender<ChatInfo>, shutdown: CancellationToken) {
    let mut count = 0u32;
    loop {
        tokio::select! {
            // Clean exit when subscription terminates
            _ = shutdown.cancelled() => {
                println!("Counter task shutting down");
                break;
            }
            _ = tokio::time::sleep(Duration::from_secs(1)) => {
                count += 1;
                // Send tick to the channel's handle_info
                let _ = info_sender.send(ChatInfo::Tick(count));
            }
        }
    }
}

const INDEX_HTML: &str = r#"<!DOCTYPE html>
<html>
<head>
    <meta charset="utf-8">
    <title>Phoenix-RS Chat</title>
    <script src="https://cdn.jsdelivr.net/npm/phoenix@1.7.14/priv/static/phoenix.min.js"></script>
    <style>
        * { box-sizing: border-box; }
        body {
            font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, sans-serif;
            max-width: 600px;
            margin: 40px auto;
            padding: 20px;
            background: #f5f5f5;
        }
        h1 { color: #333; }
        #status {
            padding: 8px 12px;
            border-radius: 4px;
            margin-bottom: 20px;
            font-weight: 500;
        }
        .connected { background: #d4edda; color: #155724; }
        .disconnected { background: #f8d7da; color: #721c24; }
        .joining { background: #fff3cd; color: #856404; }
        #messages {
            background: white;
            border: 1px solid #ddd;
            border-radius: 8px;
            height: 300px;
            overflow-y: auto;
            padding: 15px;
            margin-bottom: 15px;
        }
        .message {
            padding: 8px 12px;
            margin: 5px 0;
            border-radius: 6px;
            background: #e9ecef;
        }
        .message.mine { background: #007bff; color: white; margin-left: 20%; }
        .message.system { background: #ffc107; color: #333; font-style: italic; }
        #input-area {
            display: flex;
            gap: 10px;
        }
        input {
            flex: 1;
            padding: 12px;
            border: 1px solid #ddd;
            border-radius: 6px;
            font-size: 14px;
        }
        button {
            padding: 12px 24px;
            background: #007bff;
            color: white;
            border: none;
            border-radius: 6px;
            cursor: pointer;
            font-size: 14px;
        }
        button:hover { background: #0056b3; }
        button:disabled { background: #ccc; cursor: not-allowed; }
        #username-form {
            background: white;
            padding: 20px;
            border-radius: 8px;
            border: 1px solid #ddd;
        }
        #username-form input { margin-bottom: 10px; width: 100%; }
        #typing-indicator {
            font-size: 12px;
            color: #666;
            font-style: italic;
            height: 18px;
            margin-bottom: 5px;
        }
    </style>
</head>
<body>
    <h1>Phoenix-RS Chat</h1>
    <div id="status" class="disconnected">Disconnected</div>
    <div id="counter" style="font-size: 12px; color: #666; margin-bottom: 10px;">Session time: 0s</div>

    <div id="username-form">
        <input type="text" id="username" placeholder="Enter your username" value="">
        <button onclick="joinChat()">Join Chat</button>
    </div>

    <div id="chat-area" style="display: none;">
        <div id="messages"></div>
        <div id="typing-indicator"></div>
        <div id="input-area">
            <input type="text" id="message" placeholder="Type a message..." onkeypress="if(event.key==='Enter')sendMessage()" oninput="onTyping()">
            <button onclick="sendMessage()">Send</button>
        </div>
    </div>

    <script>
        let socket = null;
        let channel = null;
        let username = '';
        let typingTimeout = null;
        let typingUsers = new Set();

        function setStatus(text, className) {
            const status = document.getElementById('status');
            status.textContent = text;
            status.className = className;
        }

        function addMessage(text, className = '') {
            const messages = document.getElementById('messages');
            const div = document.createElement('div');
            div.className = 'message ' + className;
            div.textContent = text;
            messages.appendChild(div);
            messages.scrollTop = messages.scrollHeight;
        }

        function joinChat() {
            username = document.getElementById('username').value.trim() || 'anonymous';

            // Connect to Phoenix socket
            // phoenix.js appends /websocket automatically, so use /socket
            socket = new Phoenix.Socket("/socket", {
                params: {}
            });

            socket.onOpen(() => {
                console.log("Socket connected");
                setStatus("Connected, joining room...", "joining");
            });

            socket.onClose(() => {
                console.log("Socket closed");
                setStatus("Disconnected", "disconnected");
            });

            socket.onError((err) => {
                console.log("Socket error", err);
                setStatus("Connection error", "disconnected");
            });

            socket.connect();

            // Join the lobby channel
            channel = socket.channel("room:lobby", { username: username });

            // Handle incoming messages
            channel.on("shout", (payload) => {
                const isMine = payload.username === username;
                addMessage(`${payload.username}: ${payload.body}`, isMine ? 'mine' : '');
                // Clear typing when they send a message
                typingUsers.delete(payload.username);
                updateTypingIndicator();
            });

            // Handle typing indicators
            channel.on("typing", (payload) => {
                typingUsers.add(payload.username);
                updateTypingIndicator();
                // Clear after 2 seconds of no typing
                setTimeout(() => {
                    typingUsers.delete(payload.username);
                    updateTypingIndicator();
                }, 2000);
            });

            // Handle tick counter from background task (demonstrates handle_info)
            channel.on("tick", (payload) => {
                document.getElementById('counter').textContent = `Session time: ${payload.count}s`;
            });

            // Handle join
            channel.join()
                .receive("ok", (resp) => {
                    console.log("Joined successfully", resp);
                    setStatus(`Connected as ${username}`, "connected");
                    document.getElementById('username-form').style.display = 'none';
                    document.getElementById('chat-area').style.display = 'block';
                    addMessage(`Welcome to the chat, ${username}!`, 'system');
                })
                .receive("error", (resp) => {
                    console.log("Unable to join", resp);
                    setStatus("Failed to join: " + JSON.stringify(resp), "disconnected");
                });
        }

        function sendMessage() {
            const input = document.getElementById('message');
            const body = input.value.trim();
            if (!body || !channel) return;

            channel.push("shout", { body: body })
                .receive("ok", () => {
                    input.value = '';
                })
                .receive("error", (err) => {
                    console.log("Failed to send", err);
                });
        }

        function onTyping() {
            if (!channel) return;
            // Throttle typing events
            if (!typingTimeout) {
                channel.push("typing", {});
                typingTimeout = setTimeout(() => { typingTimeout = null; }, 1000);
            }
        }

        function updateTypingIndicator() {
            const indicator = document.getElementById('typing-indicator');
            const users = Array.from(typingUsers);
            if (users.length === 0) {
                indicator.textContent = '';
            } else if (users.length === 1) {
                indicator.textContent = `${users[0]} is typing...`;
            } else {
                indicator.textContent = `${users.join(', ')} are typing...`;
            }
        }

        // Auto-focus username input
        document.getElementById('username').focus();
    </script>
</body>
</html>
"#;

async fn index_handler() -> Html<&'static str> {
    Html(INDEX_HTML)
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let mut registry = ChannelRegistry::new();
    registry.register("room:*", ChatChannel);

    // Merge Phoenix WebSocket routes with the HTML page
    let app = Router::new()
        .route("/", get(index_handler))
        .merge(phoenix_router(registry));

    let listener = TcpListener::bind("127.0.0.1:4000").await.unwrap();
    println!("Phoenix-RS chat server running at http://127.0.0.1:4000");
    println!("Open in browser to test the chat!");

    axum::serve(listener, app).await.unwrap();
}
