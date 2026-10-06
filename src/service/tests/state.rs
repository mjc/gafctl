use super::*;
use crate::service::test_support::*;
use crate::{api::router, backend::DeviceRegistry};
use gafctl_api::DeviceId;
use std::time::Instant;
use tokio::net::TcpListener;
use tokio_util::task::AbortOnDropHandle;

#[tokio::test]
async fn state_route_reports_normalized_state_and_expired_state_as_unavailable() {
    let state =
        DeviceService::with_ble_device("private-peripheral-id".to_owned(), DeviceRegistry::new());
    let projection =
        project_legacy_snapshot(&snapshot_at(Instant::now(), SystemTime::now())).unwrap();
    state
        .registry
        .read()
        .await
        .runtime(&DeviceId::configured_ble())
        .unwrap()
        .set_state(projection)
        .await;
    let fresh = state_response(state.clone()).await;
    assert_eq!(fresh["available"], true);
    assert_eq!(fresh["backend"], "legacy_ble");
    assert_eq!(fresh["state"]["temperature_f"], 97.0);
    assert_eq!(fresh["state"]["provenance"]["backend"], "legacy_ble");

    let expired_state = project_legacy_snapshot(&snapshot_at(
        Instant::now(),
        SystemTime::now() - Duration::from_secs(120),
    ))
    .unwrap();
    state
        .registry
        .read()
        .await
        .runtime(&DeviceId::configured_ble())
        .unwrap()
        .set_state(expired_state)
        .await;
    let expired = state_response(state).await;
    assert_eq!(expired["available"], false);
    assert!(expired["state"].is_null());
}

#[tokio::test]
async fn reusable_client_reads_the_actual_service_router_without_physical_access() {
    for (sensors, humidity) in [
        (b"#sdr03ca00aa\n", Some(17.0)),
        (b"#sdr03ca0000\n", Some(0.0)),
        (b"#sdr03ca03e8\n", Some(100.0)),
        (b"#sdr03ca03e9\n", None),
        (b"#sdr03caffff\n", None),
    ] {
        assert_client_snapshot(sensors, humidity).await;
    }
}

async fn assert_client_snapshot(sensors: &'static [u8], humidity: Option<f64>) {
    let state =
        DeviceService::with_ble_device("private-peripheral-id".to_owned(), DeviceRegistry::new());
    let frame =
        |payload| gafctl_protocol::Frame::from_bytes(bytes::Bytes::from_static(payload)).unwrap();
    let snapshot = gafctl_protocol::DeviceSnapshot::from_frames(
        frame(b"#idr030000private-suffix\n"),
        frame(b"#dmraf\n"),
        frame(sensors),
        frame(b"#atr041a03e8\n"),
        frame(b"#ttr00000000\n"),
    )
    .unwrap();
    let projection = project_legacy_snapshot(&snapshot).unwrap();
    state
        .registry
        .read()
        .await
        .runtime(&DeviceId::configured_ble())
        .unwrap()
        .set_state(projection)
        .await;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let app = router(state);
    let _task = AbortOnDropHandle::new(tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    }));
    let client = gafctl_client::Client::new(
        url.parse().unwrap(),
        gafctl_client::ClientOptions::default(),
    )
    .unwrap();
    let inventory = client.devices().await.unwrap();
    assert_eq!(inventory.devices.len(), 1);
    assert_eq!(inventory.devices[0].id, DeviceId::configured_ble());
    let response = client.state(&DeviceId::configured_ble()).await.unwrap();
    assert!(response.available);
    let snapshot = response.state.unwrap();
    assert_eq!(snapshot.temperature_f, Some(97.0));
    assert_eq!(snapshot.humidity_percent, humidity);
    assert_eq!(
        match snapshot.settings {
            gafctl_api::DeviceSettings::LegacyBle {
                automatic_humidity_tenths_percent,
                ..
            } => automatic_humidity_tenths_percent,
            gafctl_api::DeviceSettings::QuickConnect { .. } => None,
        },
        Some(1000)
    );
    assert_eq!(snapshot.estimated_running, None);
    let serialized = serde_json::to_string(&snapshot).unwrap();
    assert!(serialized.contains("\"estimated_running\":null"));
    assert!(serialized.contains("\"controller_fan_on\":false"));
    assert!(!serialized.contains("private-suffix"));
    assert!(!serialized.contains("private-peripheral-id"));
}

#[tokio::test]
async fn failed_ble_poll_or_control_hides_previous_readings_and_recovers() {
    for failure in 0..3 {
        let state =
            DeviceService::with_ble_device("synthetic-ble-id".to_owned(), DeviceRegistry::new());
        let ble = state.ble_device.as_ref().unwrap();
        let first = ble.reconciler.write().await.begin_poll();
        ble.reconcile_control_snapshot(
            first,
            Some(snapshot_at(Instant::now(), SystemTime::now())),
            "readback failed",
        )
        .await;
        assert_eq!(state_response(state.clone()).await["available"], true);
        #[cfg(feature = "mqtt")]
        let (state, observed) = {
            let mut state = state;
            let (updates, observed) =
                tokio::sync::watch::channel(Arc::new(state.state_snapshot().await.unwrap()));
            state.attach_state_publication(updates, false);
            (state, observed)
        };
        let ble = state.ble_device.as_ref().unwrap();
        let poll_id = ble.reconciler.write().await.begin_poll();
        match failure {
            0 => {
                assert_eq!(
                    ble.reconcile_poll_result(poll_id, Ok(ProbeResult::NoDevices))
                        .await,
                    DeviceRefreshStatus::Failed
                );
            }
            1 => {
                ble.reconcile_control_snapshot(poll_id, None, "readback failed")
                    .await;
            }
            _ => {
                let mut invalid = snapshot_at(Instant::now(), SystemTime::now());
                invalid
                    .observe_frame(
                        gafctl_protocol::Frame::from_bytes(bytes::Bytes::from_static(b"#atrbad\n"))
                            .unwrap(),
                    )
                    .unwrap_err();
                ble.reconcile_control_snapshot(poll_id, Some(invalid), "invalid readback")
                    .await;
            }
        }
        let failed = state_response(state.clone()).await;
        assert_eq!(failed["available"], false, "failure {failure}");
        assert!(failed["state"].is_null());
        assert!(failed["last_error"].is_string());
        #[cfg(feature = "mqtt")]
        {
            state.publish_state().await;
            assert!(!observed.borrow().publications[0].available);
            assert!(observed.borrow().publications[0].state.is_none());
        }
        let poll_id = ble.reconciler.write().await.begin_poll();
        ble.reconcile_control_snapshot(
            poll_id,
            Some(snapshot_at(Instant::now(), SystemTime::now())),
            "readback failed",
        )
        .await;
        assert_eq!(state_response(state.clone()).await["available"], true);
        #[cfg(feature = "mqtt")]
        {
            state.publish_state().await;
            assert!(observed.borrow().publications[0].available);
        }
    }
}

#[tokio::test]
async fn older_refresh_timeout_cannot_hide_newer_confirmed_readings() {
    let state =
        DeviceService::with_ble_device("synthetic-ble-id".to_owned(), DeviceRegistry::new());
    let ble = state.ble_device.as_ref().unwrap();
    let old_generation = ble.device.state_generation();
    let poll_id = ble.reconciler.write().await.begin_poll();
    ble.reconcile_control_snapshot(
        poll_id,
        Some(snapshot_at(Instant::now(), SystemTime::now())),
        "readback failed",
    )
    .await;
    assert!(!ble.record_refresh_timeout(old_generation).await);
    let current = state_response(state).await;
    assert_eq!(current["available"], true);
    assert!(current["last_error"].is_null());
}
