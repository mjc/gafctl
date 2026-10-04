use serde_json::{Value, json};

use gafctl_api::{CommandCapability, DeviceBackend, DeviceDescriptor, DeviceId, EntitySource};

use super::topics::Topics;

pub(super) fn configs(devices: &[DeviceDescriptor]) -> impl Iterator<Item = (String, Value)> + '_ {
    devices
        .iter()
        .filter(|device| {
            device.state_source == EntitySource::Mqtt && device.command_source == EntitySource::Mqtt
        })
        .flat_map(device_configs)
}

pub(super) fn candidates(
    topics: Topics,
    identities: &[(DeviceId, DeviceBackend)],
) -> impl Iterator<Item = String> + '_ {
    identities.iter().flat_map(move |(id, backend)| {
        sensors(*backend)
            .map(move |sensor| topics.discovery(id, "sensor", sensor.key))
            .chain([
                topics.discovery(id, "select", "preset"),
                topics.discovery(id, "select", "mode"),
                topics.discovery(id, "sensor", "control_result"),
                topics.discovery(id, "sensor", "controller_fan_flag"),
                topics.discovery(id, "select", "automatic_thresholds"),
                topics.discovery(id, "select", "timer"),
            ])
            .chain(
                number_controls(*backend)
                    .map(move |control| topics.discovery(id, "number", control.key)),
            )
            .chain([
                topics.discovery(id, "button", "refresh"),
                topics.discovery(id, "button", "all_off"),
            ])
            .chain(
                binary_sensors(*backend)
                    .map(move |sensor| topics.discovery(id, "binary_sensor", sensor.key)),
            )
            .chain(
                ["automatic", "timer", "manual"]
                    .into_iter()
                    .map(move |mode| topics.discovery(id, "switch", &format!("{mode}_mode"))),
            )
    })
}

fn device_configs(device: &DeviceDescriptor) -> impl Iterator<Item = (String, Value)> + '_ {
    sensors(device.backend)
        .filter(|_| device.capabilities.read_state)
        .map(|sensor| (topic(device, "sensor", sensor.key), sensor.config(device)))
        .chain(control_configs(device))
        .chain(
            binary_sensors(device.backend)
                .filter(|_| device.capabilities.read_state)
                .map(|sensor| {
                    (
                        topic(device, "binary_sensor", sensor.key),
                        sensor.config(device),
                    )
                }),
        )
        .chain(number_configs(device))
        .chain(button_configs(device))
        .chain(switch_configs(device))
}

fn topic(device: &DeviceDescriptor, domain: &str, key: &str) -> String {
    Topics(device.proxy_id).discovery(&device.id, domain, key)
}

fn base(device: &DeviceDescriptor, key: &str, name: &str, field: &str) -> Value {
    let topics = Topics(device.proxy_id);
    json!({
        "name": name,
        "unique_id": format!("{}_{key}", topics.identifier(&device.id)),
        "state_topic": topics.device(&device.id, "state"),
        "value_template": nullable_template(field),
        "availability": [
            {"topic": topics.process_availability()},
            {"topic": topics.device(&device.id, "availability")},
        ],
        "availability_mode": "all",
        "payload_available": "online",
        "payload_not_available": "offline",
        "device": {
            "identifiers": [topics.identifier(&device.id)],
            "name": device.name,
            "manufacturer": "GAF",
        },
    })
}

fn nullable_template(field: &str) -> String {
    let (path, divisor) = field
        .split_once(" / ")
        .map_or((field, None), |(path, divisor)| (path, Some(divisor)));
    let reading = reading_expression(path);
    match divisor {
        Some(divisor) => format!(
            "{{% set reading = {reading} %}}{{{{ reading / {divisor} if reading is number else none }}}}"
        ),
        None => format!("{{{{ {reading} }}}}"),
    }
}

fn reading_expression(path: &str) -> String {
    path.split('.')
        .fold("value_json".to_owned(), |parent, key| {
            format!("({parent} or {{}}).get('{key}')")
        })
}

struct Sensor {
    key: &'static str,
    name: &'static str,
    field: &'static str,
    unit: Option<&'static str>,
    class: Option<&'static str>,
    measurement: bool,
}

impl Sensor {
    fn config(&self, device: &DeviceDescriptor) -> Value {
        let mut config = base(device, self.key, self.name, self.field);
        if self.key == "freshness" {
            config["value_template"] = json!(
                "{{ 'fresh' if value_json.available else ('unknown' if value_json.inventory_status == 'unknown' else 'stale') }}"
            );
        }
        [
            ("unit_of_measurement", self.unit),
            ("device_class", self.class),
            ("state_class", self.measurement.then_some("measurement")),
            (
                "entity_category",
                (!self.measurement).then_some("diagnostic"),
            ),
        ]
        .into_iter()
        .filter_map(|(key, value)| value.map(|value| (key, value)))
        .for_each(|(key, value)| config[key] = json!(value));
        config
    }
}

fn sensors(backend: DeviceBackend) -> impl Iterator<Item = Sensor> {
    [
        Sensor {
            key: "temperature",
            name: "Temperature",
            field: "state.temperature_f",
            unit: Some("°F"),
            class: Some("temperature"),
            measurement: true,
        },
        Sensor {
            key: "humidity",
            name: "Humidity",
            field: "state.humidity_percent",
            unit: Some("%"),
            class: Some("humidity"),
            measurement: true,
        },
        Sensor {
            key: "mode",
            name: "Controller mode",
            field: "state.settings.mode",
            unit: None,
            class: None,
            measurement: false,
        },
        Sensor {
            key: "firmware_version",
            name: "Firmware version",
            field: "state.diagnostics.firmware_version",
            unit: None,
            class: None,
            measurement: false,
        },
        Sensor {
            key: "last_error",
            name: "Last query error",
            field: "last_error",
            unit: None,
            class: None,
            measurement: false,
        },
        Sensor {
            key: "freshness",
            name: "State freshness",
            field: "available",
            unit: None,
            class: None,
            measurement: false,
        },
    ]
    .into_iter()
    .chain(ble_sensors(backend))
    .chain(cloud_sensors(backend))
}

fn ble_sensors(backend: DeviceBackend) -> impl Iterator<Item = Sensor> {
    [
        Sensor {
            key: "automatic_temperature_threshold",
            name: "Automatic temperature threshold",
            field: "state.settings.automatic_temperature_tenths_f / 10",
            unit: Some("°F"),
            class: Some("temperature"),
            measurement: false,
        },
        Sensor {
            key: "automatic_humidity_threshold",
            name: "Automatic humidity threshold",
            field: "state.settings.automatic_humidity_tenths_percent / 10",
            unit: Some("%"),
            class: Some("humidity"),
            measurement: false,
        },
        Sensor {
            key: "timer_remaining",
            name: "Timer remaining",
            field: "state.settings.timer_remaining_minutes",
            unit: Some("min"),
            class: Some("duration"),
            measurement: false,
        },
        Sensor {
            key: "timer_original",
            name: "Timer original duration",
            field: "state.settings.timer_original_minutes",
            unit: Some("min"),
            class: Some("duration"),
            measurement: false,
        },
    ]
    .into_iter()
    .filter(move |_| backend == DeviceBackend::LegacyBle)
}

fn control_configs(device: &DeviceDescriptor) -> impl Iterator<Item = (String, Value)> + '_ {
    let presets = device
        .capabilities
        .commands
        .iter()
        .filter_map(|capability| match capability {
            CommandCapability::LegacyPreset(preset) => Some(<&'static str>::from(preset)),
            CommandCapability::LegacyAutomaticTemperature
            | CommandCapability::LegacyAutomaticHumidity
            | CommandCapability::LegacyTimer
            | CommandCapability::QuickConnectMode
            | CommandCapability::QuickConnectTargets
            | CommandCapability::QuickConnectTimerDuration => None,
        })
        .collect::<Vec<_>>();
    let selectors = [
        ("automatic_thresholds", "Automatic thresholds", "automatic"),
        ("timer", "Fan timer", "timer"),
    ]
    .into_iter()
    .filter_map(move |(key, name, prefix)| {
        let options = presets
            .iter()
            .copied()
            .filter(|preset| preset.starts_with(prefix))
            .collect::<Vec<_>>();
        (!options.is_empty())
            .then(|| select_config(device, key, name, &options, "legacy_preset", "preset"))
    });
    let mode = device
        .capabilities
        .commands
        .contains(&CommandCapability::QuickConnectMode)
        .then(|| {
            select_config(
                device,
                "mode",
                "Mode",
                &["off", "automatic", "timer", "manual"],
                "quick_connect_mode",
                "mode",
            )
        });
    let result = (!device.capabilities.commands.is_empty()).then(|| {
        let mut config = base(device, "control_result", "Last control result", "status");
        config["state_topic"] = json!(Topics(device.proxy_id).device(&device.id, "control/result"));
        config["entity_category"] = json!("diagnostic");
        (topic(device, "sensor", "control_result"), config)
    });
    selectors.chain(mode).chain(result)
}

fn select_config(
    device: &DeviceDescriptor,
    key: &str,
    name: &str,
    options: &[&str],
    kind: &str,
    field: &str,
) -> (String, Value) {
    let mut config = base(device, key, name, "state.settings.mode");
    set_command_topic(&mut config, device, "control/set");
    config["options"] = json!(options);
    config["entity_category"] = json!("config");
    config["command_template"] = json!(command_template(&format!(
        "{{\"kind\":\"{kind}\",\"{field}\":{{{{ value | to_json }}}}}}"
    )));
    config["optimistic"] = json!(false);
    if kind == "legacy_preset" {
        config["value_template"] = json!(preset_readback_template(key));
    }
    (topic(device, "select", key), config)
}

fn preset_readback_template(key: &str) -> &'static str {
    match key {
        "automatic_thresholds" => {
            "{% set settings = (value_json.state or {}).get('settings') or {} %}{% if settings.get('automatic_temperature_tenths_f') == 1050 and settings.get('automatic_humidity_tenths_percent') == 300 %}automatic105_f30_percent{% elif settings.get('automatic_temperature_tenths_f') == 1051 and settings.get('automatic_humidity_tenths_percent') == 301 %}automatic105_1_f30_1_percent{% else %}{{ none }}{% endif %}"
        }
        "timer" => {
            "{% set settings = (value_json.state or {}).get('settings') or {} %}{% if settings.get('timer_remaining_minutes') == 0 and settings.get('timer_original_minutes') == 0 %}timer_clear{% elif settings.get('timer_remaining_minutes') == 1 and settings.get('timer_original_minutes') == 1 %}timer_one_minute{% else %}{{ none }}{% endif %}"
        }
        _ => "{{ none }}",
    }
}

fn cloud_sensors(backend: DeviceBackend) -> impl Iterator<Item = Sensor> {
    [
        Sensor {
            key: "signal_strength_raw",
            name: "Signal strength (reported)",
            field: "state.diagnostics.signal_strength_raw",
            unit: None,
            class: None,
            measurement: false,
        },
        Sensor {
            key: "verified_raw",
            name: "Verification (reported)",
            field: "state.diagnostics.verified_raw",
            unit: None,
            class: None,
            measurement: false,
        },
    ]
    .into_iter()
    .filter(move |_| backend == DeviceBackend::QuickConnect)
}

struct BinarySensor {
    key: &'static str,
    name: &'static str,
    field: &'static str,
    mode: Option<&'static str>,
    provenance: &'static str,
}

impl BinarySensor {
    fn config(&self, device: &DeviceDescriptor) -> Value {
        let mut config = base(device, self.key, self.name, self.field);
        let reading = reading_expression(self.field);
        let expression = self.mode.map_or_else(|| reading.clone(), |mode| format!("({reading} == '{mode}') if {reading} in ['off','automatic','timer','manual'] else none"));
        config["value_template"] = json!(format!(
            "{{% set reading = {expression} %}}{{{{ 'ON' if reading is sameas true else ('OFF' if reading is sameas false else none) }}}}"
        ));
        config["payload_on"] = json!("ON");
        config["payload_off"] = json!("OFF");
        config["entity_category"] = json!("diagnostic");
        config["json_attributes_topic"] = config["state_topic"].clone();
        config["json_attributes_template"] = json!(format!(
            "{{{{ {{'provenance':'{}','last_error':value_json.last_error}} | to_json }}}}",
            self.provenance
        ));
        config
    }
}

fn binary_sensors(backend: DeviceBackend) -> impl Iterator<Item = BinarySensor> {
    [
        BinarySensor {
            key: "controller_fan_flag",
            name: "Controller fan flag",
            field: "state.settings.controller_fan_on",
            mode: None,
            provenance: "controller",
        },
        BinarySensor {
            key: "running_estimate",
            name: "Running estimate",
            field: "state.estimated_running",
            mode: None,
            provenance: "inferred",
        },
        BinarySensor {
            key: "ota_in_progress",
            name: "OTA in progress",
            field: "state.diagnostics.ota_in_progress",
            mode: None,
            provenance: "reported",
        },
        BinarySensor {
            key: "humidity_monitor",
            name: "Humidity monitoring",
            field: "state.settings.humidity_monitor",
            mode: None,
            provenance: "reported",
        },
        BinarySensor {
            key: "automatic_mode",
            name: "Automatic mode",
            field: "state.settings.mode",
            mode: Some("automatic"),
            provenance: "reported",
        },
        BinarySensor {
            key: "timer_mode",
            name: "Timer mode",
            field: "state.settings.mode",
            mode: Some("timer"),
            provenance: "reported",
        },
        BinarySensor {
            key: "manual_mode",
            name: "Manual mode",
            field: "state.settings.mode",
            mode: Some("manual"),
            provenance: "reported",
        },
    ]
    .into_iter()
    .filter(move |sensor| {
        (sensor.key == "controller_fan_flag") == (backend == DeviceBackend::LegacyBle)
    })
}

struct NumberControl {
    key: &'static str,
    name: &'static str,
    kind: &'static str,
    field: &'static str,
    reading: &'static str,
    capability: CommandCapability,
    minimum: u16,
    maximum: u16,
    step: u16,
    unit: &'static str,
}

impl NumberControl {
    fn config(&self, device: &DeviceDescriptor) -> Value {
        let mut config = base(device, self.key, self.name, self.reading);
        config["min"] = json!(self.minimum);
        config["max"] = json!(self.maximum);
        config["step"] = json!(self.step);
        config["unit_of_measurement"] = json!(self.unit);
        config["mode"] = json!("box");
        config["optimistic"] = json!(false);
        set_command_topic(&mut config, device, "control/set");
        config["command_template"] = json!(format!(
            "{{% set number = value | float(default=none) %}}{}",
            command_template(&format!(
                "{{\"kind\":\"{}\",\"{}\":{{{{ (number | int if number is number and value is not boolean and number == number | int else none) | to_json }}}}}}",
                self.kind, self.field
            ))
        ));
        if self.key == "timer_duration" && device.backend == DeviceBackend::LegacyBle {
            config["value_template"] = json!(
                "{% set reading = ((value_json.state or {}).get('settings') or {}).get('timer_original_minutes') %}{{ reading if reading is number and 0 <= reading <= 360 else none }}"
            );
        }
        config
    }
}

fn number_configs(device: &DeviceDescriptor) -> impl Iterator<Item = (String, Value)> + '_ {
    number_controls(device.backend)
        .filter(|control| device.capabilities.commands.contains(&control.capability))
        .map(|control| (topic(device, "number", control.key), control.config(device)))
}

fn number_controls(backend: DeviceBackend) -> impl Iterator<Item = NumberControl> {
    let temperature = NumberControl {
        key: "automatic_temperature",
        name: "Target temperature",
        kind: "legacy_automatic_temperature",
        field: "temperature_f",
        reading: "state.settings.automatic_temperature_tenths_f / 10",
        capability: CommandCapability::LegacyAutomaticTemperature,
        minimum: 90,
        maximum: 120,
        step: 1,
        unit: "°F",
    };
    let humidity = NumberControl {
        key: "automatic_humidity",
        name: "Target humidity",
        kind: "legacy_automatic_humidity",
        field: "humidity_percent",
        reading: "state.settings.automatic_humidity_tenths_percent / 10",
        capability: CommandCapability::LegacyAutomaticHumidity,
        minimum: 30,
        maximum: 80,
        step: 1,
        unit: "%",
    };
    let timer = NumberControl {
        key: "timer_duration",
        name: "Timer duration",
        kind: "legacy_timer",
        field: "minutes",
        reading: "state.settings.timer_original_minutes",
        capability: CommandCapability::LegacyTimer,
        minimum: 0,
        maximum: 360,
        step: 1,
        unit: "min",
    };
    let controls = match backend {
        DeviceBackend::LegacyBle => [temperature, humidity, timer],
        DeviceBackend::QuickConnect => [
            NumberControl {
                kind: "quick_connect_automatic_temperature",
                reading: "state.settings.automatic_temperature_f",
                capability: CommandCapability::QuickConnectTargets,
                ..temperature
            },
            NumberControl {
                kind: "quick_connect_automatic_humidity",
                reading: "state.settings.automatic_humidity_percent",
                capability: CommandCapability::QuickConnectTargets,
                ..humidity
            },
            NumberControl {
                kind: "quick_connect_timer_duration",
                reading: "state.settings.timer_duration_minutes",
                capability: CommandCapability::QuickConnectTimerDuration,
                minimum: 30,
                step: 30,
                ..timer
            },
        ],
    };
    controls.into_iter()
}

fn command_template(command: &str) -> String {
    request_template(Some(command))
}

fn request_template(command: Option<&str>) -> String {
    let prefix = r#"{% set issued = (as_timestamp(now()) * 1000) | int %}{% set nonce = (range(0, 65536) | random) ~ '-' ~ (range(0, 65536) | random) %}{"request_id":"{{ issued }}-{{ nonce }}","issued_at_unix_ms":{{ issued }}"#;
    [
        prefix,
        command.map_or("", |_| r#","command":"#),
        command.unwrap_or_default(),
        "}",
    ]
    .concat()
}

fn button_configs(device: &DeviceDescriptor) -> impl Iterator<Item = (String, Value)> + '_ {
    let refresh = device.capabilities.read_state.then(|| {
        let mut config = button_config(device, "refresh", "Refresh readings", "refresh/set");
        config["availability"] = json!([{"topic":Topics(device.proxy_id).process_availability()}]);
        config["command_template"] = json!(request_template(None));
        config["entity_category"] = json!("diagnostic");
        (topic(device, "button", "refresh"), config)
    });
    let off = device
        .capabilities
        .commands
        .contains(&CommandCapability::QuickConnectMode)
        .then(|| {
            let mut config = button_config(device, "all_off", "All off", "control/set");
            config["command_template"] = json!(command_template(
                "{\"kind\":\"quick_connect_mode\",\"mode\":\"off\"}"
            ));
            (topic(device, "button", "all_off"), config)
        });
    refresh.into_iter().chain(off)
}

fn set_command_topic(config: &mut Value, device: &DeviceDescriptor, suffix: &str) {
    config["command_topic"] = json!(Topics(device.proxy_id).device(&device.id, suffix));
    config["qos"] = json!(1);
}

fn button_config(device: &DeviceDescriptor, key: &str, name: &str, suffix: &str) -> Value {
    let mut config = base(device, key, name, "");
    let object = config.as_object_mut().expect("base discovery is an object");
    object.remove("state_topic");
    object.remove("value_template");
    set_command_topic(&mut config, device, suffix);
    config
}

fn switch_configs(device: &DeviceDescriptor) -> impl Iterator<Item = (String, Value)> + '_ {
    ["automatic", "timer", "manual"].into_iter()
        .filter(|_| device.capabilities.commands.contains(&CommandCapability::QuickConnectMode))
        .map(|mode| {
            let key = format!("{mode}_mode");
            let mut config = base(device, &key, &format!("{mode} mode"), "state.settings.mode");
            config["payload_on"] = json!("ON");
            config["payload_off"] = json!("OFF");
            config["state_on"] = json!(mode);
            config["state_off"] = json!("inactive");
            config["value_template"] = json!(format!("{{% set mode = ((value_json.state or {{}}).get('settings') or {{}}).get('mode') %}}{{{{ '{mode}' if mode == '{mode}' else ('inactive' if mode in ['off','automatic','timer','manual'] else none) }}}}"));
            set_command_topic(&mut config, device, "control/set");
            config["optimistic"] = json!(false);
            config["command_template"] = json!(command_template(&format!("{{% if value == 'ON' %}}{{\"kind\":\"quick_connect_mode\",\"mode\":\"{mode}\"}}{{% elif value == 'OFF' %}}{{\"kind\":\"quick_connect_conditional_off\",\"only_if_current\":\"{mode}\"}}{{% else %}}null{{% endif %}}")));
            (topic(device, "switch", &key), config)
        })
}

#[cfg(test)]
mod tests {
    use super::super::{discovery, test_support::mqtt_device};
    use super::*;
    use gafctl_api::ProxyId;
    use std::collections::HashSet;

    #[test]
    fn request_envelopes_preserve_control_and_refresh_payloads() {
        let prefix = r#"{% set issued = (as_timestamp(now()) * 1000) | int %}{% set nonce = (range(0, 65536) | random) ~ '-' ~ (range(0, 65536) | random) %}{"request_id":"{{ issued }}-{{ nonce }}","issued_at_unix_ms":{{ issued }}"#;
        let command = r#"{"kind":"quick_connect_mode","mode":"off"}"#;
        assert_eq!(request_template(None), format!("{prefix}}}"));
        assert_eq!(
            request_template(Some(command)),
            format!("{prefix},\"command\":{command}}}")
        );
    }

    #[test]
    fn generated_number_configs_preserve_backend_metadata_and_cleanup_topics() {
        let mut ble = DeviceDescriptor::configured_ble();
        ble.state_source = EntitySource::Mqtt;
        ble.command_source = EntitySource::Mqtt;
        let cloud = mqtt_device(ble.proxy_id, "cloud");
        [
            (ble, [
                ("automatic_temperature", "legacy_automatic_temperature", "temperature_f", "state.settings.automatic_temperature_tenths_f / 10", 90, 120, 1, "°F"),
                ("automatic_humidity", "legacy_automatic_humidity", "humidity_percent", "state.settings.automatic_humidity_tenths_percent / 10", 30, 80, 1, "%"),
                ("timer_duration", "legacy_timer", "minutes", "state.settings.timer_original_minutes", 0, 360, 1, "min"),
            ]),
            (cloud, [
                ("automatic_temperature", "quick_connect_automatic_temperature", "temperature_f", "state.settings.automatic_temperature_f", 90, 120, 1, "°F"),
                ("automatic_humidity", "quick_connect_automatic_humidity", "humidity_percent", "state.settings.automatic_humidity_percent", 30, 80, 1, "%"),
                ("timer_duration", "quick_connect_timer_duration", "minutes", "state.settings.timer_duration_minutes", 30, 360, 30, "min"),
            ]),
        ]
        .into_iter()
        .for_each(|(device, expected)| {
            let topics = Topics(device.proxy_id);
            let generated = configs(std::slice::from_ref(&device)).collect::<Vec<_>>();
            let cleanup = candidates(topics, &[(device.id.clone(), device.backend)])
                .collect::<HashSet<_>>();
            assert_eq!(number_configs(&device).count(), expected.len());
            expected.into_iter().for_each(|(key, kind, field, reading, min, max, step, unit)| {
                let topic = topics.discovery(&device.id, "number", key);
                let (_, config) = generated.iter().find(|(candidate, _)| candidate == &topic).unwrap();
                assert!(cleanup.contains(&topic));
                assert_eq!(config["min"], json!(min));
                assert_eq!(config["max"], json!(max));
                assert_eq!(config["step"], json!(step));
                assert_eq!(config["unit_of_measurement"], unit);
                assert_eq!(config["mode"], "box");
                assert_eq!(config["optimistic"], false);
                assert_eq!(config["command_topic"], topics.device(&device.id, "control/set"));
                let command = format!(
                    "{{\"kind\":\"{kind}\",\"{field}\":{{{{ (number | int if number is number and value is not boolean and number == number | int else none) | to_json }}}}}}"
                );
                assert_eq!(config["command_template"], format!(
                    "{{% set number = value | float(default=none) %}}{}", command_template(&command)
                ));
                let expected_reading = if device.backend == DeviceBackend::LegacyBle && key == "timer_duration" {
                    "{% set reading = ((value_json.state or {}).get('settings') or {}).get('timer_original_minutes') %}{{ reading if reading is number and 0 <= reading <= 360 else none }}".to_owned()
                } else {
                    nullable_template(reading)
                };
                assert_eq!(config["value_template"], expected_reading);
            });
            // Old discovery keys still need cleanup after an ownership change.
            assert!(cleanup.contains(&topics.discovery(&device.id, "select", "preset")));
            assert!(cleanup.contains(&topics.discovery(&device.id, "sensor", "controller_fan_flag")));
        });
    }

    #[test]
    fn generated_number_configs_filter_capabilities_without_losing_cleanup_topics() {
        [
            (
                DeviceDescriptor::configured_ble(),
                vec![
                    (vec![], vec![]),
                    (
                        vec![CommandCapability::LegacyAutomaticTemperature],
                        vec!["automatic_temperature"],
                    ),
                    (
                        vec![CommandCapability::LegacyAutomaticHumidity],
                        vec!["automatic_humidity"],
                    ),
                    (vec![CommandCapability::LegacyTimer], vec!["timer_duration"]),
                    (vec![CommandCapability::QuickConnectTargets], vec![]),
                ],
            ),
            (
                mqtt_device(ProxyId::default(), "cloud"),
                vec![
                    (vec![], vec![]),
                    (
                        vec![CommandCapability::QuickConnectTargets],
                        vec!["automatic_temperature", "automatic_humidity"],
                    ),
                    (
                        vec![CommandCapability::QuickConnectTimerDuration],
                        vec!["timer_duration"],
                    ),
                    (vec![CommandCapability::QuickConnectMode], vec![]),
                    (vec![CommandCapability::LegacyTimer], vec![]),
                ],
            ),
        ]
        .into_iter()
        .for_each(|(mut device, cases)| {
            device.state_source = EntitySource::Mqtt;
            device.command_source = EntitySource::Mqtt;
            let topics = Topics(device.proxy_id);
            let cleanup =
                candidates(topics, &[(device.id.clone(), device.backend)]).collect::<HashSet<_>>();
            cases.into_iter().for_each(|(commands, expected_keys)| {
                device.capabilities.commands = commands;
                let expected = expected_keys
                    .into_iter()
                    .map(|key| topics.discovery(&device.id, "number", key))
                    .collect::<HashSet<_>>();
                let actual = number_configs(&device)
                    .map(|(topic, _)| topic)
                    .collect::<HashSet<_>>();
                assert_eq!(actual, expected);
                assert!(actual.is_subset(&cleanup));
            });
            [
                "automatic_temperature",
                "automatic_humidity",
                "timer_duration",
            ]
            .into_iter()
            .for_each(|key| {
                assert!(cleanup.contains(&topics.discovery(&device.id, "number", key)))
            });
        });
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
    fn namespaced_templates_preserve_unknown_readings_and_freshness() {
        let mut device = DeviceDescriptor::configured_ble();
        device.state_source = EntitySource::Mqtt;
        device.command_source = EntitySource::Mqtt;
        let cloud = mqtt_device(device.proxy_id, "cloud-fixture");
        let devices = [device, cloud];
        let configs = discovery::configs(&devices).collect::<Vec<_>>();
        assert!(configs.iter().any(|(topic, config)| {
            topic.ends_with("_freshness/config")
                && config["value_template"]
                    .as_str()
                    .unwrap()
                    .contains("unknown")
        }));
        let threshold = configs
            .iter()
            .find(|(topic, _)| topic.ends_with("_automatic_temperature_threshold/config"))
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
    fn discovery_has_sensor_metadata_and_distinct_availability_per_proxy() {
        let device = mqtt_device(ProxyId::default(), "same-id");
        let topics = Topics(device.proxy_id);
        let configs = discovery::configs(std::slice::from_ref(&device)).collect::<Vec<_>>();
        let temperature = configs
            .iter()
            .find(|(topic, _)| topic.ends_with("_temperature/config"))
            .unwrap();
        assert_eq!(temperature.1["device_class"], "temperature");
        assert_eq!(temperature.1["state_class"], "measurement");
        assert_eq!(temperature.1["unit_of_measurement"], "°F");
        assert!(
            configs
                .iter()
                .all(|(topic, config)| config["availability_mode"] == "all"
                    && config["availability"][0]["topic"] == topics.process_availability()
                    && if topic.ends_with("_refresh/config") {
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
}
