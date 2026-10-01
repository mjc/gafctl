use std::{
    sync::Arc,
    time::{Duration, SystemTime},
};

use crate::{
    api::ControlResponse,
    control::{CommandId, ControlPreset, ControlRequest, FreshControlRequest, unix_millis},
};
use rumqttc::{AsyncClient, Event, EventLoop, LastWill, MqttOptions, Packet, Publish, QoS};
use serde::Serialize;
use serde_json::{Value, json};
use thiserror::Error;
use tokio::{
    sync::{Semaphore, mpsc, oneshot, watch},
    time::{sleep, timeout},
};

pub(crate) const STATE_TOPIC: &str = "updraft/gaf_vent/state";
const AVAILABILITY_TOPIC: &str = "updraft/gaf_vent/availability";
const CONTROL_REQUEST_TOPIC: &str = "updraft/gaf_vent/control/set";
const CONTROL_RESULT_TOPIC: &str = "updraft/gaf_vent/control/result";
const DEVICE_IDENTIFIER: &str = "updraft_gaf_vent";
const CONTROL_QUEUE_CAPACITY: usize = 8;
const MAX_PENDING_CONTROL_RESULTS: usize = 32;
const CONTROL_RESPONSE_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_CONTROL_REQUEST_BYTES: usize = 1024;
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
    pub(crate) request: FreshControlRequest,
    pub(crate) reply: oneshot::Sender<Arc<ControlResponse>>,
}

pub(crate) struct MqttBridge {
    pub(crate) state_updates: watch::Sender<Arc<String>>,
    pub(crate) control_requests: mpsc::Receiver<MqttControlWork>,
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

pub(crate) fn start(config: MqttConfig, initial_state: String) -> MqttBridge {
    let mut options = MqttOptions::new("updraft-gaf-vent", config.host, config.port);
    options.set_keep_alive(Duration::from_secs(30));
    options.set_credentials(config.username, config.password);
    options.set_last_will(LastWill::new(
        AVAILABILITY_TOPIC,
        "offline",
        QoS::AtLeastOnce,
        true,
    ));
    let discovery_enabled = config.discovery_enabled;
    let (client, eventloop) = AsyncClient::new(options, 32);
    let (state_tx, state_rx) = watch::channel(Arc::new(initial_state));
    let (connected_tx, connected_rx) = watch::channel(false);
    let (control_tx, control_rx) = mpsc::channel(CONTROL_QUEUE_CAPACITY);
    tokio::spawn(setup_connection(
        client.clone(),
        connected_rx.clone(),
        discovery_enabled,
    ));
    tokio::spawn(run_event_loop(
        eventloop,
        client.clone(),
        connected_tx,
        control_tx,
    ));
    tokio::spawn(publish_state_updates(client, state_rx, connected_rx));

    MqttBridge {
        state_updates: state_tx,
        control_requests: control_rx,
    }
}

async fn setup_connection(
    client: AsyncClient,
    mut connected: watch::Receiver<bool>,
    discovery_enabled: bool,
) {
    while connected.changed().await.is_ok() {
        if !*connected.borrow_and_update() {
            continue;
        }
        if let Err(error) = client
            .subscribe(CONTROL_REQUEST_TOPIC, QoS::AtLeastOnce)
            .await
        {
            tracing::warn!(%error, "could not subscribe to MQTT controls");
        }
        if discovery_enabled {
            publish_discovery(&client).await;
        } else {
            clear_discovery(&client).await;
        }
        publish(&client, AVAILABILITY_TOPIC, "online").await;
    }
}

async fn run_event_loop(
    mut eventloop: EventLoop,
    client: AsyncClient,
    connected: watch::Sender<bool>,
    controls: mpsc::Sender<MqttControlWork>,
) {
    let pending_results = Arc::new(Semaphore::new(MAX_PENDING_CONTROL_RESULTS));
    loop {
        match eventloop.poll().await {
            Ok(Event::Incoming(Packet::ConnAck(_))) => {
                connected.send_replace(true);
            }
            Ok(Event::Incoming(Packet::Publish(message)))
                if message.topic == CONTROL_REQUEST_TOPIC =>
            {
                dispatch_control(&client, &controls, &pending_results, message);
            }
            Ok(_) => {}
            Err(error) => {
                connected.send_replace(false);
                tracing::warn!(%error, "MQTT connection lost; reconnecting");
                sleep(Duration::from_secs(1)).await;
            }
        }
    }
}

async fn publish_state_updates(
    client: AsyncClient,
    mut state: watch::Receiver<Arc<String>>,
    mut connected: watch::Receiver<bool>,
) {
    loop {
        let changed = tokio::select! {
            changed = state.changed() => changed,
            changed = connected.changed() => changed,
        };
        if changed.is_err() {
            return;
        }
        if !*connected.borrow_and_update() {
            continue;
        }
        let payload = Arc::clone(&*state.borrow_and_update());
        publish(&client, STATE_TOPIC, &payload).await;
    }
}

fn dispatch_control(
    client: &AsyncClient,
    control_tx: &mpsc::Sender<MqttControlWork>,
    pending_results: &Arc<Semaphore>,
    message: Publish,
) {
    let request = match parse_control_request(&message.payload) {
        Ok(request) => request,
        Err(error) => {
            tracing::warn!(%error, "rejected malformed MQTT control request");
            return;
        }
    };
    let preset = request.preset();
    let Some(now_unix_ms) = unix_millis(SystemTime::now()) else {
        try_publish_ack(
            client,
            request.request_id(),
            ControlResponse::rejected(preset, "system clock is unavailable"),
        );
        return;
    };
    let request = match request.validate_fresh_at(now_unix_ms) {
        Ok(request) => request,
        Err(request) => {
            try_publish_ack(
                client,
                request.request_id(),
                ControlResponse::rejected(preset, "stale or future-dated control request"),
            );
            return;
        }
    };
    if message.retain {
        try_publish_ack(
            client,
            request.request_id(),
            ControlResponse::rejected(preset, "retained control requests are rejected"),
        );
        return;
    }

    let Ok(permit) = Arc::clone(pending_results).try_acquire_owned() else {
        try_publish_ack(
            client,
            request.request_id(),
            ControlResponse::rejected(preset, "control results are busy"),
        );
        return;
    };

    let request_id = request.request_id().clone();
    let (reply, response) = oneshot::channel();
    match control_tx.try_send(MqttControlWork { request, reply }) {
        Ok(()) => {
            let client = client.clone();
            tokio::spawn(async move {
                let _permit = permit;
                let acknowledge = async {
                    match response.await {
                        Ok(response) => publish_ack(&client, &request_id, &response).await,
                        Err(_) => {
                            tracing::warn!("MQTT control worker ended before returning a result")
                        }
                    }
                };
                if timeout(CONTROL_RESPONSE_TIMEOUT, acknowledge)
                    .await
                    .is_err()
                {
                    tracing::warn!("MQTT control response or publication timed out");
                }
            });
        }
        Err(mpsc::error::TrySendError::Full(_)) => {
            try_publish_ack(
                client,
                &request_id,
                ControlResponse::rejected(preset, "control queue is full"),
            );
        }
        Err(mpsc::error::TrySendError::Closed(_)) => {
            try_publish_ack(
                client,
                &request_id,
                ControlResponse::rejected(preset, "control worker is unavailable"),
            );
        }
    }
}

async fn publish_ack(client: &AsyncClient, request_id: &CommandId, result: &ControlResponse) {
    match control_acknowledgement_payload(request_id, result) {
        Ok(payload) => {
            if let Err(error) = client
                .publish(CONTROL_RESULT_TOPIC, QoS::AtLeastOnce, false, payload)
                .await
            {
                tracing::warn!(%error, "could not publish MQTT control result");
            }
        }
        Err(error) => tracing::error!(%error, "could not serialize MQTT control result"),
    }
}

fn try_publish_ack(client: &AsyncClient, request_id: &CommandId, result: ControlResponse) {
    match control_acknowledgement_payload(request_id, &result) {
        Ok(payload) => {
            if let Err(error) =
                client.try_publish(CONTROL_RESULT_TOPIC, QoS::AtLeastOnce, false, payload)
            {
                tracing::warn!(%error, "could not enqueue MQTT control rejection");
            }
        }
        Err(error) => tracing::error!(%error, "could not serialize MQTT control result"),
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

async fn publish_discovery(client: &AsyncClient) {
    for (topic, config) in discovery_configs() {
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
}

async fn clear_discovery(client: &AsyncClient) {
    for topic in discovery_tombstones() {
        if let Err(error) = client
            .publish(topic, QoS::AtLeastOnce, true, Vec::new())
            .await
        {
            tracing::warn!(%error, "could not clear retained MQTT discovery config");
        }
    }
}

fn discovery_tombstones() -> impl Iterator<Item = &'static str> {
    DISCOVERY_TOPICS.into_iter()
}

async fn publish(client: &AsyncClient, topic: &str, payload: &str) {
    if let Err(error) = client.publish(topic, QoS::AtLeastOnce, true, payload).await {
        tracing::warn!(%error, topic, "could not queue MQTT message");
    }
}

fn discovery_configs() -> impl Iterator<Item = (&'static str, Value)> {
    DISCOVERY_TOPICS.into_iter().zip(
        sensor_discovery_configs()
            .chain(std::iter::once_with(control_discovery_config))
            .chain(std::iter::once_with(result_discovery_config)),
    )
}

fn sensor_discovery_configs() -> impl Iterator<Item = Value> {
    let sensors = [
        (
            "temperature",
            "Ambient temperature",
            "temperature_f",
            Some("°F"),
            Some("temperature"),
            Some("measurement"),
            None,
        ),
        (
            "humidity",
            "Relative humidity",
            "humidity_percent",
            Some("%"),
            Some("humidity"),
            Some("measurement"),
            None,
        ),
        (
            "mode",
            "Controller mode",
            "mode",
            None,
            None,
            None,
            Some("diagnostic"),
        ),
        (
            "controller_fan_flag",
            "Controller fan flag",
            "controller_fan_flag",
            None,
            None,
            None,
            Some("diagnostic"),
        ),
        (
            "firmware_version",
            "Firmware version",
            "firmware_version",
            None,
            None,
            None,
            Some("diagnostic"),
        ),
        (
            "automatic_temperature_threshold",
            "Automatic temperature threshold",
            "automatic_temperature_threshold_f",
            Some("°F"),
            Some("temperature"),
            None,
            Some("diagnostic"),
        ),
        (
            "automatic_humidity_threshold",
            "Automatic humidity threshold",
            "automatic_humidity_threshold_percent",
            Some("%"),
            Some("humidity"),
            None,
            Some("diagnostic"),
        ),
        (
            "timer_remaining",
            "Timer remaining",
            "timer_remaining_minutes",
            Some("min"),
            Some("duration"),
            None,
            Some("diagnostic"),
        ),
        (
            "timer_original",
            "Timer original duration",
            "timer_original_minutes",
            Some("min"),
            Some("duration"),
            None,
            Some("diagnostic"),
        ),
        (
            "freshness",
            "State freshness",
            "freshness",
            None,
            None,
            None,
            Some("diagnostic"),
        ),
        (
            "last_error",
            "Last query error",
            "last_error",
            None,
            None,
            None,
            Some("diagnostic"),
        ),
    ];

    sensors
        .into_iter()
        .map(
            |(key, name, field, unit, device_class, state_class, entity_category)| {
                let (value_path, availability) = match key {
                    "freshness" | "last_error" => {
                        (field.to_owned(), json!([{"topic": AVAILABILITY_TOPIC}]))
                    }
                    _ => (
                        format!("state.{field}"),
                        json!([
                            {"topic": AVAILABILITY_TOPIC},
                            {
                                "topic": STATE_TOPIC,
                                "value_template": "{{ 'online' if value_json.available else 'offline' }}"
                            }
                        ]),
                    ),
                };
                let mut payload = json!({
                    "name": name,
                    "unique_id": format!("{DEVICE_IDENTIFIER}_{key}"),
                    "state_topic": STATE_TOPIC,
                    "availability": availability,
                    "availability_mode": "all",
                    "payload_available": "online",
                    "payload_not_available": "offline",
                    "value_template": format!("{{{{ value_json.{value_path} }}}}"),
                    "device": discovery_device()
                });
                if let Some(unit) = unit {
                    payload["unit_of_measurement"] = json!(unit);
                }
                if let Some(device_class) = device_class {
                    payload["device_class"] = json!(device_class);
                }
                if let Some(state_class) = state_class {
                    payload["state_class"] = json!(state_class);
                }
                if let Some(entity_category) = entity_category {
                    payload["entity_category"] = json!(entity_category);
                }
                payload
            },
        )
}

fn control_discovery_config() -> Value {
    json!({
            "name": "Controller preset",
            "unique_id": format!("{DEVICE_IDENTIFIER}_control_preset"),
            "command_topic": CONTROL_REQUEST_TOPIC,
            "command_template": "{% set issued = (as_timestamp(now()) * 1000) | int %}{\"request_id\":\"{{ issued }}\",\"issued_at_unix_ms\":{{ issued }},\"preset\":\"{{ value }}\"}",
            "qos": 1,
            "state_topic": STATE_TOPIC,
            "value_template": "{{ value_json.state.control_preset | default('unknown') if value_json.state else 'unknown' }}",
            "options": [
                ControlPreset::Automatic105F30Percent.as_str(),
                ControlPreset::Automatic105_1F30_1Percent.as_str(),
                ControlPreset::TimerClear.as_str(),
                ControlPreset::TimerOneMinute.as_str()
            ],
            "availability": [
                {"topic": AVAILABILITY_TOPIC},
                {
                    "topic": STATE_TOPIC,
                    "value_template": "{{ 'online' if value_json.available else 'offline' }}"
                }
            ],
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
            "availability_topic": AVAILABILITY_TOPIC,
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
    use super::*;

    #[test]
    fn discovery_configs_use_stable_topics_and_the_shared_device() {
        let configs = discovery_configs().collect::<Vec<_>>();

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
                && payload["availability"].as_array().unwrap().len() == 2
        }));
        assert!(configs.iter().any(|(topic, payload)| {
            *topic == "homeassistant/sensor/updraft/freshness/config"
                && payload["value_template"] == "{{ value_json.freshness }}"
                && payload["availability"].as_array().unwrap().len() == 1
        }));
    }

    #[test]
    fn discovery_payloads_do_not_include_the_ble_device_identifier() {
        let encoded = serde_json::to_string(&discovery_configs().collect::<Vec<_>>()).unwrap();

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
        let (controls, mut queued) = mpsc::channel(CONTROL_QUEUE_CAPACITY);
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
            )
        };

        // One result fills the MQTT queue; two more wait for publication.
        for index in 0..3 {
            dispatch_control(&client, &controls, &pending, request(index));
            let work = queued.try_recv().expect("control should be accepted");
            assert!(
                work.reply
                    .send(Arc::new(ControlResponse::rejected(
                        work.request.preset(),
                        "test result",
                    )))
                    .is_ok()
            );
            tokio::task::yield_now().await;
        }
        dispatch_control(&client, &controls, &pending, request(3));
        assert!(
            queued.try_recv().is_err(),
            "stalled results must bound new device work"
        );
        drop(eventloop);
    }

    #[test]
    fn discovery_exposes_one_correlated_control_select_on_the_updraft_device() {
        let configs = discovery_configs().collect::<Vec<_>>();
        let (topic, control) = configs
            .iter()
            .find(|(topic, _)| *topic == "homeassistant/select/updraft/control/config")
            .expect("control select discovery is present");

        assert_eq!(*topic, "homeassistant/select/updraft/control/config");
        assert_eq!(control["command_topic"], "updraft/gaf_vent/control/set");
        assert_eq!(control["qos"], 1);
        assert!(control.get("command_qos").is_none());
        assert_eq!(control["state_topic"], STATE_TOPIC);
        assert_eq!(control["availability_mode"], "all");
        assert_eq!(control["availability"].as_array().unwrap().len(), 2);
        assert_eq!(
            control["availability"][1]["value_template"],
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
        let configs = discovery_configs().collect::<Vec<_>>();

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
}
