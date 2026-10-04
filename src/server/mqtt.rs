use crate::{
    mqtt::{MqttConfig, MqttRequestIntake, MqttTasks, run_device_requests},
    service::DeviceService,
};
use anyhow::Result;

pub(crate) struct MqttRuntime {
    pub(crate) intake: MqttRequestIntake,
    requests: tokio::task::JoinHandle<()>,
    tasks: Option<MqttTasks>,
}

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

pub(crate) async fn start(service: &mut DeviceService, config: MqttConfig) -> Result<MqttRuntime> {
    let allow_mqtt_ownership = config.discovery_enabled;
    let initial_state = service.state_snapshot().await?;
    let bridge = crate::mqtt::start(config, initial_state);
    service.attach_state_publication(bridge.state_updates, allow_mqtt_ownership);
    let requests = tokio::spawn(run_device_requests(service.clone(), bridge.device_requests));
    Ok(MqttRuntime {
        intake: bridge.request_intake,
        requests,
        tasks: Some(bridge.tasks),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::test_support::refresh_fixture;
    use gafctl_api::{CommandId, DeviceRefreshStatus, unix_millis};
    use std::time::SystemTime;
    use tokio::sync::mpsc;

    #[tokio::test(start_paused = true)]
    async fn mqtt_shutdown_drain_respects_the_cleanup_deadline() {
        let (sender, _receiver) = mpsc::channel(1);
        let intake = MqttRequestIntake::new(sender);
        let requests = tokio::spawn(std::future::pending::<()>());
        let mut runtime = MqttRuntime {
            intake,
            requests,
            tasks: None,
        };
        let started = tokio::time::Instant::now();
        runtime
            .drain(started + super::super::SHUTDOWN_CLEANUP_TIMEOUT)
            .await;
        assert!(runtime.requests.is_finished());
        assert_eq!(started.elapsed(), super::super::SHUTDOWN_CLEANUP_TIMEOUT);
    }

    #[tokio::test]
    async fn mqtt_shutdown_closes_intake_and_drains_an_accepted_request() {
        let (state, id, fixture, _server, _directory) = refresh_fixture().await;
        let (sender, receiver) = mpsc::channel(2);
        let intake = MqttRequestIntake::new(sender);
        let requests = tokio::spawn(run_device_requests(state, receiver));
        let work = |request_id: &str| {
            let (reply, response) = tokio::sync::oneshot::channel();
            (
                crate::mqtt::MqttDeviceWork {
                    device_id: id.clone(),
                    request: crate::mqtt::MqttRequest::Refresh(crate::mqtt::MqttRefreshRequest {
                        request_id: CommandId::parse(request_id).unwrap(),
                        issued_at_unix_ms: unix_millis(SystemTime::now()).unwrap(),
                    }),
                    reply,
                },
                response,
            )
        };
        let (accepted, response) = work("accepted-before-shutdown");
        assert!(intake.try_send(accepted).is_ok());
        fixture.entered.notified().await;
        let mut runtime = MqttRuntime {
            intake,
            requests,
            tasks: None,
        };
        runtime.intake.close();
        let (late, _) = work("after-shutdown");
        let Err(mpsc::error::TrySendError::Closed(_)) = runtime.intake.try_send(late) else {
            unreachable!("closed MQTT intake must reject later work");
        };
        assert!(!runtime.requests.is_finished());
        fixture.release.notify_one();
        runtime
            .drain(tokio::time::Instant::now() + super::super::SHUTDOWN_CLEANUP_TIMEOUT)
            .await;
        let crate::mqtt::MqttReply::Refresh { status, .. } = response.await.unwrap() else {
            unreachable!("refresh request must return a refresh reply");
        };
        assert_eq!(status, DeviceRefreshStatus::Fresh);
        assert!(runtime.requests.is_finished());
    }
}
