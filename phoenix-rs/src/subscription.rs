//! Subscription actor loop for handling channel messages.
//!
//! Each subscription (topic on a socket) runs as its own tokio task,
//! processing client messages and info messages sequentially.

use std::sync::Arc;

use serde_json::Value;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use crate::channel::{Assigns, BoxedInfo, Channel, HandleResult, JoinResult, SocketRef};
use crate::message::{events, PhxMessage};

/// Message sent to a subscription from the socket router
pub(crate) enum SubscriptionMsg {
    /// Client message with optional reply channel
    ClientMessage {
        event: String,
        payload: Value,
        msg_ref: Option<String>,
    },
    /// Leave request from client
    Leave { msg_ref: Option<String> },
    /// Terminate (disconnect or cleanup)
    Terminate { reason: String },
}

/// Result from the subscription loop
#[allow(dead_code)]
pub(crate) enum SubscriptionResult {
    /// Normal termination (leave or disconnect)
    Terminated,
    /// Channel requested stop
    Stopped { reason: String },
}

/// Handle to a running subscription
pub(crate) struct SubscriptionHandle {
    /// Join ref for this subscription
    pub join_ref: String,
    /// Sender to the subscription's message loop
    pub sender: mpsc::UnboundedSender<SubscriptionMsg>,
    /// Task handle for the subscription loop
    pub task: tokio::task::JoinHandle<SubscriptionResult>,
}

impl SubscriptionHandle {
    /// Send a client message to the subscription
    pub fn send_client_message(&self, event: String, payload: Value, msg_ref: Option<String>) {
        let _ = self.sender.send(SubscriptionMsg::ClientMessage {
            event,
            payload,
            msg_ref,
        });
    }

    /// Send a leave request to the subscription
    pub fn send_leave(&self, msg_ref: Option<String>) {
        let _ = self.sender.send(SubscriptionMsg::Leave { msg_ref });
    }

    /// Send a terminate signal to the subscription
    pub fn send_terminate(&self, reason: String) {
        let _ = self.sender.send(SubscriptionMsg::Terminate { reason });
    }
}

/// Spawn a new subscription task
pub(crate) async fn spawn_subscription(
    channel: Arc<dyn Channel>,
    topic: String,
    join_ref: String,
    join_payload: Value,
    client_sender: mpsc::UnboundedSender<PhxMessage>,
    broadcast_sender: mpsc::UnboundedSender<(String, PhxMessage)>,
) -> Result<(SubscriptionHandle, Value), Value> {
    // Create channels for the subscription
    let (msg_tx, msg_rx) = mpsc::unbounded_channel::<SubscriptionMsg>();
    let (info_tx, info_rx) = mpsc::unbounded_channel::<BoxedInfo>();

    // Create cancellation token for background tasks
    let shutdown_token = CancellationToken::new();

    // Create initial socket ref with info sender and shutdown token
    let mut socket_ref = SocketRef::with_info_sender(
        topic.clone(),
        join_ref.clone(),
        client_sender.clone(),
        broadcast_sender.clone(),
        info_tx.clone(),
        shutdown_token.clone(),
    );

    // Call join to see if we should accept
    let join_result = channel.join(&topic, join_payload, &mut socket_ref).await;

    match join_result {
        JoinResult::Ok(response) => {
            // Take the assigns set during join
            let assigns = socket_ref.take_assigns();

            // Spawn the subscription loop
            let task = tokio::spawn(subscription_loop(
                channel,
                topic.clone(),
                join_ref.clone(),
                assigns,
                msg_rx,
                info_rx,
                info_tx,
                shutdown_token,
                client_sender,
                broadcast_sender,
            ));

            let handle = SubscriptionHandle {
                join_ref,
                sender: msg_tx,
                task,
            };

            Ok((handle, response))
        }
        JoinResult::Error(reason) => Err(reason),
    }
}

/// The main subscription loop - processes messages sequentially
async fn subscription_loop(
    channel: Arc<dyn Channel>,
    topic: String,
    join_ref: String,
    initial_assigns: Assigns,
    mut msg_rx: mpsc::UnboundedReceiver<SubscriptionMsg>,
    mut info_rx: mpsc::UnboundedReceiver<BoxedInfo>,
    info_tx: mpsc::UnboundedSender<BoxedInfo>,
    shutdown_token: CancellationToken,
    client_sender: mpsc::UnboundedSender<PhxMessage>,
    broadcast_sender: mpsc::UnboundedSender<(String, PhxMessage)>,
) -> SubscriptionResult {
    let mut assigns = initial_assigns;

    loop {
        // Create a fresh socket_ref for each iteration with current assigns
        let mut socket_ref = SocketRef::with_assigns_and_info_sender(
            topic.clone(),
            join_ref.clone(),
            client_sender.clone(),
            broadcast_sender.clone(),
            std::mem::take(&mut assigns),
            info_tx.clone(),
            shutdown_token.clone(),
        );

        tokio::select! {
            // Handle messages from the socket router (client messages)
            Some(msg) = msg_rx.recv() => {
                match msg {
                    SubscriptionMsg::ClientMessage { event, payload, msg_ref } => {
                        debug!(topic = %topic, event = %event, "Handling client message");

                        let result = channel.handle_in(&event, payload, &mut socket_ref).await;

                        // Save assigns back
                        assigns = socket_ref.take_assigns();

                        match result {
                            HandleResult::Reply { status, response } => {
                                let reply = PhxMessage {
                                    join_ref: Some(join_ref.clone()),
                                    ref_: msg_ref,
                                    topic: topic.clone(),
                                    event: events::PHX_REPLY.to_string(),
                                    payload: serde_json::json!({
                                        "status": status.as_str(),
                                        "response": response,
                                    }),
                                };
                                let _ = client_sender.send(reply);
                            }
                            HandleResult::NoReply => {}
                            HandleResult::Stop { reason } => {
                                // Cancel shutdown token to stop background tasks
                                shutdown_token.cancel();

                                // Send close message
                                let close_msg = PhxMessage {
                                    join_ref: Some(join_ref.clone()),
                                    ref_: None,
                                    topic: topic.clone(),
                                    event: events::PHX_CLOSE.to_string(),
                                    payload: serde_json::json!({"reason": &reason}),
                                };
                                let _ = client_sender.send(close_msg);

                                // Call terminate
                                channel.terminate(&reason, &mut socket_ref).await;

                                return SubscriptionResult::Stopped { reason };
                            }
                        }
                    }
                    SubscriptionMsg::Leave { msg_ref } => {
                        debug!(topic = %topic, "Handling leave");

                        // Cancel shutdown token to stop background tasks
                        shutdown_token.cancel();

                        // Call terminate
                        channel.terminate("leave", &mut socket_ref).await;

                        // Send success reply
                        let reply = PhxMessage {
                            join_ref: Some(join_ref.clone()),
                            ref_: msg_ref,
                            topic: topic.clone(),
                            event: events::PHX_REPLY.to_string(),
                            payload: serde_json::json!({
                                "status": "ok",
                                "response": {},
                            }),
                        };
                        let _ = client_sender.send(reply);

                        return SubscriptionResult::Terminated;
                    }
                    SubscriptionMsg::Terminate { reason } => {
                        debug!(topic = %topic, reason = %reason, "Handling terminate");

                        // Cancel shutdown token to stop background tasks
                        shutdown_token.cancel();

                        // Call terminate
                        channel.terminate(&reason, &mut socket_ref).await;

                        return SubscriptionResult::Terminated;
                    }
                }
            }

            // Handle info messages from background tasks
            Some(info) = info_rx.recv() => {
                debug!(topic = %topic, "Handling info message");

                channel.handle_info(info, &mut socket_ref).await;

                // Save assigns back
                assigns = socket_ref.take_assigns();
            }

            // Both channels closed - shouldn't happen in normal operation
            else => {
                warn!(topic = %topic, "Subscription channels closed unexpectedly");
                return SubscriptionResult::Terminated;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use serde_json::json;
    use std::any::Any;
    use std::sync::atomic::{AtomicU32, Ordering};

    struct CounterChannel {
        join_count: AtomicU32,
    }

    impl CounterChannel {
        fn new() -> Self {
            Self {
                join_count: AtomicU32::new(0),
            }
        }
    }

    #[async_trait]
    impl Channel for CounterChannel {
        async fn join(&self, _topic: &str, _payload: Value, socket: &mut SocketRef) -> JoinResult {
            self.join_count.fetch_add(1, Ordering::SeqCst);
            socket.assign("counter", 0u32);
            JoinResult::ok(json!({"status": "joined"}))
        }

        async fn handle_in(
            &self,
            event: &str,
            _payload: Value,
            socket: &mut SocketRef,
        ) -> HandleResult {
            match event {
                "increment" => {
                    if let Some(counter) = socket.get_assign_mut::<u32>("counter") {
                        *counter += 1;
                        HandleResult::ok(json!({"counter": *counter}))
                    } else {
                        HandleResult::error(json!({"error": "no counter"}))
                    }
                }
                "get" => {
                    let counter = socket.get_assign::<u32>("counter").copied().unwrap_or(0);
                    HandleResult::ok(json!({"counter": counter}))
                }
                _ => HandleResult::no_reply(),
            }
        }

        async fn handle_info(&self, message: Box<dyn Any + Send>, socket: &mut SocketRef) {
            if let Ok(increment) = message.downcast::<u32>() {
                let new_value = {
                    if let Some(counter) = socket.get_assign_mut::<u32>("counter") {
                        *counter += *increment;
                        Some(*counter)
                    } else {
                        None
                    }
                };
                if let Some(value) = new_value {
                    socket.push("counter_updated", json!({"counter": value}));
                }
            }
        }
    }

    #[tokio::test]
    async fn test_subscription_join_and_messages() {
        let channel = Arc::new(CounterChannel::new());
        let (client_tx, mut client_rx) = mpsc::unbounded_channel();
        let (broadcast_tx, _broadcast_rx) = mpsc::unbounded_channel();

        let (handle, response) = spawn_subscription(
            channel.clone(),
            "counter:test".to_string(),
            "join1".to_string(),
            json!({}),
            client_tx,
            broadcast_tx,
        )
        .await
        .expect("join should succeed");

        assert_eq!(response["status"], "joined");

        // Send increment
        handle.send_client_message("increment".to_string(), json!({}), Some("1".to_string()));
        let reply = client_rx.recv().await.unwrap();
        assert_eq!(reply.payload["response"]["counter"], 1);

        // Send another increment
        handle.send_client_message("increment".to_string(), json!({}), Some("2".to_string()));
        let reply = client_rx.recv().await.unwrap();
        assert_eq!(reply.payload["response"]["counter"], 2);

        // Leave
        handle.send_leave(Some("3".to_string()));
        let reply = client_rx.recv().await.unwrap();
        assert_eq!(reply.payload["status"], "ok");

        // Task should complete
        let result = handle.task.await.unwrap();
        assert!(matches!(result, SubscriptionResult::Terminated));
    }

    #[tokio::test]
    async fn test_subscription_info_messages() {
        let (client_tx, mut client_rx) = mpsc::unbounded_channel();
        let (broadcast_tx, _broadcast_rx) = mpsc::unbounded_channel();

        // We need to get the info_sender during join, so let's create a channel that stores it
        struct InfoTestChannel;

        #[async_trait]
        impl Channel for InfoTestChannel {
            async fn join(
                &self,
                _topic: &str,
                _payload: Value,
                socket: &mut SocketRef,
            ) -> JoinResult {
                socket.assign("counter", 0u32);

                // Get info sender and store it in assigns for test access
                if let Some(sender) = socket.info_sender::<u32>() {
                    socket.assign("info_sender", sender);
                }

                JoinResult::ok(json!({"status": "joined"}))
            }

            async fn handle_in(
                &self,
                event: &str,
                _payload: Value,
                socket: &mut SocketRef,
            ) -> HandleResult {
                match event {
                    "get" => {
                        let counter = socket.get_assign::<u32>("counter").copied().unwrap_or(0);
                        HandleResult::ok(json!({"counter": counter}))
                    }
                    "send_info" => {
                        // Trigger an info message from within handle_in
                        if let Some(sender) = socket.get_assign::<InfoSender<u32>>("info_sender") {
                            sender.send(5).ok();
                        }
                        HandleResult::ok(json!({}))
                    }
                    _ => HandleResult::no_reply(),
                }
            }

            async fn handle_info(&self, message: Box<dyn Any + Send>, socket: &mut SocketRef) {
                if let Ok(increment) = message.downcast::<u32>() {
                    let new_value = {
                        if let Some(counter) = socket.get_assign_mut::<u32>("counter") {
                            *counter += *increment;
                            Some(*counter)
                        } else {
                            None
                        }
                    };
                    if let Some(value) = new_value {
                        socket.push("counter_updated", json!({"counter": value}));
                    }
                }
            }
        }

        use crate::channel::InfoSender;

        let channel = Arc::new(InfoTestChannel);

        let (handle, _response) = spawn_subscription(
            channel,
            "info:test".to_string(),
            "join1".to_string(),
            json!({}),
            client_tx,
            broadcast_tx,
        )
        .await
        .expect("join should succeed");

        // Trigger info message via handle_in
        handle.send_client_message("send_info".to_string(), json!({}), Some("1".to_string()));

        // Should have reply from handle_in
        let reply = client_rx.recv().await.unwrap();
        assert_eq!(reply.event, events::PHX_REPLY);

        // Should have push from handle_info
        let push = client_rx.recv().await.unwrap();
        assert_eq!(push.event, "counter_updated");
        assert_eq!(push.payload["counter"], 5);

        // Verify counter persisted
        handle.send_client_message("get".to_string(), json!({}), Some("2".to_string()));
        let reply = client_rx.recv().await.unwrap();
        assert_eq!(reply.payload["response"]["counter"], 5);

        handle.send_terminate("test".to_string());
        let _ = handle.task.await;
    }
}
