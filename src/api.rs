use std::{
    collections::VecDeque,
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use crate::control::{CommandId, ControlPreset, FreshControlRequest};
use anyhow::Result;
use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use tokio::{
    net::TcpListener,
    sync::{Mutex, RwLock, mpsc, watch},
    time::{MissedTickBehavior, interval},
};
use updraft_bluetooth::{
    DisconnectOutcome, ProbeClient, ProbeError, ProbeErrorKind, ProbeMode, ProbeOptions,
    ProbeResult,
};
use updraft_protocol::{DeviceSnapshot, StateFreshness, StateReconciler};

const DEVICE_ID: &str = "configured";
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(30);
const DEFAULT_FRESHNESS_LIMIT: Duration = Duration::from_secs(90);

#[derive(Clone)]
struct ApiState {
    reconciler: Arc<RwLock<StateReconciler>>,
    ble_lock: Arc<Mutex<()>>,
    ble_client: Arc<ProbeClient>,
    freshness_limit: Duration,
    device_id: Option<String>,
    mqtt_updates: Option<watch::Sender<String>>,
}

impl ApiState {
    fn new(freshness_limit: Duration) -> Self {
        Self {
            reconciler: Arc::new(RwLock::new(StateReconciler::default())),
            ble_lock: Arc::new(Mutex::new(())),
            ble_client: Arc::new(ProbeClient::new()),
            freshness_limit,
            device_id: None,
            mqtt_updates: None,
        }
    }

    fn for_device(freshness_limit: Duration, device_id: String) -> Self {
        Self {
            device_id: Some(device_id),
            ..Self::new(freshness_limit)
        }
    }

    async fn execute_control(&self, preset: ControlPreset) -> ControlResponse {
        let _ble_guard = self.ble_lock.lock().await;
        self.execute_control_locked(preset).await
    }

    async fn execute_mqtt_control(&self, request: &FreshControlRequest) -> ControlResponse {
        let _ble_guard = self.ble_lock.lock().await;
        let fresh = unix_millis(SystemTime::now())
            .is_some_and(|now_unix_ms| request.is_fresh_at(now_unix_ms));
        if !fresh {
            return ControlResponse::rejected(
                request.preset(),
                "stale or future-dated control request",
            );
        }
        self.execute_control_locked(request.preset()).await
    }

    async fn execute_control_locked(&self, preset: ControlPreset) -> ControlResponse {
        let poll_id = self.reconciler.write().await.begin_poll();
        let ControlOutcome {
            success,
            message,
            snapshot,
        } = control_outcome(self.probe_control(preset).await);
        let state = self
            .reconcile_control_snapshot(poll_id, snapshot, message)
            .await;
        self.publish_state().await;

        ControlResponse {
            success,
            preset,
            message,
            state,
        }
    }

    async fn probe_control(&self, preset: ControlPreset) -> Result<ProbeResult, ProbeError> {
        self.ble_client
            .probe(ProbeOptions {
                scan_duration: Duration::from_secs(6),
                response_timeout: Duration::from_secs(3),
                mode: ProbeMode::Query {
                    device_id: self.device_id.clone(),
                    control_command: Some(preset.command()),
                },
            })
            .await
    }

    async fn reconcile_control_snapshot(
        &self,
        poll_id: u64,
        snapshot: Option<DeviceSnapshot>,
        message: &'static str,
    ) -> Option<StateValues> {
        match snapshot {
            Some(snapshot) => {
                let state = StateValues::from_snapshot(&snapshot);
                self.reconciler
                    .write()
                    .await
                    .apply_success(poll_id, snapshot);
                state
            }
            None => {
                self.reconciler
                    .write()
                    .await
                    .apply_failure(poll_id, message);
                None
            }
        }
    }

    async fn publish_state(&self) {
        let Some(updates) = &self.mqtt_updates else {
            return;
        };
        match serde_json::to_string(&device_state_response(self).await) {
            Ok(payload) => {
                updates.send_replace(payload);
            }
            Err(error) => tracing::error!(%error, "could not serialize device state for MQTT"),
        }
    }
}

struct ControlOutcome {
    success: bool,
    message: &'static str,
    snapshot: Option<DeviceSnapshot>,
}

fn control_outcome(result: Result<ProbeResult, ProbeError>) -> ControlOutcome {
    let (success, message, snapshot) = match result {
        Ok(ProbeResult::Queried { result, .. }) => {
            if let DisconnectOutcome::Failed(error) = &result.disconnect {
                tracing::warn!(%error, "BLE disconnect failed after control request");
            }
            let success = result
                .control
                .as_ref()
                .is_some_and(|control| control.is_confirmed());
            let message = match (success, result.state_error.as_deref()) {
                (true, _) => "command acknowledged and readback matched",
                (false, Some(error)) => {
                    tracing::warn!(%error, "control readback failed");
                    "command was not confirmed; readback failed"
                }
                (false, None) => "command was not confirmed; acknowledgement or readback differed",
            };
            (success, message, result.snapshot)
        }
        Ok(_) => (
            false,
            "command was not confirmed; device selection failed",
            None,
        ),
        Err(error) => {
            tracing::warn!(error = %error, "control request failed");
            (false, "command was not confirmed; BLE request failed", None)
        }
    };

    ControlOutcome {
        success,
        message,
        snapshot,
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
    let mut state = ApiState::for_device(DEFAULT_FRESHNESS_LIMIT, device_id.clone());
    if let Some(config) = mqtt_config {
        let initial_state = serde_json::to_string(&device_state_response(&state).await)?;
        let bridge = crate::mqtt::start(config, initial_state);
        state.mqtt_updates = Some(bridge.state_updates);
        tokio::spawn(process_mqtt_controls(
            state.clone(),
            bridge.control_requests,
        ));
    }
    let app = router(state.clone());
    tokio::spawn(poll_device(state, device_id, DEFAULT_POLL_INTERVAL));

    tracing::info!(%address, "Updraft API listening");
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
        .route("/api/v1/devices/configured/control", post(control_device))
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
            controls: true,
        }],
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ControlRequest {
    preset: ControlPreset,
}

#[derive(Clone, Serialize)]
pub(crate) struct ControlResponse {
    success: bool,
    preset: ControlPreset,
    message: &'static str,
    state: Option<StateValues>,
}

impl ControlResponse {
    pub(crate) fn rejected(preset: ControlPreset, message: &'static str) -> Self {
        Self {
            success: false,
            preset,
            message,
            state: None,
        }
    }
}

async fn control_device(
    State(state): State<ApiState>,
    Json(request): Json<ControlRequest>,
) -> (StatusCode, Json<ControlResponse>) {
    let response = state.execute_control(request.preset).await;
    let status = if response.success {
        StatusCode::OK
    } else {
        StatusCode::BAD_GATEWAY
    };
    (status, Json(response))
}

const CONTROL_REPLAY_CAPACITY: usize = 64;

#[derive(Default)]
struct RecentControlResults(VecDeque<(CommandId, ControlPreset, ControlResponse)>);

impl RecentControlResults {
    fn get(&self, request_id: &CommandId, preset: ControlPreset) -> Option<ControlResponse> {
        self.0
            .iter()
            .find(|(seen_id, _, _)| seen_id == request_id)
            .map(|(_, seen_preset, response)| {
                if *seen_preset == preset {
                    response.clone()
                } else {
                    ControlResponse::rejected(preset, "request_id was reused for another preset")
                }
            })
    }

    fn insert(&mut self, request_id: CommandId, preset: ControlPreset, response: ControlResponse) {
        self.0.retain(|(seen_id, _, _)| seen_id != &request_id);
        self.0.push_back((request_id, preset, response));
        if self.0.len() > CONTROL_REPLAY_CAPACITY {
            self.0.pop_front();
        }
    }
}

async fn process_mqtt_controls(
    state: ApiState,
    mut controls: mpsc::Receiver<crate::mqtt::MqttControlWork>,
) {
    let mut recent = RecentControlResults::default();
    while let Some(work) = controls.recv().await {
        let fresh = unix_millis(SystemTime::now())
            .is_some_and(|now_unix_ms| work.request.is_fresh_at(now_unix_ms));
        let response = if !fresh {
            ControlResponse::rejected(
                work.request.preset(),
                "stale or future-dated control request",
            )
        } else {
            match recent.get(work.request.request_id(), work.request.preset()) {
                Some(response) => response,
                None => {
                    let response = state.execute_mqtt_control(&work.request).await;
                    recent.insert(
                        work.request.request_id().clone(),
                        work.request.preset(),
                        response.clone(),
                    );
                    response
                }
            }
        };
        let _ = work.reply.send(response);
    }
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
    control_preset: Option<&'static str>,
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
            control_preset: ControlPreset::from_readback(mode.mode, *thresholds, *timer)
                .map(ControlPreset::as_str),
            timer_remaining_minutes: timer.remaining.value(),
            timer_original_minutes: timer.original.value(),
        })
    }
}

async fn poll_device(state: ApiState, device_id: String, poll_interval: Duration) {
    let mut ticker = interval(poll_interval);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        ticker.tick().await;
        let _ble_guard = state.ble_lock.lock().await;
        let poll_id = state.reconciler.write().await.begin_poll();
        let result = state
            .ble_client
            .probe(ProbeOptions {
                scan_duration: Duration::from_secs(6),
                response_timeout: Duration::from_secs(3),
                mode: ProbeMode::Query {
                    device_id: Some(device_id.clone()),
                    control_command: None,
                },
            })
            .await;

        match result {
            Ok(ProbeResult::Queried { result, .. }) => {
                if let DisconnectOutcome::Failed(error) = &result.disconnect {
                    tracing::warn!(%error, "BLE disconnect failed after state poll");
                }
                match result.snapshot {
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
                }
            }
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
                tracing::warn!(%error, "BLE state poll failed");
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

        state.publish_state().await;
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
    async fn routes_report_health_capabilities_and_unknown_state() {
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
        assert_eq!(body["devices"][0]["controls"], true);
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
    async fn control_route_rejects_unverified_presets_before_touching_ble() {
        let app = router(ApiState::new(DEFAULT_FRESHNESS_LIMIT));
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/devices/configured/control")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"preset":"timer_999"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[test]
    fn control_presets_encode_only_the_verified_settings() {
        assert_eq!(
            ControlPreset::Automatic105F30Percent
                .command()
                .frame()
                .as_bytes(),
            b"#ams041A012C\n"
        );
        assert_eq!(
            ControlPreset::Automatic105_1F30_1Percent
                .command()
                .frame()
                .as_bytes(),
            b"#ams041B012D\n"
        );
        assert_eq!(
            ControlPreset::TimerClear.command().frame().as_bytes(),
            b"#tms0000\n"
        );
        assert_eq!(
            ControlPreset::TimerOneMinute.command().frame().as_bytes(),
            b"#tms0001\n"
        );
        assert!(serde_json::from_str::<ControlPreset>("\"arbitrary\"").is_err());
    }

    #[test]
    fn http_control_requests_reject_unknown_fields() {
        assert!(
            serde_json::from_str::<ControlRequest>(
                r#"{"preset":"timer_clear","duration_minutes":999}"#
            )
            .is_err()
        );
    }

    #[test]
    fn mqtt_control_request_ids_replay_once_and_reject_preset_changes() {
        let id = CommandId::parse("ha-command-1".to_owned()).unwrap();
        let mut recent = RecentControlResults::default();
        let response = ControlResponse {
            success: true,
            preset: ControlPreset::TimerOneMinute,
            message: "command acknowledged and readback matched",
            state: None,
        };
        recent.insert(id.clone(), ControlPreset::TimerOneMinute, response.clone());

        assert!(
            recent
                .get(&id, ControlPreset::TimerOneMinute)
                .unwrap()
                .success
        );
        let changed = recent
            .get(&id, ControlPreset::TimerClear)
            .expect("a reused request ID is rejected");
        assert!(!changed.success);
        assert_eq!(changed.message, "request_id was reused for another preset");

        for index in 0..=CONTROL_REPLAY_CAPACITY {
            recent.insert(
                CommandId::parse(format!("command-{index}")).unwrap(),
                ControlPreset::TimerClear,
                ControlResponse::rejected(ControlPreset::TimerClear, "not confirmed"),
            );
        }
        assert!(recent.0.len() <= CONTROL_REPLAY_CAPACITY);
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
        assert_eq!(fresh["state"]["control_preset"], "automatic105_f30_percent");

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
