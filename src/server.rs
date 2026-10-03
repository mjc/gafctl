use crate::{
    api::{router, serve_http_until_shutdown},
    backend::DeviceRegistry,
    service::DeviceService,
};
use anyhow::{Context, Result};
use futures_util::{Stream, StreamExt, stream};
use std::{net::SocketAddr, time::Duration};
use tokio::{
    net::TcpListener,
    time::{Interval, MissedTickBehavior, interval},
};

pub(crate) mod cli;
pub(crate) mod config;

const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(30);
const SHUTDOWN_CLEANUP_TIMEOUT: Duration = Duration::from_secs(5);

pub(crate) async fn serve(config: config::ServerConfig) -> Result<()> {
    let config::ServerConfig {
        device_id,
        identity_store,
        address,
        allow_remote,
        #[cfg(feature = "mqtt")]
        mqtt_config,
        quickconnect_config,
    } = config;
    validate_bind_address(address, allow_remote)?;
    anyhow::ensure!(
        identity_store.is_some() || (device_id.is_none() && quickconnect_config.is_none()),
        "configured devices require --identity-store"
    );
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
        Some(config) => Some(state.start_mqtt(config).await?),
        None => None,
    };
    let app = router(state.clone());
    let poll_state = state.state_polling_enabled();
    let poll_quickconnect = state.quickconnect_polling_enabled();
    let mut polls = Vec::new();
    if poll_state {
        polls.push(tokio::spawn(poll_device(
            state.clone(),
            DEFAULT_POLL_INTERVAL,
        )));
    }
    if poll_quickconnect {
        polls.push(tokio::spawn(poll_quickconnect_device(
            state.clone(),
            DEFAULT_POLL_INTERVAL,
        )));
    }

    tracing::info!(%address, "Gafctl API listening");
    let stop_polls = polls
        .iter()
        .map(tokio::task::JoinHandle::abort_handle)
        .collect::<Vec<_>>();
    #[cfg(feature = "mqtt")]
    let mqtt_intake = mqtt.as_ref().map(|runtime| runtime.intake.clone());
    let result = serve_http_until_shutdown(listener, app, async move {
        shutdown_signal().await;
        #[cfg(feature = "mqtt")]
        if let Some(intake) = mqtt_intake {
            intake.close();
        }
        stop_polls.iter().for_each(tokio::task::AbortHandle::abort);
    })
    .await;
    polls.iter().for_each(|poll| poll.abort());
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

fn validate_bind_address(address: SocketAddr, allow_remote: bool) -> Result<()> {
    anyhow::ensure!(
        address.ip().is_loopback() || allow_remote,
        "non-loopback API binding requires --allow-remote"
    );
    Ok(())
}

async fn poll_device(state: DeviceService, poll_interval: Duration) {
    poll_ticks(poll_interval)
        .for_each(|()| state.poll_and_publish_state())
        .await;
}

async fn poll_quickconnect_device(state: DeviceService, poll_interval: Duration) {
    poll_ticks(poll_interval)
        .for_each(|()| state.poll_quickconnect())
        .await;
}

fn poll_ticks(poll_interval: Duration) -> impl Stream<Item = ()> {
    let mut ticker = interval(poll_interval);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    stream::unfold(ticker, wait_for_poll_tick)
}

async fn wait_for_poll_tick(mut ticker: Interval) -> Option<((), Interval)> {
    ticker.tick().await;
    Some(((), ticker))
}

#[cfg(test)]
mod tests;
