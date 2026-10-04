use super::*;

#[tokio::test(start_paused = true)]
async fn refresh_deadline_releases_worker_without_starting_a_second_ble_owner() {
    let state =
        DeviceService::with_ble_device("no-physical-device".to_owned(), DeviceRegistry::new());
    let runtime = state
        .registry
        .read()
        .await
        .runtime(&DeviceId::configured_ble())
        .unwrap();
    let transaction = runtime.acquire_transaction().await;
    let first = state
        .refresh_device(&DeviceId::configured_ble())
        .await
        .unwrap();
    assert_eq!(first.status, DeviceRefreshStatus::Failed);
    assert_eq!(
        first.device.last_error.as_deref(),
        Some("device refresh deadline exceeded")
    );
    let second = state
        .refresh_device(&DeviceId::configured_ble())
        .await
        .unwrap();
    assert_eq!(second.status, DeviceRefreshStatus::Failed);
    assert!(!Arc::ptr_eq(&first, &second));
    drop(transaction);
    assert!(runtime.try_acquire_transaction().is_some());
}

#[tokio::test]
async fn refresh_survives_cancelled_caller_and_coalesces_overlapping_reads() {
    let (state, id, fixture, _server, _directory) = refresh_fixture().await;
    let first_state = state.clone();
    let first_id = id.clone();
    let first = tokio::spawn(async move { first_state.refresh_device(&first_id).await });
    fixture.entered.notified().await;
    first.abort();
    let second = state.refresh_device(&id);
    tokio::pin!(second);
    assert!(
        tokio::time::timeout(Duration::from_millis(10), &mut second)
            .await
            .is_err()
    );
    fixture.release.notify_one();
    let response = second.await.unwrap();
    assert_eq!(response.status, DeviceRefreshStatus::Fresh);
    assert!(response.device.available);
    assert_eq!(fixture.reads.load(std::sync::atomic::Ordering::SeqCst), 1);
    fixture.release.notify_one();
    let response = state.refresh_device(&id).await.unwrap();
    assert_eq!(response.status, DeviceRefreshStatus::Fresh);
    assert_eq!(fixture.reads.load(std::sync::atomic::Ordering::SeqCst), 2);
}

#[tokio::test]
async fn refresh_uses_transaction_lock_and_does_not_publish_superseded_read() {
    let (state, id, fixture, _server, _directory) = refresh_fixture().await;
    let runtime = state.registry.read().await.runtime(&id).unwrap();
    let transaction = runtime.acquire_transaction().await;
    let request = state.refresh_device(&id);
    tokio::pin!(request);
    assert!(
        tokio::time::timeout(Duration::from_millis(10), &mut request)
            .await
            .is_err()
    );
    assert_eq!(fixture.reads.load(std::sync::atomic::Ordering::SeqCst), 0);
    drop(transaction);
    fixture.entered.notified().await;
    runtime.begin_control_intent();
    fixture.release.notify_one();
    let response = request.await.unwrap();
    assert_eq!(response.status, DeviceRefreshStatus::Superseded);
    assert!(runtime.state().await.is_none());
}

#[tokio::test]
async fn refresh_route_reports_failed_read_instead_of_reusing_success() {
    let (state, id, fixture, _server, _directory) = refresh_fixture().await;
    fixture.release.notify_one();
    assert_eq!(
        state.refresh_device(&id).await.unwrap().status,
        DeviceRefreshStatus::Fresh
    );
    fixture
        .fail
        .store(true, std::sync::atomic::Ordering::SeqCst);
    fixture.release.notify_one();
    let response = router(state)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/v2/devices/{}/refresh", id.as_str()))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    let response: DeviceRefreshV2Response =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(response.status, DeviceRefreshStatus::Failed);
    assert!(!response.device.available);
    assert!(response.device.last_error.is_some());
}

#[tokio::test]
async fn refresh_distinguishes_unknown_device_from_unconfigured_backend() {
    let mut registry = DeviceRegistry::new();
    registry.register_configured_ble();
    let app = router(DeviceService::with_registry(registry));
    for (id, expected) in [
        ("not-registered", StatusCode::NOT_FOUND),
        ("configured", StatusCode::SERVICE_UNAVAILABLE),
    ] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/api/v2/devices/{id}/refresh"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), expected);
    }
}
