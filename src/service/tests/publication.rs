use super::*;

#[cfg(feature = "mqtt")]
#[tokio::test]
async fn mqtt_snapshot_collection_serializes_sibling_publications() {
    let (mut state, id, fixture, server, path) = refresh_fixture().await;
    fixture.release.notify_one();
    state.refresh_device(&id).await.unwrap();
    state
        .registry
        .write()
        .await
        .reconcile_quickconnect(
            "synthetic-account",
            &[crate::backend::CloudDeviceInput::new(
                "another-device".to_owned(),
                "Second".to_owned(),
            )],
        )
        .unwrap();
    let runtimes = {
        let registry = state.registry.read().await;
        registry
            .descriptors()
            .map(|device| (device.id.clone(), registry.runtime(&device.id).unwrap()))
            .collect::<Vec<_>>()
    };
    let reading = state
        .registry
        .read()
        .await
        .runtime(&id)
        .unwrap()
        .state()
        .await
        .unwrap();
    for (_, runtime) in &runtimes {
        runtime.set_state(reading.clone()).await;
    }
    let (updates, observed) = watch::channel(Arc::new(state.state_snapshot().await.unwrap()));
    state.state_updates = Some(updates);
    let gate = runtimes[1].1.block_snapshot_for_test().await;
    let older = tokio::spawn({
        let state = state.clone();
        async move { state.publish_state().await }
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    let collection_holds_publication_lock = state.snapshot_publication.try_lock().is_err();
    let mut newest = reading;
    newest.temperature_f = Some(91.0);
    runtimes[0].1.set_state(newest).await;
    let newer = tokio::spawn({
        let state = state.clone();
        async move { state.publish_state().await }
    });
    drop(gate);
    older.await.unwrap();
    newer.await.unwrap();
    let published = observed.borrow();
    let first = published
        .publications
        .iter()
        .find(|item| item.id == runtimes[0].0)
        .unwrap();
    assert_eq!(first.state.as_ref().unwrap().temperature_f, Some(91.0));
    assert!(
        collection_holds_publication_lock,
        "collection allowed concurrent stale publication"
    );
    server.abort();
    fs::remove_file(path).unwrap();
}

#[cfg(feature = "mqtt")]
#[tokio::test]
async fn mqtt_refresh_uses_device_reader_and_rechecks_queued_freshness() {
    use crate::mqtt::{MqttRefreshRequest, MqttReply};
    let (state, id, fixture, server, path) = refresh_fixture().await;
    let request = |request_id, issued_at_unix_ms| MqttRefreshRequest {
        request_id: CommandId::parse(request_id).unwrap(),
        issued_at_unix_ms,
    };
    let stale = process_mqtt_refresh(&state, &id, request("stale-read", 0)).await;
    assert_eq!(
        serde_json::to_value(stale).unwrap()["status"],
        "stale_request"
    );
    assert_eq!(fixture.reads.load(std::sync::atomic::Ordering::SeqCst), 0);
    fixture.release.notify_one();
    let response = process_mqtt_refresh(
        &state,
        &id,
        request("fresh-read", unix_millis(SystemTime::now()).unwrap()),
    )
    .await;
    let MqttReply::Refresh { request_id, status } = response else {
        unreachable!()
    };
    assert_eq!(request_id, "fresh-read");
    assert_eq!(status, DeviceRefreshStatus::Fresh);
    assert_eq!(fixture.reads.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert!(device_state_v2_data(&state, &id).await.unwrap().available);
    server.abort();
    fs::remove_file(path).unwrap();
}

#[cfg(feature = "mqtt")]
#[tokio::test(start_paused = true)]
async fn mqtt_shutdown_drain_respects_the_cleanup_deadline() {
    let (sender, _receiver) = mpsc::channel(1);
    let intake = crate::mqtt::MqttRequestIntake::new(sender);
    let requests = tokio::spawn(std::future::pending::<()>());
    let mut runtime = MqttRuntime {
        intake,
        requests,
        tasks: None,
    };
    let started = tokio::time::Instant::now();
    runtime.drain(started + SHUTDOWN_CLEANUP_TIMEOUT).await;
    assert!(runtime.requests.is_finished());
    assert_eq!(started.elapsed(), SHUTDOWN_CLEANUP_TIMEOUT);
}

#[cfg(feature = "mqtt")]
#[tokio::test]
async fn mqtt_shutdown_closes_intake_and_drains_an_accepted_request() {
    let (state, id, fixture, server, path) = refresh_fixture().await;
    let (sender, receiver) = mpsc::channel(2);
    let intake = crate::mqtt::MqttRequestIntake::new(sender);
    let requests = tokio::spawn(process_mqtt_requests(state, receiver));
    let work = |request_id: &str| {
        let (reply, response) = tokio::sync::oneshot::channel();
        (
            crate::mqtt::MqttDeviceWork {
                device_id: id.clone(),
                request: crate::mqtt::MqttRequest::Refresh(crate::mqtt::MqttRefreshRequest {
                    request_id: CommandId::parse(request_id).unwrap(),
                    issued_at_unix_ms: unix_millis(SystemTime::now()).unwrap(),
                }),
                reply,
            },
            response,
        )
    };
    let (accepted, response) = work("accepted-before-shutdown");
    assert!(intake.try_send(accepted).is_ok());
    fixture.entered.notified().await;
    let mut runtime = MqttRuntime {
        intake,
        requests,
        tasks: None,
    };
    runtime.intake.close();
    let (late, _) = work("after-shutdown");
    let Err(mpsc::error::TrySendError::Closed(_)) = runtime.intake.try_send(late) else {
        unreachable!("closed MQTT intake must reject later work");
    };
    assert!(!runtime.requests.is_finished());
    fixture.release.notify_one();
    runtime
        .drain(tokio::time::Instant::now() + SHUTDOWN_CLEANUP_TIMEOUT)
        .await;
    let crate::mqtt::MqttReply::Refresh { status, .. } = response.await.unwrap() else {
        unreachable!("refresh request must return a refresh reply");
    };
    assert_eq!(status, DeviceRefreshStatus::Fresh);
    assert!(runtime.requests.is_finished());
    server.abort();
    fs::remove_file(path).unwrap();
}

#[tokio::test]
#[cfg(feature = "mqtt")]
async fn stale_mqtt_snapshot_cannot_restore_previous_entity_owner() {
    let state =
        DeviceService::with_ble_device("no-physical-device".to_owned(), DeviceRegistry::new());
    let old = state.state_snapshot().await.unwrap();
    state
        .registry
        .write()
        .await
        .set_entity_sources(
            &DeviceId::configured_ble(),
            EntitySource::Mqtt,
            EntitySource::Mqtt,
        )
        .unwrap();
    let current = state.state_snapshot().await.unwrap();
    let (updates, observed) = watch::channel(Arc::new(current));
    assert!(!state.publish_current_snapshot(&updates, old).await);
    assert_eq!(
        observed.borrow().descriptors[0].state_source,
        EntitySource::Mqtt
    );
}

#[tokio::test]
async fn source_route_persists_owner_and_rejects_split_or_unconfigured_mqtt() {
    let path = identity_store_path();
    let state = DeviceService::with_ble_device(
        "no-physical-device".to_owned(),
        DeviceRegistry::load(&path).unwrap(),
    );
    let request = || {
        Request::builder()
            .method("PUT")
            .uri("/api/v2/devices/configured/sources")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"state_source":"http","command_source":"http"}"#,
            ))
            .unwrap()
    };
    let response = router(state.clone()).oneshot(request()).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let descriptor: gafctl_api::DeviceDescriptor = serde_json::from_slice(&body).unwrap();
    assert_eq!(descriptor.id, DeviceId::configured_ble());
    for (sources, expected) in [
        (
            r#"{"state_source":"mqtt","command_source":"http"}"#,
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            r#"{"state_source":"mqtt","command_source":"mqtt"}"#,
            StatusCode::CONFLICT,
        ),
    ] {
        let response = router(state.clone())
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/api/v2/devices/configured/sources")
                    .header("content-type", "application/json")
                    .body(Body::from(sources))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), expected);
    }
    fs::remove_dir_all(path.parent().unwrap()).unwrap();
}

#[tokio::test]
#[cfg(feature = "mqtt")]
async fn source_route_accepts_persistent_mqtt_owner_with_offline_broker() {
    let path = identity_store_path();
    let mut state = DeviceService::with_ble_device(
        "no-physical-device".to_owned(),
        DeviceRegistry::load(&path).unwrap(),
    );
    state
        .start_mqtt(crate::mqtt::MqttConfig {
            host: "127.0.0.1".to_owned(),
            port: 1,
            username: "test-user".to_owned(),
            password: "test-password".to_owned(),
            discovery_enabled: true,
        })
        .await
        .unwrap();
    let mut updates = state.state_updates.as_ref().unwrap().subscribe();
    let response = router(state.clone())
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/v2/devices/configured/sources")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"state_source":"mqtt","command_source":"mqtt"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    updates.changed().await.unwrap();
    assert_eq!(
        updates.borrow().descriptors[0].state_source,
        EntitySource::Mqtt
    );
    let mut restored = DeviceRegistry::load(&path).unwrap();
    restored.register_configured_ble();
    assert_eq!(
        restored.descriptors().next().unwrap().command_source,
        EntitySource::Mqtt
    );
    fs::remove_dir_all(path.parent().unwrap()).unwrap();
}

#[test]
#[cfg(feature = "mqtt")]
fn cloud_only_mqtt_state_is_scheduled_without_ble() {
    let mut state = DeviceService::with_registry(DeviceRegistry::new());
    assert!(!state.state_polling_enabled());
    state.state_updates = Some(
        watch::channel(Arc::new(StateSnapshot {
            descriptors: Vec::new(),
            publications: Vec::new(),
            proxy_id: gafctl_api::ProxyId::default(),
            discovery_identities: Vec::new(),
        }))
        .0,
    );
    assert!(state.state_polling_enabled());
}

#[tokio::test]
#[cfg(feature = "mqtt")]
async fn cloud_only_periodic_state_publication_refreshes_the_mqtt_snapshot() {
    let initial = StateSnapshot {
        descriptors: Vec::new(),
        publications: Vec::new(),
        proxy_id: gafctl_api::ProxyId::default(),
        discovery_identities: Vec::new(),
    };
    let (updates, mut current) = watch::channel(Arc::new(initial));
    let mut state = DeviceService::with_registry(DeviceRegistry::new());
    state.state_updates = Some(updates);

    state.poll_and_publish_state().await;

    assert!(current.changed().await.is_ok());
    assert!(current.borrow().publications.is_empty());
}

#[tokio::test]
#[cfg(feature = "mqtt")]
async fn mqtt_discovery_start_preserves_per_device_ownership() {
    let path = identity_store_path();
    let mut registry = DeviceRegistry::load(&path).unwrap();
    let ids = registry
        .reconcile_quickconnect(
            "account-a",
            &[
                crate::backend::CloudDeviceInput::new(
                    "provider-a".to_owned(),
                    "Cloud fan A".to_owned(),
                ),
                crate::backend::CloudDeviceInput::new(
                    "provider-b".to_owned(),
                    "Cloud fan B".to_owned(),
                ),
            ],
        )
        .unwrap();
    registry
        .set_entity_sources(&ids[0], EntitySource::Mqtt, EntitySource::Mqtt)
        .unwrap();
    let mut state = DeviceService::with_registry(registry);

    state
        .start_mqtt(crate::mqtt::MqttConfig {
            host: "127.0.0.1".to_owned(),
            port: 1,
            username: "test-user".to_owned(),
            password: "test-password".to_owned(),
            discovery_enabled: true,
        })
        .await
        .unwrap();

    let registry = state.registry.read().await;
    let descriptors = registry.descriptors().collect::<Vec<_>>();
    assert!(descriptors.iter().any(|device| {
        device.id == ids[0]
            && device.state_source == EntitySource::Mqtt
            && device.command_source == EntitySource::Mqtt
    }));
    assert!(descriptors.iter().any(|device| {
        device.id == ids[1]
            && device.state_source == EntitySource::Http
            && device.command_source == EntitySource::Http
    }));
    fs::remove_dir_all(path.parent().unwrap()).ok();
}

#[test]
#[cfg(feature = "mqtt")]
fn mqtt_device_requests_reject_unknown_fields() {
    assert!(
        serde_json::from_str::<gafctl_api::ControlRequest>(
            r#"{"preset":"timer_clear","duration_minutes":999}"#
        )
        .is_err()
    );
}
