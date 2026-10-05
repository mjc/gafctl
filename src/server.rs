use crate::{api::router, backend::DeviceRegistry, service::DeviceService};
use anyhow::{Context, Result};
use futures_util::StreamExt;
use std::time::Duration;
use tokio::{
    net::TcpListener,
    task::JoinSet,
    time::{MissedTickBehavior, interval},
};
use tokio_stream::wrappers::IntervalStream;

pub(crate) mod cli;
pub(crate) mod config;
#[cfg(feature = "mqtt")]
pub(crate) mod mqtt;

const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(30);
const SHUTDOWN_CLEANUP_TIMEOUT: Duration = Duration::from_secs(5);

pub(crate) async fn serve(config: config::ServerConfig) -> Result<()> {
    config.validate()?;
    let config::ServerConfig {
        device_id,
        identity_store,
        address,
        allow_remote: _,
        #[cfg(feature = "mqtt")]
        mqtt_config,
        quickconnect_config,
    } = config;
    let listener = TcpListener::bind(address)
        .await
        .context("could not bind HTTP listener")?;
    let registry = DeviceRegistry::load_optional(identity_store)
        .context("could not load local device identity mappings")?;
    #[cfg(feature = "mqtt")]
    anyhow::ensure!(
        !registry.mqtt_ownership_required(
            device_id.is_some(),
            quickconnect_config
                .as_ref()
                .map(|config| config.account_id.as_str())
        ) || mqtt_config
            .as_ref()
            .is_some_and(|config| config.discovery_enabled),
        "persisted MQTT ownership requires a configured broker and --mqtt-discovery"
    );
    let mut state = match device_id {
        Some(device_id) => DeviceService::with_ble_device(device_id, registry),
        None => DeviceService::with_registry(registry),
    };
    if let Some(config) = quickconnect_config {
        state.start_quickconnect(config).await?;
    }
    #[cfg(feature = "mqtt")]
    let mut mqtt = match mqtt_config {
        Some(config) => Some(mqtt::start(&mut state, config).await?),
        None => None,
    };
    let app = router(state.clone());
    let poll_state = state.state_polling_enabled();
    let poll_quickconnect = state.quickconnect_polling_enabled();
    let mut polls = JoinSet::new();
    let mut stop_polls = Vec::new();
    if poll_state {
        stop_polls.push(polls.spawn(poll_device(state.clone(), DEFAULT_POLL_INTERVAL)));
    }
    if poll_quickconnect {
        stop_polls.push(polls.spawn(poll_quickconnect_device(
            state.clone(),
            DEFAULT_POLL_INTERVAL,
        )));
    }

    tracing::info!(%address, "Gafctl API listening");
    #[cfg(feature = "mqtt")]
    let mqtt_intake = mqtt.as_ref().map(|runtime| runtime.intake.clone());
    let result = axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            shutdown_signal().await;
            #[cfg(feature = "mqtt")]
            if let Some(intake) = mqtt_intake {
                intake.close();
            }
            stop_polls.iter().for_each(tokio::task::AbortHandle::abort);
        })
        .await
        .context("HTTP server failed");
    polls.abort_all();
    let cleanup_deadline = tokio::time::Instant::now() + SHUTDOWN_CLEANUP_TIMEOUT;
    #[cfg(feature = "mqtt")]
    if let Some(mqtt) = &mut mqtt {
        mqtt.drain(cleanup_deadline).await;
    }
    // Cancelling a poll waiter leaves its BLE worker owning the backend until
    // disconnect cleanup completes. Give it a short grace after HTTP draining.
    state.finish_backend_cleanup(cleanup_deadline).await;
    result
}

async fn shutdown_signal() {
    let interrupt = async {
        if let Err(error) = tokio::signal::ctrl_c().await {
            tracing::error!(%error, "could not receive Ctrl-C");
        }
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(error) => tracing::error!(%error, "could not receive SIGTERM"),
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = interrupt => {},
        _ = terminate => {},
    }
    tracing::info!("HTTP shutdown requested");
}

async fn poll_device(state: DeviceService, poll_interval: Duration) {
    poll_ticks(poll_interval)
        .for_each(|_| state.poll_and_publish_state())
        .await;
}

async fn poll_quickconnect_device(state: DeviceService, poll_interval: Duration) {
    poll_ticks(poll_interval)
        .for_each(|_| state.poll_quickconnect())
        .await;
}

fn poll_ticks(poll_interval: Duration) -> IntervalStream {
    let mut ticker = interval(poll_interval);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    IntervalStream::new(ticker)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn polling_starts_immediately_and_skips_missed_ticks() {
        let started = tokio::time::Instant::now();
        let ticks = poll_ticks(Duration::from_secs(30));
        tokio::pin!(ticks);
        ticks.next().await.unwrap();
        assert_eq!(started.elapsed(), Duration::ZERO);

        tokio::time::sleep(Duration::from_secs(75)).await;
        ticks.next().await.unwrap();
        let resumed = tokio::time::Instant::now();
        ticks.next().await.unwrap();
        assert_eq!(resumed.elapsed(), Duration::from_secs(15));
    }
}
