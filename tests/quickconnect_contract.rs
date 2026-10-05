use serde_json::Value;

const MANIFEST: &str = include_str!("../fixtures/quickconnect/manifest.json");
const LOGIN_REQUEST: &str = include_str!("../fixtures/quickconnect/login_request.json");
const LOGIN_SUCCESS: &str = include_str!("../fixtures/quickconnect/login_success.json");
const LOGIN_REJECTED: &str = include_str!("../fixtures/quickconnect/login_rejected_401.json");
const DEVICE_LIST_ARRAY: &str = include_str!("../fixtures/quickconnect/device_list_array.json");
const DEVICE_LIST_OBJECT: &str = include_str!("../fixtures/quickconnect/device_list_object.json");
const DEVICE_DETAIL: &str = include_str!("../fixtures/quickconnect/device_detail.json");
const MODE_REQUEST: &str = include_str!("../fixtures/quickconnect/mode_request.json");
const TARGETS_REQUEST: &str = include_str!("../fixtures/quickconnect/targets_request.json");
const TIMER_REQUEST: &str = include_str!("../fixtures/quickconnect/timer_request.json");
const SETTINGS_REJECTED: &str = include_str!("../fixtures/quickconnect/settings_rejected_417.json");
const MALFORMED_DEVICE_LIST: &str =
    include_str!("../fixtures/quickconnect/malformed_device_list.json");
const MISSING_DEVICE_SETTINGS: &str =
    include_str!("../fixtures/quickconnect/missing_device_settings.json");

const REQUIRED_FIXTURES: &[&str] = &[
    "device_detail.json",
    "device_list_array.json",
    "device_list_object.json",
    "login_rejected_401.json",
    "login_request.json",
    "login_success.json",
    "malformed_device_list.json",
    "missing_device_settings.json",
    "mode_request.json",
    "settings_rejected_417.json",
    "targets_request.json",
    "timer_request.json",
];

fn json(source: &str) -> Value {
    serde_json::from_str(source).expect("contract fixture must be valid JSON")
}

#[test]
fn manifest_covers_every_required_synthetic_contract_fixture() {
    let manifest = json(MANIFEST);
    let actual = manifest["fixtures"]
        .as_array()
        .expect("fixture manifest must list fixture files")
        .iter()
        .map(|fixture| {
            fixture["file"]
                .as_str()
                .expect("fixture entry must name its file")
        })
        .collect::<Vec<_>>();

    assert_eq!(actual, REQUIRED_FIXTURES);
    assert_eq!(manifest["evidence"], "synthetic");
}

#[test]
fn reference_revision_and_license_are_pinned_in_the_manifest() {
    let manifest = json(MANIFEST);

    assert_eq!(
        manifest["reference"]["revision"],
        "336adfd8d8cc0a936b4585bd20301f74d585554c"
    );
    assert_eq!(manifest["reference"]["integration_version"], "1.1.0");
    assert_eq!(manifest["license"]["spdx_id"], "MIT");
    assert_eq!(
        manifest["license"]["copyright"],
        "Copyright (c) 2026 hitchin999"
    );
}

#[test]
fn synthetic_detail_keeps_live_readings_separate_from_targets() {
    let detail = json(DEVICE_DETAIL);
    let device = &detail["body"]["responseData"];

    assert_eq!(device["deviceConfig"]["setTemperature"], 78);
    assert_eq!(device["deviceConfig"]["setHumidity"], 44);
    assert_eq!(device["deviceSettings"]["setTemperature"], 105);
    assert_eq!(device["deviceSettings"]["setHumidity"], 40);
    assert_eq!(device["deviceSettings"]["timerValue"], 60);
}

#[test]
fn inventory_fixtures_cover_both_reference_envelopes() {
    let array = json(DEVICE_LIST_ARRAY);
    let object = json(DEVICE_LIST_OBJECT);

    assert!(array["body"]["responseData"].is_array());
    assert!(object["body"]["responseData"]["devices"].is_array());
}

fn keys(value: &Value) -> Vec<&str> {
    value
        .as_object()
        .expect("request body must be an object")
        .keys()
        .map(String::as_str)
        .collect()
}

#[test]
fn request_fixtures_match_each_exact_reference_write_shape() {
    for (name, source, expected) in [
        (
            "mode",
            MODE_REQUEST,
            serde_json::json!({"automaticMode":true,"timerMode":false,"fanMode":false,"desiredTemp":105,"desiredHumidity":40,"timerValue":60}),
        ),
        (
            "targets",
            TARGETS_REQUEST,
            serde_json::json!({"automaticMode":true,"desiredTemp":105,"desiredHumidity":40}),
        ),
        (
            "timer",
            TIMER_REQUEST,
            serde_json::json!({"timerMode":false,"timerValue":60}),
        ),
    ] {
        assert_eq!(json(source)["body"], expected, "{name}");
    }
}

#[test]
fn malformed_inventory_and_missing_settings_are_not_successful_empty_state() {
    let malformed = json(MALFORMED_DEVICE_LIST);
    let settings_missing = json(MISSING_DEVICE_SETTINGS);
    let empty_inventory = json(r#"{"responseData":[]}"#);

    assert!(malformed["body"]["responseData"].is_string());
    assert!(
        settings_missing["body"]["responseData"]
            .get("deviceSettings")
            .is_none()
    );
    assert_eq!(
        empty_inventory["responseData"].as_array().map(Vec::len),
        Some(0)
    );
}

#[test]
fn reference_error_fixtures_have_synthetic_credentials_and_error_bodies() {
    let login_request = json(LOGIN_REQUEST);
    let login_success = json(LOGIN_SUCCESS);
    let login_rejected = json(LOGIN_REJECTED);
    let settings_rejected = json(SETTINGS_REJECTED);
    let body = &login_request["body"];

    assert_eq!(
        keys(body),
        ["password", "userName", "userPoolId", "userRole"]
    );
    assert_eq!(body["userName"], "synthetic@example.invalid");
    assert_eq!(body["password"], "c3ludGhldGljLXBhc3N3b3JkLWRvLW5vdC11c2U=");
    assert_eq!(
        login_success["body"]["responseData"]["idToken"],
        "SYNTHETIC_TOKEN_DO_NOT_USE"
    );
    assert_eq!(login_rejected["http_status"].as_u64(), Some(401));
    assert_eq!(settings_rejected["http_status"].as_u64(), Some(417));
    assert_eq!(settings_rejected["body"]["statusCode"].as_u64(), Some(4444));
}

#[test]
fn every_wire_fixture_is_marked_synthetic() {
    assert!(
        [
            LOGIN_REQUEST,
            LOGIN_SUCCESS,
            LOGIN_REJECTED,
            DEVICE_LIST_ARRAY,
            DEVICE_LIST_OBJECT,
            DEVICE_DETAIL,
            MODE_REQUEST,
            TARGETS_REQUEST,
            TIMER_REQUEST,
            SETTINGS_REJECTED,
            MALFORMED_DEVICE_LIST,
            MISSING_DEVICE_SETTINGS,
        ]
        .iter()
        .all(|fixture| json(fixture)["evidence"] == "synthetic")
    );
}
