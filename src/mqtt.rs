use std::{collections::HashSet, sync::Arc, time::Duration};

use crate::{
    api::{
        CachedV2ControlResult, ControlResponse, DeviceControlV2Request, DeviceControlV2Response,
    },
    control::{CommandId, ControlPreset, ControlRequest},
    device::{
        CommandCapability, DeviceBackend, DeviceCommand, DeviceDescriptor, DeviceId, EntitySource,
    },
};
use futures_util::{Stream, StreamExt, future, stream};
use rumqttc::v5::{
    AsyncClient, ConnectionError, Event, EventLoop, MqttOptions,
    mqttbytes::{
        QoS,
        v5::{Filter, LastWill, Packet, Publish},
    },
};
use serde::Serialize;
use serde_json::{Value, json};
use thiserror::Error;
use tokio::{
    sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot, watch},
    time::{sleep, timeout},
};

pub(crate) const STATE_TOPIC: &str = "updraft/gaf_vent/state";
const AVAILABILITY_TOPIC: &str = "updraft/gaf_vent/availability";
const PROCESS_AVAILABILITY_TOPIC: &str = "updraft/availability";
const CONTROL_REQUEST_TOPIC: &str = "updraft/gaf_vent/control/set";
const CONTROL_RESULT_TOPIC: &str = "updraft/gaf_vent/control/result";
const DEVICE_IDENTIFIER: &str = "updraft_gaf_vent";
pub(crate) const CONTROL_QUEUE_CAPACITY: usize = 8;
const MAX_PENDING_CONTROL_RESULTS: usize = 32;
const CONTROL_RESPONSE_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_CONTROL_REQUEST_BYTES: usize = 1024;
const DEVICE_CONTROL_SUFFIX: &str = "/control/set";
const DISCOVERY_TOPICS: [&str; 13] = [
    "homeassistant/sensor/updraft/temperature/config",
    "homeassistant/sensor/updraft/humidity/config",
    "homeassistant/sensor/updraft/mode/config",
    "homeassistant/sensor/updraft/controller_fan_flag/config",
    "homeassistant/sensor/updraft/firmware_version/config",
    "homeassistant/sensor/updraft/automatic_temperature_threshold/config",
    "homeassistant/sensor/updraft/automatic_humidity_threshold/config",
    "homeassistant/sensor/updraft/timer_remaining/config",
    "homeassistant/sensor/updraft/timer_original/config",
    "homeassistant/sensor/updraft/freshness/config",
    "homeassistant/sensor/updraft/last_error/config",
    "homeassistant/select/updraft/control/config",
    "homeassistant/sensor/updraft/control_result/config",
];

pub(crate) struct MqttControlWork {
    pub(crate) device_id: DeviceId,
    pub(crate) request: DeviceControlV2Request,
    pub(crate) reply: oneshot::Sender<CachedV2ControlResult>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ControlTopic {
    LegacyAlias,
    Device(DeviceId),
}

#[derive(Clone, Copy)]
enum Rejection {
    Stale,
    Retained,
    ResultsBusy,
    QueueFull,
    WorkerUnavailable,
}

fn rejection_status(rejection: Rejection) -> &'static str {
    match rejection {
        Rejection::Stale => "stale_request",
        Rejection::Retained => "retained_request",
        Rejection::ResultsBusy => "control_results_busy",
        Rejection::QueueFull => "queue_full",
        Rejection::WorkerUnavailable => "control_worker_unavailable",
    }
}

fn rejection_message(rejection: Rejection) -> &'static str {
    match rejection {
        Rejection::Stale => "stale or future-dated control request",
        Rejection::Retained => "retained control requests are rejected",
        Rejection::ResultsBusy => "control results are busy",
        Rejection::QueueFull => "control queue is full",
        Rejection::WorkerUnavailable => "control worker is unavailable",
    }
}

fn parse_control_topic(topic: &str) -> Option<ControlTopic> {
    if topic == CONTROL_REQUEST_TOPIC {
        return Some(ControlTopic::LegacyAlias);
    }
    topic
        .strip_prefix("updraft/")
        .and_then(|topic| topic.strip_suffix(DEVICE_CONTROL_SUFFIX))
        .and_then(|id| DeviceId::parse(id.to_owned()))
        .map(ControlTopic::Device)
}

pub(crate) struct MqttBridge {
    pub(crate) state_updates: watch::Sender<Arc<MqttStateSnapshot>>,
    pub(crate) control_requests: mpsc::Receiver<MqttControlWork>,
}

pub(crate) struct MqttStateSnapshot {
    pub(crate) devices: Vec<DeviceDescriptor>,
    pub(crate) publications: Vec<MqttStatePublication>,
    pub(crate) legacy_discovery_enabled: bool,
}

#[derive(Clone)]
pub(crate) struct MqttStatePublication {
    pub(crate) id: DeviceId,
    pub(crate) payload: String,
    pub(crate) available: bool,
    pub(crate) legacy_payload: Option<String>,
}

#[derive(Debug, Error)]
enum MqttControlParseError {
    #[error("invalid MQTT control request")]
    InvalidJson(#[from] serde_json::Error),
    #[error("MQTT control request exceeds the size limit")]
    PayloadTooLarge,
}

fn parse_control_request(payload: &[u8]) -> Result<ControlRequest, MqttControlParseError> {
    if payload.len() > MAX_CONTROL_REQUEST_BYTES {
        return Err(MqttControlParseError::PayloadTooLarge);
    }

    Ok(serde_json::from_slice(payload)?)
}

pub(crate) struct MqttConfig {
    pub(crate) host: String,
    pub(crate) port: u16,
    pub(crate) username: String,
    pub(crate) password: String,
    pub(crate) discovery_enabled: bool,
}

pub(crate) fn start(config: MqttConfig, initial_state: MqttStateSnapshot) -> MqttBridge {
    let discovery_enabled = config.discovery_enabled;
    let (client, eventloop) = AsyncClient::new(mqtt_options(config), 32);
    let (state_tx, state_rx) = watch::channel(Arc::new(initial_state));
    let (connected_tx, connected_rx) = watch::channel(false);
    let (control_tx, control_rx) = mpsc::channel(CONTROL_QUEUE_CAPACITY);
    tokio::spawn(setup_connection(client.clone(), connected_rx.clone()));
    tokio::spawn(run_event_loop(
        eventloop,
        client.clone(),
        connected_tx,
        control_tx,
    ));
    tokio::spawn(publish_state_updates(
        client,
        state_rx,
        connected_rx,
        discovery_enabled,
    ));

    MqttBridge {
        state_updates: state_tx,
        control_requests: control_rx,
    }
}

fn mqtt_options(config: MqttConfig) -> MqttOptions {
    let mut options = MqttOptions::new("updraft-gaf-vent", config.host, config.port);
    options.set_keep_alive(Duration::from_secs(30));
    options.set_credentials(config.username, config.password);
    options.set_last_will(LastWill::new(
        PROCESS_AVAILABILITY_TOPIC,
        "offline",
        QoS::AtLeastOnce,
        true,
        None,
    ));
    options
}

async fn setup_connection(client: AsyncClient, connected: watch::Receiver<bool>) {
    connection_changes(connected)
        .filter(|active| future::ready(*active))
        .for_each(|_| initialize_connection(&client))
        .await;
}

fn connection_changes(connected: watch::Receiver<bool>) -> impl Stream<Item = bool> {
    stream::unfold(connected, receive_connection_change)
}

async fn receive_connection_change(
    mut connected: watch::Receiver<bool>,
) -> Option<(bool, watch::Receiver<bool>)> {
    connected.changed().await.ok()?;
    let active = *connected.borrow_and_update();
    Some((active, connected))
}

async fn initialize_connection(client: &AsyncClient) {
    subscribe_to_controls(client).await;
    publish(client, PROCESS_AVAILABILITY_TOPIC, "online").await;
}

async fn subscribe_to_controls(client: &AsyncClient) {
    if let Err(error) = client
        .subscribe_many([control_subscription(), device_control_subscription()])
        .await
    {
        tracing::warn!(%error, "could not subscribe to MQTT controls");
    }
}

fn control_subscription() -> Filter {
    Filter {
        preserve_retain: true,
        ..Filter::new(CONTROL_REQUEST_TOPIC, QoS::AtLeastOnce)
    }
}

fn device_control_subscription() -> Filter {
    Filter {
        preserve_retain: true,
        ..Filter::new("updraft/+/control/set", QoS::AtLeastOnce)
    }
}

async fn run_event_loop(
    eventloop: EventLoop,
    client: AsyncClient,
    connected: watch::Sender<bool>,
    controls: mpsc::Sender<MqttControlWork>,
) {
    let pending_results = Arc::new(Semaphore::new(MAX_PENDING_CONTROL_RESULTS));
    mqtt_events(eventloop)
        .for_each(|event| {
            handle_mqtt_event(event, &client, &connected, &controls, &pending_results)
        })
        .await;
}

fn mqtt_events(eventloop: EventLoop) -> impl Stream<Item = Result<Event, ConnectionError>> {
    stream::unfold(eventloop, receive_mqtt_event)
}

async fn receive_mqtt_event(
    mut eventloop: EventLoop,
) -> Option<(Result<Event, ConnectionError>, EventLoop)> {
    let event = eventloop.poll().await;
    Some((event, eventloop))
}

async fn handle_mqtt_event(
    event: Result<Event, ConnectionError>,
    client: &AsyncClient,
    connected: &watch::Sender<bool>,
    controls: &mpsc::Sender<MqttControlWork>,
    pending_results: &Arc<Semaphore>,
) {
    match event {
        Ok(Event::Incoming(Packet::ConnAck(_))) => {
            connected.send_replace(true);
        }
        Ok(Event::Incoming(Packet::Publish(message))) => {
            if let Ok(topic_name) = std::str::from_utf8(message.topic.as_ref())
                && let Some(topic) = parse_control_topic(topic_name)
            {
                dispatch_control(client, controls, pending_results, topic, message);
            }
        }
        Ok(_) => {}
        Err(error) => wait_to_reconnect(connected, &error).await,
    }
}

async fn wait_to_reconnect(connected: &watch::Sender<bool>, error: &ConnectionError) {
    connected.send_replace(false);
    tracing::warn!(%error, "MQTT connection lost; reconnecting");
    sleep(Duration::from_secs(1)).await;
}

async fn publish_state_updates(
    client: AsyncClient,
    state: watch::Receiver<Arc<MqttStateSnapshot>>,
    connected: watch::Receiver<bool>,
    discovery_enabled: bool,
) {
    state_payloads(state, connected)
        .fold(HashSet::new(), |mut previous_topics, snapshot| async {
            if !discovery_enabled {
                clear_discovery(&client, &snapshot, &mut previous_topics).await;
            } else {
                publish_discovery(&client, &snapshot, &mut previous_topics).await;
            }
            publish_state_payload(&client, snapshot).await;
            previous_topics
        })
        .await;
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

async fn publish_state_payload(client: &AsyncClient, snapshot: Arc<MqttStateSnapshot>) {
    stream::iter(snapshot.publications.iter())
        .for_each(|publication| publish_device_state(client, publication))
        .await;
}

async fn publish_device_state(client: &AsyncClient, publication: &MqttStatePublication) {
    stream::iter(state_messages(publication))
        .for_each(|(topic, payload)| async move { publish(client, &topic, &payload).await })
        .await;
}

fn state_messages(publication: &MqttStatePublication) -> impl Iterator<Item = (String, String)> {
    let id = publication.id.as_str();
    let availability = if publication.available {
        "online"
    } else {
        "offline"
    };
    std::iter::once((format!("updraft/{id}/state"), publication.payload.clone()))
        .chain(std::iter::once((
            format!("updraft/{id}/availability"),
            availability.to_owned(),
        )))
        .chain(publication.legacy_payload.iter().flat_map(move |payload| {
            [
                (STATE_TOPIC.to_owned(), payload.clone()),
                (AVAILABILITY_TOPIC.to_owned(), availability.to_owned()),
            ]
        }))
}

fn dispatch_control(
    client: &AsyncClient,
    controls: &mpsc::Sender<MqttControlWork>,
    pending_results: &Arc<Semaphore>,
    topic: ControlTopic,
    message: Publish,
) {
    let Some((device_id, request, legacy_alias, legacy_preset)) =
        prepare_control_request(client, topic, &message)
    else {
        return;
    };
    let Some(permit) = reserve_control_result(
        client,
        pending_results,
        &device_id,
        &request,
        legacy_alias,
        legacy_preset,
    ) else {
        return;
    };
    enqueue_control(
        client,
        controls,
        device_id,
        request,
        legacy_alias,
        legacy_preset,
        permit,
    );
}

fn prepare_control_request(
    client: &AsyncClient,
    topic: ControlTopic,
    message: &Publish,
) -> Option<(
    DeviceId,
    DeviceControlV2Request,
    bool,
    Option<ControlPreset>,
)> {
    let (device_id, request, legacy_alias, legacy_preset) =
        parse_incoming_control(topic, &message.payload)?;
    if message.retain {
        reject_device_control(
            client,
            &device_id,
            &request,
            legacy_alias,
            legacy_preset,
            Rejection::Retained,
        );
        return None;
    }
    if !crate::api::v2_request_is_fresh(request.issued_at_unix_ms) {
        reject_device_control(
            client,
            &device_id,
            &request,
            legacy_alias,
            legacy_preset,
            Rejection::Stale,
        );
        return None;
    }
    Some((device_id, request, legacy_alias, legacy_preset))
}

fn parse_incoming_control(
    topic: ControlTopic,
    payload: &[u8],
) -> Option<(
    DeviceId,
    DeviceControlV2Request,
    bool,
    Option<ControlPreset>,
)> {
    if let ControlTopic::LegacyAlias = topic {
        let legacy = parse_control_request(payload).ok()?;
        let preset = legacy.preset();
        return Some((
            DeviceId::configured_ble(),
            DeviceControlV2Request {
                request_id: legacy.request_id().clone(),
                issued_at_unix_ms: legacy.issued_at_unix_ms(),
                command: DeviceCommand::LegacyPreset { preset },
            },
            true,
            Some(preset),
        ));
    }
    let ControlTopic::Device(topic_device) = topic else {
        return None;
    };
    if payload.len() > MAX_CONTROL_REQUEST_BYTES {
        tracing::warn!("rejected oversized MQTT control request");
        return None;
    }
    match serde_json::from_slice::<DeviceControlV2Request>(payload) {
        Ok(request) => Some((topic_device, request, false, None)),
        Err(error) => {
            tracing::warn!(%error, "rejected malformed MQTT control request");
            None
        }
    }
}

fn reserve_control_result(
    client: &AsyncClient,
    pending_results: &Arc<Semaphore>,
    device_id: &DeviceId,
    request: &DeviceControlV2Request,
    legacy_alias: bool,
    legacy_preset: Option<ControlPreset>,
) -> Option<OwnedSemaphorePermit> {
    match Arc::clone(pending_results).try_acquire_owned() {
        Ok(permit) => Some(permit),
        Err(_) => {
            reject_device_control(
                client,
                device_id,
                request,
                legacy_alias,
                legacy_preset,
                Rejection::ResultsBusy,
            );
            None
        }
    }
}

fn reject_device_control(
    client: &AsyncClient,
    device_id: &DeviceId,
    request: &DeviceControlV2Request,
    legacy_alias: bool,
    legacy_preset: Option<ControlPreset>,
    rejection: Rejection,
) {
    let legacy_response = legacy_alias.then(|| {
        ControlResponse::rejected(
            legacy_preset.unwrap_or(ControlPreset::TimerClear),
            rejection_message(rejection),
        )
    });
    publish_result_without_wait(
        client,
        &request.request_id,
        device_id.clone(),
        legacy_alias,
        legacy_preset,
        CachedV2ControlResult {
            response: DeviceControlV2Response {
                request_id: request.request_id.as_str().to_owned(),
                status: rejection_status(rejection).into(),
            },
            legacy_response,
        },
    );
}

fn enqueue_control(
    client: &AsyncClient,
    controls: &mpsc::Sender<MqttControlWork>,
    device_id: DeviceId,
    request: DeviceControlV2Request,
    legacy_alias: bool,
    legacy_preset: Option<ControlPreset>,
    permit: OwnedSemaphorePermit,
) {
    let request_id = request.request_id.clone();
    let (reply, response) = oneshot::channel();
    match controls.try_send(MqttControlWork {
        device_id: device_id.clone(),
        request: request.clone(),
        reply,
    }) {
        Ok(()) => {
            tokio::spawn(publish_control_reply(
                client.clone(),
                device_id,
                legacy_alias,
                legacy_preset,
                request_id,
                response,
                permit,
            ));
        }
        Err(mpsc::error::TrySendError::Full(work)) => reject_device_control(
            client,
            &work.device_id,
            &work.request,
            legacy_alias,
            legacy_preset,
            Rejection::QueueFull,
        ),
        Err(mpsc::error::TrySendError::Closed(work)) => reject_device_control(
            client,
            &work.device_id,
            &work.request,
            legacy_alias,
            legacy_preset,
            Rejection::WorkerUnavailable,
        ),
    }
}

async fn publish_control_reply(
    client: AsyncClient,
    device_id: DeviceId,
    legacy_alias: bool,
    legacy_preset: Option<ControlPreset>,
    request_id: CommandId,
    response: oneshot::Receiver<CachedV2ControlResult>,
    _permit: OwnedSemaphorePermit,
) {
    if timeout(
        CONTROL_RESPONSE_TIMEOUT,
        wait_and_publish_reply(
            &client,
            &device_id,
            legacy_alias,
            legacy_preset,
            &request_id,
            response,
        ),
    )
    .await
    .is_err()
    {
        tracing::warn!("MQTT control response or publication timed out");
    }
}

async fn wait_and_publish_reply(
    client: &AsyncClient,
    device_id: &DeviceId,
    legacy_alias: bool,
    legacy_preset: Option<ControlPreset>,
    request_id: &CommandId,
    response: oneshot::Receiver<CachedV2ControlResult>,
) {
    match response.await {
        Ok(response) => {
            publish_ack(
                client,
                device_id,
                legacy_alias,
                legacy_preset,
                request_id,
                &response,
            )
            .await
        }
        Err(_) => tracing::warn!("MQTT control worker ended before returning a result"),
    }
}

async fn publish_ack(
    client: &AsyncClient,
    device_id: &DeviceId,
    legacy_alias: bool,
    legacy_preset: Option<ControlPreset>,
    request_id: &CommandId,
    result: &CachedV2ControlResult,
) {
    let payload = control_result_payload(request_id, legacy_alias, legacy_preset, result);
    let topic = result_topic(device_id, legacy_alias);
    match payload {
        Ok(Some(payload)) => {
            if let Err(error) = client
                .publish(topic, QoS::AtLeastOnce, false, payload)
                .await
            {
                tracing::warn!(%error, "could not publish MQTT control result");
            }
        }
        Ok(None) => tracing::warn!("legacy control result is unavailable"),
        Err(error) => tracing::error!(%error, "could not serialize MQTT control result"),
    }
}

fn result_topic(device_id: &DeviceId, legacy_alias: bool) -> String {
    if legacy_alias {
        CONTROL_RESULT_TOPIC.to_owned()
    } else {
        format!("updraft/{}/control/result", device_id.as_str())
    }
}

fn publish_result_without_wait(
    client: &AsyncClient,
    request_id: &CommandId,
    device_id: DeviceId,
    legacy_alias: bool,
    legacy_preset: Option<ControlPreset>,
    result: CachedV2ControlResult,
) {
    let payload = control_result_payload(request_id, legacy_alias, legacy_preset, &result);
    let topic = result_topic(&device_id, legacy_alias);
    match payload {
        Ok(Some(payload)) => {
            if let Err(error) = client.try_publish(topic, QoS::AtLeastOnce, false, payload) {
                tracing::warn!(%error, "could not enqueue MQTT control rejection");
            }
        }
        Ok(None) => tracing::warn!("legacy control result is unavailable"),
        Err(error) => tracing::error!(%error, "could not serialize MQTT control result"),
    }
}

fn control_result_payload(
    request_id: &CommandId,
    legacy_alias: bool,
    legacy_preset: Option<ControlPreset>,
    result: &CachedV2ControlResult,
) -> Result<Option<Vec<u8>>, serde_json::Error> {
    match legacy_alias {
        true => match &result.legacy_response {
            Some(legacy) => control_acknowledgement_payload(request_id, legacy).map(Some),
            None => control_acknowledgement_payload(
                request_id,
                &ControlResponse::rejected(
                    legacy_preset.unwrap_or(ControlPreset::TimerClear),
                    result.response.status.as_str(),
                ),
            )
            .map(Some),
        },
        false => serde_json::to_vec(&result.response).map(Some),
    }
}

fn control_acknowledgement_payload(
    request_id: &CommandId,
    result: &ControlResponse,
) -> Result<Vec<u8>, serde_json::Error> {
    #[derive(Serialize)]
    struct Acknowledgement<'a> {
        request_id: &'a str,
        #[serde(flatten)]
        result: &'a ControlResponse,
    }

    serde_json::to_vec(&Acknowledgement {
        request_id: request_id.as_str(),
        result,
    })
}

async fn publish_discovery(
    client: &AsyncClient,
    snapshot: &MqttStateSnapshot,
    previous_topics: &mut HashSet<String>,
) {
    let configs = discovery_configs(&snapshot.devices, snapshot.legacy_discovery_enabled)
        .chain(device_discovery_configs(&snapshot.devices))
        .collect::<Vec<_>>();
    let active_topics = configs
        .iter()
        .map(|(topic, _)| topic.clone())
        .collect::<HashSet<_>>();
    let inactive_topics =
        inactive_discovery_topics(&snapshot.devices, previous_topics, &active_topics);
    clear_device_discovery_topics(client, inactive_topics.into_iter()).await;
    stream::iter(configs)
        .for_each(|(topic, config)| publish_discovery_config(client, topic, config))
        .await;
    *previous_topics = active_topics;
}

async fn clear_device_discovery_topics(client: &AsyncClient, topics: impl Iterator<Item = String>) {
    stream::iter(topics)
        .for_each(|topic| clear_discovery_topic(client, topic))
        .await;
}

async fn publish_discovery_config(client: &AsyncClient, topic: String, config: Value) {
    let payload = serde_json::to_vec(&config);
    drop(config);
    match payload {
        Ok(payload) => {
            if let Err(error) = client.publish(topic, QoS::AtLeastOnce, true, payload).await {
                tracing::warn!(%error, "could not queue MQTT discovery config");
            }
        }
        Err(error) => tracing::error!(%error, "could not serialize MQTT discovery config"),
    }
}

async fn clear_discovery(
    client: &AsyncClient,
    snapshot: &MqttStateSnapshot,
    previous_topics: &mut HashSet<String>,
) {
    stream::iter(
        previous_topics
            .iter()
            .cloned()
            .chain(discovery_topic_candidates(&snapshot.devices))
            .collect::<HashSet<_>>(),
    )
    .for_each(|topic| clear_discovery_topic(client, topic))
    .await;
    previous_topics.clear();
}

fn discovery_topic_candidates(devices: &[DeviceDescriptor]) -> impl Iterator<Item = String> + '_ {
    discovery_tombstones()
        .map(str::to_owned)
        .chain(device_discovery_topics(devices))
}

fn inactive_discovery_topics(
    devices: &[DeviceDescriptor],
    previous_topics: &HashSet<String>,
    active_topics: &HashSet<String>,
) -> HashSet<String> {
    previous_topics
        .iter()
        .cloned()
        .chain(discovery_topic_candidates(devices))
        .filter(|topic| !active_topics.contains(topic))
        .collect()
}

async fn clear_discovery_topic(client: &AsyncClient, topic: String) {
    if let Err(error) = client
        .publish(topic, QoS::AtLeastOnce, true, Vec::new())
        .await
    {
        tracing::warn!(%error, "could not clear retained MQTT discovery config");
    }
}

fn discovery_tombstones() -> impl Iterator<Item = &'static str> {
    DISCOVERY_TOPICS.into_iter()
}

fn device_discovery_topics(devices: &[DeviceDescriptor]) -> impl Iterator<Item = String> + '_ {
    devices.iter().flat_map(|device| {
        [
            format!(
                "homeassistant/sensor/updraft_{}/temperature_v2/config",
                device.id.as_str()
            ),
            format!(
                "homeassistant/sensor/updraft_{}/humidity_v2/config",
                device.id.as_str()
            ),
            format!(
                "homeassistant/select/updraft_{}/mode_v2/config",
                device.id.as_str()
            ),
            format!(
                "homeassistant/sensor/updraft_{}/control_result_v2/config",
                device.id.as_str()
            ),
        ]
        .into_iter()
    })
}

async fn publish(client: &AsyncClient, topic: &str, payload: &str) {
    if let Err(error) = client
        .publish(topic, QoS::AtLeastOnce, true, payload.as_bytes().to_owned())
        .await
    {
        tracing::warn!(%error, topic, "could not queue MQTT message");
    }
}

fn discovery_configs(
    devices: &[DeviceDescriptor],
    legacy_discovery_enabled: bool,
) -> impl Iterator<Item = (String, Value)> + '_ {
    let legacy = devices.iter().find(|device| {
        device.id == DeviceId::configured_ble()
            && match device.backend {
                DeviceBackend::LegacyBle => true,
                DeviceBackend::QuickConnect => false,
            }
    });
    DISCOVERY_TOPICS
        .into_iter()
        .zip(
            sensor_discovery_configs()
                .chain(std::iter::once_with(control_discovery_config))
                .chain(std::iter::once_with(result_discovery_config)),
        )
        .filter(move |(_, _)| legacy.is_some() && legacy_discovery_enabled)
        .map(|(topic, config)| (topic.to_owned(), config))
}

fn device_discovery_configs(
    devices: &[DeviceDescriptor],
) -> impl Iterator<Item = (String, Value)> + '_ {
    devices
        .iter()
        .filter(|device| match device.backend {
            DeviceBackend::QuickConnect => true,
            DeviceBackend::LegacyBle => false,
        })
        .flat_map(|device| {
        let id = device.id.as_str();
        let identifier = format!("updraft_{id}");
        let state_topic = format!("updraft/{id}/state");
        let availability_topic = format!("updraft/{id}/availability");
        let base = |entity: &str, name: String, value_template: String| {
            json!({
                "name": name,
                "unique_id": format!("{identifier}_{entity}"),
                "state_topic": state_topic.clone(),
                "availability": [
                    {"topic": PROCESS_AVAILABILITY_TOPIC},
                    {"topic": availability_topic.clone()}
                ],
                "availability_mode": "all",
                "payload_available": "online",
                "payload_not_available": "offline",
                "value_template": value_template,
                "device": { "identifiers": [identifier.clone()], "name": device.name },
            })
        };
        let state_entities = if device.capabilities.read_state
            && device.state_source == EntitySource::Mqtt
        {
            [
                ("temperature", "Temperature", "{{ value_json.state.temperature_f }}", Some("°F")),
                ("humidity", "Humidity", "{{ value_json.state.humidity_percent }}", Some("%")),
            ]
            .into_iter()
            .map(|(key, name, value_template, unit)| {
                let mut config = base(key, format!("{} {name}", device.name), value_template.to_owned());
                config["unit_of_measurement"] = json!(unit);
                (format!("homeassistant/sensor/{identifier}/{key}_v2/config"), config)
            })
            .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        let has_mode_command = device.command_source == EntitySource::Mqtt
            && device.capabilities.commands.iter().any(is_quickconnect_mode_capability);
        let command_entities = if has_mode_command {
                let mut config = base(
                    "mode",
                    format!("{} mode", device.name),
                    "{{ value_json.state.settings.mode }}".to_owned(),
                );
                config["command_topic"] = json!(format!("updraft/{id}/control/set"));
                config["qos"] = json!(1);
                config["command_template"] = json!("{% set issued = (as_timestamp(now()) * 1000) | int %}{\"request_id\":\"{{ issued }}\",\"issued_at_unix_ms\":{{ issued }},\"command\":{\"kind\":\"quick_connect_mode\",\"mode\":\"{{ value }}\"}}");
                config["options"] = json!(["off", "automatic", "timer", "manual"]);
                config["entity_category"] = json!("config");
                vec![
                    (format!("homeassistant/select/{identifier}/mode_v2/config"), config),
                    (
                        format!("homeassistant/sensor/{identifier}/control_result_v2/config"),
                        json!({
                            "name": format!("{} control result", device.name),
                            "unique_id": format!("{identifier}_control_result"),
                            "state_topic": format!("updraft/{id}/control/result"),
                            "availability": [
                                {"topic": PROCESS_AVAILABILITY_TOPIC},
                                {"topic": availability_topic}
                            ],
                            "availability_mode": "all",
                            "payload_available": "online",
                            "payload_not_available": "offline",
                            "value_template": "{{ value_json.status }}",
                            "device": { "identifiers": [identifier], "name": device.name },
                            "entity_category": "diagnostic"
                        }),
                    ),
                ]
        } else {
            Vec::new()
        };
        state_entities.into_iter().chain(command_entities)
    })
}

fn is_quickconnect_mode_capability(capability: &CommandCapability) -> bool {
    match capability {
        CommandCapability::QuickConnectMode => true,
        CommandCapability::LegacyPreset(_)
        | CommandCapability::QuickConnectTargets
        | CommandCapability::QuickConnectTimerDuration => false,
    }
}

struct SensorDiscovery {
    key: &'static str,
    name: &'static str,
    field: &'static str,
    unit: Option<&'static str>,
    device_class: Option<&'static str>,
    state_class: Option<&'static str>,
    entity_category: Option<&'static str>,
}

const SENSOR_DISCOVERY: [SensorDiscovery; 11] = [
    SensorDiscovery {
        key: "temperature",
        name: "Ambient temperature",
        field: "temperature_f",
        unit: Some("°F"),
        device_class: Some("temperature"),
        state_class: Some("measurement"),
        entity_category: None,
    },
    SensorDiscovery {
        key: "humidity",
        name: "Relative humidity",
        field: "humidity_percent",
        unit: Some("%"),
        device_class: Some("humidity"),
        state_class: Some("measurement"),
        entity_category: None,
    },
    SensorDiscovery {
        key: "mode",
        name: "Controller mode",
        field: "mode",
        unit: None,
        device_class: None,
        state_class: None,
        entity_category: Some("diagnostic"),
    },
    SensorDiscovery {
        key: "controller_fan_flag",
        name: "Controller fan flag",
        field: "controller_fan_flag",
        unit: None,
        device_class: None,
        state_class: None,
        entity_category: Some("diagnostic"),
    },
    SensorDiscovery {
        key: "firmware_version",
        name: "Firmware version",
        field: "firmware_version",
        unit: None,
        device_class: None,
        state_class: None,
        entity_category: Some("diagnostic"),
    },
    SensorDiscovery {
        key: "automatic_temperature_threshold",
        name: "Automatic temperature threshold",
        field: "automatic_temperature_threshold_f",
        unit: Some("°F"),
        device_class: Some("temperature"),
        state_class: None,
        entity_category: Some("diagnostic"),
    },
    SensorDiscovery {
        key: "automatic_humidity_threshold",
        name: "Automatic humidity threshold",
        field: "automatic_humidity_threshold_percent",
        unit: Some("%"),
        device_class: Some("humidity"),
        state_class: None,
        entity_category: Some("diagnostic"),
    },
    SensorDiscovery {
        key: "timer_remaining",
        name: "Timer remaining",
        field: "timer_remaining_minutes",
        unit: Some("min"),
        device_class: Some("duration"),
        state_class: None,
        entity_category: Some("diagnostic"),
    },
    SensorDiscovery {
        key: "timer_original",
        name: "Timer original duration",
        field: "timer_original_minutes",
        unit: Some("min"),
        device_class: Some("duration"),
        state_class: None,
        entity_category: Some("diagnostic"),
    },
    SensorDiscovery {
        key: "freshness",
        name: "State freshness",
        field: "freshness",
        unit: None,
        device_class: None,
        state_class: None,
        entity_category: Some("diagnostic"),
    },
    SensorDiscovery {
        key: "last_error",
        name: "Last query error",
        field: "last_error",
        unit: None,
        device_class: None,
        state_class: None,
        entity_category: Some("diagnostic"),
    },
];

fn sensor_discovery_configs() -> impl Iterator<Item = Value> {
    SENSOR_DISCOVERY.iter().map(SensorDiscovery::config)
}

impl SensorDiscovery {
    fn config(&self) -> Value {
        let mut payload = self.base_config();
        self.add_optional_metadata(&mut payload);
        payload
    }

    fn base_config(&self) -> Value {
        json!({
            "name": self.name,
            "unique_id": format!("{DEVICE_IDENTIFIER}_{}", self.key),
            "state_topic": STATE_TOPIC,
            "availability": self.availability(),
            "availability_mode": "all",
            "payload_available": "online",
            "payload_not_available": "offline",
            "value_template": self.value_template(),
            "device": discovery_device()
        })
    }

    fn value_template(&self) -> String {
        if self.is_snapshot_field() {
            format!("{{{{ value_json.state.{} }}}}", self.field)
        } else {
            format!("{{{{ value_json.{} }}}}", self.field)
        }
    }

    fn availability(&self) -> Value {
        if self.is_snapshot_field() {
            state_availability(STATE_TOPIC, AVAILABILITY_TOPIC)
        } else {
            json!([
                {"topic": PROCESS_AVAILABILITY_TOPIC},
                {"topic": AVAILABILITY_TOPIC}
            ])
        }
    }

    fn is_snapshot_field(&self) -> bool {
        self.key != "freshness" && self.key != "last_error"
    }

    fn add_optional_metadata(&self, payload: &mut Value) {
        [
            ("unit_of_measurement", self.unit),
            ("device_class", self.device_class),
            ("state_class", self.state_class),
            ("entity_category", self.entity_category),
        ]
        .into_iter()
        .filter_map(|(field, value)| value.map(|value| (field, value)))
        .for_each(|(field, value)| payload[field] = json!(value));
    }
}

fn state_availability(state_topic: &str, device_availability_topic: &str) -> Value {
    json!([
        {"topic": PROCESS_AVAILABILITY_TOPIC},
        {"topic": device_availability_topic},
        {
            "topic": state_topic,
            "value_template": "{{ 'online' if value_json.available else 'offline' }}"
        }
    ])
}

fn control_discovery_config() -> Value {
    json!({
            "name": "Controller preset",
            "unique_id": format!("{DEVICE_IDENTIFIER}_control_preset"),
            "command_topic": CONTROL_REQUEST_TOPIC,
            "command_template": "{% set issued = (as_timestamp(now()) * 1000) | int %}{\"request_id\":\"{{ issued }}\",\"issued_at_unix_ms\":{{ issued }},\"preset\":\"{{ value }}\"}",
            "qos": 1,
            "state_topic": STATE_TOPIC,
            "value_template": "{{ value_json.state.control_preset | default('None', true) if value_json.state else 'None' }}",
            "options": [
                ControlPreset::Automatic105F30Percent.as_str(),
                ControlPreset::Automatic105_1F30_1Percent.as_str(),
                ControlPreset::TimerClear.as_str(),
                ControlPreset::TimerOneMinute.as_str()
            ],
            "availability": state_availability(STATE_TOPIC, AVAILABILITY_TOPIC),
            "availability_mode": "all",
            "payload_available": "online",
            "payload_not_available": "offline",
            "device": discovery_device(),
            "entity_category": "config"
    })
}

fn result_discovery_config() -> Value {
    json!({
            "name": "Last control result",
            "unique_id": format!("{DEVICE_IDENTIFIER}_control_result"),
            "state_topic": CONTROL_RESULT_TOPIC,
            "value_template": "{{ value_json.message }}",
            "availability": [
                {"topic": PROCESS_AVAILABILITY_TOPIC},
                {"topic": AVAILABILITY_TOPIC}
            ],
            "availability_mode": "all",
            "payload_available": "online",
            "payload_not_available": "offline",
            "device": discovery_device(),
            "entity_category": "diagnostic"
    })
}

fn discovery_device() -> Value {
    json!({
        "identifiers": [DEVICE_IDENTIFIER],
        "name": "Updraft GAF Wi-Fi Vent",
        "manufacturer": "GAF",
        "model": "GAF Wi-Fi Vent"
    })
}

#[cfg(test)]
mod tests {
    use std::{
        process::{Child, Command, Stdio},
        time::SystemTime,
    };

    use super::*;
    use crate::control::unix_millis;
    use tokio::{
        net::{TcpListener, TcpStream},
        sync::mpsc,
        time::sleep,
    };

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
        options.set_credentials("updraft-test", "updraft-test");
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
        topic: &'static str,
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

    fn mqtt_device(id: &str) -> DeviceDescriptor {
        DeviceDescriptor {
            id: DeviceId::parse(id.to_owned()).unwrap(),
            name: format!("Device {id}"),
            backend: DeviceBackend::QuickConnect,
            capabilities: crate::device::DeviceCapabilities::quickconnect_with_controls(),
            state_source: EntitySource::Mqtt,
            command_source: EntitySource::Mqtt,
        }
    }

    #[test]
    fn control_topics_route_by_local_device_id_and_accept_the_legacy_alias() {
        assert_eq!(
            parse_control_topic("updraft/device-a/control/set"),
            Some(ControlTopic::Device(
                DeviceId::parse("device-a".to_owned()).unwrap()
            ))
        );
        assert_eq!(
            parse_control_topic(CONTROL_REQUEST_TOPIC),
            Some(ControlTopic::LegacyAlias)
        );
        assert_eq!(parse_control_topic("updraft/device/a/control/set"), None);
        assert_eq!(parse_control_topic("updraft/device-a/state"), None);
    }

    #[test]
    fn namespaced_controls_parse_the_full_typed_request() {
        let id = DeviceId::parse("qc-one".to_owned()).unwrap();
        let payload = br#"{"request_id":"same-id","issued_at_unix_ms":1000000,"command":{"kind":"quick_connect_mode","mode":"automatic"}}"#;
        let (routed_id, request, legacy_alias, legacy_preset) =
            parse_incoming_control(ControlTopic::Device(id.clone()), payload).unwrap();

        assert_eq!(routed_id, id);
        assert!(!legacy_alias);
        assert_eq!(legacy_preset, None);
        assert_eq!(request.request_id.as_str(), "same-id");
        assert_eq!(
            request.command,
            DeviceCommand::QuickConnectMode {
                mode: crate::device::QuickConnectMode::Automatic
            }
        );
    }

    #[test]
    fn the_legacy_control_alias_routes_only_to_configured_ble() {
        let payload =
            br#"{"request_id":"legacy-id","issued_at_unix_ms":1000000,"preset":"timer_clear"}"#;
        let (id, request, legacy_alias, preset) =
            parse_incoming_control(ControlTopic::LegacyAlias, payload).unwrap();

        assert_eq!(id, DeviceId::configured_ble());
        assert!(legacy_alias);
        assert_eq!(preset, Some(ControlPreset::TimerClear));
        assert_eq!(
            request.command,
            DeviceCommand::LegacyPreset {
                preset: ControlPreset::TimerClear
            }
        );
    }

    #[test]
    fn discovery_is_namespaced_and_respects_independent_entity_sources() {
        let mut device = DeviceDescriptor {
            id: DeviceId::parse("qc-one".to_owned()).unwrap(),
            name: "Guest room vent".to_owned(),
            backend: DeviceBackend::QuickConnect,
            capabilities: crate::device::DeviceCapabilities::quickconnect_with_controls(),
            state_source: EntitySource::Mqtt,
            command_source: EntitySource::Http,
        };
        let configs = device_discovery_configs(std::slice::from_ref(&device)).collect::<Vec<_>>();
        assert_eq!(configs.len(), 2);
        assert!(configs.iter().all(|(topic, config)| {
            topic.contains("qc-one") && config["unique_id"].as_str().unwrap().contains("qc-one")
        }));
        assert!(
            configs
                .iter()
                .all(|(_, config)| config.get("command_topic").is_none())
        );

        device.state_source = EntitySource::Http;
        device.command_source = EntitySource::Mqtt;
        let configs = device_discovery_configs(&[device]).collect::<Vec<_>>();
        assert_eq!(configs.len(), 2);
        let mode = configs
            .iter()
            .find(|(topic, _)| topic.contains("select"))
            .unwrap();
        assert_eq!(mode.1["command_topic"], "updraft/qc-one/control/set");
        assert_eq!(mode.1["qos"], 1);
        assert!(configs.iter().any(|(topic, config)| {
            topic.contains("control_result")
                && config["state_topic"] == "updraft/qc-one/control/result"
        }));
    }

    #[test]
    fn source_changes_tombstone_only_entities_that_are_no_longer_selected() {
        let device = DeviceDescriptor {
            id: DeviceId::parse("qc-one".to_owned()).unwrap(),
            name: "Guest room vent".to_owned(),
            backend: DeviceBackend::QuickConnect,
            capabilities: crate::device::DeviceCapabilities::quickconnect_with_controls(),
            state_source: EntitySource::Mqtt,
            command_source: EntitySource::Http,
        };
        let active = device_discovery_configs(std::slice::from_ref(&device))
            .map(|(topic, _)| topic)
            .collect::<HashSet<_>>();
        let tombstones =
            inactive_discovery_topics(std::slice::from_ref(&device), &HashSet::new(), &active);
        assert!(tombstones.iter().any(|topic| topic.contains("mode_v2")));
        assert!(
            tombstones
                .iter()
                .any(|topic| topic.contains("control_result_v2"))
        );
        assert!(
            tombstones
                .iter()
                .all(|topic| !topic.contains("temperature_v2"))
        );
        assert!(
            tombstones
                .iter()
                .all(|topic| !topic.contains("humidity_v2"))
        );
    }

    #[test]
    fn explicit_legacy_discovery_keeps_the_configured_ble_entities() {
        let ble = DeviceDescriptor {
            id: DeviceId::configured_ble(),
            name: "Configured BLE".to_owned(),
            backend: DeviceBackend::LegacyBle,
            capabilities: crate::device::DeviceCapabilities::legacy_ble(),
            state_source: EntitySource::Http,
            command_source: EntitySource::Http,
        };
        let configs = discovery_configs(std::slice::from_ref(&ble), true).collect::<Vec<_>>();
        assert_eq!(configs.len(), DISCOVERY_TOPICS.len());
        assert!(discovery_configs(&[ble], false).next().is_none());
    }

    #[test]
    fn every_discovered_entity_uses_process_and_device_availability() {
        let configs = discovery_configs(&legacy_ble_mqtt_descriptor(), true).collect::<Vec<_>>();
        assert!(configs.iter().all(|(_, config)| {
            config["availability_mode"] == "all"
                && config["availability"]
                    .as_array()
                    .is_some_and(|availability| {
                        availability.len() >= 2
                            && availability
                                .iter()
                                .any(|item| item["topic"] == PROCESS_AVAILABILITY_TOPIC)
                    })
        }));
    }

    fn legacy_ble_mqtt_descriptor() -> Vec<DeviceDescriptor> {
        vec![DeviceDescriptor {
            id: DeviceId::configured_ble(),
            name: "Configured BLE".to_owned(),
            backend: DeviceBackend::LegacyBle,
            capabilities: crate::device::DeviceCapabilities::legacy_ble(),
            state_source: EntitySource::Mqtt,
            command_source: EntitySource::Mqtt,
        }]
    }

    #[test]
    fn rejection_status_distinguishes_stale_and_retained_commands() {
        assert_eq!(rejection_status(Rejection::Stale), "stale_request");
        assert_eq!(rejection_status(Rejection::Retained), "retained_request");
        assert_eq!(rejection_status(Rejection::QueueFull), "queue_full");
    }

    #[test]
    fn device_availability_is_independent_and_preserves_the_legacy_alias() {
        let available = MqttStatePublication {
            id: DeviceId::configured_ble(),
            payload: r#"{"available":true}"#.to_owned(),
            available: true,
            legacy_payload: Some(r#"{"available":true}"#.to_owned()),
        };
        let unavailable = MqttStatePublication {
            id: DeviceId::parse("qc-one".to_owned()).unwrap(),
            payload: r#"{"available":false}"#.to_owned(),
            available: false,
            legacy_payload: None,
        };
        let available_messages = state_messages(&available).collect::<Vec<_>>();
        let unavailable_messages = state_messages(&unavailable).collect::<Vec<_>>();

        assert!(available_messages.contains(&(
            "updraft/configured/availability".to_owned(),
            "online".to_owned()
        )));
        assert!(available_messages.contains(&(AVAILABILITY_TOPIC.to_owned(), "online".to_owned())));
        assert!(unavailable_messages.contains(&(
            "updraft/qc-one/availability".to_owned(),
            "offline".to_owned()
        )));
        assert!(
            available_messages
                .iter()
                .all(|(_, value)| value != "offline")
        );
    }

    #[test]
    fn control_subscription_preserves_the_publishers_retain_flag_on_the_wire() {
        let subscription = rumqttc::v5::mqttbytes::v5::Subscribe::new(control_subscription(), None);
        let mut encoded = bytes::BytesMut::new();
        subscription.write(&mut encoded).unwrap();

        assert_eq!(encoded.last(), Some(&0x09));

        let device_subscription =
            rumqttc::v5::mqttbytes::v5::Subscribe::new(device_control_subscription(), None);
        let mut encoded = bytes::BytesMut::new();
        device_subscription.write(&mut encoded).unwrap();
        assert_eq!(encoded.last(), Some(&0x09));
    }

    #[test]
    fn device_results_are_correlated_and_publish_to_the_device_namespace() {
        let request_id = CommandId::parse("qc-command-1").unwrap();
        let result = CachedV2ControlResult {
            response: DeviceControlV2Response {
                request_id: request_id.as_str().to_owned(),
                status: "confirmed".into(),
            },
            legacy_response: None,
        };
        let payload = control_result_payload(&request_id, false, None, &result)
            .unwrap()
            .unwrap();
        let payload: Value = serde_json::from_slice(&payload).unwrap();
        let device_id = DeviceId::parse("qc-one".to_owned()).unwrap();

        assert_eq!(
            result_topic(&device_id, false),
            "updraft/qc-one/control/result"
        );
        assert_eq!(payload["request_id"], "qc-command-1");
        assert_eq!(payload["status"], "confirmed");
    }

    #[test]
    fn legacy_alias_rejections_keep_the_legacy_correlated_result_shape() {
        let request_id = CommandId::parse("legacy-command-1").unwrap();
        let result = CachedV2ControlResult {
            response: DeviceControlV2Response {
                request_id: request_id.as_str().to_owned(),
                status: "unknown_device".into(),
            },
            legacy_response: None,
        };
        let payload = control_result_payload(
            &request_id,
            true,
            Some(ControlPreset::TimerOneMinute),
            &result,
        )
        .unwrap()
        .unwrap();
        let payload: Value = serde_json::from_slice(&payload).unwrap();

        assert_eq!(
            result_topic(&DeviceId::configured_ble(), true),
            CONTROL_RESULT_TOPIC
        );
        assert_eq!(payload["request_id"], "legacy-command-1");
        assert_eq!(payload["success"], false);
        assert_eq!(payload["preset"], "timer_one_minute");
        assert_eq!(payload["message"], "unknown_device");
    }

    #[test]
    fn discovery_configs_use_stable_topics_and_the_shared_device() {
        let configs = discovery_configs(&legacy_ble_mqtt_descriptor(), true).collect::<Vec<_>>();

        assert_eq!(configs.len(), 13);
        assert!(configs.iter().all(|(topic, payload)| {
            topic.contains("/updraft/")
                && topic.ends_with("/config")
                && payload["device"]["identifiers"][0] == DEVICE_IDENTIFIER
        }));
        assert!(configs.iter().any(|(topic, payload)| {
            *topic == "homeassistant/sensor/updraft/temperature/config"
                && payload["value_template"] == "{{ value_json.state.temperature_f }}"
                && payload["unit_of_measurement"] == "°F"
                && payload["availability"].as_array().unwrap().len() == 3
        }));
        assert!(configs.iter().any(|(topic, payload)| {
            *topic == "homeassistant/sensor/updraft/freshness/config"
                && payload["value_template"] == "{{ value_json.freshness }}"
                && payload["availability"].as_array().unwrap().len() == 2
        }));
    }

    #[test]
    fn discovery_payloads_do_not_include_the_ble_device_identifier() {
        let encoded = serde_json::to_string(
            &discovery_configs(&legacy_ble_mqtt_descriptor(), true).collect::<Vec<_>>(),
        )
        .unwrap();

        assert!(!encoded.contains("private-peripheral-id"));
    }

    #[test]
    fn control_commands_require_a_valid_correlation_id_and_supported_preset() {
        let request = parse_control_request(
            br#"{"request_id":"guest-room-42","issued_at_unix_ms":1000000,"preset":"automatic105_f30_percent"}"#,
        )
        .unwrap();

        assert_eq!(request.request_id().as_str(), "guest-room-42");
        assert_eq!(request.preset(), ControlPreset::Automatic105F30Percent);
        assert!(
            parse_control_request(
                br#"{"request_id":"guest-room-42","issued_at_unix_ms":1000000,"preset":"arbitrary_temperature"}"#,
            )
            .is_err()
        );
        assert!(
            parse_control_request(br#"{"request_id":"invalid id","issued_at_unix_ms":1000000,"preset":"timer_clear"}"#)
                .is_err()
        );
        assert!(
            parse_control_request(br#"{"request_id":"guest-room-42","preset":"timer_clear"}"#)
                .is_err()
        );
        assert!(parse_control_request(b"not json").is_err());
    }

    #[test]
    fn control_requests_are_bounded_before_json_parsing() {
        let oversized = vec![b' '; MAX_CONTROL_REQUEST_BYTES + 1];

        assert!(parse_control_request(&oversized).is_err());
    }

    #[tokio::test]
    async fn stalled_result_publication_bounds_accepted_controls() {
        let (client, eventloop) = AsyncClient::new(MqttOptions::new("test", "localhost", 1883), 1);
        let (controls, queued) = mpsc::channel(CONTROL_QUEUE_CAPACITY);
        let pending = Arc::new(Semaphore::new(2));
        let request = |index| {
            Publish::new(
                CONTROL_REQUEST_TOPIC,
                QoS::AtLeastOnce,
                serde_json::to_vec(&json!({
                    "request_id": format!("request-{index}"),
                    "issued_at_unix_ms": unix_millis(SystemTime::now()).unwrap(),
                    "preset": "timer_clear",
                }))
                .unwrap(),
                None,
            )
        };

        // One result fills the MQTT queue; two more wait for publication.
        let mut queued = stream::iter(0..3)
            .fold(queued, |mut queued, index| {
                let (client, controls, pending, request) = (&client, &controls, &pending, &request);
                async move {
                    dispatch_control(
                        client,
                        controls,
                        pending,
                        ControlTopic::LegacyAlias,
                        request(index),
                    );
                    let work = queued.try_recv().expect("control should be accepted");
                    assert!(
                        work.reply
                            .send(CachedV2ControlResult {
                                response: DeviceControlV2Response {
                                    request_id: work.request.request_id.as_str().to_owned(),
                                    status: "rejected".into(),
                                },
                                legacy_response: Some(ControlResponse::rejected(
                                    ControlPreset::TimerClear,
                                    "test result"
                                )),
                            })
                            .is_ok()
                    );
                    tokio::task::yield_now().await;

                    queued
                }
            })
            .await;
        dispatch_control(
            &client,
            &controls,
            &pending,
            ControlTopic::LegacyAlias,
            request(3),
        );
        assert!(
            queued.try_recv().is_err(),
            "stalled results must bound new device work"
        );
        drop(eventloop);
    }

    #[test]
    fn discovery_exposes_one_correlated_control_select_on_the_updraft_device() {
        let configs = discovery_configs(&legacy_ble_mqtt_descriptor(), true).collect::<Vec<_>>();
        let (topic, control) = configs
            .iter()
            .find(|(topic, _)| *topic == "homeassistant/select/updraft/control/config")
            .expect("control select discovery is present");

        assert_eq!(*topic, "homeassistant/select/updraft/control/config");
        assert_eq!(control["command_topic"], "updraft/gaf_vent/control/set");
        assert_eq!(control["qos"], 1);
        assert!(control.get("command_qos").is_none());
        assert_eq!(control["state_topic"], STATE_TOPIC);
        assert_eq!(
            control["value_template"],
            "{{ value_json.state.control_preset | default('None', true) if value_json.state else 'None' }}"
        );
        assert_eq!(control["availability_mode"], "all");
        assert_eq!(control["availability"].as_array().unwrap().len(), 3);
        assert_eq!(
            control["availability"][2]["value_template"],
            "{{ 'online' if value_json.available else 'offline' }}"
        );
        assert!(
            control["command_template"]
                .as_str()
                .unwrap()
                .contains("request_id")
        );
        assert!(
            control["command_template"]
                .as_str()
                .unwrap()
                .contains("preset")
        );
        assert!(
            control["command_template"]
                .as_str()
                .unwrap()
                .contains("issued_at_unix_ms")
        );
        assert_eq!(control["options"].as_array().unwrap().len(), 4);
        assert_eq!(control["device"]["identifiers"][0], DEVICE_IDENTIFIER);

        let result = configs
            .iter()
            .find(|(topic, _)| *topic == "homeassistant/sensor/updraft/control_result/config")
            .expect("control result sensor discovery is present");
        assert_eq!(result.1["state_topic"], "updraft/gaf_vent/control/result");
    }

    #[test]
    fn control_acknowledgement_echoes_request_id_and_http_result_fields() {
        let request = parse_control_request(
            br#"{"request_id":"guest-room-42","issued_at_unix_ms":1000000,"preset":"timer_clear"}"#,
        )
        .unwrap();
        let response = ControlResponse::rejected(
            request.preset(),
            "command was not confirmed; readback failed",
        );
        let payload = control_acknowledgement_payload(request.request_id(), &response).unwrap();
        let value: Value = serde_json::from_slice(&payload).unwrap();

        assert_eq!(value["request_id"], "guest-room-42");
        assert_eq!(value["success"], false);
        assert_eq!(value["preset"], "timer_clear");
        assert_eq!(
            value["message"],
            "command was not confirmed; readback failed"
        );
        assert!(value["state"].is_null());
    }

    #[test]
    fn disabling_discovery_clears_only_updraft_retained_config_topics() {
        let topics = discovery_tombstones().collect::<Vec<_>>();
        let configs = discovery_configs(&legacy_ble_mqtt_descriptor(), true).collect::<Vec<_>>();

        assert_eq!(topics.len(), configs.len());
        assert!(
            topics
                .iter()
                .all(|topic| topic.starts_with("homeassistant/"))
        );
        assert!(
            topics
                .iter()
                .any(|topic| { *topic == "homeassistant/select/updraft/control/config" })
        );
    }

    #[tokio::test]
    async fn native_broker_retains_device_state_and_discovery_and_applies_source_tombstones() {
        let broker = start_native_broker().await;
        let (observer, mut received) = observed_client("mqtt-observer", broker.port);
        observer
            .subscribe(PROCESS_AVAILABILITY_TOPIC, QoS::AtLeastOnce)
            .await
            .unwrap();

        let mut device = mqtt_device("qc-one");
        let other_device = mqtt_device("qc-two");
        let bridge = start(
            MqttConfig {
                host: "127.0.0.1".to_owned(),
                port: broker.port,
                username: "updraft-test".to_owned(),
                password: "updraft-test".to_owned(),
                discovery_enabled: true,
            },
            MqttStateSnapshot {
                devices: vec![device.clone(), other_device.clone()],
                publications: vec![
                    MqttStatePublication {
                        id: device.id.clone(),
                        payload: r#"{"state":{"mode":"automatic"}}"#.to_owned(),
                        available: true,
                        legacy_payload: None,
                    },
                    MqttStatePublication {
                        id: other_device.id.clone(),
                        payload: r#"{"state":{"mode":"manual"}}"#.to_owned(),
                        available: false,
                        legacy_payload: None,
                    },
                ],
                legacy_discovery_enabled: false,
            },
        );

        let process_availability = receive_topic(&mut received, PROCESS_AVAILABILITY_TOPIC).await;
        assert_eq!(process_availability.payload.as_ref(), b"online");
        observer
            .subscribe_many([Filter {
                preserve_retain: true,
                ..Filter::new("updraft/qc-one/state", QoS::AtLeastOnce)
            }])
            .await
            .unwrap();
        let retained_state = receive_topic(&mut received, "updraft/qc-one/state").await;
        assert!(retained_state.retain);
        assert_eq!(
            serde_json::from_slice::<Value>(&retained_state.payload).unwrap()["state"]["mode"],
            "automatic"
        );
        observer
            .subscribe("updraft/qc-one/availability", QoS::AtLeastOnce)
            .await
            .unwrap();
        let first_availability = receive_topic(&mut received, "updraft/qc-one/availability").await;
        assert!(first_availability.retain);
        assert_eq!(first_availability.payload.as_ref(), b"online");
        observer
            .subscribe("updraft/qc-two/availability", QoS::AtLeastOnce)
            .await
            .unwrap();
        let other_availability = receive_topic(&mut received, "updraft/qc-two/availability").await;
        assert!(other_availability.retain);
        assert_eq!(other_availability.payload.as_ref(), b"offline");

        let discovery_topic = "homeassistant/sensor/updraft_qc-one/temperature_v2/config";
        observer
            .subscribe_many([Filter {
                preserve_retain: true,
                ..Filter::new(discovery_topic, QoS::AtLeastOnce)
            }])
            .await
            .unwrap();
        let retained_discovery = receive_topic(&mut received, discovery_topic).await;
        assert!(retained_discovery.retain);
        let discovery: Value = serde_json::from_slice(&retained_discovery.payload).unwrap();
        assert_eq!(discovery["unique_id"], "updraft_qc-one_temperature");
        assert_eq!(discovery["device"]["identifiers"][0], "updraft_qc-one");
        assert_eq!(
            discovery["availability"][1]["topic"],
            "updraft/qc-one/availability"
        );
        assert_eq!(
            discovery["availability"][0]["topic"],
            PROCESS_AVAILABILITY_TOPIC
        );
        assert_eq!(discovery["availability_mode"], "all");

        let other_discovery_topic = "homeassistant/sensor/updraft_qc-two/temperature_v2/config";
        observer
            .subscribe_many([Filter {
                preserve_retain: true,
                ..Filter::new(other_discovery_topic, QoS::AtLeastOnce)
            }])
            .await
            .unwrap();
        let other_discovery = receive_topic(&mut received, other_discovery_topic).await;
        assert!(other_discovery.retain);
        let other_discovery: Value = serde_json::from_slice(&other_discovery.payload).unwrap();
        assert_eq!(other_discovery["unique_id"], "updraft_qc-two_temperature");
        assert_eq!(
            other_discovery["device"]["identifiers"][0],
            "updraft_qc-two"
        );
        assert_eq!(
            other_discovery["availability"][1]["topic"],
            "updraft/qc-two/availability"
        );
        assert_eq!(
            other_discovery["availability"][0]["topic"],
            PROCESS_AVAILABILITY_TOPIC
        );
        assert_eq!(other_discovery["availability_mode"], "all");
        assert_ne!(discovery["unique_id"], other_discovery["unique_id"]);

        device.state_source = EntitySource::Http;
        device.command_source = EntitySource::Http;
        bridge
            .state_updates
            .send_replace(Arc::new(MqttStateSnapshot {
                devices: vec![device, other_device],
                publications: vec![MqttStatePublication {
                    id: DeviceId::parse("qc-two".to_owned()).unwrap(),
                    payload: r#"{"state":{"mode":"manual"}}"#.to_owned(),
                    available: false,
                    legacy_payload: None,
                }],
                legacy_discovery_enabled: false,
            }));
        let tombstone = receive_topic(&mut received, discovery_topic).await;
        assert!(tombstone.retain);
        assert!(tombstone.payload.is_empty());
    }

    #[tokio::test]
    async fn native_broker_preserves_retained_control_metadata_for_rejection() {
        let broker = start_native_broker().await;
        let (observer, mut received) = observed_client("mqtt-control-observer", broker.port);
        observer
            .subscribe(PROCESS_AVAILABILITY_TOPIC, QoS::AtLeastOnce)
            .await
            .unwrap();
        observer
            .subscribe("updraft/qc-one/control/result", QoS::AtLeastOnce)
            .await
            .unwrap();

        let mut bridge = start(
            MqttConfig {
                host: "127.0.0.1".to_owned(),
                port: broker.port,
                username: "updraft-test".to_owned(),
                password: "updraft-test".to_owned(),
                discovery_enabled: false,
            },
            MqttStateSnapshot {
                devices: Vec::new(),
                publications: Vec::new(),
                legacy_discovery_enabled: false,
            },
        );
        let _ = receive_topic(&mut received, PROCESS_AVAILABILITY_TOPIC).await;

        let request_id = CommandId::parse("retained-command-1").unwrap();
        let request = json!({
            "request_id": request_id.as_str(),
            "issued_at_unix_ms": unix_millis(SystemTime::now()).unwrap(),
            "command": {"kind": "quick_connect_mode", "mode": "automatic"}
        });
        observer
            .publish(
                "updraft/qc-one/control/set",
                QoS::AtLeastOnce,
                true,
                serde_json::to_vec(&request).unwrap(),
            )
            .await
            .unwrap();

        let result = receive_topic(&mut received, "updraft/qc-one/control/result").await;
        assert_eq!(result.qos, QoS::AtLeastOnce);
        let result: Value = serde_json::from_slice(&result.payload).unwrap();
        assert_eq!(result["request_id"], request_id.as_str());
        assert_eq!(result["status"], "retained_request");
        assert!(bridge.control_requests.try_recv().is_err());
    }

    #[tokio::test]
    async fn native_broker_publishes_the_retained_process_will_on_unexpected_disconnect() {
        let broker = start_native_broker().await;
        let (observer, mut received) = observed_client("mqtt-will-observer", broker.port);
        observer
            .subscribe_many([Filter {
                preserve_retain: true,
                ..Filter::new(PROCESS_AVAILABILITY_TOPIC, QoS::AtLeastOnce)
            }])
            .await
            .unwrap();

        let config = MqttConfig {
            host: "127.0.0.1".to_owned(),
            port: broker.port,
            username: "updraft-test".to_owned(),
            password: "updraft-test".to_owned(),
            discovery_enabled: false,
        };
        let (client, eventloop) = AsyncClient::new(mqtt_options(config), 16);
        let (connected, mut connection_events) = mpsc::unbounded_channel();
        let poll_task = tokio::spawn(mqtt_events(eventloop).for_each(move |event| {
            if let Ok(Event::Incoming(Packet::ConnAck(_))) = event {
                let _ = connected.send(());
            }
            future::ready(())
        }));
        timeout(Duration::from_secs(5), connection_events.recv())
            .await
            .expect("MQTT client did not connect")
            .expect("MQTT event loop ended before connecting");
        client
            .publish(PROCESS_AVAILABILITY_TOPIC, QoS::AtLeastOnce, true, "online")
            .await
            .unwrap();
        let online = receive_topic(&mut received, PROCESS_AVAILABILITY_TOPIC).await;
        assert!(online.retain);
        assert_eq!(online.payload.as_ref(), b"online");

        poll_task.abort();
        let _ = poll_task.await;
        let offline = receive_topic(&mut received, PROCESS_AVAILABILITY_TOPIC).await;
        assert!(offline.retain);
        assert_eq!(offline.payload.as_ref(), b"offline");
    }

    #[tokio::test]
    async fn native_broker_restart_reconnects_and_republishes_retained_state() {
        let mut broker = start_native_broker().await;
        let (observer, mut received) = observed_client("mqtt-reconnect-observer", broker.port);
        observer
            .subscribe(PROCESS_AVAILABILITY_TOPIC, QoS::AtLeastOnce)
            .await
            .unwrap();
        let device = mqtt_device("qc-reconnect");
        let bridge = start(
            MqttConfig {
                host: "127.0.0.1".to_owned(),
                port: broker.port,
                username: "updraft-test".to_owned(),
                password: "updraft-test".to_owned(),
                discovery_enabled: false,
            },
            MqttStateSnapshot {
                devices: vec![device.clone()],
                publications: vec![MqttStatePublication {
                    id: device.id,
                    payload: r#"{"state":{"mode":"timer"}}"#.to_owned(),
                    available: true,
                    legacy_payload: None,
                }],
                legacy_discovery_enabled: false,
            },
        );

        let initial_availability = receive_topic(&mut received, PROCESS_AVAILABILITY_TOPIC).await;
        assert_eq!(initial_availability.payload.as_ref(), b"online");
        observer
            .subscribe_many([Filter {
                preserve_retain: true,
                ..Filter::new("updraft/qc-reconnect/state", QoS::AtLeastOnce)
            }])
            .await
            .unwrap();
        let initial_state = receive_topic(&mut received, "updraft/qc-reconnect/state").await;
        assert!(initial_state.retain);
        assert_eq!(
            serde_json::from_slice::<Value>(&initial_state.payload).unwrap()["state"]["mode"],
            "timer"
        );
        sleep(Duration::from_millis(100)).await;
        broker.restart().await;
        let (reconnected_observer, mut reconnected_messages) =
            observed_client("mqtt-reconnected-observer", broker.port);
        reconnected_observer
            .subscribe(PROCESS_AVAILABILITY_TOPIC, QoS::AtLeastOnce)
            .await
            .unwrap();
        let reconnected_availability =
            receive_topic(&mut reconnected_messages, PROCESS_AVAILABILITY_TOPIC).await;
        assert_eq!(reconnected_availability.payload.as_ref(), b"online");

        reconnected_observer
            .subscribe_many([Filter {
                preserve_retain: true,
                ..Filter::new("updraft/qc-reconnect/state", QoS::AtLeastOnce)
            }])
            .await
            .unwrap();
        let restored_state =
            receive_topic(&mut reconnected_messages, "updraft/qc-reconnect/state").await;
        assert!(restored_state.retain);
        assert_eq!(
            serde_json::from_slice::<Value>(&restored_state.payload).unwrap()["state"]["mode"],
            "timer"
        );
        drop(bridge);
    }
}
