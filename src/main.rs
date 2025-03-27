use anyhow::Result;
use clap::Parser;
use futures::{FutureExt, StreamExt, TryStreamExt};
use http_client::{AsyncBody, EventSource, EventSourceFragment, HttpClient};
use http_client_reqwest::HttpClientReqwest;
use std::sync::Arc;
use tokio::{
    io::{self, AsyncBufReadExt},
    sync::Mutex,
};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

/// Model Context Protocol Gateway
///
/// Bridge for connecting to an MCP server and processing messages via stdio
#[derive(Parser)]
#[clap(version)]
pub struct Config {
    /// MCP endpoint URL
    #[clap(long, env = "ENDPOINT")]
    pub endpoint: String,

    /// Set logging verbosity
    #[clap(long, env = "RUST_LOG", default_value = "debug")]
    pub log_level: String,

    /// Authentication token
    #[clap(long, env = "AUTH_TOKEN")]
    pub auth_token: Option<String>,
}

async fn process_sse_stream(
    stream: impl futures::Stream<Item = Result<EventSourceFragment>>,
) -> Result<()> {
    tracing::debug!("Processing SSE stream from response");
    let mut stream = Box::pin(stream);

    loop {
        tokio::select! {
            fragment = stream.next() => {
                match fragment {
                    Some(Ok(EventSourceFragment::Event(event))) => {
                        tracing::debug!("Received event type: {}", event);
                    }
                    Some(Ok(EventSourceFragment::Id(id))) => {
                        tracing::debug!("Received event ID: {}", id);
                    }
                    Some(Ok(EventSourceFragment::Data(data))) => {
                        println!("{}", data);
                        tracing::debug!("Processed message data from SSE stream");
                    }
                    Some(Ok(_)) => {
                        // Other event source fragments
                    }
                    Some(Err(err)) => {
                        tracing::error!("Error in SSE stream: {:?}", err);
                        break;
                    }
                    None => {
                        tracing::debug!("SSE stream ended");
                        break;
                    }
                }
            }
            _ = futures::future::poll_fn(|cx| {
                let timeout = tokio::time::sleep(tokio::time::Duration::from_secs(30));
                futures::pin_mut!(timeout);
                timeout.poll_unpin(cx)
            }).fuse() => {
                tracing::warn!("SSE stream processing timed out after 30 seconds");
                break;
            }
        }
    }

    Ok(())
}

async fn process_stdin(
    client: Arc<dyn HttpClient>,
    endpoint: String,
    session_id: Arc<Mutex<Option<String>>>,
    auth_token: Option<String>,
) -> Result<()> {
    tracing::debug!("Starting to read from stdin");
    let mut stdin = io::BufReader::new(io::stdin()).lines();

    loop {
        match stdin.next_line().await {
            Ok(Some(input)) => {
                if input.is_empty() {
                    continue;
                }

                tracing::debug!("Received input from stdin: {} bytes", input.len());
                let mut request = http_client::http::Request::post(&endpoint)
                    .header("Content-Type", "application/json")
                    .header("Accept", "application/json, text/event-stream");

                // Add session ID if we have one
                let current_session_id = session_id.lock().await.clone();
                if let Some(id) = &current_session_id {
                    request = request.header("Mcp-Session-Id", id);
                    tracing::debug!("Using session ID for request: {}", id);
                }

                // Add Authorization header if auth token is provided
                if let Some(token) = &auth_token {
                    request = request.header("Authorization", format!("Bearer {}", token));
                    tracing::debug!("Adding Authorization Bearer token to POST request");
                }

                let request = request.body(AsyncBody::from(input))?;
                let mut response = client.send(request.clone()).await?;

                // Extract session ID from response if present and update shared state
                if let Some(id) = response.headers().get("Mcp-Session-Id") {
                    if let Ok(id_str) = id.to_str() {
                        let mut session = session_id.lock().await;
                        if session.is_none() {
                            *session = Some(id_str.to_string());
                            tracing::debug!("Received session ID from POST response: {}", id_str);
                        }
                    }
                }

                // Check response status and content type
                let status = response.status();
                tracing::debug!("Server response status: {}", status);

                // Handle 202 Accepted for notifications/responses
                if status == 202 {
                    continue;
                }

                // Check if response is JSON or SSE
                if let Some(content_type) = response.headers().get("Content-Type") {
                    if let Ok(content_type_str) = content_type.to_str() {
                        if content_type_str.contains("application/json") {
                            // Handle JSON response
                            let body = response.into_body();
                            let body: Vec<_> = body.try_collect().await?;
                            let json_str = String::from_utf8(body.into_iter().flatten().collect())?;
                            println!("{}", json_str);
                            tracing::debug!("Processed JSON response");
                        } else if content_type_str.contains("text/event-stream") {
                            // Handle SSE response
                            tracing::debug!("Received SSE stream as response");
                            let stream = client.parse_event_source_response(&mut response);
                            process_sse_stream(stream).await?;
                        }
                    }
                }
            }
            Ok(None) => {
                tracing::debug!("Stdin processing task completed");
                break;
            }
            Err(e) => {
                tracing::error!("Error reading from stdin: {:?}", e);
                break;
            }
        }
    }
    Ok(())
}

async fn run_bridge(endpoint: String, auth_token: Option<String>) -> Result<()> {
    tracing::debug!("Initialising bridge with Streamable HTTP transport");
    let client = Arc::new(HttpClientReqwest::default()) as Arc<dyn HttpClient>;

    // Shared session ID between tasks
    let session_id = Arc::new(Mutex::new(None::<String>));

    tracing::debug!("Starting stdin processing");
    process_stdin(client.clone(), endpoint, session_id.clone(), auth_token).await?;

    tracing::debug!("Bridge tasks completed");
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let config = Config::parse();

    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::new(format!(
            "{}",
            config.log_level
        )))
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
        .init();

    tracing::debug!("Starting bridge");
    match run_bridge(config.endpoint, config.auth_token).await {
        Ok(_) => {
            tracing::debug!("Bridge completed successfully");
            Ok(())
        }
        Err(err) => {
            tracing::error!("Bridge failed with error: {}", err);
            Err(err)
        }
    }
}
