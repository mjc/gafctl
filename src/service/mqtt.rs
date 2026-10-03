use super::control::v2_request_is_fresh;
use super::{DeviceService, ServiceError};
use anyhow::Result;
use futures_util::{Stream, StreamExt, stream};
use gafctl_api::DeviceId;
use tokio::sync::mpsc;

#[cfg(feature = "mqtt")]
pub(crate) struct MqttRuntime {
    pub(crate) intake: crate::mqtt::MqttRequestIntake,
    pub(super) requests: tokio::task::JoinHandle<()>,
    pub(super) tasks: Option<crate::mqtt::MqttTasks>,
}

#[cfg(feature = "mqtt")]
impl MqttRuntime {
    pub(crate) async fn drain(&mut self, deadline: tokio::time::Instant) {
        self.intake.close();
        match tokio::time::timeout_at(deadline, &mut self.requests).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => tracing::warn!(%error, "MQTT request worker failed during shutdown"),
            Err(_) => {
                self.requests.abort();
                let _ = (&mut self.requests).await;
                tracing::warn!(
                    "MQTT shutdown drain deadline exceeded; unfinished command outcomes are unknown"
                );
            }
        }
        self.intake.drain_replies(deadline).await;
        if let Some(tasks) = self.tasks.take() {
            tasks.stop(deadline).await;
        }
    }
}

impl DeviceService {
    #[cfg(feature = "mqtt")]
    pub(crate) async fn start_mqtt(
        &mut self,
        config: crate::mqtt::MqttConfig,
    ) -> Result<MqttRuntime> {
        self.mqtt_discovery_enabled = config.discovery_enabled;
        let initial_state = self.state_snapshot().await?;
        let bridge = crate::mqtt::start(config, initial_state);
        self.state_updates = Some(bridge.state_updates);
        let requests = tokio::spawn(process_mqtt_requests(self.clone(), bridge.device_requests));
        Ok(MqttRuntime {
            intake: bridge.request_intake,
            requests,
            tasks: Some(bridge.tasks),
        })
    }
}

#[cfg(feature = "mqtt")]
pub(super) async fn process_mqtt_requests(
    state: DeviceService,
    controls: mpsc::Receiver<crate::mqtt::MqttDeviceWork>,
) {
    device_requests(controls)
        .for_each_concurrent(Some(crate::mqtt::CONTROL_QUEUE_CAPACITY), |work| {
            reply_to_device_request(&state, work)
        })
        .await;
}

#[cfg(feature = "mqtt")]
fn device_requests(
    controls: mpsc::Receiver<crate::mqtt::MqttDeviceWork>,
) -> impl Stream<Item = crate::mqtt::MqttDeviceWork> {
    stream::unfold(controls, receive_device_request)
}

#[cfg(feature = "mqtt")]
async fn receive_device_request(
    mut controls: mpsc::Receiver<crate::mqtt::MqttDeviceWork>,
) -> Option<(
    crate::mqtt::MqttDeviceWork,
    mpsc::Receiver<crate::mqtt::MqttDeviceWork>,
)> {
    let work = controls.recv().await?;
    Some((work, controls))
}

#[cfg(feature = "mqtt")]
async fn reply_to_device_request(state: &DeviceService, work: crate::mqtt::MqttDeviceWork) {
    let response = match work.request {
        crate::mqtt::MqttRequest::Control(request) => {
            crate::mqtt::MqttReply::Control(state.control(work.device_id, request).await)
        }
        crate::mqtt::MqttRequest::Refresh(request) => {
            process_mqtt_refresh(state, &work.device_id, request).await
        }
    };
    let _ = work.reply.send(response);
}

#[cfg(feature = "mqtt")]
pub(super) async fn process_mqtt_refresh(
    state: &DeviceService,
    device_id: &DeviceId,
    request: crate::mqtt::MqttRefreshRequest,
) -> crate::mqtt::MqttReply {
    use crate::mqtt::{MqttReply, RequestKind};
    let request_id = request.request_id.as_str().to_owned();
    if !v2_request_is_fresh(request.issued_at_unix_ms) {
        return MqttReply::Rejected {
            request_id,
            status: "stale_request",
            kind: RequestKind::Refresh,
        };
    }
    match state.refresh_device(device_id).await {
        Ok(response) => MqttReply::Refresh {
            request_id,
            status: response.status,
        },
        Err(error) => MqttReply::Rejected {
            request_id,
            status: match error {
                ServiceError::UnknownDevice => "unknown_device",
                ServiceError::BackendUnavailable => "backend_unavailable",
                ServiceError::UnsupportedRead => "unsupported_read",
                ServiceError::WorkerUnavailable
                | ServiceError::InvalidSources
                | ServiceError::OwnershipUnavailable
                | ServiceError::Persistence => "refresh_failed",
            },
            kind: RequestKind::Refresh,
        },
    }
}
