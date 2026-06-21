//! Credit-free Telco-X serve placeholder: runs the [`telco_x_event_bridge`]
//! bridge with the built-in [`EchoInvoker`], so `telco-x.call` becomes live
//! end-to-end (the runner resumes await-mode calls) before a real Telco-X
//! runtime is wired through the `TelcoXDispatchInvoker` seam.
//!
//! ```text
//! GREENTIC_EVENTS_NATS_URL=nats://127.0.0.1:4222 \
//!   cargo run -p telco-x-event-bridge --bin telco-x-serve
//! ```
//!
//! Replace `EchoInvoker` with a production `TelcoXDispatchInvoker` (the Telco-X
//! solution layer) to serve real operations.

use std::sync::Arc;

use telco_x_event_bridge::{EchoInvoker, RUNTIME_NAME, request_topic, run_bridge};

const NATS_URL_ENV: &str = "GREENTIC_EVENTS_NATS_URL";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().init();

    let nats_url = std::env::var(NATS_URL_ENV)
        .map_err(|_| anyhow::anyhow!("{NATS_URL_ENV} is required (e.g. nats://127.0.0.1:4222)"))?;

    let client = async_nats::connect(&nats_url).await?;
    tracing::info!(
        runtime = RUNTIME_NAME,
        subject = %request_topic(RUNTIME_NAME),
        "telco-x-serve (EchoInvoker placeholder) connected to NATS"
    );

    run_bridge(client, Arc::new(EchoInvoker)).await
}
