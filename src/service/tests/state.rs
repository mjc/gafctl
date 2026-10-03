use super::*;
use crate::service::test_support::*;
use crate::{api::router, backend::DeviceRegistry};
use gafctl_api::DeviceId;
use std::time::Instant;
use tokio::net::TcpListener;

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

    let mut expired_state = project_legacy_snapshot(&snapshot_at(
        Instant::now(),
        SystemTime::now() - Duration::from_secs(120),
    ))
    .unwrap();
    expired_state.provenance.fetched_at_unix_ms =
        unix_millis(SystemTime::now() - Duration::from_secs(120));
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
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let app = router(state);
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
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
    assert_eq!(snapshot.estimated_running, None);
    let serialized = serde_json::to_string(&snapshot).unwrap();
    assert!(!serialized.contains("private-suffix"));
    assert!(!serialized.contains("private-peripheral-id"));
    task.abort();
}
