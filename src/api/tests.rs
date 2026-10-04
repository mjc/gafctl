use super::*;
use crate::backend::DeviceRegistry;
use crate::test_support::identity_store_fixture;
use axum::{body::Body, http::Request};
use gafctl_api::unix_millis;
use http_body_util::BodyExt;
use std::time::SystemTime;
use tower::ServiceExt;
const DEVICE_ID: &str = "configured";
#[tokio::test]
async fn routes_report_health_v2_capabilities_and_unknown_state() {
    let state =
        DeviceService::with_ble_device("private-peripheral-id".to_owned(), DeviceRegistry::new());
    let app = router(state);

    let health = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(health.status(), axum::http::StatusCode::OK);

    let devices = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v2/devices")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = devices.into_body().collect().await.unwrap().to_bytes();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["devices"].as_array().unwrap().len(), 1);
    assert_eq!(body["devices"][0]["id"], DEVICE_ID);
    assert_eq!(body["devices"][0]["backend"], "legacy_ble");
    assert_eq!(body["devices"][0]["capabilities"]["read_state"], true);
    assert!(!body.to_string().contains("private-peripheral-id"));

    let state = app
        .oneshot(
            Request::builder()
                .uri("/api/v2/devices/configured/state")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = state.into_body().collect().await.unwrap().to_bytes();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["available"], false);
    assert!(body["state"].is_null());
}

#[tokio::test]
async fn v1_http_routes_are_absent() {
    let response = router(DeviceService::with_registry(DeviceRegistry::new()))
        .oneshot(
            Request::builder()
                .uri("/api/v1/devices")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn startup_without_ble_has_empty_inventory_and_no_configured_state() {
    let app = router(DeviceService::with_registry(DeviceRegistry::new()));
    let inventory = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v2/devices")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = inventory.into_body().collect().await.unwrap().to_bytes();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["devices"], serde_json::json!([]));

    let state = app
        .oneshot(
            Request::builder()
                .uri("/api/v2/devices/configured/state")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(state.status(), StatusCode::NOT_FOUND);

    let response = router(DeviceService::with_registry(DeviceRegistry::new()))
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v2/devices/configured/control")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "request_id": "no-ble-device",
                        "issued_at_unix_ms": unix_millis(SystemTime::now()).unwrap(),
                        "command": {"kind": "legacy_preset", "preset": "timer_clear"}
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn cloud_only_and_mixed_startup_never_alias_cloud_devices_to_configured() {
    let (_directory, path) = identity_store_fixture();
    let mut cloud_registry = DeviceRegistry::load(&path).unwrap();
    let cloud_ids = cloud_registry
        .reconcile_quickconnect(
            "account-private",
            &[
                crate::backend::CloudDeviceInput::new(
                    "provider-private-one".to_owned(),
                    "Attic cloud fan".to_owned(),
                ),
                crate::backend::CloudDeviceInput::new(
                    "provider-private-two".to_owned(),
                    "Guest cloud fan".to_owned(),
                ),
            ],
        )
        .unwrap();
    let cloud_only = router(DeviceService::with_registry(cloud_registry));
    let inventory = cloud_only
        .oneshot(
            Request::builder()
                .uri("/api/v2/devices")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = inventory.into_body().collect().await.unwrap().to_bytes();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["devices"].as_array().unwrap().len(), 2);
    assert!(!body.to_string().contains("provider-private"));
    assert_ne!(cloud_ids[0].as_str(), DEVICE_ID);
    assert_ne!(cloud_ids[1].as_str(), DEVICE_ID);

    let mut mixed_registry = DeviceRegistry::load(&path).unwrap();
    mixed_registry
        .reconcile_quickconnect(
            "account-private",
            &[
                crate::backend::CloudDeviceInput::new(
                    "provider-private-two".to_owned(),
                    "Guest cloud fan".to_owned(),
                ),
                crate::backend::CloudDeviceInput::new(
                    "provider-private-one".to_owned(),
                    "Attic cloud fan".to_owned(),
                ),
            ],
        )
        .unwrap();
    let mixed = router(DeviceService::with_ble_device(
        "private-ble-id".to_owned(),
        mixed_registry,
    ));
    let inventory = mixed
        .oneshot(
            Request::builder()
                .uri("/api/v2/devices")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = inventory.into_body().collect().await.unwrap().to_bytes();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["devices"].as_array().unwrap().len(), 3);
    assert!(!body.to_string().contains("provider-private"));
}

#[tokio::test]
async fn v2_discovery_lists_backends_and_rejects_cloud_command_for_ble() {
    let (_directory, path) = identity_store_fixture();
    let mut registry = DeviceRegistry::load(&path).unwrap();
    let cloud_ids = registry
        .reconcile_quickconnect(
            "synthetic-account",
            &[
                crate::backend::CloudDeviceInput::new(
                    "synthetic-provider-a".to_owned(),
                    "Attic fan".to_owned(),
                ),
                crate::backend::CloudDeviceInput::new(
                    "synthetic-provider-b".to_owned(),
                    "Guest fan".to_owned(),
                ),
            ],
        )
        .unwrap();
    registry.set_quickconnect_writes_enabled(true);
    let app = router(DeviceService::with_ble_device(
        "synthetic-ble-id".to_owned(),
        registry,
    ));

    let inventory = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v2/devices")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(inventory.status(), StatusCode::OK);
    let bytes = inventory.into_body().collect().await.unwrap().to_bytes();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["devices"].as_array().unwrap().len(), 3);
    let devices = body["devices"].as_array().unwrap();
    let ble = devices
        .iter()
        .find(|device| device["backend"] == "legacy_ble")
        .unwrap();
    assert_eq!(ble["id"], "configured");
    let cloud = devices
        .iter()
        .filter(|device| device["backend"] == "quick_connect")
        .collect::<Vec<_>>();
    assert_eq!(cloud.len(), 2);
    assert!(
        cloud
            .iter()
            .all(|device| device["capabilities"]["read_state"] == true)
    );
    assert!(!body.to_string().contains("synthetic-provider"));
    assert!(!body.to_string().contains("synthetic-account"));

    let issued_at_unix_ms = unix_millis(SystemTime::now()).unwrap();
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v2/devices/configured/control")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "request_id": "reject-cloud-on-ble",
                        "issued_at_unix_ms": issued_at_unix_ms,
                        "command": {
                            "kind": "quick_connect_targets",
                            "temperature_f": 110,
                            "humidity_percent": 40
                        }
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let outcome: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(outcome["request_id"], "reject-cloud-on-ble");
    assert_eq!(outcome["status"], "unsupported_command");
    assert_eq!(cloud_ids.len(), 2);
}

#[tokio::test]
async fn v2_controls_reject_unknown_fields_and_unknown_device_ids() {
    let app = router(DeviceService::with_ble_device(
        "synthetic-ble-id".to_owned(),
        DeviceRegistry::new(),
    ));
    let unknown_field = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v2/devices/configured/control")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "request_id": "strict-request",
                        "issued_at_unix_ms": unix_millis(SystemTime::now()).unwrap(),
                        "unexpected": true,
                        "command": {"kind": "legacy_preset", "preset": "timer_clear"}
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(unknown_field.status(), StatusCode::UNPROCESSABLE_ENTITY);

    let unknown_device = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v2/devices/not-registered/control")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "request_id": "unknown-device",
                        "issued_at_unix_ms": unix_millis(SystemTime::now()).unwrap(),
                        "command": {"kind": "legacy_preset", "preset": "timer_clear"}
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(unknown_device.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn v2_control_replay_returns_the_same_result_and_rejects_command_reuse() {
    let app = router(DeviceService::with_ble_device(
        "synthetic-ble-id".to_owned(),
        DeviceRegistry::new(),
    ));
    let request = |command| {
        serde_json::json!({
            "request_id": "replay-id",
            "issued_at_unix_ms": unix_millis(SystemTime::now()).unwrap(),
            "command": command
        })
        .to_string()
    };
    let send = |app: Router, body: String| async move {
        app.oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v2/devices/configured/control")
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap()
    };
    let first = send(
        app.clone(),
        request(serde_json::json!({
            "kind": "quick_connect_targets",
            "temperature_f": 110,
            "humidity_percent": 40
        })),
    )
    .await;
    let first_body = first.into_body().collect().await.unwrap().to_bytes();
    let first_response: serde_json::Value = serde_json::from_slice(&first_body).unwrap();
    assert_eq!(first_response["request_id"], "replay-id");
    assert_eq!(first_response["status"], "unsupported_command");
    let replay = send(
        app.clone(),
        request(serde_json::json!({
            "kind": "quick_connect_targets",
            "temperature_f": 110,
            "humidity_percent": 40
        })),
    )
    .await;
    let replay_body = replay.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(first_body, replay_body);

    let reused = send(
        app,
        request(serde_json::json!({"kind": "quick_connect_mode", "mode": "automatic"})),
    )
    .await;
    assert_eq!(reused.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let bytes = reused.into_body().collect().await.unwrap().to_bytes();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["status"], "request_id_reused");
    assert_eq!(body["request_id"], "replay-id");
}

#[tokio::test]
async fn control_route_rejects_unknown_commands_before_touching_ble() {
    let app = router(DeviceService::with_registry(DeviceRegistry::new()));
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v2/devices/configured/control")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "request_id": "unknown-command",
                        "issued_at_unix_ms": unix_millis(SystemTime::now()).unwrap(),
                        "command": {"kind": "legacy_preset", "preset": "timer_999"}
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
}
