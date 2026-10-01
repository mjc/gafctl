use std::fmt;

use serde_json::Value;

use crate::ClientError;

/// One inventory entry with a private provider identifier.
pub struct InventoryDevice {
    provider_id: String,
    name: Option<String>,
}

impl InventoryDevice {
    /// Return the provider identifier for authenticated API lookups.
    pub fn provider_id(&self) -> &str {
        &self.provider_id
    }

    /// Return the provider's optional display name.
    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }
}

impl fmt::Debug for InventoryDevice {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("InventoryDevice")
            .field("provider_id", &"[redacted]")
            .field("name", &self.name)
            .finish()
    }
}

/// Decoded mode flags; malformed or incomplete flags remain explicit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeviceModeStatus {
    Off,
    Automatic,
    Timer,
    Manual,
    Unknown,
    Conflicting,
}

/// QuickConnect settings decoded without filling missing fields with defaults.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QuickConnectSettings {
    pub mode: DeviceModeStatus,
    pub automatic_temperature_f: Option<u16>,
    pub automatic_humidity_percent: Option<u16>,
    pub timer_duration_minutes: Option<u16>,
    pub humidity_monitor: Option<bool>,
}

/// Diagnostic values whose units or semantics are not yet established.
#[derive(Clone, Debug, PartialEq)]
pub struct QuickConnectDiagnostics {
    pub firmware_version: Option<String>,
    pub signal_strength_raw: Option<String>,
    pub verified_raw: Option<String>,
    pub ota_in_progress: Option<bool>,
}

/// Normalized readings, settings, and provenance from one detail response.
#[derive(Clone, Debug, PartialEq)]
pub struct QuickConnectDeviceState {
    pub temperature_f: Option<f64>,
    pub humidity_percent: Option<f64>,
    pub settings: QuickConnectSettings,
    pub estimated_running: Option<bool>,
    pub diagnostics: QuickConnectDiagnostics,
    pub fetched_at_unix_ms: Option<u64>,
    pub observed_at_unix_ms: Option<u64>,
}

/// Inventory registration and its independent detail result.
pub struct QuickConnectDevicePoll {
    pub inventory: InventoryDevice,
    pub detail: Result<QuickConnectDeviceState, ClientError>,
    pub fetched_at_unix_ms: Option<u64>,
}

impl fmt::Debug for QuickConnectDevicePoll {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("QuickConnectDevicePoll")
            .field("inventory", &self.inventory)
            .field("detail", &self.detail.as_ref().map(|_| "[decoded]"))
            .field("fetched_at_unix_ms", &self.fetched_at_unix_ms)
            .finish()
    }
}

pub(crate) fn parse_inventory(payload: Value) -> Result<Vec<InventoryDevice>, ClientError> {
    let response_data = payload
        .get("responseData")
        .ok_or(ClientError::InvalidEnvelope)?;
    let devices = response_data
        .as_array()
        .or_else(|| response_data.get("devices").and_then(Value::as_array))
        .ok_or(ClientError::InvalidEnvelope)?;
    devices.iter().map(parse_inventory_device).collect()
}

fn parse_inventory_device(device: &Value) -> Result<InventoryDevice, ClientError> {
    let fields = device.as_object().ok_or(ClientError::InvalidEnvelope)?;
    let identifiers = ["deviceId", "device_id", "id"]
        .into_iter()
        .filter_map(|field| fields.get(field))
        .map(parse_provider_id)
        .collect::<Result<Vec<_>, _>>()?;
    let [provider_id, rest @ ..] = identifiers.as_slice() else {
        return Err(ClientError::InvalidEnvelope);
    };
    if rest.iter().any(|identifier| identifier != provider_id) {
        return Err(ClientError::InvalidEnvelope);
    }
    Ok(InventoryDevice {
        provider_id: provider_id.clone(),
        name: fields
            .get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_owned),
    })
}

fn parse_provider_id(value: &Value) -> Result<String, ClientError> {
    value
        .as_str()
        .filter(|identifier| !identifier.trim().is_empty())
        .map(str::to_owned)
        .or_else(|| value.as_u64().map(|identifier| identifier.to_string()))
        .ok_or(ClientError::InvalidEnvelope)
}

pub(crate) fn parse_device_state(
    payload: Value,
    fetched_at_unix_ms: Option<u64>,
) -> Result<QuickConnectDeviceState, ClientError> {
    let response_data = payload
        .get("responseData")
        .and_then(Value::as_object)
        .ok_or(ClientError::InvalidEnvelope)?;
    let config = response_data.get("deviceConfig").and_then(Value::as_object);
    let settings = response_data
        .get("deviceSettings")
        .and_then(Value::as_object);
    let temperature_f = config
        .and_then(|config| config.get("setTemperature"))
        .and_then(finite_number);
    let humidity_percent = config
        .and_then(|config| config.get("setHumidity"))
        .and_then(finite_number)
        .filter(|humidity| (0.0..=100.0).contains(humidity));
    let settings = QuickConnectSettings {
        mode: decode_mode(settings),
        automatic_temperature_f: settings
            .and_then(|settings| settings.get("setTemperature"))
            .and_then(integer),
        automatic_humidity_percent: settings
            .and_then(|settings| settings.get("setHumidity"))
            .and_then(integer)
            .filter(|humidity| *humidity <= 100),
        timer_duration_minutes: settings
            .and_then(|settings| settings.get("timerValue"))
            .and_then(integer),
        humidity_monitor: settings
            .and_then(|settings| settings.get("humidityMonitor"))
            .and_then(Value::as_bool),
    };
    let estimated_running = estimate_running(&settings, temperature_f, humidity_percent);
    let diagnostics = QuickConnectDiagnostics {
        firmware_version: response_data
            .get("firmwareVersion")
            .and_then(Value::as_str)
            .map(str::to_owned),
        signal_strength_raw: diagnostic_string(response_data.get("signalStrength")),
        verified_raw: diagnostic_string(response_data.get("isVerified")),
        ota_in_progress: response_data.get("otaInProgress").and_then(Value::as_bool),
    };
    Ok(QuickConnectDeviceState {
        temperature_f,
        humidity_percent,
        settings,
        estimated_running,
        diagnostics,
        fetched_at_unix_ms,
        observed_at_unix_ms: None,
    })
}

fn decode_mode(settings: Option<&serde_json::Map<String, Value>>) -> DeviceModeStatus {
    let Some(settings) = settings else {
        return DeviceModeStatus::Unknown;
    };
    let flags = ["automaticMode", "timerMode", "fanMode"]
        .map(|field| settings.get(field).and_then(Value::as_bool));
    let enabled = flags.iter().filter(|flag| **flag == Some(true)).count();
    if enabled > 1 {
        return DeviceModeStatus::Conflicting;
    }
    match flags {
        [Some(false), Some(false), Some(false)] => DeviceModeStatus::Off,
        [Some(true), Some(false), Some(false)] => DeviceModeStatus::Automatic,
        [Some(false), Some(true), Some(false)] => DeviceModeStatus::Timer,
        [Some(false), Some(false), Some(true)] => DeviceModeStatus::Manual,
        _ => DeviceModeStatus::Unknown,
    }
}

fn estimate_running(
    settings: &QuickConnectSettings,
    temperature_f: Option<f64>,
    humidity_percent: Option<f64>,
) -> Option<bool> {
    match settings.mode {
        DeviceModeStatus::Manual | DeviceModeStatus::Timer => Some(true),
        DeviceModeStatus::Off => Some(false),
        DeviceModeStatus::Unknown | DeviceModeStatus::Conflicting => None,
        DeviceModeStatus::Automatic => {
            let temperature_trigger = temperature_f
                .zip(settings.automatic_temperature_f)
                .map(|(current, target)| current >= f64::from(target));
            let humidity_trigger = match settings.humidity_monitor {
                Some(false) => Some(false),
                Some(true) => humidity_percent
                    .zip(settings.automatic_humidity_percent)
                    .map(|(current, target)| current >= f64::from(target)),
                None => None,
            };
            if temperature_trigger == Some(true) || humidity_trigger == Some(true) {
                Some(true)
            } else if temperature_trigger == Some(false) && humidity_trigger == Some(false) {
                Some(false)
            } else {
                None
            }
        }
    }
}

fn finite_number(value: &Value) -> Option<f64> {
    value.as_f64().filter(|number| number.is_finite())
}

fn integer(value: &Value) -> Option<u16> {
    value.as_u64().and_then(|number| u16::try_from(number).ok())
}

fn diagnostic_string(value: Option<&Value>) -> Option<String> {
    value.and_then(|value| match value {
        Value::String(value) => Some(value.clone()),
        Value::Bool(value) => Some(value.to_string()),
        Value::Number(value) => Some(value.to_string()),
        Value::Null | Value::Array(_) | Value::Object(_) => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn mode(
        automatic: Option<bool>,
        timer: Option<bool>,
        manual: Option<bool>,
    ) -> DeviceModeStatus {
        let settings = json!({
            "automaticMode": automatic,
            "timerMode": timer,
            "fanMode": manual
        });
        decode_mode(settings.as_object())
    }

    #[test]
    fn every_complete_mode_tuple_is_decoded_without_truthy_precedence() {
        let cases = [
            ([false, false, false], DeviceModeStatus::Off),
            ([true, false, false], DeviceModeStatus::Automatic),
            ([false, true, false], DeviceModeStatus::Timer),
            ([false, false, true], DeviceModeStatus::Manual),
            ([true, true, false], DeviceModeStatus::Conflicting),
            ([true, false, true], DeviceModeStatus::Conflicting),
            ([false, true, true], DeviceModeStatus::Conflicting),
            ([true, true, true], DeviceModeStatus::Conflicting),
        ];
        let decoded = cases.map(|([automatic, timer, manual], _)| {
            mode(Some(automatic), Some(timer), Some(manual))
        });
        let expected = cases.map(|(_, expected)| expected);

        assert_eq!(decoded, expected);
        assert_eq!(
            mode(Some(false), None, Some(false)),
            DeviceModeStatus::Unknown
        );
        assert_eq!(
            mode(Some(true), Some(false), None),
            DeviceModeStatus::Unknown
        );
    }

    #[test]
    fn missing_and_invalid_values_remain_unknown_and_diagnostics_are_uninterpreted() {
        let state = parse_device_state(
            json!({
                "responseData": {
                    "deviceConfig": {
                        "setTemperature": null,
                        "setHumidity": 101
                    },
                    "deviceSettings": {
                        "automaticMode": false,
                        "timerMode": false,
                        "fanMode": false,
                        "setTemperature": 105.5,
                        "setHumidity": 40,
                        "timerValue": -1,
                        "humidityMonitor": "false"
                    },
                    "signalStrength": "synthetic-units-unknown",
                    "isVerified": "synthetic-semantics-unknown"
                }
            }),
            Some(123),
        )
        .unwrap();

        assert_eq!(state.temperature_f, None);
        assert_eq!(state.humidity_percent, None);
        assert_eq!(state.settings.mode, DeviceModeStatus::Off);
        assert_eq!(state.settings.automatic_temperature_f, None);
        assert_eq!(state.settings.automatic_humidity_percent, Some(40));
        assert_eq!(state.settings.timer_duration_minutes, None);
        assert_eq!(state.settings.humidity_monitor, None);
        assert_eq!(state.estimated_running, Some(false));
        assert_eq!(state.fetched_at_unix_ms, Some(123));
        assert_eq!(state.observed_at_unix_ms, None);
        assert_eq!(
            state.diagnostics.signal_strength_raw.as_deref(),
            Some("synthetic-units-unknown")
        );
        assert_eq!(
            state.diagnostics.verified_raw.as_deref(),
            Some("synthetic-semantics-unknown")
        );
    }

    #[test]
    fn running_estimate_uses_three_valued_inputs() {
        let settings = QuickConnectSettings {
            mode: DeviceModeStatus::Automatic,
            automatic_temperature_f: Some(100),
            automatic_humidity_percent: Some(40),
            timer_duration_minutes: None,
            humidity_monitor: Some(true),
        };

        assert_eq!(estimate_running(&settings, Some(101.0), None), Some(true));
        assert_eq!(
            estimate_running(&settings, Some(99.0), Some(39.0)),
            Some(false)
        );
        assert_eq!(estimate_running(&settings, Some(99.0), None), None);
        assert_eq!(estimate_running(&settings, None, Some(41.0)), Some(true));
    }

    #[test]
    fn impossible_humidity_target_keeps_automatic_running_estimate_unknown() {
        let state = parse_device_state(
            json!({
                "responseData": {
                    "deviceConfig": {"setTemperature": 90, "setHumidity": 90},
                    "deviceSettings": {
                        "automaticMode": true,
                        "timerMode": false,
                        "fanMode": false,
                        "setTemperature": 100,
                        "setHumidity": 101,
                        "humidityMonitor": true
                    }
                }
            }),
            Some(1),
        )
        .unwrap();

        assert_eq!(state.settings.automatic_humidity_percent, None);
        assert_eq!(state.estimated_running, None);
    }

    #[test]
    fn inventory_ids_preserve_zero_and_reject_missing_or_conflicting_aliases() {
        let inventory = parse_inventory(json!({
            "responseData": {"devices": [{"deviceId": 0, "id": "0", "name": "Zero"}]}
        }))
        .unwrap();
        assert_eq!(inventory[0].provider_id(), "0");
        assert_eq!(inventory[0].name(), Some("Zero"));
        assert_eq!(
            parse_inventory(json!({"responseData": [{"name": "Missing id"}]})).unwrap_err(),
            ClientError::InvalidEnvelope
        );
        assert_eq!(
            parse_inventory(json!({"responseData": [{"deviceId": "one", "id": "two"}]}))
                .unwrap_err(),
            ClientError::InvalidEnvelope
        );
    }
}
