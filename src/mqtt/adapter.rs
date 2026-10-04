use crate::service::{DeviceService, ServiceError, control::v2_request_is_fresh};
use futures_util::{Stream, StreamExt, stream};
use gafctl_api::DeviceId;
use tokio::sync::mpsc;

pub(crate) async fn run_device_requests(
    state: DeviceService,
    controls: mpsc::Receiver<crate::mqtt::MqttDeviceWork>,
) {
    device_requests(controls)
        .for_each_concurrent(Some(crate::mqtt::CONTROL_QUEUE_CAPACITY), |work| {
            reply_to_device_request(&state, work)
        })
        .await;
}

fn device_requests(
    controls: mpsc::Receiver<crate::mqtt::MqttDeviceWork>,
) -> impl Stream<Item = crate::mqtt::MqttDeviceWork> {
    stream::unfold(controls, receive_device_request)
}

async fn receive_device_request(
    mut controls: mpsc::Receiver<crate::mqtt::MqttDeviceWork>,
) -> Option<(
    crate::mqtt::MqttDeviceWork,
    mpsc::Receiver<crate::mqtt::MqttDeviceWork>,
)> {
    let work = controls.recv().await?;
    Some((work, controls))
}

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

async fn process_mqtt_refresh(
    state: &DeviceService,
    device_id: &DeviceId,
    request: crate::mqtt::MqttRefreshRequest,
) -> crate::mqtt::MqttReply {
    use crate::mqtt::{MqttReply, RequestKind};
    let request_id = request.request_id;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        mqtt::{MqttDeviceWork, MqttRefreshRequest, MqttReply, MqttRequest, MqttRequestIntake},
        service::test_support::refresh_fixture,
    };
    use gafctl_api::{CommandId, DeviceRefreshStatus, unix_millis};
    use std::{sync::atomic::Ordering, time::SystemTime};

    #[tokio::test]
    async fn mqtt_refresh_uses_device_reader_and_rechecks_queued_freshness() {
        let (state, id, fixture, _server, _directory) = refresh_fixture().await;
        let (sender, receiver) = mpsc::channel(2);
        let intake = MqttRequestIntake::new(sender);
        let request = |request_id, issued_at_unix_ms| {
            let (reply, response) = tokio::sync::oneshot::channel();
            let work = MqttDeviceWork {
                device_id: id.clone(),
                request: MqttRequest::Refresh(MqttRefreshRequest {
                    request_id: CommandId::parse(request_id).unwrap(),
                    issued_at_unix_ms,
                }),
                reply,
            };
            assert!(intake.try_send(work).is_ok());
            response
        };
        let stale = request("stale-read", 0);
        let worker = tokio::spawn(run_device_requests(state.clone(), receiver));
        let MqttReply::Rejected {
            request_id,
            status,
            kind,
        } = stale.await.unwrap()
        else {
            unreachable!("stale queued refresh must be rejected")
        };
        assert_eq!(request_id.as_str(), "stale-read");
        assert_eq!(status, "stale_request");
        assert_eq!(kind, crate::mqtt::RequestKind::Refresh);
        assert_eq!(fixture.reads.load(Ordering::SeqCst), 0);
        fixture.release.notify_one();
        let response = request("fresh-read", unix_millis(SystemTime::now()).unwrap());
        let MqttReply::Refresh { request_id, status } = response.await.unwrap() else {
            unreachable!("fresh queued refresh must return its correlated reply")
        };
        assert_eq!(request_id.as_str(), "fresh-read");
        assert_eq!(status, DeviceRefreshStatus::Fresh);
        assert_eq!(fixture.reads.load(Ordering::SeqCst), 1);
        assert!(state.state(&id).await.unwrap().available);
        intake.close();
        worker.await.unwrap();
    }

    #[test]
    fn mqtt_device_requests_reject_unknown_fields() {
        assert!(
            serde_json::from_str::<gafctl_api::ControlRequest>(
                r#"{"preset":"timer_clear","duration_minutes":999}"#
            )
            .is_err()
        );
    }
}
