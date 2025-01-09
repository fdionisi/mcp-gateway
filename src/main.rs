use anyhow::Result;
use clap::Parser;
use futures::StreamExt;
use reqwest::Client;
use reqwest_eventsource::{Event, EventSource};
use tokio::{
    io::{self, AsyncBufReadExt},
    sync::mpsc,
};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

/// Model Context Protocol Gateway
///
/// Bridge for connecting to an SSE backend and processing messages via stdio
#[derive(Parser)]
pub struct Config {
    /// SSE endpoint URL
    #[clap(long, env = "ENDPOINT")]
    pub endpoint: String,

    /// Set logging verbosity
    #[clap(long, env = "RUST_LOG", default_value = "debug")]
    pub log_level: String,
}

async fn connect_sse_backend(
    client: Client,
    endpoint: String,
    tx: mpsc::Sender<String>,
) -> Result<()> {
    tracing::debug!("Connecting to SSE backend: {}", endpoint);
    let mut stream = EventSource::new(client.get(&endpoint))?;
    tracing::debug!("SSE stream established");

    while let Some(event) = stream.next().await {
        match event {
            Ok(Event::Open) => {
                tracing::debug!("SSE connection opened");
                continue;
            }
            Ok(Event::Message(message_event)) => {
                tracing::debug!("Received SSE message event: {:?}", message_event.event);
                if message_event.event == "endpoint" {
                    tx.send(message_event.data)
                        .await
                        .map_err(|_| anyhow::anyhow!("Failed to send endpoint URL"))?;
                    tracing::debug!("Sent endpoint URL");
                } else {
                    println!("{}", message_event.data);
                    tracing::debug!("Processed message data: {}", message_event.data);
                }
            }
            Err(err) => {
                tracing::debug!("Error in SSE stream: {:?}", err);
                stream.close();
                match err {
                    reqwest_eventsource::Error::StreamEnded => {
                        tracing::debug!("SSE stream ended");
                        continue;
                    }
                    _ => tracing::debug!("Unexpected error: {:?}", err),
                }
            }
        }
    }

    tracing::debug!("SSE connection closed");
    Ok(())
}

async fn process_stdin(client: Client, message_url: String) -> Result<()> {
    tracing::debug!("Starting to read from stdin");
    let mut stdin = io::BufReader::new(io::stdin()).lines();
    while let Some(line) = stdin.next_line().await? {
        tracing::debug!("Received input: {}", line);
        let response = client
            .post(&message_url)
            .header("Content-Type", "application/json")
            .body(line)
            .send()
            .await?;
        tracing::debug!("Sent message, response status: {:?}", response.status());
    }
    tracing::debug!("Stdin processing task completed");
    Ok(())
}

async fn run_bridge(endpoint: String) -> Result<()> {
    tracing::debug!("Initialising bridge");
    let client = Client::new();

    let (tx, mut rx) = mpsc::channel::<String>(1);

    tracing::debug!("Spawning SSE backend connection task");
    let sse_task = tokio::spawn(connect_sse_backend(client.clone(), endpoint, tx));
    tracing::debug!("Spawning stdin processing task");
    let client_clone = client.clone();
    let stdin_task = tokio::spawn(async move {
        tracing::debug!("Waiting for endpoint URL");
        let message_url = rx
            .recv()
            .await
            .ok_or_else(|| anyhow::anyhow!("Failed to receive endpoint URL"))?;
        tracing::debug!("Received endpoint URL: {}", message_url);

        process_stdin(client_clone, message_url).await?;
        Ok::<(), anyhow::Error>(())
    });

    // Wait for both tasks to complete
    let _ = tokio::try_join!(sse_task, stdin_task)?;

    tracing::debug!("Bridge tasks completed");
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let config = Config::parse();

    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::new(format!(
            "mcp_gateway={}",
            config.log_level
        )))
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
        .init();

    tracing::debug!("Starting bridge");
    match run_bridge(config.endpoint).await {
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
