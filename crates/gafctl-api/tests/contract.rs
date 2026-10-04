use gafctl_api::{CommandId, ControlStatus, DeviceControlV2Request, DeviceStateV2Response};
use serde_json::json;

#[test]
fn cloud_single_target_commands_accept_only_whole_in_range_values() {
    for (kind, field, minimum, maximum) in [
        (
            "quick_connect_automatic_temperature",
            "temperature_f",
            90,
            120,
        ),
        (
            "quick_connect_automatic_humidity",
            "humidity_percent",
            30,
            80,
        ),
    ] {
        for value in [minimum, maximum] {
            let mut command = json!({"kind":kind});
            command[field] = json!(value);
            assert!(serde_json::from_value::<gafctl_api::DeviceCommand>(command).is_ok());
        }
        for value in [
            json!(minimum - 1),
            json!(maximum + 1),
            json!(1.5),
            json!(true),
            json!(null),
        ] {
            let mut command = json!({"kind":kind});
            command[field] = value;
            assert!(serde_json::from_value::<gafctl_api::DeviceCommand>(command).is_err());
        }
    }
}

#[test]
fn adjustable_ble_controls_accept_original_app_ranges_and_reject_other_values() {
    for (kind, field, minimum, maximum) in [
        ("legacy_automatic_temperature", "temperature_f", 90, 120),
        ("legacy_automatic_humidity", "humidity_percent", 30, 80),
        ("legacy_timer", "minutes", 0, 360),
    ] {
        for value in [minimum, maximum] {
            let mut command = json!({"kind":kind});
            command[field] = json!(value);
            assert!(
                serde_json::from_value::<gafctl_api::DeviceCommand>(command).is_ok(),
                "{kind} {value}"
            );
        }
        for value in [
            json!(-1),
            json!(maximum + 1),
            json!(90.5),
            json!(true),
            json!(null),
        ] {
            let mut command = json!({"kind":kind});
            command[field] = value;
            assert!(serde_json::from_value::<gafctl_api::DeviceCommand>(command).is_err());
        }
        if minimum > 0 {
            let mut command = json!({"kind":kind});
            command[field] = json!(minimum - 1);
            assert!(serde_json::from_value::<gafctl_api::DeviceCommand>(command).is_err());
        }
    }
}

#[test]
fn request_ids_round_trip_and_commands_keep_the_v2_wire_shape() {
    let value = json!({"request_id":"cli-request_1", "issued_at_unix_ms":2000,
        "command":{"kind":"legacy_preset","preset":"timer_clear"}});
    let request: DeviceControlV2Request = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(request.request_id.as_str(), "cli-request_1");
    assert_eq!(serde_json::to_value(request).unwrap(), value);
    for invalid in ["", "../device", "space here", "é"] {
        assert!(invalid.parse::<CommandId>().is_err());
        assert!(serde_json::from_value::<CommandId>(json!(invalid)).is_err());
    }
    assert!("a".repeat(65).parse::<CommandId>().is_err());
}

#[test]
fn request_ids_preserve_borrowed_and_owned_deserializer_contracts() {
    use serde::{
        Deserialize,
        de::value::{BorrowedStrDeserializer, Error, StringDeserializer},
    };

    for value in ["request_1", "", "../device", "space here", "é"] {
        let borrowed = CommandId::deserialize(BorrowedStrDeserializer::<Error>::new(value));
        let owned = CommandId::deserialize(StringDeserializer::<Error>::new(value.to_owned()));
        assert_eq!(
            borrowed.as_ref().map(CommandId::as_str),
            owned.as_ref().map(CommandId::as_str)
        );
        if value == "request_1" {
            assert_eq!(borrowed.unwrap().as_str(), value);
            assert_eq!(owned.unwrap().as_str(), value);
        } else {
            assert_eq!(
                borrowed.unwrap_err().to_string(),
                "invalid control request ID"
            );
            assert_eq!(owned.unwrap_err().to_string(), "invalid control request ID");
        }
    }
}

#[test]
fn control_presets_format_exact_serde_wire_names() {
    use gafctl_api::ControlPreset;

    for (preset, wire) in [
        (
            ControlPreset::Automatic105F30Percent,
            "automatic105_f30_percent",
        ),
        (
            ControlPreset::Automatic105_1F30_1Percent,
            "automatic105_1_f30_1_percent",
        ),
        (ControlPreset::TimerClear, "timer_clear"),
        (ControlPreset::TimerOneMinute, "timer_one_minute"),
    ] {
        assert_eq!(<&'static str>::from(preset), wire);
        assert_eq!(serde_json::to_value(preset).unwrap(), wire);
        assert_eq!(
            serde_json::from_value::<ControlPreset>(json!(wire)).unwrap(),
            preset
        );
    }
}

#[test]
fn control_status_formats_exact_wire_names_through_borrowed_strings() {
    for (status, wire) in [
        (ControlStatus::Confirmed, "confirmed"),
        (ControlStatus::Unconfirmed, "unconfirmed"),
        (ControlStatus::Rejected, "rejected"),
        (ControlStatus::SubmittedUnconfirmed, "submitted_unconfirmed"),
        (ControlStatus::ReadbackMismatch, "readback_mismatch"),
        (ControlStatus::ReadbackUnavailable, "readback_unavailable"),
        (ControlStatus::UnsupportedCommand, "unsupported_command"),
        (ControlStatus::StaleRequest, "stale_request"),
        (ControlStatus::RequestIdReused, "request_id_reused"),
        (ControlStatus::UnknownDevice, "unknown_device"),
        (ControlStatus::DeviceUnavailable, "device_unavailable"),
        (ControlStatus::BackendUnavailable, "backend_unavailable"),
        (ControlStatus::Busy, "busy"),
        (ControlStatus::ControlFailed, "control_failed"),
        (ControlStatus::InvalidRequestId, "invalid_request_id"),
    ] {
        let borrowed: &str = status.as_ref();
        assert_eq!(borrowed, wire);
        assert_eq!(status.as_str(), wire);
        assert_eq!(status.to_string(), wire);
        assert_eq!(ControlStatus::from(wire), status);
        assert_eq!(serde_json::to_value(&status).unwrap(), wire);
    }
}

#[test]
fn unknown_control_status_keeps_owned_storage_and_borrows_it_for_display() {
    for wire in ["CONFIRMED", "Confirmed", "future_backend_outcome"] {
        let input = wire.to_owned();
        let storage = input.as_ptr();
        let status = ControlStatus::from(input);
        let ControlStatus::Unknown(value) = &status else {
            unreachable!("only exact wire names are recognized");
        };
        assert_eq!(value.as_ptr(), storage);
        let borrowed: &str = status.as_ref();
        assert_eq!(borrowed.as_ptr(), storage);
        assert_eq!(borrowed, wire);
        assert_eq!(status.as_str().as_ptr(), storage);
        assert_eq!(status.to_string(), wire);
        assert!(!status.is_confirmed());
        assert_eq!(serde_json::to_value(&status).unwrap(), wire);
    }
}

#[test]
fn unknown_control_status_is_preserved_without_becoming_confirmed() {
    let status: ControlStatus = serde_json::from_value(json!("new_backend_outcome")).unwrap();
    assert!(!status.is_confirmed());
    assert_eq!(serde_json::to_value(status).unwrap(), "new_backend_outcome");
    let confirmed: ControlStatus = serde_json::from_value(json!("confirmed")).unwrap();
    assert!(confirmed.is_confirmed());
}

#[test]
fn unavailable_state_requires_explicit_nullable_fields_but_accepts_additive_fields() {
    let value = json!({"id":"configured", "backend":"legacy_ble", "available":false,
        "inventory_status":"unknown", "last_error":null,"state":null,"future_field":123});
    let state: DeviceStateV2Response = serde_json::from_value(value.clone()).unwrap();
    assert!(state.validate().is_ok());
    for field in [
        "id",
        "backend",
        "available",
        "inventory_status",
        "last_error",
        "state",
    ] {
        let mut incomplete = value.clone();
        incomplete.as_object_mut().unwrap().remove(field);
        assert!(
            serde_json::from_value::<DeviceStateV2Response>(incomplete).is_err(),
            "missing {field}"
        );
    }
    let mut inconsistent = value;
    inconsistent["available"] = json!(true);
    let state: DeviceStateV2Response = serde_json::from_value(inconsistent).unwrap();
    assert!(state.validate().is_err());
}

#[test]
fn outbound_control_fields_are_strict() {
    let mut request = json!({"request_id":"cli-request_1","issued_at_unix_ms":2000,
        "command":{"kind":"legacy_preset","preset":"timer_clear"}});
    request["command"]["surprise"] = json!(true);
    assert!(serde_json::from_value::<DeviceControlV2Request>(request).is_err());
}

#[test]
fn nullable_snapshot_fields_are_required_and_round_trip_for_both_backends() {
    let legacy = json!({"temperature_f":null,"humidity_percent":null,"estimated_running":null,"diagnostics":null,
        "settings":{"backend":"legacy_ble","mode":null,"controller_fan_on":null,
            "automatic_temperature_tenths_f":null,"automatic_humidity_tenths_percent":null,
            "timer_remaining_minutes":null,"timer_original_minutes":null},
        "provenance":{"backend":"legacy_ble","fetched_at_unix_ms":null,"observed_at_unix_ms":null}});
    let cloud = json!({"temperature_f":101.5,"humidity_percent":null,"estimated_running":null,
        "diagnostics":{"firmware_version":null,"signal_strength_raw":null,"verified_raw":null,"ota_in_progress":null},
        "settings":{"backend":"quick_connect","mode":"unknown","automatic_temperature_f":null,
            "automatic_humidity_percent":null,"timer_duration_minutes":null,"humidity_monitor":null},
        "provenance":{"backend":"quick_connect","fetched_at_unix_ms":123,"observed_at_unix_ms":null}});
    for value in [legacy, cloud] {
        let parsed: gafctl_api::DeviceState = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(serde_json::to_value(parsed).unwrap(), value);
        for parent in ["", "settings", "provenance", "diagnostics"] {
            let object = if parent.is_empty() {
                &value
            } else {
                &value[parent]
            };
            let Some(fields) = object.as_object() else {
                continue;
            };
            for field in fields.keys() {
                let mut incomplete = value.clone();
                let object = if parent.is_empty() {
                    &mut incomplete
                } else {
                    &mut incomplete[parent]
                };
                object.as_object_mut().unwrap().remove(field);
                assert!(
                    serde_json::from_value::<gafctl_api::DeviceState>(incomplete).is_err(),
                    "missing {parent}.{field}"
                );
            }
        }
    }
}

#[test]
fn available_state_rejects_inconsistent_backend_and_measurements() {
    let value = json!({"id":"configured","backend":"legacy_ble","available":true,
        "inventory_status":"present","last_error":null,"state":{
            "temperature_f":101.5,"humidity_percent":40.0,"estimated_running":null,"diagnostics":null,
            "settings":{"backend":"legacy_ble","mode":"automatic","controller_fan_on":false,
                "automatic_temperature_tenths_f":1050,"automatic_humidity_tenths_percent":400,
                "timer_remaining_minutes":0,"timer_original_minutes":0},
            "provenance":{"backend":"legacy_ble","fetched_at_unix_ms":123,"observed_at_unix_ms":120}}});
    let valid: DeviceStateV2Response = serde_json::from_value(value.clone()).unwrap();
    assert!(valid.validate().is_ok());
    for invalid in [
        {
            let mut value = value.clone();
            value["state"]["provenance"]["backend"] = json!("quick_connect");
            value
        },
        {
            let mut value = value.clone();
            value["backend"] = json!("quick_connect");
            value
        },
        {
            let mut value = value.clone();
            value["state"]["humidity_percent"] = json!(101);
            value
        },
    ] {
        let parsed: DeviceStateV2Response = serde_json::from_value(invalid).unwrap();
        assert!(parsed.validate().is_err());
    }
}
