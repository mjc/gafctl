use std::{
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::Result;
use axum::{Json, Router, extract::State, routing::get};
use serde::Serialize;
use tokio::{
    net::TcpListener,
    sync::{RwLock, watch},
    time::{MissedTickBehavior, interval},
};
use updraft_bluetooth::{ProbeErrorKind, ProbeMode, ProbeOptions, ProbeResult, probe};
use updraft_protocol::{DeviceSnapshot, StateFreshness, StateReconciler};

const DEVICE_ID: &str = "configured";
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(30);
const DEFAULT_FRESHNESS_LIMIT: Duration = Duration::from_secs(90);

#[derive(Clone)]
struct ApiState {
    reconciler: Arc<RwLock<StateReconciler>>,
    freshness_limit: Duration,
}

impl ApiState {
    fn new(freshness_limit: Duration) -> Self {
        Self {
            reconciler: Arc::new(RwLock::new(StateReconciler::default())),
            freshness_limit,
        }
    }
}

pub(crate) async fn serve(
    device_id: String,
    address: SocketAddr,
    allow_remote: bool,
    mqtt_config: Option<crate::mqtt::MqttConfig>,
) -> Result<()> {
    validate_bind_address(address, allow_remote)?;
    let listener = TcpListener::bind(address).await?;
    let state = ApiState::new(DEFAULT_FRESHNESS_LIMIT);
    let app = router(state.clone());
    let mqtt_updates = match mqtt_config {
        Some(config) => {
            let initial_state = serde_json::to_string(&device_state_response(&state).await)?;
            Some(crate::mqtt::start(config, initial_state))
        }
        None => None,
    };
    tokio::spawn(poll_device(
        state,
        device_id,
        DEFAULT_POLL_INTERVAL,
        mqtt_updates,
    ));

    tracing::info!(%address, "Updraft read-only API listening");
    axum::serve(listener, app).await?;
    Ok(())
}

fn validate_bind_address(address: SocketAddr, allow_remote: bool) -> Result<()> {
    anyhow::ensure!(
        address.ip().is_loopback() || allow_remote,
        "non-loopback API binding requires --allow-remote"
    );
    Ok(())
}

fn router(state: ApiState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/api/v1/devices", get(devices))
        .route("/api/v1/devices/configured/state", get(device_state))
        .with_state(state)
}

async fn health() -> Json<HealthResponse> {
    Json(HealthResponse { status: "ok" })
}

async fn devices() -> Json<DeviceListResponse> {
    Json(DeviceListResponse {
        devices: vec![DeviceDescription {
            id: DEVICE_ID,
            name: "GAF Wi-Fi Vent",
            state: true,
            controls: false,
        }],
    })
}

async fn device_state(State(state): State<ApiState>) -> Json<DeviceStateResponse> {
    Json(device_state_response(&state).await)
}

async fn device_state_response(state: &ApiState) -> DeviceStateResponse {
    let reconciler = state.reconciler.read().await;
    let now = Instant::now();
    let freshness = reconciler.freshness_at(now, state.freshness_limit);
    let snapshot = reconciler.current_snapshot_at(now, state.freshness_limit);
    DeviceStateResponse {
        device_id: DEVICE_ID,
        available: freshness == StateFreshness::Fresh,
        freshness: freshness_name(freshness),
        observed_at_unix_ms: reconciler
            .latest_snapshot()
            .and_then(|snapshot| unix_millis(snapshot.observed_at)),
        last_error: reconciler.last_error().map(str::to_owned),
        state: snapshot.and_then(StateValues::from_snapshot),
    }
}

#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
}

#[derive(Serialize)]
struct DeviceListResponse {
    devices: Vec<DeviceDescription>,
}

#[derive(Serialize)]
struct DeviceDescription {
    id: &'static str,
    name: &'static str,
    state: bool,
    controls: bool,
}

#[derive(Clone, Serialize)]
struct DeviceStateResponse {
    device_id: &'static str,
    available: bool,
    freshness: &'static str,
    observed_at_unix_ms: Option<u64>,
    last_error: Option<String>,
    state: Option<StateValues>,
}

#[derive(Clone, Serialize)]
struct StateValues {
    firmware_version: String,
    mode: &'static str,
    controller_fan_flag: &'static str,
    temperature_f: f64,
    humidity_percent: f64,
    automatic_temperature_threshold_f: f64,
    automatic_humidity_threshold_percent: f64,
    timer_remaining_minutes: u16,
    timer_original_minutes: u16,
}

impl StateValues {
    fn from_snapshot(snapshot: &DeviceSnapshot) -> Option<Self> {
        let identity = snapshot.identity.decoded().ok()?;
        let mode = snapshot.mode.decoded().ok()?;
        let sensors = snapshot.sensors.decoded().ok()?;
        let thresholds = snapshot.thresholds.decoded().ok()?;
        let timer = snapshot.timer.decoded().ok()?;
        let version = identity.firmware_version;
        Some(Self {
            firmware_version: format!("{}.{}.{}", version.major, version.minor, version.patch),
            mode: match mode.mode {
                updraft_protocol::OperatingMode::Automatic => "automatic",
                updraft_protocol::OperatingMode::Timer => "timer",
                updraft_protocol::OperatingMode::Ota => "ota",
            },
            controller_fan_flag: match mode.fan {
                updraft_protocol::FanState::On => "on",
                updraft_protocol::FanState::Off => "off",
            },
            temperature_f: f64::from(sensors.temperature.value()) / 10.0,
            humidity_percent: f64::from(sensors.humidity.value()) / 10.0,
            automatic_temperature_threshold_f: f64::from(thresholds.temperature.value()) / 10.0,
            automatic_humidity_threshold_percent: f64::from(thresholds.humidity.value()) / 10.0,
            timer_remaining_minutes: timer.remaining.value(),
            timer_original_minutes: timer.original.value(),
        })
    }
}

async fn poll_device(
    state: ApiState,
    device_id: String,
    poll_interval: Duration,
    mqtt_updates: Option<watch::Sender<String>>,
) {
    let mut ticker = interval(poll_interval);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        ticker.tick().await;
        let poll_id = state.reconciler.write().await.begin_poll();
        let result = probe(ProbeOptions {
            scan_duration: Duration::from_secs(6),
            response_timeout: Duration::from_secs(3),
            mode: ProbeMode::Query {
                device_id: Some(device_id.clone()),
                control_command: None,
            },
        })
        .await;

        match result {
            Ok(ProbeResult::Queried { result, .. }) => match result.snapshot {
                Some(snapshot) => {
                    state
                        .reconciler
                        .write()
                        .await
                        .apply_success(poll_id, snapshot);
                }
                None => {
                    let error = result
                        .state_error
                        .unwrap_or_else(|| "state query returned no snapshot".to_owned());
                    state.reconciler.write().await.apply_failure(poll_id, error);
                }
            },
            Ok(ProbeResult::NoDevices) => {
                state
                    .reconciler
                    .write()
                    .await
                    .apply_failure(poll_id, "no compatible device found");
            }
            Ok(ProbeResult::Ambiguous { .. }) => {
                state
                    .reconciler
                    .write()
                    .await
                    .apply_failure(poll_id, "device selection was ambiguous");
            }
            Ok(ProbeResult::DiscoveryIncomplete { .. }) => {
                state
                    .reconciler
                    .write()
                    .await
                    .apply_failure(poll_id, "device discovery was incomplete");
            }
            Ok(ProbeResult::Discovered { .. }) => {
                state
                    .reconciler
                    .write()
                    .await
                    .apply_failure(poll_id, "device was not queried");
            }
            Err(error) => {
                let message = match error.kind() {
                    ProbeErrorKind::Unavailable => "BLE unavailable",
                    ProbeErrorKind::Authentication => "BLE permission or authentication failed",
                    ProbeErrorKind::Protocol => "GAF protocol error",
                };
                state
                    .reconciler
                    .write()
                    .await
                    .apply_failure(poll_id, message);
            }
        }

        if let Some(updates) = &mqtt_updates {
            match serde_json::to_string(&device_state_response(&state).await) {
                Ok(payload) => {
                    updates.send_replace(payload);
                }
                Err(error) => tracing::error!(%error, "could not serialize device state for MQTT"),
            }
        }
    }
}

const fn freshness_name(freshness: StateFreshness) -> &'static str {
    match freshness {
        StateFreshness::Unknown => "unknown",
        StateFreshness::Fresh => "fresh",
        StateFreshness::Stale => "stale",
    }
}

fn unix_millis(timestamp: SystemTime) -> Option<u64> {
    timestamp
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| duration.as_millis().try_into().ok())
}

#[cfg(test)]
mod tests {
    use axum::{body::Body, http::Request};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    use super::*;

    fn snapshot_at(started_at: Instant, observed_at: SystemTime) -> DeviceSnapshot {
        DeviceSnapshot::from_frames_at(
            updraft_protocol::Frame::from_bytes(bytes::Bytes::from_static(
                b"#idr030000private-suffix\n",
            ))
            .unwrap(),
            updraft_protocol::Frame::from_bytes(bytes::Bytes::from_static(b"#dmraf\n")).unwrap(),
            updraft_protocol::Frame::from_bytes(bytes::Bytes::from_static(b"#sdr03ca00aa\n"))
                .unwrap(),
            updraft_protocol::Frame::from_bytes(bytes::Bytes::from_static(b"#atr041a012c\n"))
                .unwrap(),
            updraft_protocol::Frame::from_bytes(bytes::Bytes::from_static(b"#ttr00000000\n"))
                .unwrap(),
            observed_at,
            started_at,
        )
        .unwrap()
    }

    async fn state_response(state: ApiState) -> serde_json::Value {
        let response = router(state)
            .oneshot(
                Request::builder()
                    .uri("/api/v1/devices/configured/state")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn read_only_routes_report_health_capabilities_and_unknown_state() {
        let state = ApiState::new(DEFAULT_FRESHNESS_LIMIT);
        let app = router(state);

        let health = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(health.status(), axum::http::StatusCode::OK);

        let devices = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/v1/devices")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = devices.into_body().collect().await.unwrap().to_bytes();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["devices"][0]["id"], DEVICE_ID);
        assert_eq!(body["devices"][0]["controls"], false);
        assert!(!body.to_string().contains("private-peripheral-id"));

        let state = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/devices/configured/state")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = state.into_body().collect().await.unwrap().to_bytes();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["freshness"], "unknown");
        assert_eq!(body["available"], false);
        assert!(body["state"].is_null());
    }

    #[tokio::test]
    async fn state_route_reports_fresh_stale_failed_and_rejected_malformed_snapshots() {
        let state = ApiState::new(Duration::from_secs(60));
        let now = Instant::now();
        let mut reconciler = state.reconciler.write().await;
        let valid_poll = reconciler.begin_poll();
        assert!(reconciler.apply_success(valid_poll, snapshot_at(now, SystemTime::now())));
        drop(reconciler);

        let fresh = state_response(state.clone()).await;
        assert_eq!(fresh["freshness"], "fresh");
        assert_eq!(fresh["available"], true);
        assert_eq!(fresh["state"]["temperature_f"], 97.0);

        let mut reconciler = state.reconciler.write().await;
        let failed_poll = reconciler.begin_poll();
        assert!(reconciler.apply_failure(failed_poll, "BLE unavailable"));
        drop(reconciler);
        let failed = state_response(state.clone()).await;
        assert_eq!(failed["state"]["temperature_f"], 97.0);
        assert_eq!(failed["last_error"], "BLE unavailable");

        let stale_state = ApiState::new(Duration::ZERO);
        let mut reconciler = stale_state.reconciler.write().await;
        let stale_poll = reconciler.begin_poll();
        assert!(reconciler.apply_success(
            stale_poll,
            snapshot_at(Instant::now() - Duration::from_secs(1), SystemTime::now()),
        ));
        drop(reconciler);
        let stale = state_response(stale_state).await;
        assert_eq!(stale["freshness"], "stale");
        assert_eq!(stale["available"], false);
        assert!(stale["state"].is_null());

        let malformed = DeviceSnapshot::from_frames(
            updraft_protocol::Frame::from_bytes(bytes::Bytes::from_static(b"#idr030000bad\n"))
                .unwrap(),
            updraft_protocol::Frame::from_bytes(bytes::Bytes::from_static(b"#dmrxx\n")).unwrap(),
            updraft_protocol::Frame::from_bytes(bytes::Bytes::from_static(b"#sdr03ca00aa\n"))
                .unwrap(),
            updraft_protocol::Frame::from_bytes(bytes::Bytes::from_static(b"#atr041a012c\n"))
                .unwrap(),
            updraft_protocol::Frame::from_bytes(bytes::Bytes::from_static(b"#ttr00000000\n"))
                .unwrap(),
        )
        .unwrap();
        let mut reconciler = state.reconciler.write().await;
        let malformed_poll = reconciler.begin_poll();
        assert!(!reconciler.apply_success(malformed_poll, malformed));
        drop(reconciler);
        let rejected = state_response(state).await;
        assert_eq!(rejected["state"]["temperature_f"], 97.0);
        assert!(
            rejected["last_error"]
                .as_str()
                .unwrap()
                .contains("invalid payload")
        );
    }

    #[test]
    fn bind_address_validation_requires_remote_opt_in() {
        assert!(validate_bind_address("127.0.0.1:8787".parse().unwrap(), false).is_ok());
        assert!(validate_bind_address("[::1]:8787".parse().unwrap(), false).is_ok());
        assert!(validate_bind_address("192.168.1.5:8787".parse().unwrap(), false).is_err());
        assert!(validate_bind_address("0.0.0.0:8787".parse().unwrap(), true).is_ok());
        assert!(validate_bind_address("192.168.1.5:8787".parse().unwrap(), true).is_ok());
    }

    #[test]
    fn snapshot_projection_does_not_expose_identity_suffix_or_claim_airflow() {
        let snapshot = DeviceSnapshot::from_frames(
            updraft_protocol::Frame::from_bytes(bytes::Bytes::from_static(
                b"#idr030000private-suffix\n",
            ))
            .unwrap(),
            updraft_protocol::Frame::from_bytes(bytes::Bytes::from_static(b"#dmraf\n")).unwrap(),
            updraft_protocol::Frame::from_bytes(bytes::Bytes::from_static(b"#sdr03ca00aa\n"))
                .unwrap(),
            updraft_protocol::Frame::from_bytes(bytes::Bytes::from_static(b"#atr041a012c\n"))
                .unwrap(),
            updraft_protocol::Frame::from_bytes(bytes::Bytes::from_static(b"#ttr00000000\n"))
                .unwrap(),
        )
        .unwrap();
        let projected = serde_json::to_string(&StateValues::from_snapshot(&snapshot)).unwrap();
        assert!(!projected.contains("private-suffix"));
        assert!(!projected.contains("running"));
        assert!(projected.contains("\"controller_fan_flag\":\"off\""));
    }
}
