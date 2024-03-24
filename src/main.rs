use anyhow::Result;

use tracing::info;
use tracing_subscriber::{
    filter::LevelFilter, fmt::time::UtcTime, layer::SubscriberExt, util::SubscriberInitExt, Layer,
};

use crate::compositor::Compositor;

pub mod compositor;
pub mod connection;

fn setup_tracing() {
    let fmt_layer = tracing_subscriber::fmt::layer()
        .with_ansi(true)
        .with_file(true)
        .with_timer(UtcTime::rfc_3339())
        .with_filter(
            tracing_subscriber::filter::targets::Targets::new().with_targets([
                (
                    "recomp",
                    #[cfg(debug_assertions)]
                    LevelFilter::TRACE,
                    #[cfg(not(debug_assertions))]
                    tracing_subscriber::filter::EnvFilter::from_default_env(),
                ),
                ("wgpu", LevelFilter::WARN),
                ("tokio", LevelFilter::WARN),
            ]),
        );
    let perf_layer = tracing_timing::Builder::default()
        .layer(|| tracing_timing::Histogram::new(2).expect("to create histogram"));
    tracing_subscriber::registry()
        .with(fmt_layer)
        .with(perf_layer)
        .init();
}

#[tokio::main]
async fn main() -> Result<()> {
    setup_tracing();

    let session = Compositor::new(None).await?;

    info!("Connected to X11 server");

    session.run().await?;

    Ok(())
}
