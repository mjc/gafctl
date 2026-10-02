use std::{
    collections::{HashMap, VecDeque},
    net::SocketAddr,
    sync::Arc,
    time::{Duration, SystemTime},
};

use crate::backend::{DeviceRegistry, DeviceRuntime, RefreshReceiver, RefreshReservation};
#[cfg(test)]
use crate::control::ControlPreset;
use crate::control::{CommandId, is_fresh_at, unix_millis};
use crate::device::{
    DeviceBackend, DeviceCommand, DeviceDescriptor, DeviceId, DeviceSettings, DeviceState,
    EntitySource, EntitySources, LegacyMode, StateProvenance,
};
use anyhow::{Context, Result};
use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post, put},
};
#[cfg(feature = "mqtt")]
use futures_util::TryStreamExt;
use futures_util::{Stream, StreamExt, stream};
use serde::Serialize;
#[cfg(feature = "mqtt")]
use tokio::sync::{mpsc, watch};
use tokio::{
    net::TcpListener,
    sync::RwLock,
    time::{Interval, MissedTickBehavior, interval},
};
use updraft_api::{
    ControlStatus as V2ControlStatus, DeviceListV2Response, DeviceRefreshStatus,
    DeviceRefreshV2Response, DeviceStateV2Response,
};
pub(crate) use updraft_api::{DeviceControlV2Request, DeviceControlV2Response};
use updraft_bluetooth::{
    DisconnectOutcome, ProbeClient, ProbeError, ProbeErrorKind, ProbeMode, ProbeOptions,
    ProbeResult, QueryResult,
};
use updraft_protocol::{DeviceSnapshot, StateReconciler};
use updraft_quickconnect::{
    QuickConnectClient, QuickConnectCommand, QuickConnectCommandMode, QuickConnectConfig,
};

use crate::quickconnect_control::{
    QuickConnectControlIntent, QuickConnectControlService, QuickConnectControlStatus,
};

#[cfg(test)]
const DEVICE_ID: &str = "configured";
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(30);
const DEVICE_REFRESH_TIMEOUT: Duration = Duration::from_secs(270);

#[derive(Clone)]
struct ApiState {
    registry: Arc<RwLock<DeviceRegistry>>,
    ble_device: Option<Arc<LegacyBleRuntime>>,
    #[cfg(feature = "mqtt")]
    mqtt_updates: Option<watch::Sender<Arc<crate::mqtt::MqttStateSnapshot>>>,
    #[cfg(feature = "mqtt")]
    mqtt_discovery_enabled: bool,
    quickconnect_control: Option<QuickConnectControlService>,
    quickconnect_runtime: Option<QuickConnectRuntime>,
    v2_control_results: Arc<tokio::sync::Mutex<RecentV2ControlResults>>,
}

#[derive(Clone)]
struct QuickConnectRuntime {
    account_id: String,
    client: QuickConnectClient,
}

struct LegacyBleRuntime {
    reconciler: Arc<RwLock<StateReconciler>>,
    device: Arc<DeviceRuntime>,
    ble_client: Arc<ProbeClient>,
    peripheral_id: Arc<str>,
}

#[derive(Debug, Eq, PartialEq)]
enum ControlAdmissionError {
    BackendUnavailable,
    StaleRequest,
    Busy,
    ReadbackUnavailable,
}

impl ControlAdmissionError {
    const fn status(self) -> V2ControlStatus {
        match self {
            Self::BackendUnavailable => V2ControlStatus::BackendUnavailable,
            Self::StaleRequest => V2ControlStatus::StaleRequest,
            Self::Busy => V2ControlStatus::Busy,
            Self::ReadbackUnavailable => V2ControlStatus::ReadbackUnavailable,
        }
    }
}

impl ApiState {
    fn with_registry(registry: DeviceRegistry) -> Self {
        Self {
            registry: Arc::new(RwLock::new(registry)),
            ble_device: None,
            #[cfg(feature = "mqtt")]
            mqtt_updates: None,
            #[cfg(feature = "mqtt")]
            mqtt_discovery_enabled: false,
            quickconnect_control: None,
            quickconnect_runtime: None,
            v2_control_results: Arc::default(),
        }
    }

    fn with_ble_device(device_id: String, mut registry: DeviceRegistry) -> Self {
        let device = registry.register_configured_ble();
        Self {
            registry: Arc::new(RwLock::new(registry)),
            ble_device: Some(Arc::new(LegacyBleRuntime::new(device_id, device))),
            #[cfg(feature = "mqtt")]
            mqtt_updates: None,
            #[cfg(feature = "mqtt")]
            mqtt_discovery_enabled: false,
            quickconnect_control: None,
            quickconnect_runtime: None,
            v2_control_results: Arc::default(),
        }
    }

    async fn execute_http_control(
        &self,
        issued_at_unix_ms: u64,
        command: DeviceCommand,
    ) -> Result<bool, ControlAdmissionError> {
        let device = self
            .ble_device
            .as_ref()
            .ok_or(ControlAdmissionError::BackendUnavailable)?;
        device
            .execute_http_control(self, issued_at_unix_ms, command)
            .await
    }

    async fn poll_and_publish_state(&self) {
        if self.ble_device.is_some() {
            if let Err(status) = self.refresh_device(&DeviceId::configured_ble()).await {
                tracing::warn!(%status, "device refresh worker failed");
            }
        } else {
            self.publish_state().await;
        }
    }

    async fn refresh_device(
        &self,
        id: &DeviceId,
    ) -> Result<Arc<DeviceRefreshV2Response>, StatusCode> {
        let (backend, runtime) = self.refresh_target(id).await?;
        let receiver = match runtime.reserve_refresh().await {
            RefreshReservation::Join(receiver) => receiver,
            RefreshReservation::Execute {
                receiver,
                completion,
            } => {
                let state = self.clone();
                let id = id.clone();
                tokio::spawn(async move {
                    if let Ok(response) = state.execute_device_refresh(&id, backend, &runtime).await
                    {
                        completion.send_replace(Some(Arc::new(response)));
                    }
                });
                receiver
            }
        };
        wait_for_device_refresh(receiver).await
    }

    async fn refresh_target(
        &self,
        id: &DeviceId,
    ) -> Result<(DeviceBackend, Arc<DeviceRuntime>), StatusCode> {
        let registry = self.registry.read().await;
        let descriptor = registry
            .descriptors()
            .find(|device| &device.id == id)
            .ok_or(StatusCode::NOT_FOUND)?;
        if !descriptor.capabilities.read_state {
            return Err(StatusCode::UNPROCESSABLE_ENTITY);
        }
        let configured = match descriptor.backend {
            DeviceBackend::LegacyBle => self.ble_device.is_some(),
            DeviceBackend::QuickConnect => self.quickconnect_runtime.is_some(),
        };
        if !configured {
            return Err(StatusCode::SERVICE_UNAVAILABLE);
        }
        Ok((
            descriptor.backend,
            registry.runtime(id).ok_or(StatusCode::NOT_FOUND)?,
        ))
    }

    async fn execute_device_refresh(
        &self,
        id: &DeviceId,
        backend: DeviceBackend,
        runtime: &DeviceRuntime,
    ) -> Result<DeviceRefreshV2Response, StatusCode> {
        let response = match tokio::time::timeout(
            DEVICE_REFRESH_TIMEOUT,
            self.read_device_locked(id, backend, runtime),
        )
        .await
        {
            Ok(response) => response?,
            Err(_) => {
                let mut device = device_state_v2_data(self, id).await?;
                device.last_error = Some("device refresh deadline exceeded".to_owned());
                DeviceRefreshV2Response {
                    status: DeviceRefreshStatus::Failed,
                    device,
                }
            }
        };
        self.publish_state().await;
        Ok(response)
    }

    async fn read_device_locked(
        &self,
        id: &DeviceId,
        backend: DeviceBackend,
        runtime: &DeviceRuntime,
    ) -> Result<DeviceRefreshV2Response, StatusCode> {
        let _transaction = runtime.acquire_transaction().await;
        let status = match backend {
            DeviceBackend::LegacyBle => {
                self.ble_device
                    .as_ref()
                    .ok_or(StatusCode::SERVICE_UNAVAILABLE)?
                    .read_state_locked()
                    .await
            }
            DeviceBackend::QuickConnect => self.read_quickconnect_state_locked(id, runtime).await?,
        };
        Ok(DeviceRefreshV2Response {
            status,
            device: device_state_v2_data(self, id).await?,
        })
    }

    async fn read_quickconnect_state_locked(
        &self,
        id: &DeviceId,
        runtime: &DeviceRuntime,
    ) -> Result<DeviceRefreshStatus, StatusCode> {
        let cloud = self
            .quickconnect_runtime
            .as_ref()
            .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
        let (_, provider_id) = self
            .registry
            .read()
            .await
            .quickconnect_read_target(&cloud.account_id, id)
            .map_err(|_| StatusCode::NOT_FOUND)?;
        let generation = runtime.begin_state_read();
        match cloud.client.read_device_state(&provider_id).await {
            Ok(state) => Ok(
                if runtime
                    .set_state_if_current(generation, crate::backend::common_state(state))
                    .await
                {
                    DeviceRefreshStatus::Fresh
                } else {
                    DeviceRefreshStatus::Superseded
                },
            ),
            Err(error) => {
                tracing::warn!(%error, "QuickConnect device refresh failed");
                Ok(
                    if runtime.mark_detail_unavailable_if_current(generation).await {
                        DeviceRefreshStatus::Failed
                    } else {
                        DeviceRefreshStatus::Superseded
                    },
                )
            }
        }
    }

    async fn start_quickconnect(
        &mut self,
        config: crate::cli::QuickConnectRuntimeConfig,
    ) -> Result<()> {
        let client = QuickConnectClient::new(config.credentials, QuickConnectConfig::production()?)
            .context("could not configure QuickConnect client")?;
        self.registry
            .write()
            .await
            .set_quickconnect_writes_enabled(config.writes_enabled);
        self.quickconnect_control = Some(QuickConnectControlService::new(
            Arc::clone(&self.registry),
            client.clone(),
            config.account_id.clone(),
            crate::quickconnect_control::QuickConnectControlPolicy::default(),
        ));
        self.quickconnect_runtime = Some(QuickConnectRuntime {
            account_id: config.account_id,
            client,
        });
        Ok(())
    }

    async fn poll_quickconnect(&self) {
        let Some(runtime) = self.quickconnect_runtime.clone() else {
            return;
        };
        let generations = self
            .registry
            .read()
            .await
            .begin_quickconnect_poll(&runtime.account_id);
        let polls = runtime.client.poll_devices().await;
        let mut registry = self.registry.write().await;
        let result = match polls {
            Ok(polls) => {
                registry
                    .reconcile_quickconnect_polls(&runtime.account_id, polls, &generations)
                    .await
            }
            Err(error) => {
                registry
                    .mark_quickconnect_inventory_unavailable(&runtime.account_id, &generations)
                    .await;
                Err(error.into())
            }
        };
        drop(registry);
        if let Err(error) = result {
            tracing::warn!(error = %error, "QuickConnect polling failed");
        }
        self.publish_state().await;
    }

    #[cfg(feature = "mqtt")]
    async fn start_mqtt(&mut self, config: crate::mqtt::MqttConfig) -> Result<()> {
        self.mqtt_discovery_enabled = config.discovery_enabled;
        let initial_state = self.mqtt_state_snapshot().await?;
        let bridge = crate::mqtt::start(config, initial_state);
        self.mqtt_updates = Some(bridge.state_updates);
        tokio::spawn(process_mqtt_requests(self.clone(), bridge.device_requests));
        Ok(())
    }

    #[cfg(feature = "mqtt")]
    async fn publish_state(&self) {
        let Some(updates) = &self.mqtt_updates else {
            return;
        };
        match self.mqtt_state_snapshot().await {
            Ok(snapshot) => {
                self.publish_current_mqtt_snapshot(updates, snapshot).await;
            }
            Err(error) => tracing::error!(%error, "could not collect device state for MQTT"),
        }
    }

    #[cfg(not(feature = "mqtt"))]
    async fn publish_state(&self) {}

    #[cfg(feature = "mqtt")]
    async fn publish_current_mqtt_snapshot(
        &self,
        updates: &watch::Sender<Arc<crate::mqtt::MqttStateSnapshot>>,
        snapshot: crate::mqtt::MqttStateSnapshot,
    ) -> bool {
        let registry = self.registry.read().await;
        if !registry.descriptors().eq(snapshot.devices.iter()) {
            return false;
        }
        updates.send_replace(Arc::new(snapshot));
        true
    }

    #[cfg(feature = "mqtt")]
    async fn mqtt_state_snapshot(&self) -> Result<crate::mqtt::MqttStateSnapshot> {
        let (proxy_id, descriptors, discovery_identities) = {
            let registry = self.registry.read().await;
            (
                registry.proxy_id(),
                registry.descriptors().cloned().collect::<Vec<_>>(),
                registry.discovery_identities().collect(),
            )
        };
        let publications = stream::iter(descriptors.iter())
            .then(|descriptor| async move {
                let response = device_state_v2_data(self, &descriptor.id)
                    .await
                    .map_err(|_| anyhow::anyhow!("registered device state is unavailable"))?;
                Ok::<_, anyhow::Error>(crate::mqtt::MqttStatePublication {
                    id: descriptor.id.clone(),
                    payload: serde_json::to_string(&response)
                        .context("could not serialize v2 device state for MQTT")?,
                    available: response.available,
                })
            })
            .try_collect()
            .await?;
        Ok(crate::mqtt::MqttStateSnapshot {
            proxy_id,
            discovery_identities,
            devices: descriptors,
            publications,
        })
    }
}

impl LegacyBleRuntime {
    fn new(peripheral_id: String, device: Arc<DeviceRuntime>) -> Self {
        Self {
            reconciler: Arc::new(RwLock::new(StateReconciler::default())),
            device,
            ble_client: Arc::new(ProbeClient::new()),
            peripheral_id: Arc::from(peripheral_id),
        }
    }

    async fn execute_http_control(
        &self,
        state: &ApiState,
        issued_at_unix_ms: u64,
        command: DeviceCommand,
    ) -> Result<bool, ControlAdmissionError> {
        let _permit = self
            .device
            .try_reserve_control()
            .ok_or(ControlAdmissionError::Busy)?;
        let _transaction = self.device.acquire_transaction().await;
        if !v2_request_is_fresh_at(issued_at_unix_ms, unix_millis(SystemTime::now())) {
            return Err(ControlAdmissionError::StaleRequest);
        }
        let prepared = self.prepare_control_locked(state, command).await?;
        if !v2_request_is_fresh_at(issued_at_unix_ms, unix_millis(SystemTime::now())) {
            return Err(ControlAdmissionError::StaleRequest);
        }
        Ok(self.execute_control_locked(state, prepared).await)
    }

    async fn prepare_control_locked(
        &self,
        state: &ApiState,
        command: DeviceCommand,
    ) -> Result<updraft_protocol::ControlCommand, ControlAdmissionError> {
        let thresholds = if crate::legacy_control::needs_threshold_read(command) {
            let poll_id = self.reconciler.write().await.begin_poll();
            let result = self.probe(None).await;
            let thresholds = probe_thresholds(&result);
            if let Ok(ProbeResult::Queried { result, .. }) = &result
                && let Some(snapshot) = &result.snapshot
                && let Some(projection) = project_legacy_snapshot(snapshot)
            {
                self.device.set_state(projection).await;
            }
            self.reconcile_poll_result(poll_id, result).await;
            state.publish_state().await;
            thresholds
        } else {
            None
        };
        crate::legacy_control::prepare_control(command, thresholds)
            .ok_or(ControlAdmissionError::ReadbackUnavailable)
    }

    async fn execute_control_locked(
        &self,
        state: &ApiState,
        command: updraft_protocol::ControlCommand,
    ) -> bool {
        let poll_id = self.reconciler.write().await.begin_poll();
        let outcome = control_outcome(self.probe(Some(command)).await);
        outcome.log_warnings();
        let (success, message, snapshot) = outcome.into_response_parts();
        self.reconcile_control_snapshot(poll_id, snapshot, message)
            .await;
        state.publish_state().await;
        success
    }

    async fn probe(
        &self,
        command: Option<updraft_protocol::ControlCommand>,
    ) -> Result<ProbeResult, ProbeError> {
        self.ble_client.probe(self.probe_options(command)).await
    }

    fn probe_options(&self, command: Option<updraft_protocol::ControlCommand>) -> ProbeOptions {
        ProbeOptions {
            scan_duration: Duration::from_secs(6),
            response_timeout: Duration::from_secs(3),
            mode: ProbeMode::Query {
                device_id: Some(self.peripheral_id.to_string()),
                control_command: command,
            },
        }
    }

    async fn reconcile_control_snapshot(
        &self,
        poll_id: u64,
        snapshot: Option<DeviceSnapshot>,
        message: &'static str,
    ) {
        let mut reconciler = self.reconciler.write().await;
        match snapshot {
            Some(snapshot) => {
                if let Some(projection) = project_legacy_snapshot(&snapshot) {
                    self.device.set_state(projection).await;
                }
                reconciler.apply_success(poll_id, snapshot);
            }
            None => {
                reconciler.apply_failure(poll_id, message);
            }
        }
    }

    async fn read_state_locked(&self) -> DeviceRefreshStatus {
        let poll_id = self.reconciler.write().await.begin_poll();
        let result = self.probe(None).await;
        let status = if let Ok(ProbeResult::Queried { result, .. }) = &result
            && let Some(snapshot) = &result.snapshot
            && let Some(projection) = project_legacy_snapshot(snapshot)
        {
            self.device.set_state(projection).await;
            DeviceRefreshStatus::Fresh
        } else {
            DeviceRefreshStatus::Failed
        };
        self.reconcile_poll_result(poll_id, result).await;
        status
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
        Ok(ProbeResult::Queried { result, .. }) => ControlOutcome::from_query(*result),
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
    #[cfg(feature = "mqtt")] mqtt_config: Option<crate::mqtt::MqttConfig>,
    quickconnect_config: Option<crate::cli::QuickConnectRuntimeConfig>,
) -> Result<()> {
    validate_bind_address(address, allow_remote)?;
    anyhow::ensure!(
        identity_store.is_some() || (device_id.is_none() && quickconnect_config.is_none()),
        "configured devices require --identity-store"
    );
    let listener = TcpListener::bind(address)
        .await
        .context("could not bind HTTP listener")?;
    let registry = DeviceRegistry::load_optional(identity_store)
        .context("could not load local device identity mappings")?;
    #[cfg(feature = "mqtt")]
    anyhow::ensure!(
        !registry.mqtt_ownership_required(
            device_id.is_some(),
            quickconnect_config
                .as_ref()
                .map(|config| config.account_id.as_str())
        ) || mqtt_config
            .as_ref()
            .is_some_and(|config| config.discovery_enabled),
        "persisted MQTT ownership requires a configured broker and --mqtt-discovery"
    );
    let mut state = match device_id {
        Some(device_id) => ApiState::with_ble_device(device_id, registry),
        None => ApiState::with_registry(registry),
    };
    if let Some(config) = quickconnect_config {
        state.start_quickconnect(config).await?;
    }
    #[cfg(feature = "mqtt")]
    if let Some(config) = mqtt_config {
        state.start_mqtt(config).await?;
    }
    let app = router(state.clone());
    let poll_state = state_polling_enabled(&state);
    let poll_quickconnect = quickconnect_polling_enabled(&state);
    if poll_state {
        tokio::spawn(poll_device(state.clone(), DEFAULT_POLL_INTERVAL));
    }
    if poll_quickconnect {
        tokio::spawn(poll_quickconnect_device(state, DEFAULT_POLL_INTERVAL));
    }

    tracing::info!(%address, "Updraft API listening");
    axum::serve(listener, app)
        .await
        .context("HTTP server failed")
}

fn state_polling_enabled(state: &ApiState) -> bool {
    let polling_enabled = state.ble_device.is_some();
    #[cfg(feature = "mqtt")]
    let polling_enabled = polling_enabled || state.mqtt_updates.is_some();
    polling_enabled
}

fn quickconnect_polling_enabled(state: &ApiState) -> bool {
    state.quickconnect_runtime.is_some()
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
        .route("/api/v2/devices/{id}/refresh", post(refresh_device_v2))
        .route("/api/v2/devices/{id}/control", post(control_device_v2))
        .route("/api/v2/devices/{id}/sources", put(set_device_sources_v2))
        .with_state(state)
}

async fn devices_v2(State(state): State<ApiState>) -> Json<DeviceListV2Response> {
    Json(DeviceListV2Response {
        devices: state.registry.read().await.descriptors().cloned().collect(),
    })
}

async fn set_device_sources_v2(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(sources): Json<EntitySources>,
) -> Result<Json<DeviceDescriptor>, StatusCode> {
    let id = DeviceId::parse(id).ok_or(StatusCode::NOT_FOUND)?;
    if sources.state_source != sources.command_source {
        return Err(StatusCode::UNPROCESSABLE_ENTITY);
    }
    let descriptor = {
        let mut registry = state.registry.write().await;
        if registry.runtime(&id).is_none() {
            return Err(StatusCode::NOT_FOUND);
        }
        if sources.state_source == EntitySource::Mqtt && !mqtt_ownership_available(&state) {
            return Err(StatusCode::CONFLICT);
        }
        registry
            .set_entity_sources(&id, sources.state_source, sources.command_source)
            .map_err(|error| {
                tracing::error!(%error, "could not persist entity ownership");
                StatusCode::INTERNAL_SERVER_ERROR
            })?;
        registry
            .descriptors()
            .find(|descriptor| descriptor.id == id)
            .cloned()
            .ok_or(StatusCode::NOT_FOUND)?
    };
    state.publish_state().await;
    Ok(Json(descriptor))
}

#[cfg(feature = "mqtt")]
fn mqtt_ownership_available(state: &ApiState) -> bool {
    state.mqtt_discovery_enabled && state.mqtt_updates.is_some()
}

#[cfg(not(feature = "mqtt"))]
const fn mqtt_ownership_available(_state: &ApiState) -> bool {
    false
}

async fn device_state_v2(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> Result<Json<DeviceStateV2Response>, StatusCode> {
    let id = DeviceId::parse(id).ok_or(StatusCode::NOT_FOUND)?;
    device_state_v2_data(&state, &id).await.map(Json)
}

async fn refresh_device_v2(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> Result<Response, StatusCode> {
    let id = DeviceId::parse(id).ok_or(StatusCode::NOT_FOUND)?;
    let response = state.refresh_device(&id).await?;
    let status = match response.status {
        DeviceRefreshStatus::Fresh => StatusCode::OK,
        DeviceRefreshStatus::Failed => StatusCode::BAD_GATEWAY,
        DeviceRefreshStatus::Superseded => StatusCode::CONFLICT,
    };
    Ok((status, Json(response.as_ref())).into_response())
}

async fn wait_for_device_refresh(
    mut receiver: RefreshReceiver,
) -> Result<Arc<DeviceRefreshV2Response>, StatusCode> {
    receiver
        .wait_for(|result| result.is_some())
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .as_ref()
        .cloned()
        .ok_or(StatusCode::INTERNAL_SERVER_ERROR)
}

async fn device_state_v2_data(
    state: &ApiState,
    id: &DeviceId,
) -> Result<DeviceStateV2Response, StatusCode> {
    let registry = state.registry.read().await;
    let descriptor = registry
        .descriptors()
        .find(|descriptor| descriptor.id == *id)
        .ok_or(StatusCode::NOT_FOUND)?;
    let runtime = registry.runtime(id).ok_or(StatusCode::NOT_FOUND)?;
    let snapshot = runtime.snapshot().await;
    let (last_error, inventory_status) = match (&descriptor.backend, state.ble_device.as_ref()) {
        (DeviceBackend::LegacyBle, Some(device)) if *id == DeviceId::configured_ble() => {
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
    Ok(DeviceStateV2Response {
        id: id.clone(),
        backend: descriptor.backend,
        available: snapshot.state.is_some(),
        inventory_status,
        last_error,
        state: snapshot.state,
    })
}

#[derive(Clone)]
pub(crate) struct CachedV2ControlResult {
    pub(crate) response: DeviceControlV2Response,
}

const V2_CONTROL_MAX_AGE: Duration = Duration::from_secs(30);
const V2_CONTROL_MAX_FUTURE_SKEW: Duration = Duration::from_secs(5);
const V2_REPLAY_CAPACITY: usize = 64;
const V2_IN_FLIGHT_CAPACITY: usize = 8;

#[derive(Default)]
struct RecentV2ControlResults(HashMap<DeviceId, Arc<tokio::sync::Mutex<V2DeviceControlHistory>>>);

#[derive(Default)]
struct V2DeviceControlHistory {
    completed: VecDeque<(CommandId, DeviceCommand, CachedV2ControlResult)>,
    in_flight: HashMap<
        CommandId,
        (
            DeviceCommand,
            tokio::sync::watch::Receiver<Option<CachedV2ControlResult>>,
        ),
    >,
}

enum V2ControlReservation {
    Execute(tokio::sync::watch::Sender<Option<CachedV2ControlResult>>),
    Wait(tokio::sync::watch::Receiver<Option<CachedV2ControlResult>>),
    Completed(CachedV2ControlResult),
}

async fn control_device_v2(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    Json(request): Json<DeviceControlV2Request>,
) -> (StatusCode, Json<DeviceControlV2Response>) {
    let Some(id) = DeviceId::parse(id) else {
        return v2_control_rejected(request.request_id, V2ControlStatus::UnknownDevice);
    };
    let result = process_v2_control_request(&state, id, request).await;
    let status = status_for_v2_outcome(&result.response.status);
    (status, Json(result.response))
}

async fn process_v2_control_request(
    state: &ApiState,
    id: DeviceId,
    request: DeviceControlV2Request,
) -> CachedV2ControlResult {
    if !v2_request_is_fresh(request.issued_at_unix_ms) {
        return cached_v2_result(request.request_id, V2ControlStatus::StaleRequest);
    }

    let registered = state
        .registry
        .read()
        .await
        .descriptors()
        .any(|descriptor| descriptor.id == id);
    if !registered {
        return cached_v2_result(request.request_id, V2ControlStatus::UnknownDevice);
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
        V2ControlReservation::Completed(result) => return result,
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
    receiver
        .wait_for(Option::is_some)
        .await
        .ok()
        .and_then(|response| response.clone())
        .unwrap_or_else(|| cached_v2_result(request.request_id, V2ControlStatus::ControlFailed))
}

async fn execute_v2_control(
    state: &ApiState,
    id: &DeviceId,
    request: &DeviceControlV2Request,
) -> CachedV2ControlResult {
    let backend = state.registry.read().await.dispatch(id, request.command);
    let outcome = match backend {
        Ok(DeviceBackend::LegacyBle) => execute_ble_v2_control(state, request).await,
        Ok(DeviceBackend::QuickConnect) => execute_cloud_v2_control(state, id, request).await,
        Err(crate::backend::DeviceRegistryError::UnknownDevice) => V2ControlStatus::UnknownDevice,
        Err(crate::backend::DeviceRegistryError::UnsupportedCommand) => {
            V2ControlStatus::UnsupportedCommand
        }
        Err(_) => V2ControlStatus::ControlFailed,
    };
    CachedV2ControlResult {
        response: DeviceControlV2Response {
            request_id: request.request_id.as_str().to_owned(),
            status: outcome,
        },
    }
}

async fn execute_ble_v2_control(
    state: &ApiState,
    request: &DeviceControlV2Request,
) -> V2ControlStatus {
    if state.ble_device.is_none() {
        return V2ControlStatus::DeviceUnavailable;
    }
    match state
        .execute_http_control(request.issued_at_unix_ms, request.command)
        .await
    {
        Ok(true) => V2ControlStatus::Confirmed,
        Ok(false) => V2ControlStatus::Unconfirmed,
        Err(error) => error.status(),
    }
}

async fn execute_cloud_v2_control(
    state: &ApiState,
    id: &DeviceId,
    request: &DeviceControlV2Request,
) -> V2ControlStatus {
    let Some(service) = state.quickconnect_control.as_ref() else {
        return V2ControlStatus::BackendUnavailable;
    };
    let Some(command) = quickconnect_command(request.command) else {
        return V2ControlStatus::UnsupportedCommand;
    };
    let Some(intent) = QuickConnectControlIntent::new(
        request.request_id.as_str(),
        request.issued_at_unix_ms,
        command,
    ) else {
        return V2ControlStatus::InvalidRequestId;
    };
    quickconnect_control_status(service.execute(id, intent).await.status())
}

fn spawn_v2_control_execution(
    history: Arc<tokio::sync::Mutex<V2DeviceControlHistory>>,
    request_id: CommandId,
    command: DeviceCommand,
    sender: tokio::sync::watch::Sender<Option<CachedV2ControlResult>>,
    execution: impl std::future::Future<Output = CachedV2ControlResult> + Send + 'static,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let response = execution.await;
        {
            let mut history = history.lock().await;
            history.in_flight.remove(&request_id);
            remember_v2_control_result(
                &mut history.completed,
                request_id,
                command,
                response.clone(),
            );
        }
        sender.send_replace(Some(response));
    })
}

fn reserve_v2_control(
    history: &mut V2DeviceControlHistory,
    request_id: &CommandId,
    command: DeviceCommand,
) -> V2ControlReservation {
    reap_abandoned_v2_controls(history);
    if let Some((_, previous_command, response)) = history
        .completed
        .iter()
        .find(|(previous_id, _, _)| previous_id == request_id)
    {
        return V2ControlReservation::Completed(if *previous_command == command {
            response.clone()
        } else {
            cached_v2_result(request_id.clone(), V2ControlStatus::RequestIdReused)
        });
    }
    if let Some((previous_command, receiver)) = history.in_flight.get(request_id) {
        if *previous_command != command {
            return V2ControlReservation::Completed(cached_v2_result(
                request_id.clone(),
                V2ControlStatus::RequestIdReused,
            ));
        }
        if receiver.has_changed().is_ok() {
            return V2ControlReservation::Wait(receiver.clone());
        }
        history.in_flight.remove(request_id);
        let result = cached_v2_result(request_id.clone(), V2ControlStatus::ControlFailed);
        remember_v2_control_result(
            &mut history.completed,
            request_id.clone(),
            command,
            result.clone(),
        );
        return V2ControlReservation::Completed(result);
    }

    if history.in_flight.len() >= V2_IN_FLIGHT_CAPACITY {
        return V2ControlReservation::Completed(cached_v2_result(
            request_id.clone(),
            V2ControlStatus::Busy,
        ));
    }
    let (sender, receiver) = tokio::sync::watch::channel(None);
    history
        .in_flight
        .insert(request_id.clone(), (command, receiver));
    V2ControlReservation::Execute(sender)
}

fn reap_abandoned_v2_controls(history: &mut V2DeviceControlHistory) {
    history
        .in_flight
        .extract_if(|_, (_, receiver)| receiver.has_changed().is_err())
        .for_each(|(request_id, (command, _))| {
            let result = cached_v2_result(request_id.clone(), V2ControlStatus::ControlFailed);
            remember_v2_control_result(&mut history.completed, request_id, command, result);
        });
}

fn remember_v2_control_result(
    completed: &mut VecDeque<(CommandId, DeviceCommand, CachedV2ControlResult)>,
    request_id: CommandId,
    command: DeviceCommand,
    response: CachedV2ControlResult,
) {
    completed.push_back((request_id, command, response));
    if completed.len() > V2_REPLAY_CAPACITY {
        completed.pop_front();
    }
}

fn cached_v2_result(request_id: CommandId, status: V2ControlStatus) -> CachedV2ControlResult {
    CachedV2ControlResult {
        response: DeviceControlV2Response {
            request_id: request_id.as_str().to_owned(),
            status,
        },
    }
}

fn v2_control_rejected(
    request_id: CommandId,
    status: V2ControlStatus,
) -> (StatusCode, Json<DeviceControlV2Response>) {
    (
        status_for_v2_outcome(&status),
        Json(DeviceControlV2Response {
            request_id: request_id.as_str().to_owned(),
            status,
        }),
    )
}

pub(crate) fn v2_request_is_fresh(issued_at_unix_ms: u64) -> bool {
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
            mode: cloud_mode(mode),
        }),
        DeviceCommand::QuickConnectConditionalOff { only_if_current } => {
            Some(QuickConnectCommand::ClearMode {
                mode: cloud_mode(only_if_current),
            })
        }
        DeviceCommand::QuickConnectTargets {
            temperature_f,
            humidity_percent,
        } => Some(QuickConnectCommand::SetAutomaticTargets {
            temperature_f: Some(temperature_f),
            humidity_percent: Some(humidity_percent),
        }),
        DeviceCommand::QuickConnectAutomaticTemperature { temperature_f } => {
            Some(QuickConnectCommand::SetAutomaticTargets {
                temperature_f: Some(temperature_f.value()),
                humidity_percent: None,
            })
        }
        DeviceCommand::QuickConnectAutomaticHumidity { humidity_percent } => {
            Some(QuickConnectCommand::SetAutomaticTargets {
                temperature_f: None,
                humidity_percent: Some(humidity_percent.value()),
            })
        }
        DeviceCommand::QuickConnectTimerDuration { minutes } => {
            Some(QuickConnectCommand::SetTimerDuration {
                duration_minutes: minutes,
            })
        }
        DeviceCommand::LegacyPreset { .. }
        | DeviceCommand::LegacyAutomaticTemperature { .. }
        | DeviceCommand::LegacyAutomaticHumidity { .. }
        | DeviceCommand::LegacyTimer { .. } => None,
    }
}

const fn cloud_mode(mode: crate::device::QuickConnectMode) -> QuickConnectCommandMode {
    match mode {
        crate::device::QuickConnectMode::Off => QuickConnectCommandMode::Off,
        crate::device::QuickConnectMode::Automatic => QuickConnectCommandMode::Automatic,
        crate::device::QuickConnectMode::Timer => QuickConnectCommandMode::Timer,
        crate::device::QuickConnectMode::Manual => QuickConnectCommandMode::Manual,
    }
}

fn quickconnect_control_status(status: QuickConnectControlStatus) -> V2ControlStatus {
    match status {
        QuickConnectControlStatus::Rejected => V2ControlStatus::Rejected,
        QuickConnectControlStatus::SubmittedUnconfirmed => V2ControlStatus::SubmittedUnconfirmed,
        QuickConnectControlStatus::ReadbackMismatch => V2ControlStatus::ReadbackMismatch,
        QuickConnectControlStatus::ReadbackUnavailable => V2ControlStatus::ReadbackUnavailable,
        QuickConnectControlStatus::Confirmed => V2ControlStatus::Confirmed,
    }
}

fn status_for_v2_outcome(outcome: &V2ControlStatus) -> StatusCode {
    match outcome {
        V2ControlStatus::Confirmed => StatusCode::OK,
        V2ControlStatus::UnknownDevice | V2ControlStatus::DeviceUnavailable => {
            StatusCode::NOT_FOUND
        }
        V2ControlStatus::BackendUnavailable => StatusCode::SERVICE_UNAVAILABLE,
        V2ControlStatus::Busy => StatusCode::TOO_MANY_REQUESTS,
        V2ControlStatus::InvalidRequestId => StatusCode::BAD_REQUEST,
        V2ControlStatus::ControlFailed => StatusCode::INTERNAL_SERVER_ERROR,
        V2ControlStatus::Unconfirmed
        | V2ControlStatus::SubmittedUnconfirmed
        | V2ControlStatus::ReadbackMismatch
        | V2ControlStatus::ReadbackUnavailable => StatusCode::BAD_GATEWAY,
        V2ControlStatus::Rejected
        | V2ControlStatus::UnsupportedCommand
        | V2ControlStatus::StaleRequest
        | V2ControlStatus::RequestIdReused
        | V2ControlStatus::Unknown(_) => StatusCode::UNPROCESSABLE_ENTITY,
    }
}

async fn health() -> Json<HealthResponse> {
    Json(HealthResponse { status: "ok" })
}

#[cfg(feature = "mqtt")]
async fn process_mqtt_requests(
    state: ApiState,
    controls: mpsc::Receiver<crate::mqtt::MqttDeviceWork>,
) {
    device_requests(controls)
        .for_each_concurrent(Some(crate::mqtt::CONTROL_QUEUE_CAPACITY), |work| {
            reply_to_device_request(&state, work)
        })
        .await;
}

#[cfg(feature = "mqtt")]
fn device_requests(
    controls: mpsc::Receiver<crate::mqtt::MqttDeviceWork>,
) -> impl Stream<Item = crate::mqtt::MqttDeviceWork> {
    stream::unfold(controls, receive_device_request)
}

#[cfg(feature = "mqtt")]
async fn receive_device_request(
    mut controls: mpsc::Receiver<crate::mqtt::MqttDeviceWork>,
) -> Option<(
    crate::mqtt::MqttDeviceWork,
    mpsc::Receiver<crate::mqtt::MqttDeviceWork>,
)> {
    let work = controls.recv().await?;
    Some((work, controls))
}

#[cfg(feature = "mqtt")]
async fn reply_to_device_request(state: &ApiState, work: crate::mqtt::MqttDeviceWork) {
    let response = match work.request {
        crate::mqtt::MqttRequest::Control(request) => crate::mqtt::MqttReply::Control(
            process_v2_control_request(state, work.device_id, request)
                .await
                .response,
        ),
        crate::mqtt::MqttRequest::Refresh(request) => {
            process_mqtt_refresh(state, &work.device_id, request).await
        }
    };
    let _ = work.reply.send(response);
}

#[cfg(feature = "mqtt")]
async fn process_mqtt_refresh(
    state: &ApiState,
    device_id: &DeviceId,
    request: crate::mqtt::MqttRefreshRequest,
) -> crate::mqtt::MqttReply {
    use crate::mqtt::{MqttReply, RequestKind};
    let request_id = request.request_id.as_str().to_owned();
    if !v2_request_is_fresh(request.issued_at_unix_ms) {
        return MqttReply::Rejected {
            request_id,
            status: "stale_request",
            kind: RequestKind::Refresh,
        };
    }
    match state.refresh_device(device_id).await {
        Ok(response) => MqttReply::Refresh {
            request_id,
            status: response.status,
        },
        Err(status) => MqttReply::Rejected {
            request_id,
            status: match status {
                StatusCode::NOT_FOUND => "unknown_device",
                StatusCode::SERVICE_UNAVAILABLE => "backend_unavailable",
                StatusCode::UNPROCESSABLE_ENTITY => "unsupported_read",
                _ => "refresh_failed",
            },
            kind: RequestKind::Refresh,
        },
    }
}

#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
}

fn project_legacy_snapshot(snapshot: &DeviceSnapshot) -> Option<DeviceState> {
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
            firmware_version: Some(firmware_version),
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
    Some(device_state)
}

async fn poll_device(state: ApiState, poll_interval: Duration) {
    poll_ticks(poll_interval)
        .for_each(|()| state.poll_and_publish_state())
        .await;
}

async fn poll_quickconnect_device(state: ApiState, poll_interval: Duration) {
    poll_ticks(poll_interval)
        .for_each(|()| state.poll_quickconnect())
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

fn probe_thresholds(
    result: &Result<ProbeResult, ProbeError>,
) -> Option<updraft_protocol::AutomaticThresholds> {
    let Ok(ProbeResult::Queried { result, .. }) = result else {
        return None;
    };
    if result.state_error.is_some() {
        return None;
    }
    result.snapshot.as_ref()?.thresholds.decoded().ok().copied()
}

fn record_poll_result(
    reconciler: &mut StateReconciler,
    poll_id: u64,
    result: Result<ProbeResult, ProbeError>,
) {
    match result {
        Ok(ProbeResult::Queried { result, .. }) => {
            record_query_result(reconciler, poll_id, *result)
        }
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

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::PathBuf,
        time::{Instant, SystemTime},
    };

    #[cfg(feature = "mqtt")]
    use crate::device::EntitySource;
    use axum::{body::Body, http::Request};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    use super::*;

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

    #[tokio::test(start_paused = true)]
    async fn refresh_deadline_releases_worker_without_starting_a_second_ble_owner() {
        let state =
            ApiState::with_ble_device("no-physical-device".to_owned(), DeviceRegistry::new());
        let runtime = state
            .registry
            .read()
            .await
            .runtime(&DeviceId::configured_ble())
            .unwrap();
        let transaction = runtime.acquire_transaction().await;
        let first = state
            .refresh_device(&DeviceId::configured_ble())
            .await
            .unwrap();
        assert_eq!(first.status, DeviceRefreshStatus::Failed);
        assert_eq!(
            first.device.last_error.as_deref(),
            Some("device refresh deadline exceeded")
        );
        let second = state
            .refresh_device(&DeviceId::configured_ble())
            .await
            .unwrap();
        assert_eq!(second.status, DeviceRefreshStatus::Failed);
        assert!(!Arc::ptr_eq(&first, &second));
        drop(transaction);
        assert!(runtime.try_acquire_transaction().is_some());
    }

    #[tokio::test]
    async fn refresh_waiter_reports_closed_worker_without_hanging() {
        let (sender, receiver) = tokio::sync::watch::channel(None);
        drop(sender);
        assert_eq!(
            wait_for_device_refresh(receiver).await.unwrap_err(),
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }

    #[derive(Default)]
    struct RefreshFixture {
        reads: std::sync::atomic::AtomicUsize,
        entered: tokio::sync::Notify,
        release: tokio::sync::Notify,
        fail: std::sync::atomic::AtomicBool,
    }

    async fn refresh_fixture() -> (
        ApiState,
        DeviceId,
        Arc<RefreshFixture>,
        tokio::task::JoinHandle<()>,
        PathBuf,
    ) {
        let path = identity_store_path();
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
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = QuickConnectClient::new(
            updraft_quickconnect::Credentials::new(
                "synthetic-user",
                "synthetic-password",
                updraft_quickconnect::AccountRole::Contractor,
            ),
            QuickConnectConfig::new(
                format!("{base}cognito/").parse().unwrap(),
                format!("{base}gaf/").parse().unwrap(),
            ),
        )
        .unwrap();
        let mut state = ApiState::with_registry(registry);
        state.quickconnect_runtime = Some(QuickConnectRuntime {
            account_id: "synthetic-account".to_owned(),
            client,
        });
        (state, id, fixture, server, path)
    }

    async fn refresh_fixture_detail(
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

    #[tokio::test]
    async fn refresh_survives_cancelled_caller_and_coalesces_overlapping_reads() {
        let (state, id, fixture, server, path) = refresh_fixture().await;
        let first_state = state.clone();
        let first_id = id.clone();
        let first = tokio::spawn(async move { first_state.refresh_device(&first_id).await });
        fixture.entered.notified().await;
        first.abort();
        let second = state.refresh_device(&id);
        tokio::pin!(second);
        assert!(
            tokio::time::timeout(Duration::from_millis(10), &mut second)
                .await
                .is_err()
        );
        fixture.release.notify_one();
        let response = second.await.unwrap();
        assert_eq!(response.status, DeviceRefreshStatus::Fresh);
        assert!(response.device.available);
        assert_eq!(fixture.reads.load(std::sync::atomic::Ordering::SeqCst), 1);
        fixture.release.notify_one();
        let response = state.refresh_device(&id).await.unwrap();
        assert_eq!(response.status, DeviceRefreshStatus::Fresh);
        assert_eq!(fixture.reads.load(std::sync::atomic::Ordering::SeqCst), 2);
        server.abort();
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn refresh_uses_transaction_lock_and_does_not_publish_superseded_read() {
        let (state, id, fixture, server, path) = refresh_fixture().await;
        let runtime = state.registry.read().await.runtime(&id).unwrap();
        let transaction = runtime.acquire_transaction().await;
        let request = state.refresh_device(&id);
        tokio::pin!(request);
        assert!(
            tokio::time::timeout(Duration::from_millis(10), &mut request)
                .await
                .is_err()
        );
        assert_eq!(fixture.reads.load(std::sync::atomic::Ordering::SeqCst), 0);
        drop(transaction);
        fixture.entered.notified().await;
        runtime.begin_control_intent();
        fixture.release.notify_one();
        let response = request.await.unwrap();
        assert_eq!(response.status, DeviceRefreshStatus::Superseded);
        assert!(runtime.state().await.is_none());
        server.abort();
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn refresh_route_reports_failed_read_instead_of_reusing_success() {
        let (state, id, fixture, server, path) = refresh_fixture().await;
        fixture.release.notify_one();
        assert_eq!(
            state.refresh_device(&id).await.unwrap().status,
            DeviceRefreshStatus::Fresh
        );
        fixture
            .fail
            .store(true, std::sync::atomic::Ordering::SeqCst);
        fixture.release.notify_one();
        let response = router(state)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/api/v2/devices/{}/refresh", id.as_str()))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        let response: DeviceRefreshV2Response =
            serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
                .unwrap();
        assert_eq!(response.status, DeviceRefreshStatus::Failed);
        assert!(!response.device.available);
        assert!(response.device.last_error.is_some());
        server.abort();
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn refresh_distinguishes_unknown_device_from_unconfigured_backend() {
        let mut registry = DeviceRegistry::new();
        registry.register_configured_ble();
        let app = router(ApiState::with_registry(registry));
        for (id, expected) in [
            ("not-registered", StatusCode::NOT_FOUND),
            ("configured", StatusCode::SERVICE_UNAVAILABLE),
        ] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri(format!("/api/v2/devices/{id}/refresh"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), expected);
        }
    }

    #[tokio::test]
    #[cfg(feature = "mqtt")]
    async fn stale_mqtt_snapshot_cannot_restore_previous_entity_owner() {
        let state =
            ApiState::with_ble_device("no-physical-device".to_owned(), DeviceRegistry::new());
        let old = state.mqtt_state_snapshot().await.unwrap();
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
        let current = state.mqtt_state_snapshot().await.unwrap();
        let (updates, observed) = watch::channel(Arc::new(current));
        assert!(!state.publish_current_mqtt_snapshot(&updates, old).await);
        assert_eq!(
            observed.borrow().devices[0].state_source,
            EntitySource::Mqtt
        );
    }

    fn reservation_is_wait(reservation: V2ControlReservation) -> bool {
        match reservation {
            V2ControlReservation::Wait(_) => true,
            V2ControlReservation::Execute(_) | V2ControlReservation::Completed(_) => false,
        }
    }

    fn reservation_is_reused(reservation: V2ControlReservation) -> bool {
        match reservation {
            V2ControlReservation::Completed(response) => {
                response.response.status.as_str() == "request_id_reused"
            }
            V2ControlReservation::Execute(_) | V2ControlReservation::Wait(_) => false,
        }
    }

    fn reservation_has_status(reservation: V2ControlReservation, status: &str) -> bool {
        match reservation {
            V2ControlReservation::Completed(response) => response.response.status == status,
            V2ControlReservation::Execute(_) | V2ControlReservation::Wait(_) => false,
        }
    }

    #[tokio::test]
    async fn source_route_persists_owner_and_rejects_split_or_unconfigured_mqtt() {
        let path = identity_store_path();
        let state = ApiState::with_ble_device(
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
        let descriptor: crate::device::DeviceDescriptor = serde_json::from_slice(&body).unwrap();
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
        let mut state = ApiState::with_ble_device(
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
        let mut updates = state.mqtt_updates.as_ref().unwrap().subscribe();
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
        assert_eq!(updates.borrow().devices[0].state_source, EntitySource::Mqtt);
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
        let mut state = ApiState::with_registry(DeviceRegistry::new());
        assert!(!state_polling_enabled(&state));
        state.mqtt_updates = Some(
            watch::channel(Arc::new(crate::mqtt::MqttStateSnapshot {
                devices: Vec::new(),
                publications: Vec::new(),
                proxy_id: crate::device::ProxyId::default(),
                discovery_identities: Vec::new(),
            }))
            .0,
        );
        assert!(state_polling_enabled(&state));
    }

    #[tokio::test]
    #[cfg(feature = "mqtt")]
    async fn cloud_only_periodic_state_publication_refreshes_the_mqtt_snapshot() {
        let initial = crate::mqtt::MqttStateSnapshot {
            devices: Vec::new(),
            publications: Vec::new(),
            proxy_id: crate::device::ProxyId::default(),
            discovery_identities: Vec::new(),
        };
        let (updates, mut current) = watch::channel(Arc::new(initial));
        let mut state = ApiState::with_registry(DeviceRegistry::new());
        state.mqtt_updates = Some(updates);

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
        let mut state = ApiState::with_registry(registry);

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

    fn reservation_is_execute(
        reservation: V2ControlReservation,
    ) -> Option<tokio::sync::watch::Sender<Option<CachedV2ControlResult>>> {
        match reservation {
            V2ControlReservation::Execute(sender) => Some(sender),
            V2ControlReservation::Wait(_) | V2ControlReservation::Completed(_) => None,
        }
    }

    #[test]
    fn v2_control_admission_bounds_distinct_requests_and_keeps_duplicate_joining() {
        let mut history = V2DeviceControlHistory::default();
        let command = DeviceCommand::LegacyPreset {
            preset: ControlPreset::TimerClear,
        };
        let senders = (0..8)
            .map(|index| {
                let id = CommandId::parse(&format!("pending-{index}")).unwrap();
                reservation_is_execute(reserve_v2_control(&mut history, &id, command)).unwrap()
            })
            .collect::<Vec<_>>();
        let extra = CommandId::parse("excess-request").unwrap();
        assert!(reservation_has_status(
            reserve_v2_control(&mut history, &extra, command),
            "busy"
        ));
        assert_eq!(history.in_flight.len(), 8);
        assert!(reservation_is_wait(reserve_v2_control(
            &mut history,
            &CommandId::parse("pending-0").unwrap(),
            command
        )));
        drop(senders);
    }

    #[test]
    fn abandoned_control_reservations_release_capacity_without_replaying_writes() {
        let mut history = V2DeviceControlHistory::default();
        let command = DeviceCommand::LegacyPreset {
            preset: ControlPreset::TimerClear,
        };
        let senders = (0..V2_IN_FLIGHT_CAPACITY)
            .map(|index| {
                let id = CommandId::parse(&format!("abandoned-{index}")).unwrap();
                reservation_is_execute(reserve_v2_control(&mut history, &id, command)).unwrap()
            })
            .collect::<Vec<_>>();
        drop(senders);
        let new_id = CommandId::parse("new-after-abandoned").unwrap();
        let sender = reservation_is_execute(reserve_v2_control(&mut history, &new_id, command));
        assert!(sender.is_some());
        assert_eq!(history.in_flight.len(), 1);
        assert!(reservation_has_status(
            reserve_v2_control(
                &mut history,
                &CommandId::parse("abandoned-0").unwrap(),
                command
            ),
            "control_failed"
        ));
        assert!(reservation_is_reused(reserve_v2_control(
            &mut history,
            &CommandId::parse("abandoned-0").unwrap(),
            DeviceCommand::LegacyPreset {
                preset: ControlPreset::TimerOneMinute
            }
        )));
    }

    #[test]
    fn control_admission_prunes_only_abandoned_reservations() {
        let mut history = V2DeviceControlHistory::default();
        let command = DeviceCommand::LegacyPreset {
            preset: ControlPreset::TimerClear,
        };
        let mut senders = (0..V2_IN_FLIGHT_CAPACITY)
            .map(|index| {
                let id = CommandId::parse(&format!("mixed-{index}")).unwrap();
                reservation_is_execute(reserve_v2_control(&mut history, &id, command)).unwrap()
            })
            .collect::<Vec<_>>();
        drop(senders.pop());
        let new_id = CommandId::parse("replacement").unwrap();
        let replacement =
            reservation_is_execute(reserve_v2_control(&mut history, &new_id, command));
        assert!(replacement.is_some());
        assert!(reservation_is_wait(reserve_v2_control(
            &mut history,
            &CommandId::parse("mixed-0").unwrap(),
            command
        )));
        assert!(reservation_has_status(
            reserve_v2_control(
                &mut history,
                &CommandId::parse("still-full").unwrap(),
                command
            ),
            "busy"
        ));
    }

    #[tokio::test]
    async fn completed_execution_frees_capacity_for_a_previously_busy_id() {
        let history = Arc::new(tokio::sync::Mutex::new(V2DeviceControlHistory::default()));
        let command = DeviceCommand::LegacyPreset {
            preset: ControlPreset::TimerClear,
        };
        let mut senders = {
            let mut history = history.lock().await;
            (0..V2_IN_FLIGHT_CAPACITY)
                .map(|index| {
                    let id = CommandId::parse(&format!("complete-{index}")).unwrap();
                    reservation_is_execute(reserve_v2_control(&mut history, &id, command)).unwrap()
                })
                .collect::<Vec<_>>()
        };
        let extra = CommandId::parse("try-after-completion").unwrap();
        assert!(reservation_has_status(
            reserve_v2_control(&mut *history.lock().await, &extra, command),
            "busy"
        ));
        let id = CommandId::parse(&format!("complete-{}", V2_IN_FLIGHT_CAPACITY - 1)).unwrap();
        let response = cached_v2_result(id.clone(), V2ControlStatus::Confirmed);
        spawn_v2_control_execution(
            Arc::clone(&history),
            id.clone(),
            command,
            senders.pop().unwrap(),
            async { response },
        )
        .await
        .unwrap();
        let mut history = history.lock().await;
        let sender = reservation_is_execute(reserve_v2_control(&mut history, &extra, command));
        assert!(sender.is_some());
        assert!(reservation_has_status(
            reserve_v2_control(&mut history, &id, command),
            "confirmed"
        ));
    }

    #[tokio::test]
    async fn ble_stale_control_releases_its_admission_slot() {
        let state =
            ApiState::with_ble_device("no-physical-device".to_owned(), DeviceRegistry::new());
        let runtime = state.ble_device.as_ref().unwrap();
        assert_eq!(
            runtime
                .execute_http_control(
                    &state,
                    0,
                    DeviceCommand::LegacyPreset {
                        preset: ControlPreset::TimerClear
                    }
                )
                .await
                .err(),
            Some(ControlAdmissionError::StaleRequest)
        );
        let permits = std::iter::repeat_with(|| runtime.device.try_reserve_control().unwrap())
            .take(8)
            .collect::<Vec<_>>();
        assert!(runtime.device.try_reserve_control().is_none());
        drop(permits);
    }

    #[tokio::test]
    async fn ble_http_admission_rejects_busy_before_waiting_or_touching_bluetooth() {
        let state =
            ApiState::with_ble_device("no-physical-device".to_owned(), DeviceRegistry::new());
        let runtime = state.ble_device.as_ref().unwrap();
        let permits = std::iter::repeat_with(|| runtime.device.try_reserve_control().unwrap())
            .take(8)
            .collect::<Vec<_>>();
        let _transaction = runtime.device.acquire_transaction().await;
        let request = DeviceControlV2Request {
            request_id: CommandId::parse("busy-ble-request").unwrap(),
            issued_at_unix_ms: unix_millis(SystemTime::now()).unwrap(),
            command: DeviceCommand::LegacyPreset {
                preset: ControlPreset::TimerClear,
            },
        };
        let result = tokio::time::timeout(
            Duration::from_millis(100),
            router(state.clone()).oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v2/devices/configured/control")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&request).unwrap()))
                    .unwrap(),
            ),
        )
        .await
        .expect("busy admission must not wait for the device transaction");
        let result = result.unwrap();
        assert_eq!(result.status(), StatusCode::TOO_MANY_REQUESTS);
        let body = result.into_body().collect().await.unwrap().to_bytes();
        let response: DeviceControlV2Response = serde_json::from_slice(&body).unwrap();
        assert_eq!(response.status, updraft_api::ControlStatus::Busy);
        assert_eq!(response.request_id, request.request_id.as_str());
        drop(permits);
        assert!(runtime.device.try_reserve_control().is_some());
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
    async fn v2_replay_reservations_are_scoped_to_each_local_device_id() {
        let mut results = RecentV2ControlResults::default();
        let request_id = CommandId::parse("same-request").unwrap();
        let command = DeviceCommand::QuickConnectMode {
            mode: crate::device::QuickConnectMode::Automatic,
        };
        let first_device = DeviceId::parse("quickconnect-a".to_owned()).unwrap();
        let second_device = DeviceId::parse("quickconnect-b".to_owned()).unwrap();

        let first = Arc::clone(results.0.entry(first_device).or_insert_with(|| {
            Arc::new(tokio::sync::Mutex::new(V2DeviceControlHistory::default()))
        }));
        let second = Arc::clone(results.0.entry(second_device).or_insert_with(|| {
            Arc::new(tokio::sync::Mutex::new(V2DeviceControlHistory::default()))
        }));
        let mut first_history = first.lock().await;
        assert!(
            reservation_is_execute(reserve_v2_control(&mut first_history, &request_id, command,))
                .is_some()
        );
        let mut second_history = second.lock().await;
        assert!(
            reservation_is_execute(reserve_v2_control(
                &mut second_history,
                &request_id,
                command,
            ))
            .is_some()
        );
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
        let response = CachedV2ControlResult {
            response: DeviceControlV2Response {
                request_id: request_id.as_str().to_owned(),
                status: "unconfirmed".into(),
            },
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
        assert_eq!(
            history.completed.front().unwrap().2.response.status,
            "unconfirmed"
        );
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
        let state =
            ApiState::with_ble_device("private-peripheral-id".to_owned(), DeviceRegistry::new());
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
    #[cfg(feature = "mqtt")]
    fn mqtt_device_requests_reject_unknown_fields() {
        assert!(
            serde_json::from_str::<crate::control::ControlRequest>(
                r#"{"preset":"timer_clear","duration_minutes":999}"#
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn state_route_reports_normalized_state_and_expired_state_as_unavailable() {
        let state =
            ApiState::with_ble_device("private-peripheral-id".to_owned(), DeviceRegistry::new());
        let projection =
            project_legacy_snapshot(&snapshot_at(Instant::now(), SystemTime::now())).unwrap();
        state
            .registry
            .read()
            .await
            .runtime(&DeviceId::configured_ble())
            .unwrap()
            .set_state(projection)
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
        .unwrap();
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
    async fn reusable_client_reads_the_actual_service_router_without_physical_access() {
        let state =
            ApiState::with_ble_device("private-peripheral-id".to_owned(), DeviceRegistry::new());
        let projection =
            project_legacy_snapshot(&snapshot_at(Instant::now(), SystemTime::now())).unwrap();
        state
            .registry
            .read()
            .await
            .runtime(&DeviceId::configured_ble())
            .unwrap()
            .set_state(projection)
            .await;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let app = router(state);
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let client = updraft_client::Client::new(
            url.parse().unwrap(),
            updraft_client::ClientOptions::default(),
        )
        .unwrap();
        let inventory = client.devices().await.unwrap();
        assert_eq!(inventory.devices.len(), 1);
        assert_eq!(inventory.devices[0].id, DeviceId::configured_ble());
        let response = client.state(&DeviceId::configured_ble()).await.unwrap();
        assert!(response.available);
        let snapshot = response.state.unwrap();
        assert_eq!(snapshot.temperature_f, Some(97.0));
        assert_eq!(snapshot.estimated_running, None);
        let serialized = serde_json::to_string(&snapshot).unwrap();
        assert!(!serialized.contains("private-suffix"));
        assert!(!serialized.contains("private-peripheral-id"));
        task.abort();
    }

    #[tokio::test]
    async fn initial_ble_poll_failure_reports_unavailable_inventory_and_error() {
        let state = ApiState::with_ble_device("synthetic-ble-id".to_owned(), DeviceRegistry::new());
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
        let projected = serde_json::to_string(&project_legacy_snapshot(&snapshot)).unwrap();
        assert!(!projected.contains("private-suffix"));
        assert!(projected.contains("\"estimated_running\":null"));
        assert!(projected.contains("\"controller_fan_on\":false"));
    }
}
