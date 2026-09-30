use std::time::Duration;

use rumqttc::{AsyncClient, Event, LastWill, MqttOptions, Packet, QoS};
use serde_json::{Value, json};
use tokio::{sync::watch, time::sleep};

pub(crate) const STATE_TOPIC: &str = "updraft/gaf_vent/state";
const AVAILABILITY_TOPIC: &str = "updraft/gaf_vent/availability";
const DEVICE_IDENTIFIER: &str = "updraft_gaf_vent";

pub(crate) struct MqttConfig {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
}

pub(crate) fn start(config: MqttConfig, initial_state: String) -> watch::Sender<String> {
    let mut options = MqttOptions::new("updraft-gaf-vent", config.host, config.port);
    options.set_keep_alive(Duration::from_secs(30));
    options.set_credentials(config.username, config.password);
    options.set_last_will(LastWill::new(
        AVAILABILITY_TOPIC,
        "offline",
        QoS::AtLeastOnce,
        true,
    ));
    let (client, mut eventloop) = AsyncClient::new(options, 32);
    let (state_tx, _) = watch::channel(initial_state);
    let (connected_tx, connected_rx) = watch::channel(false);

    tokio::spawn(async move {
        loop {
            match eventloop.poll().await {
                Ok(Event::Incoming(Packet::ConnAck(_))) => {
                    connected_tx.send_replace(true);
                }
                Ok(_) => {}
                Err(error) => {
                    connected_tx.send_replace(false);
                    tracing::warn!(%error, "MQTT connection lost; reconnecting");
                    sleep(Duration::from_secs(1)).await;
                }
            }
        }
    });

    let update_client = client;
    let update_rx = state_tx.subscribe();
    tokio::spawn(async move {
        let client = update_client;
        let mut state_rx = update_rx;
        let mut connected_rx = connected_rx;
        loop {
            tokio::select! {
                changed = state_rx.changed() => {
                    if changed.is_err() {
                        return;
                    }
                    if *connected_rx.borrow() {
                        let payload = state_rx.borrow().clone();
                        publish(&client, STATE_TOPIC, &payload).await;
                    }
                }
                changed = connected_rx.changed() => {
                    if changed.is_err() {
                        return;
                    }
                    if *connected_rx.borrow() {
                        publish_discovery(&client).await;
                        publish(&client, AVAILABILITY_TOPIC, "online").await;
                        let payload = state_rx.borrow().clone();
                        publish(&client, STATE_TOPIC, &payload).await;
                    }
                }
            }
        }
    });

    state_tx
}

async fn publish_discovery(client: &AsyncClient) {
    for (topic, payload) in discovery_configs() {
        match serde_json::to_vec(&payload) {
            Ok(payload) => {
                if let Err(error) = client.publish(topic, QoS::AtLeastOnce, true, payload).await {
                    tracing::warn!(%error, "could not queue MQTT discovery config");
                }
            }
            Err(error) => tracing::error!(%error, "could not serialize MQTT discovery config"),
        }
    }
}

async fn publish(client: &AsyncClient, topic: &str, payload: &str) {
    if let Err(error) = client.publish(topic, QoS::AtLeastOnce, true, payload).await {
        tracing::warn!(%error, topic, "could not queue MQTT message");
    }
}

fn discovery_configs() -> Vec<(String, Value)> {
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
                    "device": {
                        "identifiers": [DEVICE_IDENTIFIER],
                        "name": "Updraft GAF Wi-Fi Vent",
                        "manufacturer": "GAF",
                        "model": "GAF Wi-Fi Vent"
                    }
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
                (
                    format!("homeassistant/sensor/updraft/{key}/config"),
                    payload,
                )
            },
        )
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovery_configs_use_stable_topics_and_the_shared_device() {
        let configs = discovery_configs();

        assert_eq!(configs.len(), 11);
        assert!(configs.iter().all(|(topic, payload)| {
            topic.starts_with("homeassistant/sensor/updraft/")
                && topic.ends_with("/config")
                && payload["state_topic"] == STATE_TOPIC
                && payload["availability"][0]["topic"] == AVAILABILITY_TOPIC
                && payload["device"]["identifiers"][0] == DEVICE_IDENTIFIER
        }));
        assert!(configs.iter().any(|(topic, payload)| {
            topic == "homeassistant/sensor/updraft/temperature/config"
                && payload["value_template"] == "{{ value_json.state.temperature_f }}"
                && payload["unit_of_measurement"] == "°F"
                && payload["availability"].as_array().unwrap().len() == 2
        }));
        assert!(configs.iter().any(|(topic, payload)| {
            topic == "homeassistant/sensor/updraft/freshness/config"
                && payload["value_template"] == "{{ value_json.freshness }}"
                && payload["availability"].as_array().unwrap().len() == 1
        }));
    }

    #[test]
    fn discovery_payloads_do_not_include_the_ble_device_identifier() {
        let encoded = serde_json::to_string(&discovery_configs()).unwrap();

        assert!(!encoded.contains("private-peripheral-id"));
    }
}
