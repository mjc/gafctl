use super::*;
use crate::service::test_support::*;
use crate::{api::router, backend::DeviceRegistry};
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
#[cfg(feature = "mqtt")]
use gafctl_api::DeviceId;
use gafctl_api::{CommandId, ControlPreset, DeviceControlV2Request, DeviceControlV2Response};
use http_body_util::BodyExt;
use tower::ServiceExt;

#[tokio::test]
async fn restoration_error_clears_only_after_confirmed_explicit_recovery() {
    let state =
        DeviceService::with_ble_device("no-physical-device".to_owned(), DeviceRegistry::new());
    let runtime = state.ble_device.as_ref().unwrap();
    *runtime.timer_error.write().await = Some(TimerFailure::RestoreUnconfirmed);
    runtime
        .execute_control(
            &state,
            unix_millis(SystemTime::now()).unwrap(),
            DeviceCommand::LegacyTimerDuration {
                minutes: 7.try_into().unwrap(),
            },
        )
        .await
        .unwrap();
    assert!(
        state_response(state.clone()).await["last_error"]
            .as_str()
            .unwrap()
            .contains("not confirmed")
    );
    runtime.finish_control(&state, false, None).await.unwrap();
    assert!(
        state_response(state.clone()).await["last_error"]
            .as_str()
            .unwrap()
            .contains("not confirmed")
    );
    runtime.finish_control(&state, true, None).await.unwrap();
    assert!(state_response(state).await["last_error"].is_null());
}

#[tokio::test]
async fn saving_timer_duration_needs_no_bluetooth_and_survives_restart() {
    let (_directory, path) = crate::test_support::identity_store_fixture();
    let state = DeviceService::with_ble_device(
        "no-physical-device".to_owned(),
        DeviceRegistry::load(&path).unwrap(),
    );
    let runtime = state.ble_device.as_ref().unwrap();
    assert_eq!(
        state_response(state.clone()).await["timer_duration_minutes"],
        360
    );
    for minutes in [0, 1, 360] {
        assert_eq!(
            runtime
                .execute_control(
                    &state,
                    unix_millis(SystemTime::now()).unwrap(),
                    DeviceCommand::LegacyTimerDuration {
                        minutes: minutes.try_into().unwrap()
                    }
                )
                .await,
            Ok(true)
        );
        let saved = state_response(state.clone()).await;
        assert_eq!(saved["timer_duration_minutes"], minutes);
        assert!(saved["state"].is_null());
        let restarted = DeviceService::with_ble_device(
            "no-physical-device".to_owned(),
            DeviceRegistry::load(&path).unwrap(),
        );
        assert_eq!(
            state_response(restarted).await["timer_duration_minutes"],
            minutes
        );
    }
    #[cfg(feature = "mqtt")]
    {
        let previous = state.state(&DeviceId::configured_ble()).await.unwrap();
        assert!(
            runtime
                .state_response_matches(&previous, unix_millis(SystemTime::now()))
                .await
        );
        runtime
            .execute_control(
                &state,
                unix_millis(SystemTime::now()).unwrap(),
                DeviceCommand::LegacyTimerDuration {
                    minutes: 7.try_into().unwrap(),
                },
            )
            .await
            .unwrap();
        assert!(
            !runtime
                .state_response_matches(&previous, unix_millis(SystemTime::now()))
                .await
        );
    }
}

#[tokio::test]
async fn failed_timer_preference_write_is_reported_without_changing_value() {
    let (_directory, path) = crate::test_support::identity_store_fixture();
    let state = DeviceService::with_ble_device(
        "no-physical-device".to_owned(),
        DeviceRegistry::load(&path).unwrap(),
    );
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();
    let runtime = state.ble_device.as_ref().unwrap();
    assert_eq!(
        runtime
            .execute_control(
                &state,
                unix_millis(SystemTime::now()).unwrap(),
                DeviceCommand::LegacyTimerDuration {
                    minutes: 7.try_into().unwrap()
                }
            )
            .await,
        Err(ControlAdmissionError::Persistence)
    );
    let response = state_response(state).await;
    assert_eq!(response["timer_duration_minutes"], 360);
    assert!(
        response["last_error"]
            .as_str()
            .unwrap()
            .contains("could not save timer settings")
    );
}

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
    for (preset, expected) in [
        (
            ControlPreset::Automatic105F30Percent,
            b"#ams041A012C\n".as_slice(),
        ),
        (
            ControlPreset::Automatic105_1F30_1Percent,
            b"#ams041B012D\n".as_slice(),
        ),
        (ControlPreset::TimerClear, b"#tms0000\n".as_slice()),
        (ControlPreset::TimerOneMinute, b"#tms0001\n".as_slice()),
    ] {
        assert_eq!(preset.command().frame().as_bytes(), expected, "{preset:?}");
    }
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
