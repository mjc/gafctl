mod adapter;
mod connection;
mod discovery;
mod requests;
mod state;
#[cfg(test)]
pub(crate) mod test_support;
mod topics;

use std::sync::Arc;

use rumqttc_next::{AsyncClient, DisconnectProperties, DisconnectReasonCode};
use tokio::{
    sync::{mpsc, watch},
    task::JoinSet,
};
use tokio_util::task::AbortOnDropHandle;

use crate::service::DeviceService;
use crate::service::publication::StateSnapshot;
pub(crate) use adapter::run_device_requests;
pub(crate) use requests::{
    CONTROL_QUEUE_CAPACITY, MqttDeviceWork, MqttRefreshRequest, MqttReply, MqttRequest,
    MqttRequestIntake, RequestKind,
};
use topics::Topics;

pub(crate) struct MqttTasks {
    client: AsyncClient,
    event_loop: AbortOnDropHandle<()>,
    publishers: JoinSet<()>,
}

impl MqttTasks {
    pub(crate) async fn stop(self, deadline: tokio::time::Instant) {
        let mut publishers = self.publishers;
        let _ = tokio::time::timeout_at(deadline, publishers.shutdown()).await;
        let mut event_loop = self.event_loop;
        let disconnect = async {
            self.client
                .disconnect_with_properties_timeout(
                    DisconnectReasonCode::DisconnectWithWillMessage,
                    DisconnectProperties {
                        session_expiry_interval: None,
                        reason_string: None,
                        user_properties: Vec::new(),
                        server_reference: None,
                    },
                    deadline.saturating_duration_since(tokio::time::Instant::now()),
                )
                .await?;
            if let Err(error) = (&mut event_loop).await {
                tracing::warn!(%error, "MQTT event loop failed during shutdown");
            }
            Ok::<(), rumqttc_next::ClientError>(())
        };
        match tokio::time::timeout_at(deadline, disconnect).await {
            Ok(Ok(())) => {}
            outcome => {
                tracing::warn!(?outcome, "MQTT disconnect did not finish");
                event_loop.abort();
                let _ = event_loop.await;
            }
        }
    }
}

pub(crate) struct MqttBridge {
    pub(crate) tasks: MqttTasks,
    #[cfg(test)]
    pub(crate) state_updates: watch::Sender<Arc<StateSnapshot>>,
    pub(crate) device_requests: mpsc::Receiver<MqttDeviceWork>,
    pub(crate) request_intake: MqttRequestIntake,
}

pub(crate) struct MqttConfig {
    pub(crate) host: String,
    pub(crate) port: u16,
    pub(crate) username: String,
    pub(crate) password: String,
    pub(crate) discovery_enabled: bool,
}

pub(crate) fn start(
    config: MqttConfig,
    initial_state: StateSnapshot,
    service: &mut DeviceService,
) -> MqttBridge {
    start_inner(config, initial_state, Some(service))
}

#[cfg(test)]
pub(crate) fn start_for_test(config: MqttConfig, initial_state: StateSnapshot) -> MqttBridge {
    start_inner(config, initial_state, None)
}

fn start_inner(
    config: MqttConfig,
    initial_state: StateSnapshot,
    service: Option<&mut DeviceService>,
) -> MqttBridge {
    let discovery_enabled = config.discovery_enabled;
    let topics = Topics(initial_state.proxy_id);
    let (state_tx, state_rx) = watch::channel(Arc::new(initial_state));
    let service = service.map(|service| {
        service.attach_state_publication(state_tx.clone(), discovery_enabled);
        service.clone()
    });
    let (connected_tx, connected_rx) = watch::channel(false);
    let (control_tx, control_rx) = mpsc::channel(CONTROL_QUEUE_CAPACITY);
    let intake = MqttRequestIntake::new(control_tx);
    let (client, eventloop) = AsyncClient::builder(connection::mqtt_options(config, topics))
        .capacity(32)
        .build();
    let mut publishers = JoinSet::new();
    publishers.spawn(connection::setup_connection(
        client.clone(),
        topics,
        connected_rx.clone(),
    ));
    let event_loop = tokio::spawn(connection::run_event_loop(
        eventloop,
        client.clone(),
        topics,
        connected_tx,
        intake.clone(),
    ));
    publishers.spawn(state::publish_state_updates(
        client.clone(),
        topics,
        state_rx,
        connected_rx,
        discovery_enabled,
        service,
    ));
    MqttBridge {
        tasks: MqttTasks {
            client,
            event_loop: AbortOnDropHandle::new(event_loop),
            publishers,
        },
        #[cfg(test)]
        state_updates: state_tx,
        device_requests: control_rx,
        request_intake: intake,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rumqttc_next::{MqttOptions, PublishOptions, QoS};
    use std::time::Duration;
    use test_support::{
        config, mqtt_device, observed_client, receive_topic, snapshot, start_native_broker,
    };

    #[tokio::test]
    async fn native_shutdown_retains_offline_availability() {
        let broker = start_native_broker().await;
        let device = mqtt_device(gafctl_api::ProxyId::default(), "shutdown-availability");
        let topic = Topics(device.proxy_id).process_availability();
        let (observer, mut received) =
            observed_client("shutdown-availability-observer", broker.port);
        observer.subscribe(&topic, QoS::AtLeastOnce).await.unwrap();
        let bridge = start_for_test(config(broker.port, false), snapshot(device));
        let online = receive_topic(&mut received, &topic).await;
        assert_eq!(online.payload.as_ref(), b"online");
        bridge.request_intake.close();
        bridge
            .tasks
            .stop(tokio::time::Instant::now() + Duration::from_secs(5))
            .await;
        let offline = receive_topic(&mut received, &topic).await;
        assert_eq!(offline.payload.as_ref(), b"offline");
        let (late, mut replayed) = observed_client("shutdown-late-observer", broker.port);
        late.subscribe(&topic, QoS::AtLeastOnce).await.unwrap();
        let retained = receive_topic(&mut replayed, &topic).await;
        assert!(retained.retain);
        assert_eq!(retained.payload.as_ref(), b"offline");
    }

    #[tokio::test]
    async fn shutdown_handles_an_already_cancelled_event_loop() {
        let (client, _eventloop) =
            AsyncClient::builder(MqttOptions::new("cancelled-shutdown", ("localhost", 1))).build();
        let event_loop = tokio::spawn(std::future::pending::<()>());
        event_loop.abort();
        let tasks = MqttTasks {
            client,
            event_loop: AbortOnDropHandle::new(event_loop),
            publishers: (0..2).map(|_| async {}).collect(),
        };
        tasks
            .stop(tokio::time::Instant::now() + Duration::from_secs(1))
            .await;
    }

    #[tokio::test(start_paused = true)]
    async fn native_shutdown_obeys_the_deadline_when_the_event_loop_stalls() {
        let (client, eventloop) =
            AsyncClient::builder(MqttOptions::new("stalled-shutdown", ("localhost", 1))).build();
        let event_loop = tokio::spawn(async move {
            std::future::pending::<()>().await;
            drop(eventloop);
        });
        let tasks = MqttTasks {
            client: client.clone(),
            event_loop: AbortOnDropHandle::new(event_loop),
            publishers: (0..2).map(|_| std::future::pending()).collect(),
        };
        let started = tokio::time::Instant::now();
        tasks.stop(started + Duration::from_secs(3)).await;
        assert_eq!(started.elapsed(), Duration::from_secs(3));
        assert!(
            client
                .try_publish("after-stop", "payload", PublishOptions::at_least_once())
                .is_err()
        );
    }
}
