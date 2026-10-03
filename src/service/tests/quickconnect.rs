use super::*;

#[tokio::test]
async fn detail_failure_keeps_device_registered_while_other_state_and_ble_remain_available() {
    let path = identity_store_path();
    let mut registry = DeviceRegistry::load(&path).unwrap();
    let ble_runtime = registry.register_configured_ble();
    let (client, server) = mock_quickconnect_client().await;

    let mut state = DeviceService::with_registry(registry);
    state.quickconnect = Some(super::super::quickconnect::QuickConnectBackend::new(
        Arc::clone(&state.registry),
        client,
        "synthetic-account",
    ));
    state.poll_quickconnect().await;
    let registry = state.registry.read().await;
    assert_eq!(
        registry
            .descriptors()
            .filter(|device| device.backend == DeviceBackend::QuickConnect)
            .count(),
        2,
    );
    let descriptors = registry
        .descriptors()
        .map(|descriptor| (descriptor.name.as_str(), descriptor.id.clone()))
        .collect::<std::collections::BTreeMap<_, _>>();
    let failed = registry.runtime(&descriptors["Failed detail"]).unwrap();
    let live = registry.runtime(&descriptors["Live detail"]).unwrap();
    let failed_snapshot = failed.snapshot().await;
    assert!(failed_snapshot.state.is_none());
    assert_eq!(
        failed_snapshot.inventory_status,
        crate::backend::DeviceInventoryStatus::Present
    );
    assert_eq!(live.state().await.unwrap().temperature_f, Some(78.0));
    assert_eq!(
        live.snapshot().await.inventory_status,
        crate::backend::DeviceInventoryStatus::Present
    );
    assert!(ble_runtime.state().await.is_none());
    assert!(
        registry
            .descriptors()
            .any(|descriptor| descriptor.id == DeviceId::configured_ble())
    );

    server.abort();
    fs::remove_dir_all(path.parent().unwrap()).ok();
}

#[tokio::test]
async fn invalid_inventory_marks_only_that_accounts_current_state_unavailable() {
    let path = identity_store_path();
    let mut registry = DeviceRegistry::load(&path).unwrap();
    let first = registry
        .reconcile_quickconnect(
            "account-a",
            &[crate::backend::CloudDeviceInput::new(
                "provider-a".to_owned(),
                "One".to_owned(),
            )],
        )
        .unwrap();
    let second = registry
        .reconcile_quickconnect(
            "account-b",
            &[crate::backend::CloudDeviceInput::new(
                "provider-a".to_owned(),
                "Two".to_owned(),
            )],
        )
        .unwrap();
    let first_runtime = registry.runtime(&first[0]).unwrap();
    let second_runtime = registry.runtime(&second[0]).unwrap();
    let state = || DeviceState {
        temperature_f: Some(78.0),
        humidity_percent: Some(40.0),
        settings: DeviceSettings::QuickConnect {
            mode: gafctl_api::QuickConnectModeStatus::Automatic,
            automatic_temperature_f: Some(100),
            automatic_humidity_percent: Some(40),
            timer_duration_minutes: None,
            humidity_monitor: Some(true),
        },
        estimated_running: Some(false),
        diagnostics: None,
        provenance: StateProvenance {
            backend: DeviceBackend::QuickConnect,
            fetched_at_unix_ms: unix_millis(SystemTime::now()),
            observed_at_unix_ms: None,
        },
    };
    first_runtime.set_state(state()).await;
    second_runtime.set_state(state()).await;
    let (client, server) = mock_duplicate_inventory_client().await;

    let mut state = DeviceService::with_registry(registry);
    state.quickconnect = Some(super::super::quickconnect::QuickConnectBackend::new(
        Arc::clone(&state.registry),
        client,
        "account-a",
    ));
    state.poll_quickconnect().await;

    let failed = first_runtime.snapshot().await;
    assert_eq!(
        failed.inventory_status,
        crate::backend::DeviceInventoryStatus::Unavailable
    );
    assert!(failed.state.is_none());
    let unaffected = second_runtime.snapshot().await;
    assert_eq!(
        unaffected.inventory_status,
        crate::backend::DeviceInventoryStatus::Present
    );
    assert_eq!(unaffected.state.unwrap().temperature_f, Some(78.0));

    server.abort();
    fs::remove_dir_all(path.parent().unwrap()).ok();
}

#[cfg(feature = "mqtt")]
#[tokio::test]
async fn cloud_poll_publishes_fast_device_before_blocked_sibling() {
    let (mut state, fixture, server, path) = cloud_poll_fixture(&["slow", "fast"], &["slow"]).await;
    let (updates, observed) = watch::channel(Arc::new(state.state_snapshot().await.unwrap()));
    state.attach_state_publication(updates, false);
    let polling = tokio::spawn({
        let state = state.clone();
        async move { state.poll_quickconnect().await }
    });
    let publications = stream::unfold(observed, |mut observed| async move {
        observed.changed().await.ok()?;
        let ready = {
            let snapshot = observed.borrow_and_update();
            snapshot
                .descriptors
                .iter()
                .find(|device| device.name == "fast")
                .is_some_and(|device| {
                    snapshot
                        .publications
                        .iter()
                        .any(|item| item.id == device.id && item.available)
                })
        };
        Some((ready, observed))
    })
    .filter_map(|ready| futures_util::future::ready(ready.then_some(())));
    tokio::pin!(publications);
    let fast_published = tokio::time::timeout(Duration::from_secs(2), publications.next())
        .await
        .ok()
        .flatten()
        .is_some();
    fixture.release();
    polling.await.unwrap();
    server.abort();
    fs::remove_file(path).unwrap();
    assert!(
        fast_published,
        "fast cloud publication waited for blocked sibling"
    );
}

#[tokio::test]
async fn cloud_poll_bounds_concurrent_device_reads() {
    let devices = ["a", "b", "c", "d", "e", "f"];
    let (state, fixture, server, path) = cloud_poll_fixture(&devices, &devices).await;
    let polling = tokio::spawn(async move { state.poll_quickconnect().await });
    wait_for_cloud_reads(&fixture, 4).await;
    tokio::time::sleep(Duration::from_millis(30)).await;
    let blocked_reads = fixture.reads.load(std::sync::atomic::Ordering::SeqCst);
    fixture.release();
    polling.await.unwrap();
    server.abort();
    fs::remove_file(path).unwrap();
    assert_eq!(blocked_reads, 4);
    assert_eq!(fixture.reads.load(std::sync::atomic::Ordering::SeqCst), 6);
    assert_eq!(fixture.peak.load(std::sync::atomic::Ordering::SeqCst), 4);
}
