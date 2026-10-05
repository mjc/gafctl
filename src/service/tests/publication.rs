use super::*;
use crate::service::test_support::*;
use crate::test_support::identity_store_fixture;
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
async fn cloud_only_mqtt_state_is_scheduled_without_ble() {
    let mut state = DeviceService::with_registry(DeviceRegistry::new());
    assert!(!state.state_polling_enabled());
    state.attach_state_publication(
        watch::channel(Arc::new(state.state_snapshot().await.unwrap())).0,
        false,
    );
    assert!(state.state_polling_enabled());
}

#[tokio::test]
#[cfg(feature = "mqtt")]
async fn cloud_only_periodic_state_publication_refreshes_the_mqtt_snapshot() {
    let mut state = DeviceService::with_registry(DeviceRegistry::new());
    let (updates, mut current) = watch::channel(Arc::new(state.state_snapshot().await.unwrap()));
    state.attach_state_publication(updates, false);

    state.poll_and_publish_state().await;

    assert!(current.changed().await.is_ok());
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
