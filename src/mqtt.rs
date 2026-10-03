mod connection;
mod discovery;
mod requests;
mod state;
#[cfg(test)]
mod test_support;
mod topics;

use std::sync::Arc;

use futures_util::{StreamExt, stream};
use tokio::sync::{mpsc, watch};

use crate::service::publication::StateSnapshot;
pub(crate) use requests::{
    CONTROL_QUEUE_CAPACITY, MqttDeviceWork, MqttRefreshRequest, MqttReply, MqttRequest,
    MqttRequestIntake, RequestKind,
};
use topics::Topics;

pub(crate) struct MqttTasks(Vec<tokio::task::JoinHandle<()>>);

impl MqttTasks {
    pub(crate) async fn stop(self, deadline: tokio::time::Instant) {
        self.0.iter().for_each(tokio::task::JoinHandle::abort);
        stream::iter(self.0)
            .for_each(|task| async move {
                if tokio::time::timeout_at(deadline, task).await.is_err() {
                    tracing::warn!("MQTT task shutdown deadline exceeded");
                }
            })
            .await;
    }
}

pub(crate) struct MqttBridge {
    pub(crate) tasks: MqttTasks,
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

pub(crate) fn start(config: MqttConfig, initial_state: StateSnapshot) -> MqttBridge {
    let discovery_enabled = config.discovery_enabled;
    let topics = Topics(initial_state.proxy_id);
    let (state_tx, state_rx) = watch::channel(Arc::new(initial_state));
    let (connected_tx, connected_rx) = watch::channel(false);
    let (control_tx, control_rx) = mpsc::channel(CONTROL_QUEUE_CAPACITY);
    let intake = MqttRequestIntake::new(control_tx);
    let (connection, [setup, event_loop, publisher]) = connection::start(
        config,
        topics,
        intake.clone(),
        connected_tx,
        connected_rx.clone(),
    );
    let states = tokio::spawn(state::publish_state_updates(
        connection,
        state_rx,
        connected_rx,
        discovery_enabled,
    ));
    MqttBridge {
        tasks: MqttTasks(vec![setup, event_loop, states, publisher]),
        state_updates: state_tx,
        device_requests: control_rx,
        request_intake: intake,
    }
}
