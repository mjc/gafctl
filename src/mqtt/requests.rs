use std::{sync::Arc, time::Duration};

use futures_util::{StreamExt, stream};
use gafctl_api::{CommandId, DeviceControlV2Request, DeviceControlV2Response};
use rumqttc::v5::mqttbytes::v5::Publish;
use serde::{Deserialize, Serialize};
use tokio::{
    sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot},
    time::timeout,
};

use super::connection::MqttConnection;
use gafctl_api::DeviceId;

pub(crate) const CONTROL_QUEUE_CAPACITY: usize = 8;
pub(super) const MAX_PENDING_CONTROL_RESULTS: usize = 32;
const CONTROL_RESPONSE_TIMEOUT: Duration = Duration::from_secs(300);
const MAX_CONTROL_REQUEST_BYTES: usize = 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RequestKind {
    Control,
    Refresh,
}

impl RequestKind {
    pub(super) const fn result_suffix(self) -> &'static str {
        match self {
            Self::Control => "control/result",
            Self::Refresh => "refresh/result",
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MqttRefreshRequest {
    pub(crate) request_id: CommandId,
    pub(crate) issued_at_unix_ms: u64,
}

pub(crate) enum MqttRequest {
    Control(DeviceControlV2Request),
    Refresh(MqttRefreshRequest),
}

impl MqttRequest {
    pub(super) fn kind(&self) -> RequestKind {
        match self {
            Self::Control(_) => RequestKind::Control,
            Self::Refresh(_) => RequestKind::Refresh,
        }
    }
    pub(super) fn request_id(&self) -> &CommandId {
        match self {
            Self::Control(request) => &request.request_id,
            Self::Refresh(request) => &request.request_id,
        }
    }

    fn issued_at_unix_ms(&self) -> u64 {
        match self {
            Self::Control(request) => request.issued_at_unix_ms,
            Self::Refresh(request) => request.issued_at_unix_ms,
        }
    }
}

fn parse_request(kind: RequestKind, payload: &[u8]) -> Option<MqttRequest> {
    if payload.len() > MAX_CONTROL_REQUEST_BYTES {
        return None;
    }
    match kind {
        RequestKind::Control => parse_control_request(payload).map(MqttRequest::Control),
        RequestKind::Refresh => serde_json::from_slice(payload)
            .ok()
            .map(MqttRequest::Refresh),
    }
}

#[derive(Serialize)]
#[serde(untagged)]
pub(crate) enum MqttReply {
    Control(DeviceControlV2Response),
    Refresh {
        request_id: String,
        status: gafctl_api::DeviceRefreshStatus,
    },
    Rejected {
        request_id: String,
        status: &'static str,
        #[serde(skip)]
        kind: RequestKind,
    },
}

impl MqttReply {
    pub(super) fn kind(&self) -> RequestKind {
        match self {
            Self::Control(_) => RequestKind::Control,
            Self::Refresh { .. } => RequestKind::Refresh,
            Self::Rejected { kind, .. } => *kind,
        }
    }
}

pub(crate) struct MqttDeviceWork {
    pub(crate) device_id: DeviceId,
    pub(crate) request: MqttRequest,
    pub(crate) reply: oneshot::Sender<MqttReply>,
}

/// Closing intake drops the sole channel sender while preserving queued work.
#[derive(Clone)]
pub(crate) struct MqttRequestIntake {
    sender: Arc<std::sync::Mutex<Option<mpsc::Sender<MqttDeviceWork>>>>,
    replies: Arc<std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>>,
}

impl MqttRequestIntake {
    pub(crate) fn new(sender: mpsc::Sender<MqttDeviceWork>) -> Self {
        Self {
            sender: Arc::new(std::sync::Mutex::new(Some(sender))),
            replies: Arc::default(),
        }
    }

    pub(crate) fn close(&self) {
        self.sender
            .lock()
            .expect("MQTT intake lock poisoned")
            .take();
    }

    pub(crate) async fn drain_replies(&self, deadline: tokio::time::Instant) {
        let replies =
            std::mem::take(&mut *self.replies.lock().expect("MQTT replies lock poisoned"));
        stream::iter(replies)
            .for_each(|mut reply| async move {
                if tokio::time::timeout_at(deadline, &mut reply).await.is_err() {
                    reply.abort();
                    let _ = reply.await;
                    tracing::warn!("MQTT reply acknowledgement deadline exceeded");
                }
            })
            .await;
    }

    fn try_send_with_reply(
        &self,
        work: MqttDeviceWork,
        spawn_reply: impl FnOnce() -> tokio::task::JoinHandle<()>,
    ) -> Result<(), mpsc::error::TrySendError<MqttDeviceWork>> {
        let intake = self.sender.lock().expect("MQTT intake lock poisoned");
        let Some(sender) = intake.as_ref() else {
            return Err(mpsc::error::TrySendError::Closed(work));
        };
        sender.try_send(work)?;
        // Keep admission locked until the accepted request's publisher is owned.
        // close() therefore fences both work admission and reply registration.
        let reply = spawn_reply();
        let mut replies = self.replies.lock().expect("MQTT replies lock poisoned");
        replies.retain(|task| !task.is_finished());
        replies.push(reply);
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn try_send(
        &self,
        work: MqttDeviceWork,
    ) -> Result<(), mpsc::error::TrySendError<MqttDeviceWork>> {
        let intake = self.sender.lock().expect("MQTT intake lock poisoned");
        match intake.as_ref() {
            Some(sender) => sender.try_send(work),
            None => Err(mpsc::error::TrySendError::Closed(work)),
        }
    }
}

fn parse_control_request(payload: &[u8]) -> Option<DeviceControlV2Request> {
    if payload.len() > MAX_CONTROL_REQUEST_BYTES {
        tracing::warn!("rejected oversized MQTT control request");
        return None;
    }
    match serde_json::from_slice(payload) {
        Ok(request) => Some(request),
        Err(error) => {
            tracing::warn!(%error, "rejected malformed MQTT control request");
            None
        }
    }
}

pub(super) fn dispatch_request(
    connection: &MqttConnection,
    controls: &MqttRequestIntake,
    pending_results: &Arc<Semaphore>,
    device: DeviceId,
    kind: RequestKind,
    message: Publish,
) {
    let Some(request) = parse_request(kind, &message.payload) else {
        return;
    };
    if message.retain {
        connection.reject(&device, &request, "retained_request");
        return;
    }
    if !crate::service::control::v2_request_is_fresh(request.issued_at_unix_ms()) {
        connection.reject(&device, &request, "stale_request");
        return;
    }
    let Ok(permit) = Arc::clone(pending_results).try_acquire_owned() else {
        connection.reject(&device, &request, "control_results_busy");
        return;
    };
    enqueue_control(connection, controls, device, request, permit);
}

fn enqueue_control(
    connection: &MqttConnection,
    controls: &MqttRequestIntake,
    device: DeviceId,
    request: MqttRequest,
    permit: OwnedSemaphorePermit,
) {
    let (reply, response) = oneshot::channel();
    let uncertain = MqttReply::Rejected {
        request_id: request.request_id().as_str().to_owned(),
        status: "outcome_unknown",
        kind: request.kind(),
    };
    match controls.try_send_with_reply(
        MqttDeviceWork {
            device_id: device.clone(),
            request,
            reply,
        },
        || {
            tokio::spawn(publish_control_reply(
                connection.clone(),
                device,
                response,
                uncertain,
                permit,
            ))
        },
    ) {
        Ok(()) => {}
        Err(mpsc::error::TrySendError::Full(work)) => {
            connection.reject(&work.device_id, &work.request, "queue_full")
        }
        Err(mpsc::error::TrySendError::Closed(work)) => {
            connection.reject(&work.device_id, &work.request, "control_worker_unavailable")
        }
    }
}

async fn publish_control_reply(
    connection: MqttConnection,
    device: DeviceId,
    response: oneshot::Receiver<MqttReply>,
    uncertain: MqttReply,
    _permit: OwnedSemaphorePermit,
) {
    let result = wait_for_device_reply(response, uncertain).await;
    if timeout(
        Duration::from_secs(30),
        connection.publish_result(&device, &result),
    )
    .await
    .is_err()
    {
        tracing::warn!("MQTT result publication timed out");
    }
}

async fn wait_for_device_reply(
    response: oneshot::Receiver<MqttReply>,
    uncertain: MqttReply,
) -> MqttReply {
    match timeout(CONTROL_RESPONSE_TIMEOUT, response).await {
        Ok(Ok(result)) => result,
        Ok(Err(_)) | Err(_) => uncertain,
    }
}

#[cfg(test)]
mod tests {
    use super::super::{
        start,
        test_support::{
            config, mqtt_device, observed_client, receive_topic, request, snapshot,
            start_native_broker, test_connection,
        },
        topics::Topics,
    };
    use super::*;
    use gafctl_api::ProxyId;
    use gafctl_api::unix_millis;
    use rumqttc::v5::{AsyncClient, MqttOptions, mqttbytes::QoS};
    use serde_json::{Value, json};
    use std::time::SystemTime;

    #[tokio::test(start_paused = true)]
    async fn mqtt_deadline_or_closed_worker_returns_correlated_unknown_outcome() {
        for closed in [false, true] {
            let (sender, response) = oneshot::channel();
            let uncertain = MqttReply::Rejected {
                request_id: "uncertain".to_owned(),
                status: "outcome_unknown",
                kind: RequestKind::Control,
            };
            let sender = (!closed).then_some(sender);
            let response = wait_for_device_reply(response, uncertain).await;
            let response = serde_json::to_value(response).unwrap();
            assert_eq!(response["request_id"], "uncertain");
            assert_eq!(response["status"], "outcome_unknown");
            if let Some(sender) = sender {
                assert!(sender.is_closed());
            }
        }
    }

    #[test]
    fn refresh_topic_rejects_control_shapes_and_uses_correlated_fresh_requests() {
        let topics = Topics(ProxyId::default());
        let id = DeviceId::configured_ble();
        assert_eq!(
            topics.request_device(&topics.device(&id, "refresh/set")),
            Some((id.clone(), RequestKind::Refresh))
        );
        assert!(parse_request(RequestKind::Refresh, &request("wrong-control-shape")).is_none());
        let payload =
            serde_json::to_vec(&json!({"request_id":"fresh-read", "issued_at_unix_ms":0})).unwrap();
        let request = parse_request(RequestKind::Refresh, &payload).unwrap();
        assert_eq!(request.request_id().as_str(), "fresh-read");
        assert_eq!(request.issued_at_unix_ms(), 0);
    }

    #[test]
    fn controls_require_bounded_typed_correlated_requests() {
        let parsed = parse_control_request(&request("command-1")).unwrap();
        assert_eq!(parsed.request_id.as_str(), "command-1");
        for payload in [b"not json".as_slice(), br#"{"request_id":"invalid id","issued_at_unix_ms":1,"command":{"kind":"quick_connect_mode","mode":"automatic"}}"#.as_slice(), br#"{"request_id":"id","command":{"kind":"quick_connect_mode","mode":"automatic"}}"#.as_slice(), br#"{"request_id":"id","issued_at_unix_ms":1,"preset":"timer_clear"}"#.as_slice()] {
            assert!(parse_control_request(payload).is_none());
        }
        assert!(parse_control_request(&vec![b' '; MAX_CONTROL_REQUEST_BYTES + 1]).is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mqtt_shutdown_cannot_miss_a_reply_being_registered() {
        use futures_util::FutureExt;
        let (sender, mut work_receiver) = mpsc::channel(1);
        let intake = MqttRequestIntake::new(sender);
        let (reply, _response) = oneshot::channel();
        let work = MqttDeviceWork {
            device_id: DeviceId::configured_ble(),
            request: MqttRequest::Control(
                parse_control_request(&request("atomic-admission")).unwrap(),
            ),
            reply,
        };
        let (entered, entering) = oneshot::channel();
        let (release, released) = std::sync::mpsc::channel();
        let (published, publication) = oneshot::channel();
        let admitting = intake.clone();
        let runtime = tokio::runtime::Handle::current();
        let admission = tokio::task::spawn_blocking(move || {
            admitting
                .try_send_with_reply(work, || {
                    entered.send(()).unwrap();
                    released.recv().unwrap();
                    runtime.spawn(async {
                        publication.await.unwrap();
                    })
                })
                .unwrap();
        });
        entering.await.unwrap();
        assert!(
            intake.sender.try_lock().is_err(),
            "admission must hold the closure lock through registration"
        );
        let closing = intake.clone();
        let closure = tokio::task::spawn_blocking(move || closing.close());
        release.send(()).unwrap();
        admission.await.unwrap();
        closure.await.unwrap();
        assert!(work_receiver.recv().await.is_some());
        assert!(work_receiver.recv().await.is_none());
        let drain = intake.drain_replies(tokio::time::Instant::now() + Duration::from_secs(5));
        tokio::pin!(drain);
        assert!(
            drain.as_mut().now_or_never().is_none(),
            "drain must wait for the registered publisher"
        );
        published.send(()).unwrap();
        drain.await;
    }

    #[tokio::test]
    async fn stalled_results_bound_accepted_controls() {
        let (client, eventloop) = AsyncClient::new(MqttOptions::new("test", "localhost", 1883), 1);
        let (connection, _publisher) = test_connection(client, Topics(ProxyId::default()));
        let (controls, mut queued) = mpsc::channel(CONTROL_QUEUE_CAPACITY);
        let controls = MqttRequestIntake::new(controls);
        let pending = Arc::new(Semaphore::new(2));
        for index in 0..2 {
            dispatch_request(
                &connection,
                &controls,
                &pending,
                DeviceId::configured_ble(),
                RequestKind::Control,
                Publish::new(
                    "unused",
                    QoS::AtLeastOnce,
                    request(&format!("request-{index}")),
                    None,
                ),
            );
            let work = queued.try_recv().expect("control should be accepted");
            work.reply
                .send(MqttReply::Control(DeviceControlV2Response {
                    request_id: work.request.request_id().as_str().to_owned(),
                    status: "confirmed".into(),
                }))
                .ok()
                .expect("control reply receiver ended");
            tokio::task::yield_now().await;
        }
        dispatch_request(
            &connection,
            &controls,
            &pending,
            DeviceId::configured_ble(),
            RequestKind::Control,
            Publish::new("unused", QoS::AtLeastOnce, request("excess"), None),
        );
        assert!(queued.try_recv().is_err());
        drop(eventloop);
    }

    #[tokio::test]
    async fn native_broker_rejects_retained_and_stale_requests_and_correlates_results() {
        let broker = start_native_broker().await;
        let device = mqtt_device(ProxyId::default(), "qc-one");
        let topics = Topics(device.proxy_id);
        let (observer, mut received) = observed_client("control-observer", broker.port);
        observer
            .subscribe(topics.process_availability(), QoS::AtLeastOnce)
            .await
            .unwrap();
        observer
            .subscribe(
                topics.device(&device.id, "control/result"),
                QoS::AtLeastOnce,
            )
            .await
            .unwrap();
        let mut bridge = start(config(broker.port, false), snapshot(device.clone()));
        receive_topic(&mut received, &topics.process_availability()).await;
        observer
            .publish(
                topics.device(&device.id, "control/set"),
                QoS::AtLeastOnce,
                true,
                request("retained"),
            )
            .await
            .unwrap();
        let result =
            receive_topic(&mut received, &topics.device(&device.id, "control/result")).await;
        let result: Value = serde_json::from_slice(&result.payload).unwrap();
        assert_eq!(result["request_id"], "retained");
        assert_eq!(result["status"], "retained_request");
        assert!(bridge.device_requests.try_recv().is_err());
        let stale = json!({"request_id":"stale", "issued_at_unix_ms":1, "command":{"kind":"quick_connect_mode","mode":"off"}});
        observer
            .publish(
                topics.device(&device.id, "control/set"),
                QoS::AtLeastOnce,
                false,
                serde_json::to_vec(&stale).unwrap(),
            )
            .await
            .unwrap();
        let result =
            receive_topic(&mut received, &topics.device(&device.id, "control/result")).await;
        assert_eq!(
            serde_json::from_slice::<Value>(&result.payload).unwrap()["status"],
            "stale_request"
        );
        observer
            .publish(
                topics.device(&device.id, "control/set"),
                QoS::AtLeastOnce,
                false,
                request("confirmed"),
            )
            .await
            .unwrap();
        let work = timeout(Duration::from_secs(5), bridge.device_requests.recv())
            .await
            .unwrap()
            .unwrap();
        work.reply
            .send(MqttReply::Control(DeviceControlV2Response {
                request_id: work.request.request_id().as_str().to_owned(),
                status: "confirmed".into(),
            }))
            .ok()
            .expect("control reply receiver ended");
        let result =
            receive_topic(&mut received, &topics.device(&device.id, "control/result")).await;
        assert_eq!(result.qos, QoS::AtLeastOnce);
        assert!(!result.retain);
        let result: Value = serde_json::from_slice(&result.payload).unwrap();
        assert_eq!(result["request_id"], "confirmed");
        assert_eq!(result["status"], "confirmed");
    }

    #[tokio::test]
    async fn native_broker_shutdown_delivers_an_accepted_correlated_reply() {
        let broker = start_native_broker().await;
        let device = mqtt_device(ProxyId::default(), "shutdown-device");
        let topics = Topics(device.proxy_id);
        let (observer, mut received) = observed_client("shutdown-observer", broker.port);
        observer
            .subscribe(topics.process_availability(), QoS::AtLeastOnce)
            .await
            .unwrap();
        observer
            .subscribe(
                topics.device(&device.id, "control/result"),
                QoS::AtLeastOnce,
            )
            .await
            .unwrap();
        let mut bridge = start(config(broker.port, false), snapshot(device.clone()));
        receive_topic(&mut received, &topics.process_availability()).await;
        observer
            .publish(
                topics.device(&device.id, "control/set"),
                QoS::AtLeastOnce,
                false,
                request("shutdown-reply"),
            )
            .await
            .unwrap();
        let work = timeout(Duration::from_secs(5), bridge.device_requests.recv())
            .await
            .unwrap()
            .unwrap();
        bridge.request_intake.close();
        let intake = bridge.request_intake.clone();
        let draining = tokio::spawn(async move {
            intake
                .drain_replies(tokio::time::Instant::now() + Duration::from_secs(5))
                .await;
            bridge
                .tasks
                .stop(tokio::time::Instant::now() + Duration::from_secs(5))
                .await;
        });
        tokio::task::yield_now().await;
        assert!(!draining.is_finished());
        work.reply
            .send(MqttReply::Control(DeviceControlV2Response {
                request_id: "shutdown-reply".into(),
                status: "confirmed".into(),
            }))
            .ok()
            .unwrap();
        let result =
            receive_topic(&mut received, &topics.device(&device.id, "control/result")).await;
        let result: Value = serde_json::from_slice(&result.payload).unwrap();
        assert_eq!(result["request_id"], "shutdown-reply");
        assert_eq!(result["status"], "confirmed");
        draining.await.unwrap();
    }

    #[tokio::test]
    async fn native_broker_refresh_rejects_retained_and_stale_and_correlates_reads() {
        let broker = start_native_broker().await;
        let device = mqtt_device(ProxyId::default(), "refresh-fixture");
        let topics = Topics(device.proxy_id);
        let (observer, mut received) = observed_client("refresh-observer", broker.port);
        observer
            .subscribe(topics.process_availability(), QoS::AtLeastOnce)
            .await
            .unwrap();
        let result_topic = topics.device(&device.id, "refresh/result");
        observer
            .subscribe(&result_topic, QoS::AtLeastOnce)
            .await
            .unwrap();
        let mut bridge = start(config(broker.port, true), snapshot(device.clone()));
        receive_topic(&mut received, &topics.process_availability()).await;
        for (request_id, issued_at_unix_ms, retain, expected) in [
            (
                "retained-read",
                unix_millis(SystemTime::now()).unwrap(),
                true,
                "retained_request",
            ),
            ("stale-read", 1, false, "stale_request"),
        ] {
            observer
                .publish(
                    topics.device(&device.id, "refresh/set"),
                    QoS::AtLeastOnce,
                    retain,
                    serde_json::to_vec(
                        &json!({"request_id":request_id,"issued_at_unix_ms":issued_at_unix_ms}),
                    )
                    .unwrap(),
                )
                .await
                .unwrap();
            let result = receive_topic(&mut received, &result_topic).await;
            let result: Value = serde_json::from_slice(&result.payload).unwrap();
            assert_eq!(result["request_id"], request_id);
            assert_eq!(result["status"], expected);
            assert!(bridge.device_requests.try_recv().is_err());
        }
        observer.publish(topics.device(&device.id, "refresh/set"), QoS::AtLeastOnce, false,
            serde_json::to_vec(&json!({"request_id":"read-confirmed","issued_at_unix_ms":unix_millis(SystemTime::now()).unwrap()})).unwrap()).await.unwrap();
        let work = timeout(Duration::from_secs(5), bridge.device_requests.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(work.request.kind(), RequestKind::Refresh);
        work.reply
            .send(MqttReply::Refresh {
                request_id: work.request.request_id().as_str().to_owned(),
                status: gafctl_api::DeviceRefreshStatus::Fresh,
            })
            .ok()
            .unwrap();
        let result = receive_topic(&mut received, &result_topic).await;
        assert!(!result.retain);
        let result: Value = serde_json::from_slice(&result.payload).unwrap();
        assert_eq!(result["request_id"], "read-confirmed");
        assert_eq!(result["status"], "fresh");
    }
}
