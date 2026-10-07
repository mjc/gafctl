use crate::{api::router, backend::DeviceRegistry, service::DeviceService};
use anyhow::{Context, Result};
use futures_util::StreamExt;
use std::{future::IntoFuture, time::Duration};
use tokio::{
    net::TcpListener,
    sync::oneshot,
    task::JoinSet,
    time::{Instant, MissedTickBehavior, interval, timeout_at},
};
use tokio_stream::wrappers::IntervalStream;

pub(crate) mod cli;
pub(crate) mod config;
#[cfg(feature = "mqtt")]
pub(crate) mod mqtt;

const CLOUD_POLL_INTERVAL: Duration = Duration::from_secs(30);
#[cfg(feature = "mqtt")]
const STATE_PUBLICATION_INTERVAL: Duration = Duration::from_secs(1);
const TRANSPORT_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

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
    let registry = DeviceRegistry::load_optional(identity_store)
        .context("could not load local device identity mappings")?;
    config::validate_mqtt_ownership(
        &registry,
        device_id.is_some(),
        quickconnect_config
            .as_ref()
            .map(|config| config.account_id.as_str()),
        {
            #[cfg(feature = "mqtt")]
            {
                mqtt_config
                    .as_ref()
                    .is_some_and(|config| config.discovery_enabled)
            }
            #[cfg(not(feature = "mqtt"))]
            {
                false
            }
        },
    )?;
    let listener = TcpListener::bind(address)
        .await
        .context("could not bind HTTP listener")?;
    let mut state = match device_id {
        Some(device_id) => DeviceService::with_ble_device(device_id, registry),
        None => DeviceService::with_registry(registry),
    };
    if let Some(config) = quickconnect_config {
        state.start_quickconnect(config).await?;
    }
    #[cfg(feature = "mqtt")]
    let mqtt = match mqtt_config {
        Some(config) => Some(mqtt::start(&mut state, config).await?),
        None => None,
    };
    tracing::info!(%address, "Gafctl API listening");
    serve_http(
        listener,
        state,
        #[cfg(feature = "mqtt")]
        mqtt,
    )
    .await
}

pub(crate) fn start_polling(state: &DeviceService) -> (JoinSet<()>, Vec<tokio::task::AbortHandle>) {
    let mut polls = JoinSet::new();
    let stop_polls = [
        state
            .state_polling_enabled()
            .then(|| polls.spawn(poll_device(state.clone(), Duration::from_secs(3)))),
        #[cfg(feature = "mqtt")]
        state
            .state_publication_enabled()
            .then(|| polls.spawn(publish_state_periodically(state.clone()))),
        state
            .quickconnect_polling_enabled()
            .then(|| polls.spawn(poll_quickconnect_device(state.clone(), CLOUD_POLL_INTERVAL))),
    ]
    .into_iter()
    .flatten()
    .collect();
    (polls, stop_polls)
}

#[cfg(feature = "mqtt")]
pub(crate) async fn publish_state_periodically(state: DeviceService) {
    poll_ticks(STATE_PUBLICATION_INTERVAL)
        .for_each(|_| state.publish_state())
        .await;
}

async fn serve_http(
    listener: TcpListener,
    state: DeviceService,
    #[cfg(feature = "mqtt")] mut mqtt: Option<mqtt::MqttRuntime>,
) -> Result<()> {
    let (mut polls, stop_polls) = start_polling(&state);
    let shutdown_state = state.clone();
    #[cfg(feature = "mqtt")]
    let mqtt_intake = mqtt.as_ref().map(|runtime| runtime.intake.clone());
    let (shutdown_started, mut shutdown_deadlines) = oneshot::channel();
    let server = axum::serve(listener, router(state.clone()))
        .with_graceful_shutdown(async move {
            shutdown_signal().await;
            let transport_deadline = Instant::now() + TRANSPORT_SHUTDOWN_TIMEOUT;
            let backend_deadline = Instant::now() + crate::bluetooth::SHUTDOWN_DRAIN_TIMEOUT;
            shutdown_state.begin_backend_shutdown();
            #[cfg(feature = "mqtt")]
            if let Some(intake) = mqtt_intake {
                intake.close();
            }
            stop_polls.iter().for_each(tokio::task::AbortHandle::abort);
            let _ = shutdown_started.send((transport_deadline, backend_deadline));
        })
        .into_future();
    tokio::pin!(server);
    let (transport_deadline, backend_deadline) = tokio::select! {
        result = &mut server => {
            state.begin_backend_shutdown();
            polls.abort_all();
            let (transport_deadline, backend_deadline) = shutdown_deadlines
                .try_recv()
                .unwrap_or_else(|_| (
                    Instant::now() + TRANSPORT_SHUTDOWN_TIMEOUT,
                    Instant::now() + crate::bluetooth::SHUTDOWN_DRAIN_TIMEOUT,
                ));
            cleanup_transports(
                &state,
                #[cfg(feature = "mqtt")]
                &mut mqtt,
                transport_deadline,
                backend_deadline,
            ).await;
            return result.context("HTTP server failed");
        }
        deadlines = &mut shutdown_deadlines => deadlines.context("HTTP shutdown notification failed")?,
    };
    let (result, ()) = tokio::join!(
        timeout_at(transport_deadline, &mut server),
        cleanup_transports(
            &state,
            #[cfg(feature = "mqtt")]
            &mut mqtt,
            transport_deadline,
            backend_deadline,
        ),
    );
    match result {
        Ok(result) => result.context("HTTP server failed"),
        Err(_) => {
            tracing::warn!(
                "HTTP shutdown deadline exceeded; unfinished command outcomes are unknown"
            );
            Ok(())
        }
    }
}

async fn cleanup_transports(
    state: &DeviceService,
    #[cfg(feature = "mqtt")] mqtt: &mut Option<mqtt::MqttRuntime>,
    transport_deadline: Instant,
    backend_deadline: Instant,
) {
    let backend_cleanup = state.finish_backend_cleanup(backend_deadline);
    #[cfg(not(feature = "mqtt"))]
    let _ = transport_deadline;
    #[cfg(feature = "mqtt")]
    tokio::join!(backend_cleanup, async {
        if let Some(mqtt) = mqtt {
            mqtt.drain(transport_deadline).await;
        }
    });
    #[cfg(not(feature = "mqtt"))]
    backend_cleanup.await;
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
    futures_util::stream::repeat(())
        .fold(
            poll_failure_backoff(poll_interval),
            |mut backoff, ()| async {
                let status = state.poll_and_publish_state().await;
                let delay = if status == crate::model::DeviceRefreshStatus::Fresh {
                    backoff = poll_failure_backoff(poll_interval);
                    poll_interval
                } else {
                    backoff
                        .next()
                        .unwrap_or(Duration::from_secs(60))
                        .min(Duration::from_secs(60))
                };
                tokio::time::sleep(delay).await;
                backoff
            },
        )
        .await;
}

fn poll_failure_backoff(minimum: Duration) -> backon::ExponentialBackoff {
    use backon::BackoffBuilder as _;
    backon::ExponentialBuilder::default()
        .with_min_delay(minimum)
        .with_max_delay(Duration::from_secs(60))
        .without_max_times()
        .with_jitter()
        .build()
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
