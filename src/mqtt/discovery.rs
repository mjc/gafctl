use serde_json::{Value, json};

use gafctl_api::{CommandCapability, DeviceBackend, DeviceDescriptor, DeviceId, EntitySource};

use super::topics::Topics;

pub(super) fn configs(devices: &[DeviceDescriptor]) -> impl Iterator<Item = (String, Value)> + '_ {
    devices
        .iter()
        .filter(|device| {
            device.state_source == EntitySource::Mqtt && device.command_source == EntitySource::Mqtt
        })
        .map(device_config)
}

pub(super) fn component_topics(
    topics: Topics,
    identities: &[(DeviceId, DeviceBackend)],
) -> impl Iterator<Item = String> + '_ {
    identities.iter().flat_map(move |(id, backend)| {
        possible_components(*backend)
            .chain([("select", "preset"), ("sensor", "controller_fan_flag")])
            .map(move |(platform, key)| component_topic(topics, id, platform, key))
    })
}

fn component_topic(topics: Topics, id: &DeviceId, platform: &str, key: &str) -> String {
    format!(
        "homeassistant/{platform}/gafctl/{}_{key}/config",
        topics.identifier(id)
    )
}

fn possible_components(
    backend: DeviceBackend,
) -> impl Iterator<Item = (&'static str, &'static str)> {
    sensors(backend)
        .map(|sensor| ("sensor", sensor.key))
        .chain([
            ("select", "mode"),
            ("sensor", "control_result"),
            ("select", "automatic_thresholds"),
            ("select", "timer"),
        ])
        .chain(number_controls(backend).map(|control| ("number", control.key)))
        // Keep the old key so grouped discovery removes previously published buttons.
        .chain([("button", "refresh"), ("button", "all_off")])
        .chain(binary_sensors(backend).map(|sensor| ("binary_sensor", sensor.key)))
        .chain([
            ("switch", "automatic_mode"),
            ("switch", "timer_mode"),
            ("switch", "manual_mode"),
        ])
}

fn device_config(device: &DeviceDescriptor) -> (String, Value) {
    let topics = Topics(device.proxy_id);
    let mut components = possible_components(device.backend)
        .map(|(platform, key)| (component_key(platform, key), json!({"platform": platform})))
        .collect::<serde_json::Map<_, _>>();
    active_components(device).for_each(|(key, config)| {
        components.insert(key, config);
    });
    (
        topics.discovery(&device.id),
        json!({
            "device": {"identifiers": [topics.identifier(&device.id)], "name": device.name, "manufacturer": "GAF"},
            "origin": {"name": "gafctl", "sw_version": env!("CARGO_PKG_VERSION")},
            "state_topic": topics.device(&device.id, "state"),
            "availability": [
                {"topic": topics.process_availability()},
                {"topic": topics.device(&device.id, "availability")},
            ],
            "availability_mode": "all",
            "payload_available": "online",
            "payload_not_available": "offline",
            "components": components,
        }),
    )
}

fn active_components(device: &DeviceDescriptor) -> impl Iterator<Item = (String, Value)> + '_ {
    sensors(device.backend)
        .filter(|_| device.capabilities.read_state)
        .map(|sensor| component("sensor", sensor.key, sensor.config(device)))
        .chain(control_configs(device))
        .chain(
            binary_sensors(device.backend)
                .filter(|_| device.capabilities.read_state)
                .map(|sensor| component("binary_sensor", sensor.key, sensor.config(device))),
        )
        .chain(number_configs(device))
        .chain(button_configs(device))
        .chain(switch_configs(device))
}

fn component(platform: &str, key: &str, mut config: Value) -> (String, Value) {
    config["platform"] = json!(platform);
    (component_key(platform, key), config)
}

fn component_key(domain: &str, key: &str) -> String {
    format!("{domain}_{key}")
}

fn base(device: &DeviceDescriptor, key: &str, name: &str, field: &str) -> Value {
    let topics = Topics(device.proxy_id);
    json!({
        "name": name,
        "unique_id": format!("{}_{key}", topics.identifier(&device.id)),
        "value_template": nullable_template(field),
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
            name: "Timer duration",
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
        component("sensor", "control_result", config)
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
    component("select", key, config)
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
        config["json_attributes_topic"] =
            json!(Topics(device.proxy_id).device(&device.id, "state"));
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
        .map(|control| component("number", control.key, control.config(device)))
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
        name: "Set timer",
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
    let off = device
        .capabilities
        .commands
        .contains(&CommandCapability::QuickConnectMode)
        .then(|| {
            let mut config = button_config(device, "all_off", "All off", "control/set");
            config["command_template"] = json!(command_template(
                "{\"kind\":\"quick_connect_mode\",\"mode\":\"off\"}"
            ));
            component("button", "all_off", config)
        });
    off.into_iter()
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
            component("switch", &key, config)
        })
}

#[cfg(test)]
mod tests {
    use super::super::test_support::mqtt_device;
    use super::*;
    use gafctl_api::ProxyId;
    use std::collections::HashSet;

    fn mqtt_ble() -> DeviceDescriptor {
        let mut device = DeviceDescriptor::configured_ble();
        device.state_source = EntitySource::Mqtt;
        device.command_source = EntitySource::Mqtt;
        device
    }

    #[test]
    fn mqtt_owned_device_has_one_grouped_discovery_with_distinct_component_keys() {
        let device = mqtt_device(ProxyId::default(), "grouped");
        let generated = configs(std::slice::from_ref(&device)).collect::<Vec<_>>();
        assert_eq!(generated.len(), 1);
        assert_eq!(
            generated[0].0,
            Topics(device.proxy_id).discovery(&device.id)
        );
        let config = &generated[0].1;
        for (key, platform) in [
            ("sensor_mode", "sensor"),
            ("select_mode", "select"),
            ("binary_sensor_automatic_mode", "binary_sensor"),
            ("switch_automatic_mode", "switch"),
        ] {
            assert_eq!(config["components"][key]["platform"], platform);
        }
        assert_eq!(config["origin"]["name"], "gafctl");
        assert_eq!(config["origin"]["sw_version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(
            config["device"]["identifiers"][0],
            Topics(device.proxy_id).identifier(&device.id)
        );
        let temperature = &config["components"]["sensor_temperature"];
        assert_eq!(
            temperature["unique_id"],
            format!(
                "{}_temperature",
                Topics(device.proxy_id).identifier(&device.id)
            )
        );
        assert_eq!(temperature["device_class"], "temperature");
        assert_eq!(temperature["state_class"], "measurement");
        assert_eq!(temperature["unit_of_measurement"], "°F");
        for component in config["components"].as_object().unwrap().values() {
            assert!(component.get("device").is_none());
            assert!(component.get("origin").is_none());
            assert!(component.get("availability_mode").is_none());
        }
    }

    #[test]
    fn grouped_discovery_preserves_shared_metadata_and_component_overrides() {
        let device = mqtt_device(ProxyId::default(), "cloud");
        let topics = Topics(device.proxy_id);
        let (_, config) = configs(std::slice::from_ref(&device)).next().unwrap();
        assert_eq!(config["availability_mode"], "all");
        assert_eq!(
            config["availability"][0]["topic"],
            topics.process_availability()
        );
        assert_eq!(
            config["availability"][1]["topic"],
            topics.device(&device.id, "availability")
        );
        assert_eq!(config["state_topic"], topics.device(&device.id, "state"));
        assert_eq!(
            config["components"]["sensor_control_result"]["state_topic"],
            topics.device(&device.id, "control/result")
        );
        assert_eq!(
            config["components"]["button_refresh"],
            json!({"platform": "button"})
        );
        assert!(config["components"]["button_all_off"]["command_topic"].is_string());
        let other = mqtt_device(ProxyId::default(), "cloud");
        assert_ne!(
            configs(&[other]).next().unwrap().0,
            topics.discovery(&device.id)
        );
    }

    #[test]
    fn inactive_components_have_only_platform_tombstones_and_old_topics_use_same_catalogue() {
        for mut device in [mqtt_ble(), mqtt_device(ProxyId::default(), "cloud")] {
            let topics = Topics(device.proxy_id);
            let migration = component_topics(topics, &[(device.id.clone(), device.backend)])
                .collect::<HashSet<_>>();
            device.capabilities.read_state = false;
            device.capabilities.commands.clear();
            let (_, config) = configs(std::slice::from_ref(&device)).next().unwrap();
            for (platform, key) in possible_components(device.backend) {
                assert_eq!(
                    config["components"][component_key(platform, key)],
                    json!({"platform": platform})
                );
                assert!(migration.contains(&format!(
                    "homeassistant/{platform}/gafctl/{}_{key}/config",
                    topics.identifier(&device.id)
                )));
            }
            assert!(migration.contains(&component_topic(topics, &device.id, "select", "preset")));
            assert!(migration.contains(&component_topic(
                topics,
                &device.id,
                "sensor",
                "controller_fan_flag"
            )));
            assert!(config["components"].get("select_preset").is_none());
            assert!(
                config["components"]
                    .get("sensor_controller_fan_flag")
                    .is_none()
            );
        }
    }

    #[test]
    fn number_components_preserve_backend_metadata_and_capability_tombstones() {
        for device in [mqtt_ble(), mqtt_device(ProxyId::default(), "cloud")] {
            let (_, config) = configs(std::slice::from_ref(&device)).next().unwrap();
            let expected = match device.backend {
                DeviceBackend::LegacyBle => [
                    (
                        "legacy_automatic_temperature",
                        "temperature_f",
                        "state.settings.automatic_temperature_tenths_f / 10",
                        90,
                        120,
                        1,
                        "°F",
                    ),
                    (
                        "legacy_automatic_humidity",
                        "humidity_percent",
                        "state.settings.automatic_humidity_tenths_percent / 10",
                        30,
                        80,
                        1,
                        "%",
                    ),
                    (
                        "legacy_timer",
                        "minutes",
                        "state.settings.timer_original_minutes",
                        0,
                        360,
                        1,
                        "min",
                    ),
                ],
                DeviceBackend::QuickConnect => [
                    (
                        "quick_connect_automatic_temperature",
                        "temperature_f",
                        "state.settings.automatic_temperature_f",
                        90,
                        120,
                        1,
                        "°F",
                    ),
                    (
                        "quick_connect_automatic_humidity",
                        "humidity_percent",
                        "state.settings.automatic_humidity_percent",
                        30,
                        80,
                        1,
                        "%",
                    ),
                    (
                        "quick_connect_timer_duration",
                        "minutes",
                        "state.settings.timer_duration_minutes",
                        30,
                        360,
                        30,
                        "min",
                    ),
                ],
            };
            for (control, (kind, field, reading, minimum, maximum, step, unit)) in
                number_controls(device.backend).zip(expected)
            {
                let component = &config["components"][component_key("number", control.key)];
                assert_eq!(component["platform"], "number");
                assert_eq!(component["min"], minimum);
                assert_eq!(component["max"], maximum);
                assert_eq!(component["step"], step);
                assert_eq!(component["unit_of_measurement"], unit);
                assert_eq!(component["mode"], "box");
                assert_eq!(component["optimistic"], false);
                assert_eq!(component["qos"], 1);
                assert_eq!(
                    component["command_topic"],
                    Topics(device.proxy_id).device(&device.id, "control/set")
                );
                let command = format!(
                    "{{\"kind\":\"{kind}\",\"{field}\":{{{{ (number | int if number is number and value is not boolean and number == number | int else none) | to_json }}}}}}"
                );
                assert_eq!(
                    component["command_template"],
                    format!(
                        "{{% set number = value | float(default=none) %}}{}",
                        command_template(&command)
                    )
                );
                let expected_reading = if device.backend == DeviceBackend::LegacyBle
                    && control.key == "timer_duration"
                {
                    "{% set reading = ((value_json.state or {}).get('settings') or {}).get('timer_original_minutes') %}{{ reading if reading is number and 0 <= reading <= 360 else none }}".to_owned()
                } else {
                    nullable_template(reading)
                };
                assert_eq!(component["value_template"], expected_reading);
                let mut limited = device.clone();
                limited
                    .capabilities
                    .commands
                    .retain(|capability| *capability != control.capability);
                let (_, limited_config) = configs(&[limited]).next().unwrap();
                assert_eq!(
                    limited_config["components"][component_key("number", control.key)],
                    json!({"platform": "number"})
                );
            }
        }
    }

    #[test]
    fn namespaced_templates_preserve_unknown_readings_and_freshness() {
        let devices = [mqtt_ble(), mqtt_device(ProxyId::default(), "cloud-fixture")];
        let generated = configs(&devices).collect::<Vec<_>>();
        let components = &generated[0].1["components"];
        assert!(
            components["sensor_freshness"]["value_template"]
                .as_str()
                .unwrap()
                .contains("unknown")
        );
        assert!(
            components["sensor_automatic_temperature_threshold"]["value_template"]
                .as_str()
                .unwrap()
                .contains("if reading is number else none")
        );
        if let Some(path) = std::env::var_os("GAFCTL_DISCOVERY_FIXTURE") {
            std::fs::write(path, serde_json::to_vec_pretty(&generated).unwrap()).unwrap();
        }
    }

    #[test]
    fn ble_discovery_does_not_override_http_ownership() {
        let mut device = DeviceDescriptor::configured_ble();
        assert_eq!(configs(std::slice::from_ref(&device)).count(), 0);
        device.state_source = EntitySource::Mqtt;
        assert_eq!(configs(std::slice::from_ref(&device)).count(), 0);
        device.command_source = EntitySource::Mqtt;
        assert_eq!(configs(std::slice::from_ref(&device)).count(), 1);
    }

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
}
