use super::quickconnect::QuickConnectBackend;
use super::*;
use crate::api::router;
use crate::test_support::identity_store_fixture;
use axum::{
    Json, Router,
    body::Body,
    extract::State,
    http::{Request, StatusCode},
    routing::{get, post},
};
use futures_util::{StreamExt, stream};
use gafctl_api::DeviceId;
use gafctl_protocol::DeviceSnapshot;
use http_body_util::BodyExt;
use std::time::{Duration, Instant, SystemTime};
use tower::ServiceExt;
#[derive(Default)]
pub(super) struct CloudPollFixture {
    devices: Vec<String>,
    blocked: Vec<String>,
    pub(super) reads: std::sync::atomic::AtomicUsize,
    pub(super) active: std::sync::atomic::AtomicUsize,
    pub(super) peak: std::sync::atomic::AtomicUsize,
    released: std::sync::atomic::AtomicBool,
    release: tokio::sync::Notify,
}

impl CloudPollFixture {
    pub(super) fn release(&self) {
        self.released
            .store(true, std::sync::atomic::Ordering::SeqCst);
        self.release.notify_waiters();
    }
}

pub(super) async fn cloud_poll_fixture_detail(
    State(fixture): State<Arc<CloudPollFixture>>,
    uri: axum::http::Uri,
) -> Json<serde_json::Value> {
    use std::sync::atomic::Ordering::SeqCst;
    fixture.reads.fetch_add(1, SeqCst);
    let active = fixture.active.fetch_add(1, SeqCst) + 1;
    fixture.peak.fetch_max(active, SeqCst);
    let blocked = fixture.blocked.iter().any(|id| {
        uri.query()
            .is_some_and(|query| query.strip_prefix("deviceId=") == Some(id.as_str()))
    });
    if blocked {
        let released = fixture.release.notified();
        tokio::pin!(released);
        released.as_mut().enable();
        if !fixture.released.load(SeqCst) {
            released.await;
        }
    }
    fixture.active.fetch_sub(1, SeqCst);
    Json(serde_json::json!({"responseData":{
        "deviceConfig":{"setTemperature":78,"setHumidity":44},
        "deviceSettings":{"automaticMode":true,"timerMode":false,"fanMode":false,
            "setTemperature":105,"setHumidity":40,"humidityMonitor":true}
    }}))
}

pub(super) async fn cloud_poll_fixture(
    devices: &[&str],
    blocked: &[&str],
) -> (
    DeviceService,
    Arc<CloudPollFixture>,
    tokio::task::JoinHandle<()>,
    tempfile::TempDir,
) {
    let (directory, path) = identity_store_fixture();
    let registry = DeviceRegistry::load(&path).unwrap();
    let fixture = Arc::new(CloudPollFixture {
        devices: devices.iter().map(|id| (*id).to_owned()).collect(),
        blocked: blocked.iter().map(|id| (*id).to_owned()).collect(),
        ..CloudPollFixture::default()
    });
    let app = Router::new()
        .route(
            "/cognito/login",
            post(|| async {
                Json(serde_json::json!({"responseData":{"idToken":"synthetic-token"}}))
            }),
        )
        .route(
            "/gaf/device/deviceList",
            get(|State(fixture): State<Arc<CloudPollFixture>>| async move {
                let devices = fixture
                    .devices
                    .iter()
                    .map(|id| serde_json::json!({"deviceId":id,"name":id}))
                    .collect::<Vec<_>>();
                Json(serde_json::json!({"responseData":{"devices":devices}}))
            }),
        )
        .route("/gaf/device", get(cloud_poll_fixture_detail))
        .with_state(Arc::clone(&fixture));
    let (client, server) = crate::test_support::mock_client(app).await;
    let mut state = DeviceService::with_registry(registry);
    state.quickconnect = Some(QuickConnectBackend::new(
        Arc::clone(&state.registry),
        client,
        "synthetic-account",
    ));
    (state, fixture, server, directory)
}

pub(super) async fn mock_quickconnect_client() -> (
    gafctl_quickconnect::QuickConnectClient,
    tokio::task::JoinHandle<()>,
) {
    let app = Router::new()
        .route("/cognito/login", post(mock_login))
        .route("/gaf/device/deviceList", get(mock_inventory))
        .route("/gaf/device", get(mock_detail));
    crate::test_support::mock_client(app).await
}

pub(super) async fn mock_duplicate_inventory_client() -> (
    gafctl_quickconnect::QuickConnectClient,
    tokio::task::JoinHandle<()>,
) {
    let app = Router::new()
        .route("/cognito/login", post(mock_login))
        .route("/gaf/device/deviceList", get(mock_duplicate_inventory))
        .route("/gaf/device", get(mock_detail));
    crate::test_support::mock_client(app).await
}

pub(super) async fn mock_login() -> Json<serde_json::Value> {
    Json(serde_json::json!({"responseData": {"idToken": "SYNTHETIC_TOKEN_DO_NOT_USE"}}))
}

pub(super) async fn mock_inventory() -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "responseData": {"devices": [
            {"deviceId": "synthetic-failed-detail", "name": "Failed detail"},
            {"deviceId": "synthetic-live-detail", "name": "Live detail"}
        ]}
    }))
}

pub(super) async fn mock_duplicate_inventory() -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "responseData": [
            {"deviceId": "synthetic-device", "name": "Duplicate one"},
            {"deviceId": "synthetic-device", "name": "Duplicate two"}
        ]
    }))
}

pub(super) async fn mock_detail(
    uri: axum::http::Uri,
) -> (axum::http::StatusCode, Json<serde_json::Value>) {
    if uri
        .query()
        .is_some_and(|query| query.contains("synthetic-failed-detail"))
    {
        return (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"message": "synthetic failure"})),
        );
    }
    (
        axum::http::StatusCode::OK,
        Json(serde_json::json!({
            "responseData": {
                "deviceConfig": {"setTemperature": 78, "setHumidity": 44},
                "deviceSettings": {
                    "automaticMode": true,
                    "timerMode": false,
                    "fanMode": false,
                    "setTemperature": 105,
                    "setHumidity": 40,
                    "humidityMonitor": true
                }
            }
        })),
    )
}

pub(super) async fn wait_for_cloud_reads(fixture: &CloudPollFixture, count: usize) {
    let attempts = stream::iter(0..100)
        .then(|_| async {
            tokio::time::sleep(Duration::from_millis(10)).await;
            fixture.reads.load(std::sync::atomic::Ordering::SeqCst) >= count
        })
        .filter_map(|ready| futures_util::future::ready(ready.then_some(())));
    tokio::pin!(attempts);
    assert!(attempts.next().await.is_some(), "cloud reads did not start");
}

#[derive(Default)]
pub(crate) struct RefreshFixture {
    pub(crate) reads: std::sync::atomic::AtomicUsize,
    pub(crate) entered: tokio::sync::Notify,
    pub(crate) release: tokio::sync::Notify,
    pub(super) fail: std::sync::atomic::AtomicBool,
}

pub(crate) async fn refresh_fixture() -> (
    DeviceService,
    DeviceId,
    Arc<RefreshFixture>,
    tokio::task::JoinHandle<()>,
    tempfile::TempDir,
) {
    let (directory, path) = identity_store_fixture();
    let mut registry = DeviceRegistry::load(&path).unwrap();
    let id = registry
        .reconcile_quickconnect(
            "synthetic-account",
            &[crate::backend::CloudDeviceInput::new(
                "private-fixture-id".to_owned(),
                "Vent".to_owned(),
            )],
        )
        .unwrap()
        .pop()
        .unwrap();
    let fixture = Arc::new(RefreshFixture::default());
    let app = Router::new()
        .route(
            "/cognito/login",
            post(|| async {
                Json(serde_json::json!({"responseData": {"idToken": "synthetic-token"}}))
            }),
        )
        .route("/gaf/device", get(refresh_fixture_detail))
        .with_state(Arc::clone(&fixture));
    let (client, server) = crate::test_support::mock_client(app).await;
    let mut state = DeviceService::with_registry(registry);
    state.quickconnect = Some(QuickConnectBackend::new(
        Arc::clone(&state.registry),
        client,
        "synthetic-account",
    ));
    (state, id, fixture, server, directory)
}

pub(super) async fn refresh_fixture_detail(
    State(fixture): State<Arc<RefreshFixture>>,
) -> (StatusCode, Json<serde_json::Value>) {
    fixture
        .reads
        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    fixture.entered.notify_one();
    fixture.release.notified().await;
    if fixture.fail.load(std::sync::atomic::Ordering::SeqCst) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"message":"synthetic failure"})),
        );
    }
    (
        StatusCode::OK,
        Json(serde_json::json!({"responseData": {
            "deviceConfig": {"setTemperature": 78, "setHumidity": 44},
            "deviceSettings": {"automaticMode":true,"timerMode":false,"fanMode":false,
                "setTemperature":105,"setHumidity":40,"humidityMonitor":true}
        }})),
    )
}

pub(super) fn snapshot_at(started_at: Instant, observed_at: SystemTime) -> DeviceSnapshot {
    DeviceSnapshot::from_frames_at(
        gafctl_protocol::Frame::from_bytes(bytes::Bytes::from_static(
            b"#idr030000private-suffix\n",
        ))
        .unwrap(),
        gafctl_protocol::Frame::from_bytes(bytes::Bytes::from_static(b"#dmraf\n")).unwrap(),
        gafctl_protocol::Frame::from_bytes(bytes::Bytes::from_static(b"#sdr03ca00aa\n")).unwrap(),
        gafctl_protocol::Frame::from_bytes(bytes::Bytes::from_static(b"#atr041a012c\n")).unwrap(),
        gafctl_protocol::Frame::from_bytes(bytes::Bytes::from_static(b"#ttr00000000\n")).unwrap(),
        observed_at,
        started_at,
    )
    .unwrap()
}

pub(super) async fn state_response(state: DeviceService) -> serde_json::Value {
    let response = router(state)
        .oneshot(
            Request::builder()
                .uri("/api/v2/devices/configured/state")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}
