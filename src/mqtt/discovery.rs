use serde_json::{Value, json};

use crate::device::{CommandCapability, DeviceBackend, DeviceDescriptor, DeviceId, EntitySource};

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
            ])
    })
}

fn device_configs(device: &DeviceDescriptor) -> impl Iterator<Item = (String, Value)> + '_ {
    sensors(device.backend)
        .filter(|_| device.capabilities.read_state)
        .map(|sensor| (topic(device, "sensor", sensor.key), sensor.config(device)))
        .chain(control_configs(device))
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
    let reading = path
        .split('.')
        .fold("value_json".to_owned(), |parent, key| {
            format!("({parent} or {{}}).get('{key}')")
        });
    match divisor {
        Some(divisor) => format!(
            "{{% set reading = {reading} %}}{{{{ reading / {divisor} if reading is number else none }}}}"
        ),
        None => format!("{{{{ {reading} }}}}"),
    }
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
}

fn ble_sensors(backend: DeviceBackend) -> impl Iterator<Item = Sensor> {
    [
        Sensor {
            key: "controller_fan_flag",
            name: "Controller fan flag",
            field: "state.settings.controller_fan_on",
            unit: None,
            class: None,
            measurement: false,
        },
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
            CommandCapability::LegacyPreset(preset) => Some(preset.as_str()),
            CommandCapability::QuickConnectMode
            | CommandCapability::QuickConnectTargets
            | CommandCapability::QuickConnectTimerDuration => None,
        })
        .collect::<Vec<_>>();
    let preset = (!presets.is_empty()).then(|| {
        select_config(
            device,
            "preset",
            "Controller preset",
            &presets,
            "legacy_preset",
            "preset",
        )
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
    preset.into_iter().chain(mode).chain(result)
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
    config["command_topic"] = json!(Topics(device.proxy_id).device(&device.id, "control/set"));
    config["qos"] = json!(1);
    config["options"] = json!(options);
    config["entity_category"] = json!("config");
    config["command_template"] = json!(format!(
        "{{% set issued = (as_timestamp(now()) * 1000) | int %}}{{% set nonce = range(0, 2147483647) | random %}}{{\"request_id\":\"{{{{ issued }}}}-{{{{ nonce }}}}\",\"issued_at_unix_ms\":{{{{ issued }}}},\"command\":{{\"kind\":\"{kind}\",\"{field}\":\"{{{{ value }}}}\"}}}}"
    ));
    if key == "preset" {
        config["value_template"] = json!(preset_readback_template());
        config["optimistic"] = json!(false);
    }
    (topic(device, "select", key), config)
}

fn preset_readback_template() -> &'static str {
    "{% set settings = (value_json.state or {}).get('settings') or {} %}{% if settings.get('mode') == 'automatic' and settings.get('automatic_temperature_tenths_f') == 1050 and settings.get('automatic_humidity_tenths_percent') == 300 %}automatic105_f30_percent{% elif settings.get('mode') == 'automatic' and settings.get('automatic_temperature_tenths_f') == 1051 and settings.get('automatic_humidity_tenths_percent') == 301 %}automatic105_1_f30_1_percent{% elif settings.get('mode') == 'timer' and settings.get('timer_remaining_minutes') == 0 and settings.get('timer_original_minutes') == 0 %}timer_clear{% elif settings.get('mode') == 'timer' and settings.get('timer_remaining_minutes') == 1 and settings.get('timer_original_minutes') == 1 %}timer_one_minute{% else %}{{ none }}{% endif %}"
}
