use super::*;
use crate::service::test_support::*;
use std::fs;

#[tokio::test]
async fn cloud_poll_skips_transaction_queued_read_superseded_by_control() {
    let (state, fixture, server, path) = cloud_poll_fixture(&["vent"], &[]).await;
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
    let target = registry
        .reconcile_quickconnect_inventory(
            &cloud.account_id,
            cloud.client.read_inventory().await.unwrap(),
            &generations,
        )
        .await
        .unwrap()
        .pop()
        .unwrap();
    let runtime = Arc::clone(&target.runtime);
    drop(registry);
    let transaction = runtime.acquire_transaction().await;
    let client = cloud.client.clone();
    let polling = tokio::spawn(async move { target.read(&client).await });
    tokio::time::sleep(Duration::from_millis(30)).await;
    let blocked_reads = fixture.reads.load(std::sync::atomic::Ordering::SeqCst);
    runtime.begin_control_intent();
    drop(transaction);
    polling.await.unwrap();
    server.abort();
    fs::remove_file(path).unwrap();
    assert_eq!(blocked_reads, 0, "poll bypassed the device transaction");
    assert_eq!(
        fixture.reads.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "superseded poll reached the backend"
    );
}
