mod discovery;
mod topics;

use std::{collections::HashSet, sync::Arc, time::Duration};

use crate::{
    api::{DeviceControlV2Request, DeviceControlV2Response},
    device::{DeviceBackend, DeviceDescriptor, DeviceId, ProxyId},
};
use futures_util::{Stream, StreamExt, future, stream};
use gafctl_api::CommandId;
use rumqttc::v5::{
    AsyncClient, ConnectionError, Event, EventLoop, MqttOptions,
    mqttbytes::{
        QoS,
        v5::{Filter, LastWill, Packet, Publish},
    },
};
use serde::{Deserialize, Serialize};
use tokio::{
    sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot, watch},
    time::{sleep, timeout},
};
use topics::Topics;

pub(crate) const CONTROL_QUEUE_CAPACITY: usize = 8;
const MAX_PENDING_CONTROL_RESULTS: usize = 32;
const CONTROL_RESPONSE_TIMEOUT: Duration = Duration::from_secs(300);
const MAX_CONTROL_REQUEST_BYTES: usize = 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RequestKind {
    Control,
    Refresh,
}

impl RequestKind {
    const fn result_suffix(self) -> &'static str {
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
    fn kind(&self) -> RequestKind {
        match self {
            Self::Control(_) => RequestKind::Control,
            Self::Refresh(_) => RequestKind::Refresh,
        }
    }
    fn request_id(&self) -> &CommandId {
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
    fn kind(&self) -> RequestKind {
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

pub(crate) struct MqttBridge {
    pub(crate) state_updates: watch::Sender<Arc<MqttStateSnapshot>>,
    pub(crate) device_requests: mpsc::Receiver<MqttDeviceWork>,
}

pub(crate) struct MqttStateSnapshot {
    pub(crate) proxy_id: ProxyId,
    pub(crate) discovery_identities: Vec<(DeviceId, DeviceBackend)>,
    pub(crate) devices: Vec<DeviceDescriptor>,
    pub(crate) publications: Vec<MqttStatePublication>,
}

pub(crate) struct MqttStatePublication {
    pub(crate) id: DeviceId,
    pub(crate) payload: String,
    pub(crate) available: bool,
}

pub(crate) struct MqttConfig {
    pub(crate) host: String,
    pub(crate) port: u16,
    pub(crate) username: String,
    pub(crate) password: String,
    pub(crate) discovery_enabled: bool,
}

#[derive(Clone)]
struct MqttConnection {
    client: AsyncClient,
    topics: Topics,
}

impl MqttConnection {
    async fn publish_retained(&self, topic: String, payload: impl Into<axum::body::Bytes>) {
        if let Err(error) = self
            .client
            .publish(topic, QoS::AtLeastOnce, true, payload)
            .await
        {
            tracing::warn!(%error, "could not queue retained MQTT message");
        }
    }

    async fn publish_result(&self, device: &DeviceId, response: &MqttReply) {
        match serde_json::to_vec(response) {
            Ok(payload) => {
                if let Err(error) = self
                    .client
                    .publish(
                        self.topics.device(device, response.kind().result_suffix()),
                        QoS::AtLeastOnce,
                        false,
                        payload,
                    )
                    .await
                {
                    tracing::warn!(%error, "could not publish MQTT control result");
                }
            }
            Err(error) => tracing::error!(%error, "could not serialize MQTT control result"),
        }
    }

    fn reject(&self, device: &DeviceId, request: &MqttRequest, status: &'static str) {
        let response = MqttReply::Rejected {
            request_id: request.request_id().as_str().to_owned(),
            status,
            kind: request.kind(),
        };
        match serde_json::to_vec(&response) {
            Ok(payload) => {
                if let Err(error) = self.client.try_publish(
                    self.topics.device(device, response.kind().result_suffix()),
                    QoS::AtLeastOnce,
                    false,
                    payload,
                ) {
                    tracing::warn!(%error, "could not enqueue MQTT control rejection");
                }
            }
            Err(error) => tracing::error!(%error, "could not serialize MQTT control rejection"),
        }
    }
}

pub(crate) fn start(config: MqttConfig, initial_state: MqttStateSnapshot) -> MqttBridge {
    let discovery_enabled = config.discovery_enabled;
    let topics = Topics(initial_state.proxy_id);
    let (client, eventloop) = AsyncClient::new(mqtt_options(config, topics), 32);
    let connection = MqttConnection { client, topics };
    let (state_tx, state_rx) = watch::channel(Arc::new(initial_state));
    let (connected_tx, connected_rx) = watch::channel(false);
    let (control_tx, control_rx) = mpsc::channel(CONTROL_QUEUE_CAPACITY);
    tokio::spawn(setup_connection(connection.clone(), connected_rx.clone()));
    tokio::spawn(run_event_loop(
        eventloop,
        connection.clone(),
        connected_tx,
        control_tx,
    ));
    tokio::spawn(publish_state_updates(
        connection,
        state_rx,
        connected_rx,
        discovery_enabled,
    ));
    MqttBridge {
        state_updates: state_tx,
        device_requests: control_rx,
    }
}

fn mqtt_options(config: MqttConfig, topics: Topics) -> MqttOptions {
    let mut options = MqttOptions::new(topics.client_id(), config.host, config.port);
    options.set_keep_alive(Duration::from_secs(30));
    options.set_credentials(config.username, config.password);
    options.set_last_will(LastWill::new(
        topics.process_availability(),
        "offline",
        QoS::AtLeastOnce,
        true,
        None,
    ));
    options
}

fn control_subscription(topics: Topics) -> Filter {
    Filter {
        preserve_retain: true,
        ..Filter::new(topics.controls(), QoS::AtLeastOnce)
    }
}

async fn setup_connection(connection: MqttConnection, connected: watch::Receiver<bool>) {
    connection_changes(connected)
        .filter(|active| future::ready(*active))
        .for_each(|_| initialize_connection(&connection))
        .await;
}

fn connection_changes(connected: watch::Receiver<bool>) -> impl Stream<Item = bool> {
    stream::unfold(connected, |mut connected| async {
        connected.changed().await.ok()?;
        let active = *connected.borrow_and_update();
        Some((active, connected))
    })
}

async fn initialize_connection(connection: &MqttConnection) {
    if let Err(error) = connection
        .client
        .subscribe_many([
            control_subscription(connection.topics),
            Filter {
                preserve_retain: true,
                ..Filter::new(connection.topics.refreshes(), QoS::AtLeastOnce)
            },
        ])
        .await
    {
        tracing::warn!(%error, "could not subscribe to MQTT controls");
    }
    connection
        .publish_retained(connection.topics.process_availability(), "online")
        .await;
}

fn mqtt_events(eventloop: EventLoop) -> impl Stream<Item = Result<Event, ConnectionError>> {
    stream::unfold(eventloop, |mut eventloop| async {
        let event = eventloop.poll().await;
        Some((event, eventloop))
    })
}

async fn run_event_loop(
    eventloop: EventLoop,
    connection: MqttConnection,
    connected: watch::Sender<bool>,
    controls: mpsc::Sender<MqttDeviceWork>,
) {
    let pending_results = Arc::new(Semaphore::new(MAX_PENDING_CONTROL_RESULTS));
    mqtt_events(eventloop)
        .for_each(|event| {
            handle_mqtt_event(event, &connection, &connected, &controls, &pending_results)
        })
        .await;
}

async fn handle_mqtt_event(
    event: Result<Event, ConnectionError>,
    connection: &MqttConnection,
    connected: &watch::Sender<bool>,
    controls: &mpsc::Sender<MqttDeviceWork>,
    pending_results: &Arc<Semaphore>,
) {
    match event {
        Ok(Event::Incoming(Packet::ConnAck(_))) => {
            connected.send_replace(true);
        }
        Ok(Event::Incoming(Packet::Publish(message))) => {
            if let Ok(topic) = std::str::from_utf8(&message.topic)
                && let Some((device, kind)) = connection.topics.request_device(topic)
            {
                dispatch_request(connection, controls, pending_results, device, kind, message);
            }
        }
        Ok(_) => {}
        Err(error) => {
            connected.send_replace(false);
            tracing::warn!(%error, "MQTT connection lost; reconnecting");
            sleep(Duration::from_secs(1)).await;
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

fn dispatch_request(
    connection: &MqttConnection,
    controls: &mpsc::Sender<MqttDeviceWork>,
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
    if !crate::api::v2_request_is_fresh(request.issued_at_unix_ms()) {
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
    controls: &mpsc::Sender<MqttDeviceWork>,
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
    match controls.try_send(MqttDeviceWork {
        device_id: device.clone(),
        request,
        reply,
    }) {
        Ok(()) => {
            tokio::spawn(publish_control_reply(
                connection.clone(),
                device,
                response,
                uncertain,
                permit,
            ));
        }
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

struct StateSubscriptions {
    state: watch::Receiver<Arc<MqttStateSnapshot>>,
    connected: watch::Receiver<bool>,
}

fn state_payloads(
    state: watch::Receiver<Arc<MqttStateSnapshot>>,
    connected: watch::Receiver<bool>,
) -> impl Stream<Item = Arc<MqttStateSnapshot>> {
    stream::unfold(
        StateSubscriptions { state, connected },
        receive_state_update,
    )
    .filter_map(future::ready)
}

async fn receive_state_update(
    mut subscriptions: StateSubscriptions,
) -> Option<(Option<Arc<MqttStateSnapshot>>, StateSubscriptions)> {
    tokio::select! {
        changed = subscriptions.state.changed() => changed,
        changed = subscriptions.connected.changed() => changed,
    }
    .ok()?;
    let active = *subscriptions.connected.borrow_and_update();
    let payload = active.then(|| Arc::clone(&*subscriptions.state.borrow_and_update()));
    Some((payload, subscriptions))
}

async fn publish_state_updates(
    connection: MqttConnection,
    state: watch::Receiver<Arc<MqttStateSnapshot>>,
    connected: watch::Receiver<bool>,
    discovery_enabled: bool,
) {
    state_payloads(state, connected)
        .fold(HashSet::new(), |previous_topics, snapshot| {
            let connection = &connection;
            async move {
                let active =
                    publish_discovery(connection, &snapshot, discovery_enabled, previous_topics)
                        .await;
                stream::iter(snapshot.publications.iter())
                    .for_each(|publication| publish_device_state(connection, publication))
                    .await;
                active
            }
        })
        .await;
}

fn state_messages(
    topics: Topics,
    publication: &MqttStatePublication,
) -> impl Iterator<Item = (String, &str)> {
    [
        (
            topics.device(&publication.id, "state"),
            publication.payload.as_str(),
        ),
        (
            topics.device(&publication.id, "availability"),
            if publication.available {
                "online"
            } else {
                "offline"
            },
        ),
    ]
    .into_iter()
}

async fn publish_device_state(connection: &MqttConnection, publication: &MqttStatePublication) {
    stream::iter(state_messages(connection.topics, publication))
        .for_each(|(topic, payload)| {
            connection.publish_retained(topic, payload.as_bytes().to_owned())
        })
        .await;
}

fn inactive_discovery_topics(
    topics: Topics,
    identities: &[(DeviceId, DeviceBackend)],
    previous: HashSet<String>,
    active: &HashSet<String>,
) -> impl Iterator<Item = String> {
    previous
        .into_iter()
        .chain(discovery::candidates(topics, identities))
        .filter(|topic| !active.contains(topic))
        .collect::<HashSet<_>>()
        .into_iter()
}

async fn publish_discovery(
    connection: &MqttConnection,
    snapshot: &MqttStateSnapshot,
    enabled: bool,
    previous: HashSet<String>,
) -> HashSet<String> {
    let configs = discovery::configs(&snapshot.devices)
        .filter(|_| enabled)
        .collect::<Vec<_>>();
    let active = configs
        .iter()
        .map(|(topic, _)| topic.clone())
        .collect::<HashSet<_>>();
    stream::iter(inactive_discovery_topics(
        connection.topics,
        &snapshot.discovery_identities,
        previous,
        &active,
    ))
    .for_each(|topic| connection.publish_retained(topic, Vec::new()))
    .await;
    stream::iter(configs)
        .for_each(|(topic, config)| publish_discovery_config(connection, topic, config))
        .await;
    active
}

async fn publish_discovery_config(
    connection: &MqttConnection,
    topic: String,
    config: serde_json::Value,
) {
    match serde_json::to_vec(&config) {
        Ok(payload) => connection.publish_retained(topic, payload).await,
        Err(error) => tracing::error!(%error, "could not serialize MQTT discovery config"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        control::unix_millis,
        device::{DeviceBackend, DeviceCapabilities, EntitySource},
    };
    use serde_json::{Value, json};
    use std::{
        process::{Child, Command, Stdio},
        time::SystemTime,
    };
    use tokio::net::{TcpListener, TcpStream};
    struct NativeBroker {
        process: Child,
        port: u16,
    }

    impl NativeBroker {
        async fn restart(&mut self) {
            let _ = self.process.kill();
            let _ = self.process.wait();
            sleep(Duration::from_millis(1_200)).await;
            self.process = launch_native_broker(self.port);
            wait_for_native_broker(self.port).await;
        }
    }

    impl Drop for NativeBroker {
        fn drop(&mut self) {
            let _ = self.process.kill();
            let _ = self.process.wait();
        }
    }

    async fn start_native_broker() -> NativeBroker {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let broker = NativeBroker {
            process: launch_native_broker(port),
            port,
        };
        wait_for_native_broker(port).await;
        broker
    }

    fn launch_native_broker(port: u16) -> Child {
        Command::new("mosquitto")
            .args(["-p", &port.to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("devenv supplies the native Mosquitto broker")
    }

    async fn wait_for_native_broker(port: u16) {
        let attempts = stream::iter(0..50)
            .then(|_| async {
                sleep(Duration::from_millis(20)).await;
                TcpStream::connect(("127.0.0.1", port)).await.ok()
            })
            .filter_map(future::ready);
        tokio::pin!(attempts);
        let ready = attempts.next().await;
        assert!(ready.is_some(), "native Mosquitto did not start");
        drop(ready);
    }

    fn test_mqtt_options(client_id: &str, port: u16) -> MqttOptions {
        let mut options = MqttOptions::new(client_id, "127.0.0.1", port);
        options.set_keep_alive(Duration::from_secs(5));
        options.set_credentials("gafctl-test", "gafctl-test");
        options
    }

    fn observed_client(
        client_id: &str,
        port: u16,
    ) -> (AsyncClient, mpsc::UnboundedReceiver<Publish>) {
        let (client, eventloop) = AsyncClient::new(test_mqtt_options(client_id, port), 16);
        let (messages, received) = mpsc::unbounded_channel();
        tokio::spawn(
            mqtt_events(eventloop)
                .filter_map(|event| async move { event.ok() })
                .filter_map(|event| async move {
                    match event {
                        Event::Incoming(Packet::Publish(message)) => Some(message),
                        _ => None,
                    }
                })
                .for_each(move |message| {
                    let _ = messages.send(message);
                    future::ready(())
                }),
        );
        (client, received)
    }

    async fn receive_topic(
        received: &mut mpsc::UnboundedReceiver<Publish>,
        topic: &str,
    ) -> Publish {
        let messages = stream::unfold(received, |received| async {
            received.recv().await.map(|message| (message, received))
        })
        .filter(|message| future::ready(message.topic == topic));
        tokio::pin!(messages);
        timeout(Duration::from_secs(15), messages.next())
            .await
            .expect("timed out waiting for MQTT publication")
            .expect("observer stream ended")
    }

    fn mqtt_device(proxy_id: ProxyId, id: &str) -> DeviceDescriptor {
        DeviceDescriptor {
            proxy_id,
            id: DeviceId::parse(id.to_owned()).unwrap(),
            name: format!("Device {id}"),
            backend: DeviceBackend::QuickConnect,
            capabilities: DeviceCapabilities::quickconnect_with_controls(),
            state_source: EntitySource::Mqtt,
            command_source: EntitySource::Mqtt,
        }
    }

    fn config(port: u16, discovery_enabled: bool) -> MqttConfig {
        MqttConfig {
            host: "127.0.0.1".to_owned(),
            port,
            username: "test".to_owned(),
            password: "test".to_owned(),
            discovery_enabled,
        }
    }

    fn snapshot(device: DeviceDescriptor) -> MqttStateSnapshot {
        MqttStateSnapshot {
            proxy_id: device.proxy_id,
            discovery_identities: vec![(device.id.clone(), device.backend)],
            publications: vec![MqttStatePublication {
                id: device.id.clone(),
                payload: r#"{"available":true,"state":{"settings":{"mode":"automatic"}}}"#
                    .to_owned(),
                available: true,
            }],
            devices: vec![device],
        }
    }

    fn request(id: &str) -> Vec<u8> {
        serde_json::to_vec(&json!({"request_id": id, "issued_at_unix_ms": unix_millis(SystemTime::now()).unwrap(), "command": {"kind": "quick_connect_mode", "mode": "automatic"}})).unwrap()
    }

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
    fn mqtt_discovery_includes_applicable_ha_entities_and_cleanup_candidates() {
        let mut ble = DeviceDescriptor::configured_ble();
        ble.state_source = EntitySource::Mqtt;
        ble.command_source = EntitySource::Mqtt;
        let cloud = mqtt_device(ble.proxy_id, "cloud");
        for (device, expected) in [
            (
                &ble,
                vec![
                    ("binary_sensor", "controller_fan_flag"),
                    ("number", "automatic_temperature"),
                    ("number", "automatic_humidity"),
                    ("number", "timer_duration"),
                    ("button", "refresh"),
                    ("select", "automatic_thresholds"),
                    ("select", "timer"),
                ],
            ),
            (
                &cloud,
                vec![
                    ("binary_sensor", "running_estimate"),
                    ("binary_sensor", "ota_in_progress"),
                    ("sensor", "signal_strength_raw"),
                    ("sensor", "verified_raw"),
                    ("binary_sensor", "automatic_mode"),
                    ("binary_sensor", "humidity_monitor"),
                    ("number", "automatic_temperature"),
                    ("number", "automatic_humidity"),
                    ("number", "timer_duration"),
                    ("switch", "automatic_mode"),
                    ("switch", "timer_mode"),
                    ("switch", "manual_mode"),
                    ("button", "all_off"),
                    ("button", "refresh"),
                ],
            ),
        ] {
            let topics = Topics(device.proxy_id);
            let configs = discovery::configs(std::slice::from_ref(device)).collect::<Vec<_>>();
            let candidates = discovery::candidates(topics, &[(device.id.clone(), device.backend)])
                .collect::<HashSet<_>>();
            for (domain, key) in expected {
                let topic = topics.discovery(&device.id, domain, key);
                assert!(
                    configs.iter().any(|(candidate, _)| candidate == &topic),
                    "{domain}/{key}"
                );
                assert!(candidates.contains(&topic), "cleanup {domain}/{key}");
            }
        }
    }

    #[test]
    fn discovery_topics_include_the_persisted_proxy_identity() {
        let device = mqtt_device(ProxyId::default(), "configured");
        let topics = Topics(device.proxy_id);
        let configs = discovery::configs(std::slice::from_ref(&device)).collect::<Vec<_>>();
        assert!(
            configs
                .iter()
                .any(|(_, config)| config["state_topic"] == topics.device(&device.id, "state"))
        );
        assert!(configs.iter().all(
            |(topic, config)| topic.contains(&device.proxy_id.to_string())
                && config["device"]["identifiers"][0] == topics.identifier(&device.id)
        ));
    }

    #[test]
    fn namespaced_templates_preserve_unknown_readings_and_freshness() {
        let mut device = DeviceDescriptor::configured_ble();
        device.state_source = EntitySource::Mqtt;
        device.command_source = EntitySource::Mqtt;
        let cloud = mqtt_device(device.proxy_id, "cloud-fixture");
        let devices = [device, cloud];
        let configs = discovery::configs(&devices).collect::<Vec<_>>();
        assert!(configs.iter().any(|(topic, config)| {
            topic.ends_with("/freshness/config")
                && config["value_template"]
                    .as_str()
                    .unwrap()
                    .contains("unknown")
        }));
        let threshold = configs
            .iter()
            .find(|(topic, _)| topic.ends_with("/automatic_temperature_threshold/config"))
            .unwrap();
        assert!(
            threshold.1["value_template"]
                .as_str()
                .unwrap()
                .contains("if reading is number else none")
        );
        if let Some(path) = std::env::var_os("GAFCTL_DISCOVERY_FIXTURE") {
            std::fs::write(path, serde_json::to_vec_pretty(&configs).unwrap()).unwrap();
        }
    }

    #[test]
    fn ble_discovery_does_not_override_http_ownership() {
        let mut device = DeviceDescriptor::configured_ble();
        assert!(
            discovery::configs(std::slice::from_ref(&device))
                .next()
                .is_none()
        );
        device.state_source = EntitySource::Mqtt;
        assert!(
            discovery::configs(std::slice::from_ref(&device))
                .next()
                .is_none()
        );
        device.command_source = EntitySource::Mqtt;
        assert!(discovery::configs(std::slice::from_ref(&device)).count() > 10);
    }

    #[test]
    fn controls_only_route_within_this_proxy() {
        let topics = Topics(ProxyId::default());
        let id = DeviceId::configured_ble();
        assert_eq!(
            topics.request_device(&topics.device(&id, "control/set")),
            Some((id, RequestKind::Control))
        );
        assert_eq!(
            topics.request_device(
                &Topics(ProxyId::default()).device(&DeviceId::configured_ble(), "control/set")
            ),
            None
        );
        assert_eq!(topics.request_device("gafctl/gaf_vent/control/set"), None);
        assert_eq!(
            topics.request_device(&format!("gafctl/{}/a/b/control/set", topics.0)),
            None
        );
        assert_eq!(
            topics.request_device(&topics.device(&DeviceId::configured_ble(), "state")),
            None
        );
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

    #[test]
    fn subscriptions_preserve_retained_request_metadata() {
        let filter = control_subscription(Topics(ProxyId::default()));
        let subscription = rumqttc::v5::mqttbytes::v5::Subscribe::new(filter, None);
        let mut encoded = bytes::BytesMut::new();
        subscription.write(&mut encoded).unwrap();
        assert_eq!(encoded.last(), Some(&0x09));
    }

    #[test]
    fn restarted_http_owner_tombstones_all_prior_discovery_without_history() {
        let mut device = mqtt_device(ProxyId::default(), "qc-one");
        let previous = discovery::configs(std::slice::from_ref(&device))
            .map(|(topic, _)| topic)
            .collect::<HashSet<_>>();
        device.state_source = EntitySource::Http;
        device.command_source = EntitySource::Http;
        let inactive = inactive_discovery_topics(
            Topics(device.proxy_id),
            &[(device.id.clone(), device.backend)],
            HashSet::new(),
            &HashSet::new(),
        )
        .collect::<HashSet<_>>();
        assert!(previous.is_subset(&inactive));
        assert!(
            inactive
                .iter()
                .all(|topic| topic.contains(&device.proxy_id.to_string()))
        );
    }

    #[test]
    fn discovery_has_sensor_metadata_and_distinct_availability_per_proxy() {
        let device = mqtt_device(ProxyId::default(), "same-id");
        let topics = Topics(device.proxy_id);
        let configs = discovery::configs(std::slice::from_ref(&device)).collect::<Vec<_>>();
        let temperature = configs
            .iter()
            .find(|(topic, _)| topic.ends_with("/temperature/config"))
            .unwrap();
        assert_eq!(temperature.1["device_class"], "temperature");
        assert_eq!(temperature.1["state_class"], "measurement");
        assert_eq!(temperature.1["unit_of_measurement"], "°F");
        assert!(
            configs
                .iter()
                .all(|(topic, config)| config["availability_mode"] == "all"
                    && config["availability"][0]["topic"] == topics.process_availability()
                    && if topic.ends_with("/refresh/config") {
                        config["availability"].as_array().unwrap().len() == 1
                    } else {
                        config["availability"][1]["topic"]
                            == topics.device(&device.id, "availability")
                    })
        );
        let other = mqtt_device(ProxyId::default(), "same-id");
        let other_topics = discovery::configs(&[other])
            .map(|(topic, _)| topic)
            .collect::<HashSet<_>>();
        assert!(
            configs
                .iter()
                .all(|(topic, _)| !other_topics.contains(topic))
        );
    }

    #[tokio::test]
    async fn stalled_results_bound_accepted_controls() {
        let (client, eventloop) = AsyncClient::new(MqttOptions::new("test", "localhost", 1883), 1);
        let connection = MqttConnection {
            client,
            topics: Topics(ProxyId::default()),
        };
        let (controls, mut queued) = mpsc::channel(CONTROL_QUEUE_CAPACITY);
        let pending = Arc::new(Semaphore::new(2));
        for index in 0..3 {
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
    async fn native_broker_retains_state_and_tombstones_http_owner_after_restart() {
        let broker = start_native_broker().await;
        let device = mqtt_device(ProxyId::default(), "configured");
        let topics = Topics(device.proxy_id);
        let (observer, mut received) = observed_client("observer", broker.port);
        observer
            .subscribe(topics.process_availability(), QoS::AtLeastOnce)
            .await
            .unwrap();
        let bridge = start(config(broker.port, true), snapshot(device.clone()));
        let online = receive_topic(&mut received, &topics.process_availability()).await;
        assert_eq!(online.payload.as_ref(), b"online");
        let discovery_topic = topics.discovery(&device.id, "sensor", "temperature");
        observer
            .subscribe_many([Filter {
                preserve_retain: true,
                ..Filter::new(&discovery_topic, QoS::AtLeastOnce)
            }])
            .await
            .unwrap();
        let discovered = receive_topic(&mut received, &discovery_topic).await;
        assert!(discovered.retain);
        let config: Value = serde_json::from_slice(&discovered.payload).unwrap();
        assert_eq!(
            config["device"]["identifiers"][0],
            topics.identifier(&device.id)
        );
        observer
            .subscribe(topics.device(&device.id, "state"), QoS::AtLeastOnce)
            .await
            .unwrap();
        let state = receive_topic(&mut received, &topics.device(&device.id, "state")).await;
        assert!(state.retain);
        let mut changed = device;
        changed.state_source = EntitySource::Http;
        changed.command_source = EntitySource::Http;
        // This fresh publisher has no in-memory discovery history.
        let (client, _) = observed_client("restarted-publisher", broker.port);
        let connection = MqttConnection { client, topics };
        let mut inactive = snapshot(changed);
        inactive.devices.clear();
        publish_discovery(&connection, &inactive, true, HashSet::new()).await;
        let tombstone = receive_topic(&mut received, &discovery_topic).await;
        assert!(tombstone.payload.is_empty());
        drop(bridge);
    }

    #[tokio::test]
    async fn native_broker_isolates_two_proxies_with_the_same_local_device_id() {
        let broker = start_native_broker().await;
        let first = mqtt_device(ProxyId::default(), "configured");
        let second = mqtt_device(ProxyId::default(), "configured");
        let topics = Topics(first.proxy_id);
        let other = Topics(second.proxy_id);
        let (observer, mut received) = observed_client("two-proxy-observer", broker.port);
        observer
            .subscribe(topics.process_availability(), QoS::AtLeastOnce)
            .await
            .unwrap();
        let mut first_bridge = start(config(broker.port, true), snapshot(first.clone()));
        receive_topic(&mut received, &topics.process_availability()).await;
        observer
            .subscribe(other.process_availability(), QoS::AtLeastOnce)
            .await
            .unwrap();
        let mut second_bridge = start(config(broker.port, true), snapshot(second.clone()));
        receive_topic(&mut received, &other.process_availability()).await;
        observer
            .publish(
                topics.device(&first.id, "control/set"),
                QoS::AtLeastOnce,
                false,
                request("first-only"),
            )
            .await
            .unwrap();
        let work = timeout(Duration::from_secs(5), first_bridge.device_requests.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(work.request.request_id().as_str(), "first-only");
        assert!(
            timeout(
                Duration::from_millis(200),
                second_bridge.device_requests.recv()
            )
            .await
            .is_err()
        );
        drop(work);
        observer
            .subscribe(
                other.discovery(&second.id, "sensor", "temperature"),
                QoS::AtLeastOnce,
            )
            .await
            .unwrap();
        let discovered = receive_topic(
            &mut received,
            &other.discovery(&second.id, "sensor", "temperature"),
        )
        .await;
        assert!(!discovered.payload.is_empty());
        first_bridge
            .state_updates
            .send_replace(Arc::new(MqttStateSnapshot {
                proxy_id: first.proxy_id,
                discovery_identities: vec![(first.id.clone(), first.backend)],
                devices: vec![DeviceDescriptor {
                    state_source: EntitySource::Http,
                    command_source: EntitySource::Http,
                    ..first
                }],
                publications: Vec::new(),
            }));
        sleep(Duration::from_millis(100)).await;
        let (late, mut retained) = observed_client("late-observer", broker.port);
        late.subscribe(
            other.discovery(&second.id, "sensor", "temperature"),
            QoS::AtLeastOnce,
        )
        .await
        .unwrap();
        assert!(
            !receive_topic(
                &mut retained,
                &other.discovery(&second.id, "sensor", "temperature")
            )
            .await
            .payload
            .is_empty()
        );
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

    #[tokio::test]
    async fn native_broker_publishes_process_will_on_unexpected_disconnect() {
        let broker = start_native_broker().await;
        let topics = Topics(ProxyId::default());
        let (observer, mut received) = observed_client("will-observer", broker.port);
        observer
            .subscribe_many([Filter {
                preserve_retain: true,
                ..Filter::new(topics.process_availability(), QoS::AtLeastOnce)
            }])
            .await
            .unwrap();
        let (client, eventloop) =
            AsyncClient::new(mqtt_options(config(broker.port, false), topics), 16);
        let (connected, mut connection_events) = mpsc::unbounded_channel();
        let task = tokio::spawn(mqtt_events(eventloop).for_each(move |event| {
            if let Ok(Event::Incoming(Packet::ConnAck(_))) = event {
                let _ = connected.send(());
            }
            future::ready(())
        }));
        timeout(Duration::from_secs(5), connection_events.recv())
            .await
            .unwrap()
            .unwrap();
        client
            .publish(
                topics.process_availability(),
                QoS::AtLeastOnce,
                true,
                "online",
            )
            .await
            .unwrap();
        receive_topic(&mut received, &topics.process_availability()).await;
        task.abort();
        let _ = task.await;
        let offline = receive_topic(&mut received, &topics.process_availability()).await;
        assert!(offline.retain);
        assert_eq!(offline.payload.as_ref(), b"offline");
    }

    #[tokio::test]
    async fn native_broker_restart_republishes_current_state() {
        let mut broker = start_native_broker().await;
        let device = mqtt_device(ProxyId::default(), "qc-reconnect");
        let topics = Topics(device.proxy_id);
        let (observer, mut received) = observed_client("reconnect-observer", broker.port);
        observer
            .subscribe(topics.process_availability(), QoS::AtLeastOnce)
            .await
            .unwrap();
        let bridge = start(config(broker.port, true), snapshot(device.clone()));
        receive_topic(&mut received, &topics.process_availability()).await;
        broker.restart().await;
        let (observer, mut received) = observed_client("reconnected-observer", broker.port);
        observer
            .subscribe(topics.process_availability(), QoS::AtLeastOnce)
            .await
            .unwrap();
        receive_topic(&mut received, &topics.process_availability()).await;
        observer
            .subscribe(topics.device(&device.id, "state"), QoS::AtLeastOnce)
            .await
            .unwrap();
        assert!(
            !receive_topic(&mut received, &topics.device(&device.id, "state"))
                .await
                .payload
                .is_empty()
        );
        observer
            .subscribe(
                topics.discovery(&device.id, "sensor", "temperature"),
                QoS::AtLeastOnce,
            )
            .await
            .unwrap();
        assert!(
            !receive_topic(
                &mut received,
                &topics.discovery(&device.id, "sensor", "temperature")
            )
            .await
            .payload
            .is_empty()
        );
        drop(bridge);
    }
}
