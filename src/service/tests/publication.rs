use super::*;
use crate::service::test_support::*;
use crate::test_support::{cloud_device, identity_store_fixture};
use crate::{api::router, backend::DeviceRegistry};
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use gafctl_api::EntitySource;
use std::time::Duration;
use tower::ServiceExt;

#[cfg(feature = "mqtt")]
#[tokio::test]
async fn mqtt_snapshot_collection_serializes_sibling_publications() {
    let (mut state, id, fixture, _server, _directory) = refresh_fixture().await;
    fixture.release.notify_one();
    state.refresh_device(&id).await.unwrap();
    state
        .registry
        .write()
        .await
        .reconcile_quickconnect(
            "synthetic-account",
            &[cloud_device("another-device", "Second")],
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
    state.attach_state_publication(updates, false);
    let gate = runtimes[1].1.block_snapshot_for_test().await;
    let older = tokio::spawn({
        let state = state.clone();
        async move { state.publish_state().await }
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    let collection_holds_publication_lock = state
        .publication
        .as_ref()
        .unwrap()
        .collection
        .try_lock()
        .is_err();
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
}

#[cfg(feature = "mqtt")]
#[tokio::test]
async fn publication_timer_expires_state_while_backend_refresh_is_blocked_and_recovers() {
    let (mut state, id, fixture, _server, _directory) = refresh_fixture().await;
    fixture.release.notify_one();
    state.refresh_device(&id).await.unwrap();
    fixture.entered.notified().await;
    let (updates, mut observed) = watch::channel(Arc::new(state.state_snapshot().await.unwrap()));
    state.attach_state_publication(updates, false);
    let (mut polling_tasks, stop_polls) = crate::server::start_polling(&state);
    stop_polls[2].abort();
    tokio::time::sleep(Duration::from_millis(20)).await;
    super::super::test_support::age_state_for_test(&state, &id, Duration::from_secs(91)).await;
    let polling_state = state.clone();
    let polling_id = id.clone();
    let blocked_poll = tokio::spawn(async move { polling_state.refresh_device(&polling_id).await });
    fixture.entered.notified().await;
    tokio::time::timeout(Duration::from_secs(2), observed.changed())
        .await
        .unwrap()
        .unwrap();
    {
        let expired = observed.borrow();
        let publication = expired
            .publications
            .iter()
            .find(|item| item.id == id)
            .unwrap();
        assert!(
            !publication.available,
            "expired MQTT state remained available"
        );
        assert!(publication.state.is_none());
    }
    assert_eq!(
        super::super::test_support::state_response_for_device(state.clone(), &id).await["available"],
        false
    );

    fixture.release.notify_one();
    let recovered = blocked_poll.await.unwrap().unwrap();
    assert!(
        recovered.device.available,
        "backend refresh did not recover: {recovered:?}"
    );
    observed.changed().await.unwrap();
    assert!(observed.borrow().publications[0].available);
    polling_tasks.abort_all();
    polling_tasks.shutdown().await;
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
#[cfg(feature = "mqtt")]
async fn source_route_accepts_persistent_mqtt_owner_with_offline_broker() {
    let (_directory, path) = identity_store_fixture();
    let mut state = DeviceService::with_ble_device(
        "no-physical-device".to_owned(),
        DeviceRegistry::load(&path).unwrap(),
    );
    crate::server::mqtt::start(&mut state, crate::mqtt::test_support::config(1, true))
        .await
        .unwrap();
    let mut updates = state.publication.as_ref().unwrap().updates.subscribe();
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
}

#[tokio::test]
#[cfg(feature = "mqtt")]
async fn cloud_only_state_is_scheduled_and_periodically_published() {
    let mut disconnected = DeviceService::with_registry(DeviceRegistry::new());
    assert!(!disconnected.state_polling_enabled());
    disconnected.attach_state_publication(
        watch::channel(Arc::new(disconnected.state_snapshot().await.unwrap())).0,
        false,
    );
    assert!(disconnected.state_polling_enabled());
    let mut state = DeviceService::with_registry(DeviceRegistry::new());
    assert!(!state.state_polling_enabled());
    let (updates, current) = watch::channel(Arc::new(state.state_snapshot().await.unwrap()));
    state.attach_state_publication(updates, false);
    assert!(state.state_polling_enabled());
    state.poll_and_publish_state().await;
    assert!(!current.has_changed().unwrap());
    assert!(current.borrow().publications.is_empty());
}

#[tokio::test]
#[cfg(feature = "mqtt")]
async fn mqtt_discovery_start_preserves_per_device_ownership() {
    let (_directory, path) = identity_store_fixture();
    let mut registry = DeviceRegistry::load(&path).unwrap();
    let ids = registry
        .reconcile_quickconnect(
            "account-a",
            &[
                cloud_device("provider-a", "Cloud fan A"),
                cloud_device("provider-b", "Cloud fan B"),
            ],
        )
        .unwrap();
    registry
        .set_entity_sources(&ids[0], EntitySource::Mqtt, EntitySource::Mqtt)
        .unwrap();
    let mut state = DeviceService::with_registry(registry);

    crate::server::mqtt::start(&mut state, crate::mqtt::test_support::config(1, true))
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
}
