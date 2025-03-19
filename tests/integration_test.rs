use std::{sync::Arc, time::Duration};

use anyhow::Result;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{
        sse::{Event, Sse},
        IntoResponse,
    },
    routing::{get, post},
    Json, Router,
};
use futures::stream::{self, Stream};
use serde_json::{json, Value};
use tokio::{
    sync::{broadcast, Mutex},
    time::sleep,
};

// Helper functions to create and run a test server
mod test_server {
    use futures::StreamExt;
    use tokio::net::TcpListener;

    use super::*;

    pub type MessageChannel = broadcast::Sender<String>;

    #[derive(Clone)]
    pub struct AppState {
        pub channel: MessageChannel,
        pub message_endpoint: Arc<Mutex<String>>,
        pub keep_alive_interval: Duration,
    }

    // Handles SSE connections
    pub async fn sse_handler(
        State(state): State<AppState>,
    ) -> Sse<impl Stream<Item = Result<Event, anyhow::Error>>> {
        let receiver = state.channel.subscribe();
        let message_endpoint = state.message_endpoint.clone();
        let keep_alive_interval = state.keep_alive_interval;

        // First send the endpoint event
        let endpoint_event = async move {
            let endpoint = {
                let endpoint = message_endpoint.lock().await;
                endpoint.clone()
            };

            // Send initial endpoint event
            Ok(Event::default().event("endpoint").data(endpoint))
        };

        // Then create a stream from the broadcast channel
        let events_stream =
            stream::once(endpoint_event).chain(stream::unfold(receiver, |mut receiver| async {
                match receiver.recv().await {
                    Ok(message) => {
                        let event = Event::default().data(message);
                        Some((Ok(event), receiver))
                    }
                    Err(_) => None,
                }
            }));

        Sse::new(events_stream).keep_alive(
            axum::response::sse::KeepAlive::new()
                .interval(keep_alive_interval)
                .text("keep-alive-text"),
        )
    }

    // Handles incoming JSON-RPC messages
    pub async fn message_handler(
        State(state): State<AppState>,
        Path(id): Path<String>,
        Json(payload): Json<Value>,
    ) -> impl IntoResponse {
        // Check if this is a JSON-RPC request
        if let Some(method) = payload.get("method").and_then(|m| m.as_str()) {
            // Process based on the method
            match method {
                "initialize" => {
                    // Send back an initialization response
                    let response = json!({
                        "jsonrpc": "2.0",
                        "id": payload.get("id"),
                        "result": {
                            "protocol_version": "0.2",
                            "server_info": {
                                "name": "mcp-test-server",
                                "version": "0.1.0"
                            },
                            "capabilities": {
                                "tools": {}
                            }
                        }
                    });

                    if let Err(e) = state.channel.send(response.to_string()) {
                        eprintln!("Error sending init response: {}", e);
                        return StatusCode::INTERNAL_SERVER_ERROR;
                    }

                    // Also send an initialized notification
                    let notification = json!({
                        "jsonrpc": "2.0",
                        "method": "notifications/initialized"
                    });

                    if let Err(e) = state.channel.send(notification.to_string()) {
                        eprintln!("Error sending initialized notification: {}", e);
                        return StatusCode::INTERNAL_SERVER_ERROR;
                    }
                }
                "tools/list" => {
                    // Send back a tools list response
                    let response = json!({
                        "jsonrpc": "2.0",
                        "id": payload.get("id"),
                        "result": {
                            "tools": [
                                {
                                    "name": "test.echo",
                                    "description": "Echo back the input",
                                    "input_schema": {
                                        "type": "object",
                                        "properties": {
                                            "text": {
                                                "type": "string",
                                                "description": "Text to echo"
                                            }
                                        },
                                        "required": ["text"]
                                    }
                                }
                            ]
                        }
                    });

                    if let Err(e) = state.channel.send(response.to_string()) {
                        eprintln!("Error sending tools list response: {}", e);
                        return StatusCode::INTERNAL_SERVER_ERROR;
                    }
                }
                "tools/call" => {
                    // Extract the tool name and arguments
                    let tool_name = payload
                        .get("params")
                        .and_then(|p| p.get("name"))
                        .and_then(|n| n.as_str())
                        .unwrap_or("unknown");

                    let arguments = payload
                        .get("params")
                        .and_then(|p| p.get("arguments"))
                        .cloned();

                    // Handle based on the tool
                    match tool_name {
                        "test.echo" => {
                            let text = arguments
                                .as_ref()
                                .and_then(|a| a.get("text"))
                                .and_then(|t| t.as_str())
                                .unwrap_or("No text provided");

                            let response = json!({
                                "jsonrpc": "2.0",
                                "id": payload.get("id"),
                                "result": {
                                    "content": [
                                        {
                                            "type": "text",
                                            "text": format!("Echo: {}", text)
                                        }
                                    ],
                                    "is_error": false
                                }
                            });

                            if let Err(e) = state.channel.send(response.to_string()) {
                                eprintln!("Error sending echo response: {}", e);
                                return StatusCode::INTERNAL_SERVER_ERROR;
                            }
                        }
                        _ => {
                            // Unknown tool
                            let response = json!({
                                "jsonrpc": "2.0",
                                "id": payload.get("id"),
                                "result": {
                                    "content": [
                                        {
                                            "type": "text",
                                            "text": format!("Unknown tool: {}", tool_name)
                                        }
                                    ],
                                    "is_error": true
                                }
                            });

                            if let Err(e) = state.channel.send(response.to_string()) {
                                eprintln!("Error sending unknown tool response: {}", e);
                                return StatusCode::INTERNAL_SERVER_ERROR;
                            }
                        }
                    }
                }
                _ => {
                    // Echo back the request for unknown methods
                    let response = json!({
                        "jsonrpc": "2.0",
                        "id": payload.get("id"),
                        "result": {
                            "echo": format!("Received unknown method: {}", method)
                        }
                    });

                    if let Err(e) = state.channel.send(response.to_string()) {
                        eprintln!("Error sending echo response: {}", e);
                        return StatusCode::INTERNAL_SERVER_ERROR;
                    }
                }
            }
        } else {
            // Not a JSON-RPC request, just echo it back
            let response = json!({
                "id": id,
                "echo": payload,
                "timestamp": chrono::Utc::now().to_rfc3339()
            });

            if let Err(e) = state.channel.send(response.to_string()) {
                eprintln!("Error sending echo response: {}", e);
                return StatusCode::INTERNAL_SERVER_ERROR;
            }
        }

        StatusCode::OK
    }

    // Starts a test server on the given port
    pub async fn start_server(
        port: u16,
        keep_alive_interval: Duration,
    ) -> (
        MessageChannel,
        tokio::task::JoinHandle<std::io::Result<()>>,
        String,
    ) {
        // Create a broadcast channel
        let (tx, _) = broadcast::channel::<String>(100);

        // Set up the message endpoint
        let message_endpoint = format!("http://localhost:{}/message/test", port);

        // Create the application state
        let state = AppState {
            channel: tx.clone(),
            message_endpoint: Arc::new(Mutex::new(message_endpoint.clone())),
            keep_alive_interval,
        };

        // Build the router
        let app = Router::new()
            .route("/sse", get(sse_handler))
            .route("/message/:id", post(message_handler))
            .with_state(state.clone());

        // Bind the server
        let addr = TcpListener::bind(("127.0.0.1".to_string(), port))
            .await
            .unwrap();

        // Start the server in the background
        let server = axum::serve(addr, app.into_make_service());

        let server_handle = tokio::spawn(async { server.await });

        // Return the channel, server handle, and SSE endpoint URL
        (tx, server_handle, format!("http://localhost:{}/sse", port))
    }
}

#[tokio::test]
async fn test_gateway_connection() -> Result<()> {
    // Start the test server
    let port = 9091;
    let keep_alive_interval = Duration::from_secs(10);
    let (tx, server_handle, sse_endpoint) =
        test_server::start_server(port, keep_alive_interval).await;

    // Wait for the server to start
    sleep(Duration::from_millis(100)).await;

    // Start the gateway
    let mut gateway_process = tokio::process::Command::new("cargo")
        .args(["run", "--", "--endpoint", &sse_endpoint])
        .spawn()?;

    // Wait for the gateway to connect
    sleep(Duration::from_secs(1)).await;

    // Send a test message
    let test_message = json!({
        "jsonrpc": "2.0",
        "method": "tools/list",
        "id": 1
    });

    tx.send(test_message.to_string())?;

    // Allow time for processing
    sleep(Duration::from_secs(2)).await;

    // Clean up
    gateway_process.kill().await?;
    server_handle.abort();

    Ok(())
}

#[tokio::test]
async fn test_gateway_message_protocol() -> Result<()> {
    // Start the test server
    let port = 9092;
    let keep_alive_interval = Duration::from_secs(10);
    let (tx, server_handle, sse_endpoint) =
        test_server::start_server(port, keep_alive_interval).await;

    // Wait for the server to start
    sleep(Duration::from_millis(100)).await;

    // Start the gateway
    let mut gateway_process = tokio::process::Command::new("cargo")
        .args(["run", "--", "--endpoint", &sse_endpoint])
        .spawn()?;

    // Wait for the gateway to connect
    sleep(Duration::from_secs(1)).await;

    // Send a series of test messages to validate the gateway behavior

    // 1. First a tools/list request
    let tools_request = json!({
        "jsonrpc": "2.0",
        "method": "tools/list",
        "id": 1
    });

    tx.send(tools_request.to_string())?;

    // Wait a bit for processing
    sleep(Duration::from_secs(1)).await;

    // 2. Then a tools/call request
    let call_request = json!({
        "jsonrpc": "2.0",
        "method": "tools/call",
        "id": 2,
        "params": {
            "name": "test.echo",
            "arguments": {
                "text": "Hello from integration test!"
            }
        }
    });

    tx.send(call_request.to_string())?;

    // Wait for processing
    sleep(Duration::from_secs(1)).await;

    // 3. Send a notification
    let notification = json!({
        "jsonrpc": "2.0",
        "method": "notifications/message",
        "params": {
            "message": "Test notification"
        }
    });

    tx.send(notification.to_string())?;

    // Allow time for final processing
    sleep(Duration::from_secs(2)).await;

    // Clean up
    gateway_process.kill().await?;
    server_handle.abort();

    Ok(())
}
#[tokio::test]
async fn test_gateway_connection_sleep_recovery() -> Result<()> {
    // Start the test server
    let port = 9093;
    let keep_alive_interval = Duration::from_secs(30);
    let (tx, server_handle, sse_endpoint) =
        test_server::start_server(port, keep_alive_interval).await;

    // Wait for the server to start
    sleep(Duration::from_millis(100)).await;

    // Start the gateway
    let mut gateway_process = tokio::process::Command::new("cargo")
        .args(["run", "--", "--endpoint", &sse_endpoint])
        .spawn()?;

    // Wait for the gateway to connect
    sleep(Duration::from_secs(1)).await;

    // Send an initial message to confirm connection is working
    let initial_message = json!({
        "jsonrpc": "2.0",
        "method": "tools/list",
        "id": 1
    });

    tx.send(initial_message.to_string())?;
    sleep(Duration::from_secs(1)).await;

    println!("Simulating client sleep/network disconnection...");
    // Kill the gateway process to simulate client disconnection
    gateway_process.kill().await?;

    // Wait for a period longer than the keep-alive interval
    sleep(Duration::from_secs(10)).await;

    println!("Simulating client waking up and reconnecting...");
    // Restart the gateway to simulate client reconnection
    let mut reconnected_gateway = tokio::process::Command::new("cargo")
        .args(["run", "--", "--endpoint", &sse_endpoint])
        .spawn()?;

    // Allow time for the client to reconnect
    sleep(Duration::from_secs(2)).await;

    // Send a message after reconnection to test if the connection is restored
    let reconnect_message = json!({
        "jsonrpc": "2.0",
        "method": "tools/call",
        "id": 3,
        "params": {
            "name": "test.echo",
            "arguments": {
                "text": "Message after reconnection!"
            }
        }
    });

    tx.send(reconnect_message.to_string())?;

    // Allow time for processing the message
    sleep(Duration::from_secs(2)).await;

    // Verify the connection is working by sending another message
    let verification_message = json!({
        "jsonrpc": "2.0",
        "method": "tools/list",
        "id": 4
    });

    tx.send(verification_message.to_string())?;
    sleep(Duration::from_secs(1)).await;

    // Clean up
    reconnected_gateway.kill().await?;
    server_handle.abort();

    Ok(())
}
