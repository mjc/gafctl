use super::*;
use crate::service::test_support::*;
use crate::{api::router, backend::DeviceRegistry};
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use gafctl_api::{CommandId, ControlPreset, DeviceControlV2Request, DeviceControlV2Response};
use http_body_util::BodyExt;
use std::time::Instant;
use tower::ServiceExt;

#[tokio::test]
async fn ble_stale_control_releases_its_admission_slot() {
    let state =
        DeviceService::with_ble_device("no-physical-device".to_owned(), DeviceRegistry::new());
    let runtime = state.ble_device.as_ref().unwrap();
    assert_eq!(
        runtime
            .execute_control(
                &state,
                0,
                DeviceCommand::LegacyPreset {
                    preset: ControlPreset::TimerClear
                }
            )
            .await
            .err(),
        Some(ControlAdmissionError::StaleRequest)
    );
    let permits = std::iter::repeat_with(|| runtime.device.try_reserve_control().unwrap())
        .take(8)
        .collect::<Vec<_>>();
    assert!(runtime.device.try_reserve_control().is_none());
    drop(permits);
}

#[tokio::test]
async fn ble_http_admission_rejects_busy_before_waiting_or_touching_bluetooth() {
    let state =
        DeviceService::with_ble_device("no-physical-device".to_owned(), DeviceRegistry::new());
    let runtime = state.ble_device.as_ref().unwrap();
    let permits = std::iter::repeat_with(|| runtime.device.try_reserve_control().unwrap())
        .take(8)
        .collect::<Vec<_>>();
    let _transaction = runtime.device.acquire_transaction().await;
    let request = DeviceControlV2Request {
        request_id: CommandId::parse("busy-ble-request").unwrap(),
        issued_at_unix_ms: unix_millis(SystemTime::now()).unwrap(),
        command: DeviceCommand::LegacyPreset {
            preset: ControlPreset::TimerClear,
        },
    };
    let result = tokio::time::timeout(
        Duration::from_millis(100),
        router(state.clone()).oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v2/devices/configured/control")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&request).unwrap()))
                .unwrap(),
        ),
    )
    .await
    .expect("busy admission must not wait for the device transaction");
    let result = result.unwrap();
    assert_eq!(result.status(), StatusCode::TOO_MANY_REQUESTS);
    let body = result.into_body().collect().await.unwrap().to_bytes();
    let response: DeviceControlV2Response = serde_json::from_slice(&body).unwrap();
    assert_eq!(response.status, gafctl_api::ControlStatus::Busy);
    assert_eq!(response.request_id, request.request_id.as_str());
    drop(permits);
    assert!(runtime.device.try_reserve_control().is_some());
}

#[test]
fn control_presets_encode_only_the_verified_settings() {
    assert_eq!(
        ControlPreset::Automatic105F30Percent
            .command()
            .frame()
            .as_bytes(),
        b"#ams041A012C\n"
    );
    assert_eq!(
        ControlPreset::Automatic105_1F30_1Percent
            .command()
            .frame()
            .as_bytes(),
        b"#ams041B012D\n"
    );
    assert_eq!(
        ControlPreset::TimerClear.command().frame().as_bytes(),
        b"#tms0000\n"
    );
    assert_eq!(
        ControlPreset::TimerOneMinute.command().frame().as_bytes(),
        b"#tms0001\n"
    );
    assert!(serde_json::from_str::<ControlPreset>("\"arbitrary\"").is_err());
}

#[tokio::test]
async fn initial_ble_poll_failure_reports_unavailable_inventory_and_error() {
    let state =
        DeviceService::with_ble_device("synthetic-ble-id".to_owned(), DeviceRegistry::new());
    let ble = state.ble_device.as_ref().unwrap();
    let mut reconciler = ble.reconciler.write().await;
    let poll_id = reconciler.begin_poll();
    reconciler.apply_failure(poll_id, "BLE query failed");
    drop(reconciler);

    let response = state_response(state).await;

    assert_eq!(response["available"], false);
    assert_eq!(response["inventory_status"], "unavailable");
    assert_eq!(response["last_error"], "BLE query failed");
    assert!(response["state"].is_null());
}

#[test]
fn snapshot_projection_requires_every_field_to_decode() {
    let valid: [&'static [u8]; 5] = [
        b"#idr030000private-suffix\n",
        b"#dmraf\n",
        b"#sdr03ca00aa\n",
        b"#atr041a012c\n",
        b"#ttr00000000\n",
    ];
    let invalid: [&'static [u8]; 5] = [
        b"#idrnot-a-version\n",
        b"#dmrunrecognized\n",
        b"#sdrbad\n",
        b"#atrbad\n",
        b"#ttrbad\n",
    ];
    for (field, malformed) in invalid.into_iter().enumerate() {
        let mut wires = valid;
        wires[field] = malformed;
        let [identity, mode, sensors, thresholds, timer] = wires.map(|wire| {
            gafctl_protocol::Frame::from_bytes(bytes::Bytes::from_static(wire)).unwrap()
        });
        let snapshot =
            DeviceSnapshot::from_frames(identity, mode, sensors, thresholds, timer).unwrap();
        assert!(
            project_legacy_snapshot(&snapshot).is_none(),
            "field {field}"
        );
    }
}

#[test]
fn snapshot_projection_does_not_expose_identity_suffix_or_claim_airflow() {
    let snapshot = snapshot_at(Instant::now(), SystemTime::now());
    let projected = serde_json::to_string(&project_legacy_snapshot(&snapshot)).unwrap();
    assert!(!projected.contains("private-suffix"));
    assert!(projected.contains("\"estimated_running\":null"));
    assert!(projected.contains("\"controller_fan_on\":false"));
}
