use std::{
    future::Future,
    sync::{Arc, Mutex},
    time::Duration,
};

use futures_util::{StreamExt, stream};
use gafctl_api::{CommandId, DeviceControlV2Request, DeviceControlV2Response};
use serde::{Deserialize, Serialize};
use tokio::{
    sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot},
    task::JoinSet,
    time::timeout,
};

use super::topics::Topics;
use gafctl_api::DeviceId;
use rumqttc_next::{AsyncClient, Publish, PublishOptions};

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
        tracing::warn!(?kind, "rejected oversized MQTT request");
        return None;
    }
    let result = match kind {
        RequestKind::Control => serde_json::from_slice(payload).map(MqttRequest::Control),
        RequestKind::Refresh => serde_json::from_slice(payload).map(MqttRequest::Refresh),
    };
    match result {
        Ok(request) => Some(request),
        Err(error) => {
            tracing::warn!(?kind, %error, "rejected malformed MQTT request");
            None
        }
    }
}

#[derive(Serialize)]
#[serde(untagged)]
pub(crate) enum MqttReply {
    Control(DeviceControlV2Response),
    Refresh {
        request_id: CommandId,
        status: gafctl_api::DeviceRefreshStatus,
    },
    Rejected {
        request_id: CommandId,
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
    state: Arc<Mutex<IntakeState>>,
}

struct IntakeState {
    sender: Option<mpsc::Sender<MqttDeviceWork>>,
    replies: JoinSet<()>,
}

impl MqttRequestIntake {
    pub(crate) fn new(sender: mpsc::Sender<MqttDeviceWork>) -> Self {
        Self {
            state: Arc::new(Mutex::new(IntakeState {
                sender: Some(sender),
                replies: JoinSet::new(),
            })),
        }
    }

    pub(crate) fn close(&self) {
        self.state
            .lock()
            .expect("MQTT intake lock poisoned")
            .sender
            .take();
    }

    pub(crate) async fn drain_replies(&self, deadline: tokio::time::Instant) {
        let mut replies = {
            let mut state = self.state.lock().expect("MQTT intake lock poisoned");
            std::mem::take(&mut state.replies)
        };
        if tokio::time::timeout_at(
            deadline,
            stream::poll_fn(|cx| replies.poll_join_next(cx)).for_each(|_| std::future::ready(())),
        )
        .await
        .is_err()
        {
            replies.shutdown().await;
            tracing::warn!("MQTT reply acknowledgement deadline exceeded");
        }
    }

    fn try_send_with_reply<F>(
        &self,
        work: MqttDeviceWork,
        reply: impl FnOnce() -> F,
    ) -> Result<(), mpsc::error::TrySendError<MqttDeviceWork>>
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let mut state = self.state.lock().expect("MQTT intake lock poisoned");
        let Some(sender) = state.sender.as_ref() else {
            return Err(mpsc::error::TrySendError::Closed(work));
        };
        sender.try_send(work)?;
        // Admission and reply ownership share the close() fence.
        std::iter::from_fn(|| state.replies.try_join_next()).for_each(drop);
        state.replies.spawn(reply());
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn try_send(
        &self,
        work: MqttDeviceWork,
    ) -> Result<(), mpsc::error::TrySendError<MqttDeviceWork>> {
        let state = self.state.lock().expect("MQTT intake lock poisoned");
        match state.sender.as_ref() {
            Some(sender) => sender.try_send(work),
            None => Err(mpsc::error::TrySendError::Closed(work)),
        }
    }
}

async fn publish_result(
    client: &AsyncClient,
    topics: Topics,
    device: &DeviceId,
    response: &MqttReply,
) {
    let payload = match serde_json::to_vec(response) {
        Ok(payload) => payload,
        Err(error) => {
            tracing::error!(%error, "could not serialize MQTT control result");
            return;
        }
    };
    match client
        .publish_tracked(
            topics.device(device, response.kind().result_suffix()),
            payload,
            PublishOptions::at_least_once(),
        )
        .await
    {
        Ok(notice) => {
            if let Err(error) = notice.wait_completion_async().await {
                tracing::warn!(%error, "could not acknowledge MQTT control result");
            }
        }
        Err(error) => tracing::warn!(%error, "could not enqueue MQTT control result"),
    }
}

fn reject(
    client: &AsyncClient,
    topics: Topics,
    device: &DeviceId,
    request: &MqttRequest,
    status: &'static str,
) {
    let response = MqttReply::Rejected {
        request_id: request.request_id().clone(),
        status,
        kind: request.kind(),
    };
    match serde_json::to_vec(&response) {
        Ok(payload) => {
            if let Err(error) = client.try_publish(
                topics.device(device, response.kind().result_suffix()),
                payload,
                PublishOptions::at_least_once(),
            ) {
                tracing::warn!(%error, "could not enqueue MQTT control rejection");
            }
        }
        Err(error) => tracing::error!(%error, "could not serialize MQTT control rejection"),
    }
}

pub(super) fn dispatch_request(
    client: &AsyncClient,
    topics: Topics,
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
        reject(client, topics, &device, &request, "retained_request");
        return;
    }
    if !crate::service::control::v2_request_is_fresh(request.issued_at_unix_ms()) {
        reject(client, topics, &device, &request, "stale_request");
        return;
    }
    let Ok(permit) = Arc::clone(pending_results).try_acquire_owned() else {
        reject(client, topics, &device, &request, "control_results_busy");
        return;
    };
    enqueue_control(client, topics, controls, device, request, permit);
}

fn enqueue_control(
    client: &AsyncClient,
    topics: Topics,
    controls: &MqttRequestIntake,
    device: DeviceId,
    request: MqttRequest,
    permit: OwnedSemaphorePermit,
) {
    let (reply, response) = oneshot::channel();
    let uncertain = MqttReply::Rejected {
        request_id: request.request_id().clone(),
        status: "outcome_unknown",
        kind: request.kind(),
    };
    match controls.try_send_with_reply(
        MqttDeviceWork {
            device_id: device.clone(),
            request,
            reply,
        },
        || publish_control_reply(client.clone(), topics, device, response, uncertain, permit),
    ) {
        Ok(()) => {}
        Err(mpsc::error::TrySendError::Full(work)) => {
            reject(client, topics, &work.device_id, &work.request, "queue_full")
        }
        Err(mpsc::error::TrySendError::Closed(work)) => reject(
            client,
            topics,
            &work.device_id,
            &work.request,
            "control_worker_unavailable",
        ),
    }
}

async fn publish_control_reply(
    client: AsyncClient,
    topics: Topics,
    device: DeviceId,
    response: oneshot::Receiver<MqttReply>,
    uncertain: MqttReply,
    _permit: OwnedSemaphorePermit,
) {
    let result = wait_for_device_reply(response, uncertain).await;
    if timeout(
        Duration::from_secs(30),
        publish_result(&client, topics, &device, &result),
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
            start_native_broker,
        },
        topics::Topics,
    };
    use super::*;
    use gafctl_api::ProxyId;
    use gafctl_api::unix_millis;
    use rumqttc_next::{MqttOptions, QoS};
    use serde_json::{Value, json};
    use std::time::SystemTime;

    #[test]
    fn mqtt_reply_wire_format_preserves_correlation_and_kind() {
        [
            gafctl_api::DeviceRefreshStatus::Fresh,
            gafctl_api::DeviceRefreshStatus::Failed,
            gafctl_api::DeviceRefreshStatus::Superseded,
        ]
        .into_iter()
        .for_each(|status| {
            let reply = MqttReply::Refresh {
                request_id: CommandId::parse("refresh-id").unwrap(),
                status,
            };
            assert_eq!(reply.kind(), RequestKind::Refresh);
            assert_eq!(
                serde_json::to_value(reply).unwrap(),
                json!({"request_id": "refresh-id", "status": status})
            );
        });
        [RequestKind::Control, RequestKind::Refresh]
            .into_iter()
            .for_each(|kind| {
                [
                    "retained_request",
                    "stale_request",
                    "control_results_busy",
                    "queue_full",
                    "control_worker_unavailable",
                    "outcome_unknown",
                    "unknown_device",
                    "backend_unavailable",
                    "unsupported_read",
                    "refresh_failed",
                ]
                .into_iter()
                .for_each(|status| {
                    let reply = MqttReply::Rejected {
                        request_id: CommandId::parse("rejected-id").unwrap(),
                        status,
                        kind,
                    };
                    assert_eq!(reply.kind(), kind);
                    assert_eq!(
                        serde_json::to_value(reply).unwrap(),
                        json!({"request_id": "rejected-id", "status": status})
                    );
                });
            });
    }

    #[tokio::test(start_paused = true)]
    async fn mqtt_deadline_or_closed_worker_returns_correlated_unknown_outcome() {
        for closed in [false, true] {
            let (sender, response) = oneshot::channel();
            let uncertain = MqttReply::Rejected {
                request_id: CommandId::parse("uncertain").unwrap(),
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
    fn mqtt_request_parser_enforces_payload_boundary_for_both_kinds() {
        [
            (RequestKind::Control, request("bounded-request")),
            (
                RequestKind::Refresh,
                serde_json::to_vec(&json!({
                    "request_id": "bounded-request", "issued_at_unix_ms": 1
                }))
                .unwrap(),
            ),
        ]
        .into_iter()
        .for_each(|(kind, mut payload)| {
            let issued_at = serde_json::from_slice::<Value>(&payload).unwrap()["issued_at_unix_ms"]
                .as_u64()
                .unwrap();
            payload.resize(MAX_CONTROL_REQUEST_BYTES, b' ');
            let parsed = parse_request(kind, &payload).unwrap();
            assert_eq!(parsed.kind(), kind);
            assert_eq!(parsed.request_id().as_str(), "bounded-request");
            assert_eq!(parsed.issued_at_unix_ms(), issued_at);
            payload.push(b' ');
            assert!(parse_request(kind, &payload).is_none());
        });
    }

    #[test]
    fn mqtt_request_parser_rejects_malformed_and_unknown_fields_for_both_kinds() {
        [
            (RequestKind::Control, request("strict-request")),
            (
                RequestKind::Refresh,
                serde_json::to_vec(&json!({
                    "request_id": "strict-request", "issued_at_unix_ms": 1
                }))
                .unwrap(),
            ),
        ]
        .into_iter()
        .for_each(|(kind, payload)| {
            assert!(parse_request(kind, b"not json").is_none());
            let valid: Value = serde_json::from_slice(&payload).unwrap();
            let mut unknown = valid.clone();
            unknown["unknown_field"] = json!(true);
            assert!(parse_request(kind, &serde_json::to_vec(&unknown).unwrap()).is_none());
            let mut missing = valid.clone();
            missing.as_object_mut().unwrap().remove("issued_at_unix_ms");
            assert!(parse_request(kind, &serde_json::to_vec(&missing).unwrap()).is_none());
            let mut invalid_id = valid;
            invalid_id["request_id"] = json!("invalid id");
            assert!(parse_request(kind, &serde_json::to_vec(&invalid_id).unwrap()).is_none());
        });
    }

    #[test]
    fn controls_require_bounded_typed_correlated_requests() {
        let parsed = parse_request(RequestKind::Control, &request("command-1")).unwrap();
        assert_eq!(parsed.request_id().as_str(), "command-1");
        for payload in [b"not json".as_slice(), br#"{"request_id":"invalid id","issued_at_unix_ms":1,"command":{"kind":"quick_connect_mode","mode":"automatic"}}"#.as_slice(), br#"{"request_id":"id","command":{"kind":"quick_connect_mode","mode":"automatic"}}"#.as_slice(), br#"{"request_id":"id","issued_at_unix_ms":1,"preset":"timer_clear"}"#.as_slice()] {
            assert!(parse_request(RequestKind::Control, payload).is_none());
        }
        assert!(
            parse_request(
                RequestKind::Control,
                &vec![b' '; MAX_CONTROL_REQUEST_BYTES + 1]
            )
            .is_none()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mqtt_shutdown_cannot_miss_a_reply_being_registered() {
        use futures_util::FutureExt;
        let (sender, mut work_receiver) = mpsc::channel(1);
        let intake = MqttRequestIntake::new(sender);
        let (reply, _response) = oneshot::channel();
        let work = MqttDeviceWork {
            device_id: DeviceId::configured_ble(),
            request: parse_request(RequestKind::Control, &request("atomic-admission")).unwrap(),
            reply,
        };
        let (entered, entering) = oneshot::channel();
        let (release, released) = std::sync::mpsc::channel();
        let (published, publication) = oneshot::channel();
        let admitting = intake.clone();
        let admission = tokio::task::spawn_blocking(move || {
            admitting
                .try_send_with_reply(work, || {
                    entered.send(()).unwrap();
                    released.recv().unwrap();
                    async {
                        publication.await.unwrap();
                    }
                })
                .unwrap();
        });
        entering.await.unwrap();
        assert!(
            intake.state.try_lock().is_err(),
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

    #[tokio::test(start_paused = true)]
    async fn dropping_intake_cancels_pending_device_replies() {
        let (client, _event_loop) =
            AsyncClient::builder(MqttOptions::new("test", ("localhost", 1883)))
                .capacity(1)
                .build();
        let (sender, mut queued) = mpsc::channel(1);
        let intake = MqttRequestIntake::new(sender);
        dispatch_request(
            &client,
            Topics(ProxyId::default()),
            &intake,
            &Arc::new(Semaphore::new(1)),
            DeviceId::configured_ble(),
            RequestKind::Control,
            Publish::new("unused", QoS::AtLeastOnce, request("cancelled"), None),
        );
        let mut work = queued.recv().await.expect("control was admitted");
        drop(intake);
        timeout(Duration::from_secs(1), work.reply.closed())
            .await
            .expect("dropping intake must cancel its pending reply task");
    }

    #[tokio::test]
    async fn stalled_results_bound_accepted_controls() {
        let (client, eventloop) =
            AsyncClient::builder(MqttOptions::new("test", ("localhost", 1883)))
                .capacity(1)
                .build();
        let topics = Topics(ProxyId::default());
        let (controls, mut queued) = mpsc::channel(CONTROL_QUEUE_CAPACITY);
        let controls = MqttRequestIntake::new(controls);
        let pending = Arc::new(Semaphore::new(2));
        for index in 0..2 {
            dispatch_request(
                &client,
                topics,
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
            &client,
            topics,
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
                request("retained"),
                PublishOptions::at_least_once().retained(),
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
                serde_json::to_vec(&stale).unwrap(),
                PublishOptions::at_least_once(),
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
                request("confirmed"),
                PublishOptions::at_least_once(),
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
                request("shutdown-reply"),
                PublishOptions::at_least_once(),
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
                    serde_json::to_vec(
                        &json!({"request_id":request_id,"issued_at_unix_ms":issued_at_unix_ms}),
                    )
                    .unwrap(),
                    PublishOptions::at_least_once().retain(retain),
                )
                .await
                .unwrap();
            let result = receive_topic(&mut received, &result_topic).await;
            let result: Value = serde_json::from_slice(&result.payload).unwrap();
            assert_eq!(result["request_id"], request_id);
            assert_eq!(result["status"], expected);
            assert!(bridge.device_requests.try_recv().is_err());
        }
        observer
            .publish(
                topics.device(&device.id, "refresh/set"),
                serde_json::to_vec(&json!({
                    "request_id":"read-confirmed",
                    "issued_at_unix_ms":unix_millis(SystemTime::now()).unwrap()
                }))
                .unwrap(),
                PublishOptions::at_least_once(),
            )
            .await
            .unwrap();
        let work = timeout(Duration::from_secs(5), bridge.device_requests.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(work.request.kind(), RequestKind::Refresh);
        work.reply
            .send(MqttReply::Refresh {
                request_id: work.request.request_id().clone(),
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
