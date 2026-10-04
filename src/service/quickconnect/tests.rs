use super::*;
use crate::service::test_support::*;

#[tokio::test]
async fn cloud_poll_skips_transaction_queued_read_superseded_by_control() {
    let (state, fixture, server, _directory) = cloud_poll_fixture(&["vent"], &[]).await;
    let cloud = state.quickconnect.as_ref().unwrap();
    let mut registry = state.registry.write().await;
    registry
        .reconcile_quickconnect(
            &cloud.account_id,
            &[crate::backend::CloudDeviceInput::new(
                "vent".to_owned(),
                "vent".to_owned(),
            )],
        )
        .unwrap();
    let generations = registry.begin_quickconnect_poll(&cloud.account_id);
    drop(registry);
    let target = cloud
        .reconcile_inventory(cloud.client.read_inventory().await.unwrap(), &generations)
        .await
        .unwrap()
        .pop()
        .unwrap();
    let runtime = Arc::clone(&target.runtime);
    let transaction = runtime.acquire_transaction().await;
    let client = cloud.client.clone();
    let polling = tokio::spawn(async move { target.read(&client).await });
    tokio::time::sleep(Duration::from_millis(30)).await;
    let blocked_reads = fixture.reads.load(std::sync::atomic::Ordering::SeqCst);
    runtime.begin_control_intent();
    drop(transaction);
    polling.await.unwrap();
    server.abort();
    assert_eq!(blocked_reads, 0, "poll bypassed the device transaction");
    assert_eq!(
        fixture.reads.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "superseded poll reached the backend"
    );
}

#[tokio::test]
async fn polling_detail_results_do_not_replace_newer_control_state() {
    stream::iter([false, true])
        .for_each(|failed| superseded_detail_result_is_ignored(failed, true))
        .await;
}

#[tokio::test]
async fn refresh_detail_results_report_superseded_and_preserve_newer_control_state() {
    stream::iter([false, true])
        .for_each(|failed| superseded_detail_result_is_ignored(failed, false))
        .await;
}

async fn superseded_detail_result_is_ignored(failed: bool, polling: bool) {
    use gafctl_api::{DeviceSettings, QuickConnectModeStatus, StateProvenance};
    use std::sync::atomic::Ordering::SeqCst;

    let (state, id, fixture, server, _directory) = refresh_fixture().await;
    fixture.fail.store(failed, SeqCst);
    let cloud = state.quickconnect.as_ref().unwrap();
    let (runtime, provider_id) = state
        .registry
        .read()
        .await
        .quickconnect_read_target(&cloud.account_id, &id)
        .unwrap();
    let target = QuickConnectReadTarget {
        provider_id,
        runtime: Arc::clone(&runtime),
        generation: runtime.begin_state_read(),
    };
    let reading = tokio::spawn({
        let state = state.clone();
        async move {
            if polling {
                target
                    .read(&state.quickconnect.as_ref().unwrap().client)
                    .await;
                None
            } else {
                Some(state.refresh_device(&id).await.unwrap().status)
            }
        }
    });
    fixture.entered.notified().await;
    let generation = runtime.begin_control_intent();
    let confirmed = DeviceState {
        temperature_f: Some(102.0),
        humidity_percent: Some(43.0),
        settings: DeviceSettings::QuickConnect {
            mode: QuickConnectModeStatus::Manual,
            automatic_temperature_f: None,
            automatic_humidity_percent: None,
            timer_duration_minutes: None,
            humidity_monitor: None,
        },
        estimated_running: Some(true),
        diagnostics: None,
        provenance: StateProvenance {
            backend: DeviceBackend::QuickConnect,
            fetched_at_unix_ms: gafctl_api::unix_millis(std::time::SystemTime::now()),
            observed_at_unix_ms: None,
        },
    };
    assert!(
        runtime
            .set_control_state_if_current(generation, confirmed.clone())
            .await
    );
    fixture.release.notify_one();
    let status = reading.await.unwrap();
    assert_eq!(
        status,
        (!polling).then_some(DeviceRefreshStatus::Superseded)
    );
    let snapshot = runtime.snapshot().await;
    assert_eq!(snapshot.state, Some(confirmed));
    assert_eq!(
        snapshot.inventory_status,
        gafctl_api::DeviceInventoryStatus::Present
    );
    assert_eq!(snapshot.last_error, None);
    server.abort();
}
