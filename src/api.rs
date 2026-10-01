use std::{
    collections::{HashMap, VecDeque},
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant, SystemTime},
};

use crate::backend::{DeviceRegistry, DeviceRuntime};
use crate::control::{CommandId, ControlPreset, FreshControlRequest, is_fresh_at, unix_millis};
use crate::device::{
    DeviceBackend, DeviceCommand, DeviceId, DeviceSettings, DeviceState, LegacyMode,
    StateProvenance,
};
use anyhow::{Context, Result};
use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    routing::{get, post},
};
use futures_util::{Stream, StreamExt, stream};
use serde::{Deserialize, Serialize};
use tokio::{
    net::TcpListener,
    sync::{RwLock, mpsc, watch},
    time::{Interval, MissedTickBehavior, interval},
};
use updraft_bluetooth::{
    DisconnectOutcome, ProbeClient, ProbeError, ProbeErrorKind, ProbeMode, ProbeOptions,
    ProbeResult, QueryResult,
};
use updraft_protocol::{DeviceSnapshot, StateFreshness, StateReconciler};
use updraft_quickconnect::{QuickConnectCommand, QuickConnectCommandMode};

use crate::quickconnect_control::{
    QuickConnectControlIntent, QuickConnectControlService, QuickConnectControlStatus,
};

const DEVICE_ID: &str = "configured";
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(30);
const DEFAULT_FRESHNESS_LIMIT: Duration = Duration::from_secs(90);

#[derive(Clone)]
struct ApiState {
    registry: Arc<RwLock<DeviceRegistry>>,
    ble_device: Option<Arc<LegacyBleRuntime>>,
    mqtt_updates: Option<watch::Sender<Arc<String>>>,
    quickconnect_control: Option<QuickConnectControlService>,
    v2_control_results: Arc<tokio::sync::Mutex<RecentV2ControlResults>>,
}

struct LegacyBleRuntime {
    reconciler: Arc<RwLock<StateReconciler>>,
    device: Arc<DeviceRuntime>,
    ble_client: Arc<ProbeClient>,
    freshness_limit: Duration,
    peripheral_id: Arc<str>,
}

impl ApiState {
    fn with_registry(registry: DeviceRegistry) -> Self {
        Self {
            registry: Arc::new(RwLock::new(registry)),
            ble_device: None,
            mqtt_updates: None,
            quickconnect_control: None,
            v2_control_results: Arc::default(),
        }
    }

    fn with_ble_device(
        freshness_limit: Duration,
        device_id: String,
        mut registry: DeviceRegistry,
    ) -> Self {
        let device = registry.register_configured_ble();
        Self {
            registry: Arc::new(RwLock::new(registry)),
            ble_device: Some(Arc::new(LegacyBleRuntime::new(
                freshness_limit,
                device_id,
                device,
            ))),
            mqtt_updates: None,
            quickconnect_control: None,
            v2_control_results: Arc::default(),
        }
    }

    async fn execute_http_control(
        &self,
        issued_at_unix_ms: u64,
        preset: ControlPreset,
    ) -> Result<ControlResponse, ()> {
        let device = self.ble_device.as_ref().ok_or(())?;
        device
            .execute_http_control(self, issued_at_unix_ms, preset)
            .await
    }

    async fn execute_mqtt_control(&self, request: &FreshControlRequest) -> ControlResponse {
        match &self.ble_device {
            Some(device) => device.execute_mqtt_control(self, request).await,
            None => ControlResponse::rejected(
                request.preset(),
                "configured BLE device is not available",
            ),
        }
    }

    async fn replay_or_execute_control(
        &self,
        request: &FreshControlRequest,
        recent: &mut RecentControlResults,
    ) -> Arc<ControlResponse> {
        if !request.is_fresh_now() {
            return Arc::new(ControlResponse::rejected(
                request.preset(),
                "stale or future-dated control request",
            ));
        }
        if let Some(response) = recent.get(request.request_id(), request.preset()) {
            return response;
        }
        let response = Arc::new(self.execute_mqtt_control(request).await);
        recent.insert(
            request.request_id().clone(),
            request.preset(),
            Arc::clone(&response),
        );
        response
    }

    async fn poll_and_publish_state(&self) {
        if let Some(device) = &self.ble_device {
            device.poll_and_publish_state(self).await;
        }
    }

    async fn start_mqtt(&mut self, config: crate::mqtt::MqttConfig) -> Result<()> {
        let Some(device) = &self.ble_device else {
            tracing::warn!("MQTT is not started until a device backend is registered");
            return Ok(());
        };
        let initial_state = serde_json::to_string(&device_state_response(device).await)
            .context("could not serialize initial MQTT state")?;
        let bridge = crate::mqtt::start(config, initial_state);
        self.mqtt_updates = Some(bridge.state_updates);
        tokio::spawn(process_mqtt_controls(self.clone(), bridge.control_requests));
        Ok(())
    }

    async fn publish_state(&self) {
        let Some(updates) = &self.mqtt_updates else {
            return;
        };
        let Some(device) = &self.ble_device else {
            return;
        };
        match serde_json::to_string(&device_state_response(device).await) {
            Ok(payload) => {
                updates.send_replace(Arc::new(payload));
            }
            Err(error) => tracing::error!(%error, "could not serialize device state for MQTT"),
        }
    }
}

impl LegacyBleRuntime {
    fn new(freshness_limit: Duration, peripheral_id: String, device: Arc<DeviceRuntime>) -> Self {
        Self {
            reconciler: Arc::new(RwLock::new(StateReconciler::default())),
            device,
            ble_client: Arc::new(ProbeClient::new()),
            freshness_limit,
            peripheral_id: Arc::from(peripheral_id),
        }
    }

    async fn execute_mqtt_control(
        &self,
        state: &ApiState,
        request: &FreshControlRequest,
    ) -> ControlResponse {
        let _transaction = self.device.acquire_transaction().await;
        if !request.is_fresh_now() {
            return ControlResponse::rejected(
                request.preset(),
                "stale or future-dated control request",
            );
        }
        self.execute_control_locked(state, request.preset()).await
    }

    async fn execute_http_control(
        &self,
        state: &ApiState,
        issued_at_unix_ms: u64,
        preset: ControlPreset,
    ) -> Result<ControlResponse, ()> {
        let _transaction = self.device.acquire_transaction().await;
        if !v2_request_is_fresh_at(issued_at_unix_ms, unix_millis(SystemTime::now())) {
            return Err(());
        }
        Ok(self.execute_control_locked(state, preset).await)
    }

    async fn execute_control_locked(
        &self,
        state: &ApiState,
        preset: ControlPreset,
    ) -> ControlResponse {
        let poll_id = self.reconciler.write().await.begin_poll();
        let outcome = control_outcome(self.probe(Some(preset)).await);
        outcome.log_warnings();
        let (success, message, snapshot) = outcome.into_response_parts();
        let response_state = self
            .reconcile_control_snapshot(poll_id, snapshot, message)
            .await;
        state.publish_state().await;
        ControlResponse {
            success,
            preset,
            message,
            state: response_state,
        }
    }

    async fn probe(&self, preset: Option<ControlPreset>) -> Result<ProbeResult, ProbeError> {
        self.ble_client.probe(self.probe_options(preset)).await
    }

    fn probe_options(&self, preset: Option<ControlPreset>) -> ProbeOptions {
        ProbeOptions {
            scan_duration: Duration::from_secs(6),
            response_timeout: Duration::from_secs(3),
            mode: ProbeMode::Query {
                device_id: Some(self.peripheral_id.to_string()),
                control_command: preset.map(ControlPreset::command),
            },
        }
    }

    async fn reconcile_control_snapshot(
        &self,
        poll_id: u64,
        snapshot: Option<DeviceSnapshot>,
        message: &'static str,
    ) -> Option<StateValues> {
        let mut reconciler = self.reconciler.write().await;
        match snapshot {
            Some(snapshot) => {
                let projection = project_legacy_snapshot(&snapshot);
                let state = projection
                    .as_ref()
                    .map(|projection| projection.values.clone());
                if let Some(projection) = projection {
                    self.device.set_state(projection.device_state).await;
                }
                reconciler.apply_success(poll_id, snapshot);
                state
            }
            None => {
                reconciler.apply_failure(poll_id, message);
                None
            }
        }
    }

    async fn poll_and_publish_state(&self, state: &ApiState) {
        let _transaction = self.device.acquire_transaction().await;
        let poll_id = self.reconciler.write().await.begin_poll();
        let result = self.probe(None).await;
        if let Ok(ProbeResult::Queried { result, .. }) = &result
            && let Some(snapshot) = &result.snapshot
            && let Some(projection) = project_legacy_snapshot(snapshot)
        {
            self.device.set_state(projection.device_state).await;
        }
        self.reconcile_poll_result(poll_id, result).await;
        state.publish_state().await;
    }

    async fn reconcile_poll_result(&self, poll_id: u64, result: Result<ProbeResult, ProbeError>) {
        let mut reconciler = self.reconciler.write().await;
        record_poll_result(&mut reconciler, poll_id, result);
    }
}

enum ControlStatus {
    Confirmed,
    ReadbackFailed(String),
    Mismatch,
    DeviceSelectionFailed,
    BleRequestFailed(ProbeError),
}

impl ControlStatus {
    fn from_readback(
        control: Option<&updraft_protocol::ControlOutcome>,
        state_error: Option<String>,
    ) -> Self {
        match (
            control.is_some_and(updraft_protocol::ControlOutcome::is_confirmed),
            state_error,
        ) {
            (true, _) => Self::Confirmed,
            (false, Some(error)) => Self::ReadbackFailed(error),
            (false, None) => Self::Mismatch,
        }
    }
}

fn disconnect_error(outcome: DisconnectOutcome) -> Option<String> {
    match outcome {
        DisconnectOutcome::Failed(error) => Some(error),
        DisconnectOutcome::Disconnected => None,
    }
}

struct ControlOutcome {
    status: ControlStatus,
    snapshot: Option<DeviceSnapshot>,
    disconnect_error: Option<String>,
}

impl ControlOutcome {
    fn from_query(result: QueryResult) -> Self {
        Self {
            status: ControlStatus::from_readback(result.control.as_ref(), result.state_error),
            snapshot: result.snapshot,
            disconnect_error: disconnect_error(result.disconnect),
        }
    }

    fn log_warnings(&self) {
        if let Some(error) = &self.disconnect_error {
            tracing::warn!(%error, "BLE disconnect failed after control request");
        }
        match &self.status {
            ControlStatus::ReadbackFailed(error) => {
                tracing::warn!(%error, "control readback failed");
            }
            ControlStatus::BleRequestFailed(error) => {
                tracing::warn!(error = %error, "control request failed");
            }
            ControlStatus::Confirmed
            | ControlStatus::Mismatch
            | ControlStatus::DeviceSelectionFailed => {}
        }
    }

    fn into_response_parts(self) -> (bool, &'static str, Option<DeviceSnapshot>) {
        let (success, message) = match self.status {
            ControlStatus::Confirmed => (true, "command acknowledged and readback matched"),
            ControlStatus::ReadbackFailed(_) => {
                (false, "command was not confirmed; readback failed")
            }
            ControlStatus::Mismatch => (
                false,
                "command was not confirmed; acknowledgement or readback differed",
            ),
            ControlStatus::DeviceSelectionFailed => {
                (false, "command was not confirmed; device selection failed")
            }
            ControlStatus::BleRequestFailed(_) => {
                (false, "command was not confirmed; BLE request failed")
            }
        };
        (success, message, self.snapshot)
    }
}

fn control_outcome(result: Result<ProbeResult, ProbeError>) -> ControlOutcome {
    match result {
        Ok(ProbeResult::Queried { result, .. }) => ControlOutcome::from_query(result),
        Ok(
            ProbeResult::NoDevices
            | ProbeResult::Discovered { .. }
            | ProbeResult::Ambiguous { .. }
            | ProbeResult::DiscoveryIncomplete { .. },
        ) => ControlOutcome {
            status: ControlStatus::DeviceSelectionFailed,
            snapshot: None,
            disconnect_error: None,
        },
        Err(error) => ControlOutcome {
            status: ControlStatus::BleRequestFailed(error),
            snapshot: None,
            disconnect_error: None,
        },
    }
}

pub(crate) async fn serve(
    device_id: Option<String>,
    identity_store: Option<std::path::PathBuf>,
    address: SocketAddr,
    allow_remote: bool,
    mqtt_config: Option<crate::mqtt::MqttConfig>,
) -> Result<()> {
    validate_bind_address(address, allow_remote)?;
    let listener = TcpListener::bind(address)
        .await
        .context("could not bind HTTP listener")?;
    let registry = DeviceRegistry::load_optional(identity_store)
        .context("could not load local device identity mappings")?;
    let mut state = match device_id {
        Some(device_id) => ApiState::with_ble_device(DEFAULT_FRESHNESS_LIMIT, device_id, registry),
        None => ApiState::with_registry(registry),
    };
    if let Some(config) = mqtt_config {
        state.start_mqtt(config).await?;
    }
    let app = router(state.clone());
    if state.ble_device.is_some() {
        tokio::spawn(poll_device(state, DEFAULT_POLL_INTERVAL));
    }

    tracing::info!(%address, "Updraft API listening");
    axum::serve(listener, app)
        .await
        .context("HTTP server failed")
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
        .route("/api/v2/devices", get(devices_v2))
        .route("/api/v2/devices/{id}/state", get(device_state_v2))
        .route("/api/v2/devices/{id}/control", post(control_device_v2))
        .with_state(state)
}

#[derive(Serialize)]
struct DeviceListV2Response {
    devices: Vec<crate::device::DeviceDescriptor>,
}

async fn devices_v2(State(state): State<ApiState>) -> Json<DeviceListV2Response> {
    Json(DeviceListV2Response {
        devices: state.registry.read().await.descriptors().cloned().collect(),
    })
}

#[derive(Serialize)]
struct DeviceStateV2Response {
    id: DeviceId,
    backend: DeviceBackend,
    available: bool,
    inventory_status: crate::backend::DeviceInventoryStatus,
    last_error: Option<String>,
    state: Option<DeviceState>,
}

async fn device_state_v2(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> Result<Json<DeviceStateV2Response>, StatusCode> {
    let id = DeviceId::parse(id).ok_or(StatusCode::NOT_FOUND)?;
    let registry = state.registry.read().await;
    let descriptor = registry
        .descriptors()
        .find(|descriptor| descriptor.id == id)
        .ok_or(StatusCode::NOT_FOUND)?;
    let runtime = registry.runtime(&id).ok_or(StatusCode::NOT_FOUND)?;
    let snapshot = runtime.snapshot().await;
    let (last_error, inventory_status) = match (&descriptor.backend, state.ble_device.as_ref()) {
        (DeviceBackend::LegacyBle, Some(device)) if id == DeviceId::configured_ble() => {
            let reconciler = device.reconciler.read().await;
            let poll_error = reconciler.last_error().map(str::to_owned);
            let inventory_status = if poll_error.is_some() && reconciler.latest_snapshot().is_none()
            {
                crate::backend::DeviceInventoryStatus::Unavailable
            } else {
                snapshot.inventory_status
            };
            (poll_error.or(snapshot.last_error), inventory_status)
        }
        _ => (snapshot.last_error, snapshot.inventory_status),
    };
    Ok(Json(DeviceStateV2Response {
        id,
        backend: descriptor.backend,
        available: snapshot.state.is_some(),
        inventory_status,
        last_error,
        state: snapshot.state,
    }))
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
struct DeviceControlV2Request {
    request_id: CommandId,
    issued_at_unix_ms: u64,
    command: DeviceCommand,
}

#[derive(Clone, Debug, Serialize)]
struct DeviceControlV2Response {
    request_id: String,
    status: &'static str,
}

const V2_CONTROL_MAX_AGE: Duration = Duration::from_secs(30);
const V2_CONTROL_MAX_FUTURE_SKEW: Duration = Duration::from_secs(5);
const V2_REPLAY_CAPACITY: usize = 64;

#[derive(Default)]
struct RecentV2ControlResults(HashMap<DeviceId, Arc<tokio::sync::Mutex<V2DeviceControlHistory>>>);

#[derive(Default)]
struct V2DeviceControlHistory {
    completed: VecDeque<(CommandId, DeviceCommand, DeviceControlV2Response)>,
    in_flight: HashMap<
        CommandId,
        (
            DeviceCommand,
            tokio::sync::watch::Receiver<Option<DeviceControlV2Response>>,
        ),
    >,
}

enum V2ControlReservation {
    Execute(tokio::sync::watch::Sender<Option<DeviceControlV2Response>>),
    Wait(tokio::sync::watch::Receiver<Option<DeviceControlV2Response>>),
    Completed(DeviceControlV2Response),
}

async fn control_device_v2(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(request): Json<DeviceControlV2Request>,
) -> (StatusCode, Json<DeviceControlV2Response>) {
    let id = match DeviceId::parse(id) {
        Some(id) => id,
        None => {
            return v2_control_rejected(
                request.request_id,
                "unknown_device",
                StatusCode::NOT_FOUND,
            );
        }
    };
    if !v2_request_is_fresh(request.issued_at_unix_ms) {
        return v2_control_rejected(
            request.request_id,
            "stale_request",
            StatusCode::UNPROCESSABLE_ENTITY,
        );
    }

    let registered = state
        .registry
        .read()
        .await
        .descriptors()
        .any(|descriptor| descriptor.id == id);
    if !registered {
        return v2_control_rejected(request.request_id, "unknown_device", StatusCode::NOT_FOUND);
    }
    let history = Arc::clone(
        state
            .v2_control_results
            .lock()
            .await
            .0
            .entry(id.clone())
            .or_insert_with(|| {
                Arc::new(tokio::sync::Mutex::new(V2DeviceControlHistory::default()))
            }),
    );
    let reservation = {
        let mut history = history.lock().await;
        reserve_v2_control(&mut history, &request.request_id, request.command)
    };
    let mut receiver = match reservation {
        V2ControlReservation::Completed(response) => {
            return (status_for_v2_outcome(response.status), Json(response));
        }
        V2ControlReservation::Wait(receiver) => receiver,
        V2ControlReservation::Execute(sender) => {
            let receiver = sender.subscribe();
            let task_state = state.clone();
            let task_id = id.clone();
            let task_request = request.clone();
            drop(spawn_v2_control_execution(
                history,
                request.request_id.clone(),
                request.command,
                sender,
                async move { execute_v2_control(&task_state, &task_id, &task_request).await },
            ));
            receiver
        }
    };
    let Some(response) = receiver
        .wait_for(Option::is_some)
        .await
        .ok()
        .and_then(|response| response.clone())
    else {
        return v2_control_rejected(
            request.request_id,
            "control_failed",
            StatusCode::INTERNAL_SERVER_ERROR,
        );
    };
    (status_for_v2_outcome(response.status), Json(response))
}

async fn execute_v2_control(
    state: &ApiState,
    id: &DeviceId,
    request: &DeviceControlV2Request,
) -> DeviceControlV2Response {
    let backend = state.registry.read().await.dispatch(id, request.command);
    let outcome = match backend {
        Ok(DeviceBackend::LegacyBle) => {
            if state.ble_device.is_none() {
                "device_unavailable"
            } else if let DeviceCommand::LegacyPreset { preset } = request.command {
                match state
                    .execute_http_control(request.issued_at_unix_ms, preset)
                    .await
                {
                    Ok(response) if response.success => "confirmed",
                    Ok(_) => "unconfirmed",
                    Err(()) => "stale_request",
                }
            } else {
                "unsupported_command"
            }
        }
        Ok(DeviceBackend::QuickConnect) => match (
            state.quickconnect_control.as_ref(),
            quickconnect_command(request.command),
        ) {
            (Some(service), Some(command)) => {
                let Some(intent) = QuickConnectControlIntent::new(
                    request.request_id.as_str(),
                    request.issued_at_unix_ms,
                    command,
                ) else {
                    return DeviceControlV2Response {
                        request_id: request.request_id.as_str().to_owned(),
                        status: "invalid_request_id",
                    };
                };
                let result = service.execute(id, intent).await;
                quickconnect_status_name(result.status())
            }
            (None, _) => "backend_unavailable",
            (_, None) => "unsupported_command",
        },
        Err(crate::backend::DeviceRegistryError::UnknownDevice) => "unknown_device",
        Err(crate::backend::DeviceRegistryError::UnsupportedCommand) => "unsupported_command",
        Err(_) => "control_failed",
    };
    DeviceControlV2Response {
        request_id: request.request_id.as_str().to_owned(),
        status: outcome,
    }
}

fn spawn_v2_control_execution(
    history: Arc<tokio::sync::Mutex<V2DeviceControlHistory>>,
    request_id: CommandId,
    command: DeviceCommand,
    sender: tokio::sync::watch::Sender<Option<DeviceControlV2Response>>,
    execution: impl std::future::Future<Output = DeviceControlV2Response> + Send + 'static,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let response = execution.await;
        {
            let mut history = history.lock().await;
            history.in_flight.remove(&request_id);
            remember_v2_control_result(&mut history, request_id, command, response.clone());
        }
        sender.send_replace(Some(response));
    })
}

fn reserve_v2_control(
    history: &mut V2DeviceControlHistory,
    request_id: &CommandId,
    command: DeviceCommand,
) -> V2ControlReservation {
    if let Some((_, previous_command, response)) = history
        .completed
        .iter()
        .find(|(previous_id, _, _)| previous_id == request_id)
    {
        return V2ControlReservation::Completed(if *previous_command == command {
            response.clone()
        } else {
            DeviceControlV2Response {
                request_id: request_id.as_str().to_owned(),
                status: "request_id_reused",
            }
        });
    }
    if let Some((previous_command, receiver)) = history.in_flight.get(request_id) {
        if *previous_command != command {
            return V2ControlReservation::Completed(DeviceControlV2Response {
                request_id: request_id.as_str().to_owned(),
                status: "request_id_reused",
            });
        }
        if receiver.has_changed().is_ok() {
            return V2ControlReservation::Wait(receiver.clone());
        }
        history.in_flight.remove(request_id);
        let response = DeviceControlV2Response {
            request_id: request_id.as_str().to_owned(),
            status: "control_failed",
        };
        remember_v2_control_result(history, request_id.clone(), command, response.clone());
        return V2ControlReservation::Completed(response);
    }

    let (sender, receiver) = tokio::sync::watch::channel(None);
    history
        .in_flight
        .insert(request_id.clone(), (command, receiver));
    V2ControlReservation::Execute(sender)
}

fn remember_v2_control_result(
    history: &mut V2DeviceControlHistory,
    request_id: CommandId,
    command: DeviceCommand,
    response: DeviceControlV2Response,
) {
    history.completed.push_back((request_id, command, response));
    if history.completed.len() > V2_REPLAY_CAPACITY {
        history.completed.pop_front();
    }
}

fn v2_control_rejected(
    request_id: CommandId,
    outcome: &'static str,
    status: StatusCode,
) -> (StatusCode, Json<DeviceControlV2Response>) {
    (
        status,
        Json(DeviceControlV2Response {
            request_id: request_id.as_str().to_owned(),
            status: outcome,
        }),
    )
}

fn v2_request_is_fresh(issued_at_unix_ms: u64) -> bool {
    v2_request_is_fresh_at(issued_at_unix_ms, unix_millis(SystemTime::now()))
}

fn v2_request_is_fresh_at(issued_at_unix_ms: u64, now: Option<u64>) -> bool {
    let Some(now) = now else { return false };
    is_fresh_at(
        issued_at_unix_ms,
        now,
        V2_CONTROL_MAX_AGE,
        V2_CONTROL_MAX_FUTURE_SKEW,
    )
}

fn quickconnect_command(command: DeviceCommand) -> Option<QuickConnectCommand> {
    match command {
        DeviceCommand::QuickConnectMode { mode } => Some(QuickConnectCommand::SetMode {
            mode: match mode {
                crate::device::QuickConnectMode::Off => QuickConnectCommandMode::Off,
                crate::device::QuickConnectMode::Automatic => QuickConnectCommandMode::Automatic,
                crate::device::QuickConnectMode::Timer => QuickConnectCommandMode::Timer,
                crate::device::QuickConnectMode::Manual => QuickConnectCommandMode::Manual,
            },
        }),
        DeviceCommand::QuickConnectTargets {
            temperature_f,
            humidity_percent,
        } => Some(QuickConnectCommand::SetAutomaticTargets {
            temperature_f: Some(temperature_f),
            humidity_percent: Some(humidity_percent),
        }),
        DeviceCommand::QuickConnectTimerDuration { minutes } => {
            Some(QuickConnectCommand::SetTimerDuration {
                duration_minutes: minutes,
            })
        }
        DeviceCommand::LegacyPreset { .. } => None,
    }
}

fn quickconnect_status_name(status: QuickConnectControlStatus) -> &'static str {
    match status {
        QuickConnectControlStatus::Rejected => "rejected",
        QuickConnectControlStatus::SubmittedUnconfirmed => "submitted_unconfirmed",
        QuickConnectControlStatus::ReadbackMismatch => "readback_mismatch",
        QuickConnectControlStatus::ReadbackUnavailable => "readback_unavailable",
        QuickConnectControlStatus::Confirmed => "confirmed",
    }
}

fn status_for_v2_outcome(outcome: &str) -> StatusCode {
    match outcome {
        "confirmed" => StatusCode::OK,
        "unknown_device" | "device_unavailable" => StatusCode::NOT_FOUND,
        "backend_unavailable" => StatusCode::SERVICE_UNAVAILABLE,
        "invalid_request_id" => StatusCode::BAD_REQUEST,
        "control_failed" => StatusCode::INTERNAL_SERVER_ERROR,
        "unconfirmed" | "submitted_unconfirmed" | "readback_mismatch" | "readback_unavailable" => {
            StatusCode::BAD_GATEWAY
        }
        _ => StatusCode::UNPROCESSABLE_ENTITY,
    }
}

async fn health() -> Json<HealthResponse> {
    Json(HealthResponse { status: "ok" })
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

const CONTROL_REPLAY_CAPACITY: usize = 64;

#[derive(Default)]
struct RecentControlResults(VecDeque<(CommandId, ControlPreset, Arc<ControlResponse>)>);

impl RecentControlResults {
    fn get(&self, request_id: &CommandId, preset: ControlPreset) -> Option<Arc<ControlResponse>> {
        let (_, seen_preset, response) = self
            .0
            .iter()
            .find(|(seen_id, _, _)| seen_id == request_id)?;
        Some(if *seen_preset == preset {
            Arc::clone(response)
        } else {
            Arc::new(ControlResponse::rejected(
                preset,
                "request_id was reused for another preset",
            ))
        })
    }

    fn insert(
        &mut self,
        request_id: CommandId,
        preset: ControlPreset,
        response: Arc<ControlResponse>,
    ) {
        self.0.retain(|(seen_id, _, _)| seen_id != &request_id);
        self.0.push_back((request_id, preset, response));
        if self.0.len() > CONTROL_REPLAY_CAPACITY {
            self.0.pop_front();
        }
    }
}

async fn process_mqtt_controls(
    state: ApiState,
    controls: mpsc::Receiver<crate::mqtt::MqttControlWork>,
) {
    control_requests(controls)
        .fold(RecentControlResults::default(), |recent, work| {
            reply_to_control_request(&state, recent, work)
        })
        .await;
}

fn control_requests(
    controls: mpsc::Receiver<crate::mqtt::MqttControlWork>,
) -> impl Stream<Item = crate::mqtt::MqttControlWork> {
    stream::unfold(controls, receive_control_request)
}

async fn receive_control_request(
    mut controls: mpsc::Receiver<crate::mqtt::MqttControlWork>,
) -> Option<(
    crate::mqtt::MqttControlWork,
    mpsc::Receiver<crate::mqtt::MqttControlWork>,
)> {
    let work = controls.recv().await?;
    Some((work, controls))
}

async fn reply_to_control_request(
    state: &ApiState,
    mut recent: RecentControlResults,
    work: crate::mqtt::MqttControlWork,
) -> RecentControlResults {
    let response = state
        .replay_or_execute_control(&work.request, &mut recent)
        .await;
    let _ = work.reply.send(response);
    recent
}

async fn device_state_response(device: &LegacyBleRuntime) -> DeviceStateResponse {
    let reconciler = device.reconciler.read().await;
    let now = Instant::now();
    let freshness = reconciler.freshness_at(now, device.freshness_limit);
    let snapshot = reconciler.current_snapshot_at(now, device.freshness_limit);
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
        project_legacy_snapshot(snapshot).map(|projection| projection.values)
    }
}

struct LegacyStateProjection {
    device_state: DeviceState,
    values: StateValues,
}

fn project_legacy_snapshot(snapshot: &DeviceSnapshot) -> Option<LegacyStateProjection> {
    let identity = snapshot.identity.decoded().ok()?;
    let mode = snapshot.mode.decoded().ok()?;
    let sensors = snapshot.sensors.decoded().ok()?;
    let thresholds = snapshot.thresholds.decoded().ok()?;
    let timer = snapshot.timer.decoded().ok()?;
    let version = identity.firmware_version;
    let firmware_version = format!("{}.{}.{}", version.major, version.minor, version.patch);
    let device_state = DeviceState {
        temperature_f: Some(tenths_to_decimal(sensors.temperature.value())),
        humidity_percent: Some(tenths_to_decimal(sensors.humidity.value())),
        settings: DeviceSettings::LegacyBle {
            mode: Some(match mode.mode {
                updraft_protocol::OperatingMode::Automatic => LegacyMode::Automatic,
                updraft_protocol::OperatingMode::Timer => LegacyMode::Timer,
                updraft_protocol::OperatingMode::Ota => LegacyMode::Ota,
            }),
            controller_fan_on: Some(mode.fan == updraft_protocol::FanState::On),
            automatic_temperature_tenths_f: Some(thresholds.temperature.value()),
            automatic_humidity_tenths_percent: Some(thresholds.humidity.value()),
            timer_remaining_minutes: Some(timer.remaining.value()),
            timer_original_minutes: Some(timer.original.value()),
        },
        estimated_running: None,
        diagnostics: Some(crate::device::DeviceDiagnostics {
            firmware_version: Some(firmware_version.clone()),
            signal_strength_raw: None,
            verified_raw: None,
            ota_in_progress: None,
        }),
        provenance: StateProvenance {
            backend: DeviceBackend::LegacyBle,
            fetched_at_unix_ms: unix_millis(SystemTime::now()),
            observed_at_unix_ms: unix_millis(snapshot.observed_at),
        },
    };
    let values = StateValues {
        firmware_version,
        mode: operating_mode_name(mode.mode),
        controller_fan_flag: fan_state_name(mode.fan),
        temperature_f: tenths_to_decimal(sensors.temperature.value()),
        humidity_percent: tenths_to_decimal(sensors.humidity.value()),
        automatic_temperature_threshold_f: tenths_to_decimal(thresholds.temperature.value()),
        automatic_humidity_threshold_percent: tenths_to_decimal(thresholds.humidity.value()),
        control_preset: ControlPreset::from_readback(mode.mode, *thresholds, *timer)
            .map(ControlPreset::as_str),
        timer_remaining_minutes: timer.remaining.value(),
        timer_original_minutes: timer.original.value(),
    };
    Some(LegacyStateProjection {
        device_state,
        values,
    })
}

async fn poll_device(state: ApiState, poll_interval: Duration) {
    poll_ticks(poll_interval)
        .for_each(|()| state.poll_and_publish_state())
        .await;
}

fn poll_ticks(poll_interval: Duration) -> impl Stream<Item = ()> {
    let mut ticker = interval(poll_interval);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    stream::unfold(ticker, wait_for_poll_tick)
}

async fn wait_for_poll_tick(mut ticker: Interval) -> Option<((), Interval)> {
    ticker.tick().await;
    Some(((), ticker))
}

fn record_poll_result(
    reconciler: &mut StateReconciler,
    poll_id: u64,
    result: Result<ProbeResult, ProbeError>,
) {
    match result {
        Ok(ProbeResult::Queried { result, .. }) => record_query_result(reconciler, poll_id, result),
        Ok(ProbeResult::NoDevices) => {
            reconciler.apply_failure(poll_id, "no compatible device found");
        }
        Ok(ProbeResult::Ambiguous { .. }) => {
            reconciler.apply_failure(poll_id, "device selection was ambiguous");
        }
        Ok(ProbeResult::DiscoveryIncomplete { .. }) => {
            reconciler.apply_failure(poll_id, "device discovery was incomplete");
        }
        Ok(ProbeResult::Discovered { .. }) => {
            reconciler.apply_failure(poll_id, "device was not queried");
        }
        Err(error) => {
            tracing::warn!(%error, "BLE state poll failed");
            reconciler.apply_failure(poll_id, probe_error_message(&error));
        }
    }
}

fn record_query_result(reconciler: &mut StateReconciler, poll_id: u64, result: QueryResult) {
    if let DisconnectOutcome::Failed(error) = &result.disconnect {
        tracing::warn!(%error, "BLE disconnect failed after state poll");
    }
    match result.snapshot {
        Some(snapshot) => {
            reconciler.apply_success(poll_id, snapshot);
        }
        None => {
            reconciler.apply_failure(
                poll_id,
                result
                    .state_error
                    .unwrap_or_else(|| "state query returned no snapshot".to_owned()),
            );
        }
    }
}

fn probe_error_message(error: &ProbeError) -> &'static str {
    match error.kind() {
        ProbeErrorKind::Unavailable => "BLE unavailable",
        ProbeErrorKind::Authentication => "BLE permission or authentication failed",
        ProbeErrorKind::Protocol => "GAF protocol error",
    }
}

fn tenths_to_decimal(value: u16) -> f64 {
    f64::from(value) / 10.0
}

const fn operating_mode_name(mode: updraft_protocol::OperatingMode) -> &'static str {
    match mode {
        updraft_protocol::OperatingMode::Automatic => "automatic",
        updraft_protocol::OperatingMode::Timer => "timer",
        updraft_protocol::OperatingMode::Ota => "ota",
    }
}

const fn fan_state_name(fan: updraft_protocol::FanState) -> &'static str {
    match fan {
        updraft_protocol::FanState::On => "on",
        updraft_protocol::FanState::Off => "off",
    }
}

const fn freshness_name(freshness: StateFreshness) -> &'static str {
    match freshness {
        StateFreshness::Unknown => "unknown",
        StateFreshness::Fresh => "fresh",
        StateFreshness::Stale => "stale",
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf, time::SystemTime};

    use axum::{body::Body, http::Request};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    use super::*;

    fn reservation_is_wait(reservation: V2ControlReservation) -> bool {
        match reservation {
            V2ControlReservation::Wait(_) => true,
            V2ControlReservation::Execute(_) | V2ControlReservation::Completed(_) => false,
        }
    }

    fn reservation_is_reused(reservation: V2ControlReservation) -> bool {
        match reservation {
            V2ControlReservation::Completed(response) => response.status == "request_id_reused",
            V2ControlReservation::Execute(_) | V2ControlReservation::Wait(_) => false,
        }
    }

    fn reservation_has_status(reservation: V2ControlReservation, status: &str) -> bool {
        match reservation {
            V2ControlReservation::Completed(response) => response.status == status,
            V2ControlReservation::Execute(_) | V2ControlReservation::Wait(_) => false,
        }
    }

    fn reservation_is_execute(
        reservation: V2ControlReservation,
    ) -> Option<tokio::sync::watch::Sender<Option<DeviceControlV2Response>>> {
        match reservation {
            V2ControlReservation::Execute(sender) => Some(sender),
            V2ControlReservation::Wait(_) | V2ControlReservation::Completed(_) => None,
        }
    }

    #[test]
    fn v2_control_reservations_deduplicate_without_serializing_distinct_commands() {
        let mut history = V2DeviceControlHistory::default();
        let request_id = CommandId::parse("same-request").unwrap();
        let other_request_id = CommandId::parse("newer-request").unwrap();
        let command = DeviceCommand::QuickConnectMode {
            mode: crate::device::QuickConnectMode::Automatic,
        };
        let sender = reservation_is_execute(reserve_v2_control(&mut history, &request_id, command));
        assert!(sender.is_some());
        let Some(sender) = sender else {
            return;
        };
        assert!(reservation_is_wait(reserve_v2_control(
            &mut history,
            &request_id,
            command
        ),));
        assert!(reservation_is_reused(reserve_v2_control(
            &mut history,
            &request_id,
            DeviceCommand::LegacyPreset {
                preset: ControlPreset::TimerClear
            }
        )));
        assert!(
            reservation_is_execute(reserve_v2_control(&mut history, &other_request_id, command))
                .is_some()
        );
        drop(sender);
        assert!(reservation_has_status(
            reserve_v2_control(&mut history, &request_id, command),
            "control_failed"
        ));
    }

    #[tokio::test]
    async fn v2_control_execution_survives_waiter_cancellation_and_records_result() {
        let request_id = CommandId::parse("cancelled-waiter").unwrap();
        let command = DeviceCommand::LegacyPreset {
            preset: ControlPreset::TimerClear,
        };
        let history = Arc::new(tokio::sync::Mutex::new(V2DeviceControlHistory::default()));
        let reservation = reserve_v2_control(&mut *history.lock().await, &request_id, command);
        let sender = reservation_is_execute(reservation);
        assert!(sender.is_some());
        let Some(sender) = sender else {
            return;
        };
        let cancelled_waiter = sender.subscribe();
        drop(cancelled_waiter);
        let (started_sender, started_receiver) = tokio::sync::oneshot::channel();
        let (finish_sender, finish_receiver) = tokio::sync::oneshot::channel();
        let response = DeviceControlV2Response {
            request_id: request_id.as_str().to_owned(),
            status: "unconfirmed",
        };
        let execution = spawn_v2_control_execution(
            Arc::clone(&history),
            request_id.clone(),
            command,
            sender,
            async move {
                let _ = started_sender.send(());
                let _ = finish_receiver.await;
                response
            },
        );
        let _ = started_receiver.await;

        assert!(finish_sender.send(()).is_ok());
        assert!(execution.await.is_ok());
        let history = history.lock().await;
        assert!(!history.in_flight.contains_key(&request_id));
        assert_eq!(history.completed.front().unwrap().2.status, "unconfirmed");
    }

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

    fn identity_store_path() -> PathBuf {
        std::env::temp_dir()
            .join(format!("updraft-api-identities-{}", uuid::Uuid::new_v4()))
            .join("identities.json")
    }

    async fn state_response(state: ApiState) -> serde_json::Value {
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

    #[tokio::test]
    async fn routes_report_health_v2_capabilities_and_unknown_state() {
        let state = ApiState::with_ble_device(
            DEFAULT_FRESHNESS_LIMIT,
            "private-peripheral-id".to_owned(),
            DeviceRegistry::new(),
        );
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
                    .uri("/api/v2/devices")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = devices.into_body().collect().await.unwrap().to_bytes();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["devices"].as_array().unwrap().len(), 1);
        assert_eq!(body["devices"][0]["id"], DEVICE_ID);
        assert_eq!(body["devices"][0]["backend"], "legacy_ble");
        assert_eq!(body["devices"][0]["capabilities"]["read_state"], true);
        assert!(!body.to_string().contains("private-peripheral-id"));

        let state = app
            .oneshot(
                Request::builder()
                    .uri("/api/v2/devices/configured/state")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = state.into_body().collect().await.unwrap().to_bytes();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["available"], false);
        assert!(body["state"].is_null());
    }

    #[tokio::test]
    async fn v1_http_routes_are_absent() {
        let response = router(ApiState::with_registry(DeviceRegistry::new()))
            .oneshot(
                Request::builder()
                    .uri("/api/v1/devices")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn startup_without_ble_has_empty_inventory_and_no_configured_state() {
        let app = router(ApiState::with_registry(DeviceRegistry::new()));
        let inventory = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/v2/devices")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = inventory.into_body().collect().await.unwrap().to_bytes();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["devices"], serde_json::json!([]));

        let state = app
            .oneshot(
                Request::builder()
                    .uri("/api/v2/devices/configured/state")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(state.status(), StatusCode::NOT_FOUND);

        let response = router(ApiState::with_registry(DeviceRegistry::new()))
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v2/devices/configured/control")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "request_id": "no-ble-device",
                            "issued_at_unix_ms": unix_millis(SystemTime::now()).unwrap(),
                            "command": {"kind": "legacy_preset", "preset": "timer_clear"}
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn cloud_only_and_mixed_startup_never_alias_cloud_devices_to_configured() {
        let path = identity_store_path();
        let mut cloud_registry = DeviceRegistry::load(&path).unwrap();
        let cloud_ids = cloud_registry
            .reconcile_quickconnect(
                "account-private",
                &[
                    crate::backend::CloudDeviceInput::new(
                        "provider-private-one".to_owned(),
                        "Attic cloud fan".to_owned(),
                    ),
                    crate::backend::CloudDeviceInput::new(
                        "provider-private-two".to_owned(),
                        "Guest cloud fan".to_owned(),
                    ),
                ],
            )
            .unwrap();
        let cloud_only = router(ApiState::with_registry(cloud_registry));
        let inventory = cloud_only
            .oneshot(
                Request::builder()
                    .uri("/api/v2/devices")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = inventory.into_body().collect().await.unwrap().to_bytes();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["devices"].as_array().unwrap().len(), 2);
        assert!(!body.to_string().contains("provider-private"));
        assert_ne!(cloud_ids[0].as_str(), DEVICE_ID);
        assert_ne!(cloud_ids[1].as_str(), DEVICE_ID);

        let mut mixed_registry = DeviceRegistry::load(&path).unwrap();
        mixed_registry
            .reconcile_quickconnect(
                "account-private",
                &[
                    crate::backend::CloudDeviceInput::new(
                        "provider-private-two".to_owned(),
                        "Guest cloud fan".to_owned(),
                    ),
                    crate::backend::CloudDeviceInput::new(
                        "provider-private-one".to_owned(),
                        "Attic cloud fan".to_owned(),
                    ),
                ],
            )
            .unwrap();
        let mixed = router(ApiState::with_ble_device(
            DEFAULT_FRESHNESS_LIMIT,
            "private-ble-id".to_owned(),
            mixed_registry,
        ));
        let inventory = mixed
            .oneshot(
                Request::builder()
                    .uri("/api/v2/devices")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = inventory.into_body().collect().await.unwrap().to_bytes();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["devices"].as_array().unwrap().len(), 3);
        assert!(!body.to_string().contains("provider-private"));
        fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[tokio::test]
    async fn v2_discovery_lists_backends_and_rejects_cloud_command_for_ble() {
        let path = identity_store_path();
        let mut registry = DeviceRegistry::load(&path).unwrap();
        let cloud_ids = registry
            .reconcile_quickconnect(
                "synthetic-account",
                &[
                    crate::backend::CloudDeviceInput::new(
                        "synthetic-provider-a".to_owned(),
                        "Attic fan".to_owned(),
                    ),
                    crate::backend::CloudDeviceInput::new(
                        "synthetic-provider-b".to_owned(),
                        "Guest fan".to_owned(),
                    ),
                ],
            )
            .unwrap();
        registry.set_quickconnect_writes_enabled(true);
        let app = router(ApiState::with_ble_device(
            DEFAULT_FRESHNESS_LIMIT,
            "synthetic-ble-id".to_owned(),
            registry,
        ));

        let inventory = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/v2/devices")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(inventory.status(), StatusCode::OK);
        let bytes = inventory.into_body().collect().await.unwrap().to_bytes();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["devices"].as_array().unwrap().len(), 3);
        let devices = body["devices"].as_array().unwrap();
        let ble = devices
            .iter()
            .find(|device| device["backend"] == "legacy_ble")
            .unwrap();
        assert_eq!(ble["id"], "configured");
        let cloud = devices
            .iter()
            .filter(|device| device["backend"] == "quick_connect")
            .collect::<Vec<_>>();
        assert_eq!(cloud.len(), 2);
        assert!(
            cloud
                .iter()
                .all(|device| device["capabilities"]["read_state"] == true)
        );
        assert!(!body.to_string().contains("synthetic-provider"));
        assert!(!body.to_string().contains("synthetic-account"));

        let issued_at_unix_ms = unix_millis(SystemTime::now()).unwrap();
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v2/devices/configured/control")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "request_id": "reject-cloud-on-ble",
                            "issued_at_unix_ms": issued_at_unix_ms,
                            "command": {
                                "kind": "quick_connect_targets",
                                "temperature_f": 110,
                                "humidity_percent": 40
                            }
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let outcome: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(outcome["request_id"], "reject-cloud-on-ble");
        assert_eq!(outcome["status"], "unsupported_command");
        assert_eq!(cloud_ids.len(), 2);

        fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[tokio::test]
    async fn v2_controls_reject_unknown_fields_and_unknown_device_ids() {
        let app = router(ApiState::with_ble_device(
            DEFAULT_FRESHNESS_LIMIT,
            "synthetic-ble-id".to_owned(),
            DeviceRegistry::new(),
        ));
        let unknown_field = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v2/devices/configured/control")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "request_id": "strict-request",
                            "issued_at_unix_ms": unix_millis(SystemTime::now()).unwrap(),
                            "unexpected": true,
                            "command": {"kind": "legacy_preset", "preset": "timer_clear"}
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(unknown_field.status(), StatusCode::UNPROCESSABLE_ENTITY);

        let unknown_device = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v2/devices/not-registered/control")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "request_id": "unknown-device",
                            "issued_at_unix_ms": unix_millis(SystemTime::now()).unwrap(),
                            "command": {"kind": "legacy_preset", "preset": "timer_clear"}
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(unknown_device.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn v2_control_replay_returns_the_same_result_and_rejects_command_reuse() {
        let app = router(ApiState::with_ble_device(
            DEFAULT_FRESHNESS_LIMIT,
            "synthetic-ble-id".to_owned(),
            DeviceRegistry::new(),
        ));
        let request = |command| {
            serde_json::json!({
                "request_id": "replay-id",
                "issued_at_unix_ms": unix_millis(SystemTime::now()).unwrap(),
                "command": command
            })
            .to_string()
        };
        let send = |app: Router, body: String| async move {
            app.oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v2/devices/configured/control")
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap()
        };
        let first = send(
            app.clone(),
            request(serde_json::json!({
                "kind": "quick_connect_targets",
                "temperature_f": 110,
                "humidity_percent": 40
            })),
        )
        .await;
        let first_body = first.into_body().collect().await.unwrap().to_bytes();
        let replay = send(
            app.clone(),
            request(serde_json::json!({
                "kind": "quick_connect_targets",
                "temperature_f": 110,
                "humidity_percent": 40
            })),
        )
        .await;
        let replay_body = replay.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(first_body, replay_body);

        let reused = send(
            app,
            request(serde_json::json!({"kind": "quick_connect_mode", "mode": "automatic"})),
        )
        .await;
        assert_eq!(reused.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let bytes = reused.into_body().collect().await.unwrap().to_bytes();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["status"], "request_id_reused");
    }

    #[tokio::test]
    async fn control_route_rejects_unknown_commands_before_touching_ble() {
        let app = router(ApiState::with_registry(DeviceRegistry::new()));
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v2/devices/configured/control")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "request_id": "unknown-command",
                            "issued_at_unix_ms": unix_millis(SystemTime::now()).unwrap(),
                            "command": {"kind": "legacy_preset", "preset": "timer_999"}
                        })
                        .to_string(),
                    ))
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
    fn mqtt_control_requests_reject_unknown_fields() {
        assert!(
            serde_json::from_str::<crate::control::ControlRequest>(
                r#"{"preset":"timer_clear","duration_minutes":999}"#
            )
            .is_err()
        );
    }

    #[test]
    fn mqtt_control_request_ids_replay_once_and_reject_preset_changes() {
        let id = CommandId::parse("ha-command-1").unwrap();
        let mut recent = RecentControlResults::default();
        let response = ControlResponse {
            success: true,
            preset: ControlPreset::TimerOneMinute,
            message: "command acknowledged and readback matched",
            state: None,
        };
        recent.insert(
            id.clone(),
            ControlPreset::TimerOneMinute,
            Arc::new(response),
        );

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

        (0..=CONTROL_REPLAY_CAPACITY).for_each(|index| {
            recent.insert(
                CommandId::parse(&format!("command-{index}")).unwrap(),
                ControlPreset::TimerClear,
                Arc::new(ControlResponse::rejected(
                    ControlPreset::TimerClear,
                    "not confirmed",
                )),
            );
        });
        assert!(recent.0.len() <= CONTROL_REPLAY_CAPACITY);
    }

    #[tokio::test]
    async fn state_route_reports_normalized_state_and_expired_state_as_unavailable() {
        let state = ApiState::with_ble_device(
            Duration::from_secs(60),
            "private-peripheral-id".to_owned(),
            DeviceRegistry::new(),
        );
        let projection =
            project_legacy_snapshot(&snapshot_at(Instant::now(), SystemTime::now())).unwrap();
        state
            .registry
            .read()
            .await
            .runtime(&DeviceId::configured_ble())
            .unwrap()
            .set_state(projection.device_state)
            .await;
        let fresh = state_response(state.clone()).await;
        assert_eq!(fresh["available"], true);
        assert_eq!(fresh["backend"], "legacy_ble");
        assert_eq!(fresh["state"]["temperature_f"], 97.0);
        assert_eq!(fresh["state"]["provenance"]["backend"], "legacy_ble");

        let mut expired_state = project_legacy_snapshot(&snapshot_at(
            Instant::now(),
            SystemTime::now() - Duration::from_secs(120),
        ))
        .unwrap()
        .device_state;
        expired_state.provenance.fetched_at_unix_ms =
            unix_millis(SystemTime::now() - Duration::from_secs(120));
        state
            .registry
            .read()
            .await
            .runtime(&DeviceId::configured_ble())
            .unwrap()
            .set_state(expired_state)
            .await;
        let expired = state_response(state).await;
        assert_eq!(expired["available"], false);
        assert!(expired["state"].is_null());
    }

    #[tokio::test]
    async fn initial_ble_poll_failure_reports_unavailable_inventory_and_error() {
        let state = ApiState::with_ble_device(
            DEFAULT_FRESHNESS_LIMIT,
            "synthetic-ble-id".to_owned(),
            DeviceRegistry::new(),
        );
        let ble = state.ble_device.as_ref().unwrap();
        let mut reconciler = ble.reconciler.write().await;
        let poll_id = reconciler.begin_poll();
        reconciler.apply_failure(poll_id, "BLE query failed");
        drop(reconciler);

        let response = state_response(state).await;

        assert_eq!(response["available"], false);
        assert_eq!(response["inventory_status"], "unavailable");
        assert_eq!(response["last_error"], "BLE query failed");
        assert!(response["state"].is_null());
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
