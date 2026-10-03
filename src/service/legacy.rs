use super::DeviceService;
use super::control::{V2_CONTROL_MAX_AGE, v2_request_is_fresh_at};
use crate::backend::DeviceRuntime;
use anyhow::Result;
use gafctl_api::unix_millis;
use gafctl_api::{ControlStatus as V2ControlStatus, DeviceRefreshStatus};
use gafctl_api::{DeviceCommand, DeviceState};
use gafctl_bluetooth::{
    DisconnectOutcome, ProbeClient, ProbeError, ProbeErrorKind, ProbeMode, ProbeOptions,
    ProbeResult, QueryResult,
};
use gafctl_protocol::{DeviceSnapshot, StateReconciler};
use std::{
    sync::Arc,
    time::{Duration, SystemTime},
};
use tokio::sync::RwLock;

pub(super) struct LegacyBleRuntime {
    reconciler: Arc<RwLock<StateReconciler>>,
    device: Arc<DeviceRuntime>,
    ble_client: Arc<ProbeClient>,
    peripheral_id: Arc<str>,
}

#[derive(Debug, Eq, PartialEq)]
pub(super) enum ControlAdmissionError {
    BackendUnavailable,
    StaleRequest,
    Busy,
    ReadbackUnavailable,
}

impl ControlAdmissionError {
    pub(super) const fn status(self) -> V2ControlStatus {
        match self {
            Self::BackendUnavailable => V2ControlStatus::BackendUnavailable,
            Self::StaleRequest => V2ControlStatus::StaleRequest,
            Self::Busy => V2ControlStatus::Busy,
            Self::ReadbackUnavailable => V2ControlStatus::ReadbackUnavailable,
        }
    }
}

impl LegacyBleRuntime {
    pub(super) async fn wait_until_idle(&self) {
        self.ble_client.wait_until_idle().await;
    }

    pub(super) async fn decorate_state_response(
        &self,
        response: &mut gafctl_api::DeviceStateV2Response,
    ) {
        let reconciler = self.reconciler.read().await;
        let poll_error = reconciler.last_error().map(str::to_owned);
        if poll_error.is_some() && reconciler.latest_snapshot().is_none() {
            response.inventory_status = crate::backend::DeviceInventoryStatus::Unavailable;
        }
        response.last_error = poll_error.or(response.last_error.take());
    }

    pub(super) fn new(peripheral_id: String, device: Arc<DeviceRuntime>) -> Self {
        Self {
            reconciler: Arc::new(RwLock::new(StateReconciler::default())),
            device,
            ble_client: Arc::new(ProbeClient::new()),
            peripheral_id: Arc::from(peripheral_id),
        }
    }

    pub(super) async fn execute_control(
        &self,
        state: &DeviceService,
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
        let now = unix_millis(SystemTime::now()).ok_or(ControlAdmissionError::StaleRequest)?;
        let remaining = issued_at_unix_ms
            .saturating_add(u64::try_from(V2_CONTROL_MAX_AGE.as_millis()).unwrap_or(u64::MAX))
            .saturating_sub(now);
        let deadline = tokio::time::Instant::now() + Duration::from_millis(remaining);
        self.execute_control_locked(state, prepared, deadline).await
    }

    async fn prepare_control_locked(
        &self,
        state: &DeviceService,
        command: DeviceCommand,
    ) -> Result<gafctl_protocol::ControlCommand, ControlAdmissionError> {
        let thresholds = if crate::legacy_control::needs_threshold_read(command) {
            let poll_id = self.reconciler.write().await.begin_poll();
            let result = self.probe(None).await;
            let thresholds = probe_thresholds(&result);
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
        state: &DeviceService,
        command: gafctl_protocol::ControlCommand,
        deadline: tokio::time::Instant,
    ) -> Result<bool, ControlAdmissionError> {
        let poll_id = self.reconciler.write().await.begin_poll();
        let mut options = self.probe_options(Some(command));
        options.control_deadline = Some(deadline);
        let result = self.ble_client.probe(options).await;
        if result
            .as_ref()
            .is_err_and(|error| error.kind() == ProbeErrorKind::StaleControl)
        {
            return Err(ControlAdmissionError::StaleRequest);
        }
        let outcome = control_outcome(result);
        outcome.log_warnings();
        let (success, message, snapshot) = outcome.into_response_parts();
        self.reconcile_control_snapshot(poll_id, snapshot, message)
            .await;
        state.publish_state().await;
        Ok(success)
    }

    async fn probe(
        &self,
        command: Option<gafctl_protocol::ControlCommand>,
    ) -> Result<ProbeResult, ProbeError> {
        self.ble_client.probe(self.probe_options(command)).await
    }

    fn probe_options(&self, command: Option<gafctl_protocol::ControlCommand>) -> ProbeOptions {
        ProbeOptions {
            scan_duration: Duration::from_secs(6),
            response_timeout: Duration::from_secs(3),
            control_deadline: None,
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

    pub(super) async fn read_state_locked(&self) -> DeviceRefreshStatus {
        let poll_id = self.reconciler.write().await.begin_poll();
        let result = self.probe(None).await;
        self.reconcile_poll_result(poll_id, result).await
    }

    async fn reconcile_poll_result(
        &self,
        poll_id: u64,
        result: Result<ProbeResult, ProbeError>,
    ) -> DeviceRefreshStatus {
        let status = if let Ok(ProbeResult::Queried { result, .. }) = &result
            && let Some(snapshot) = &result.snapshot
            && let Some(projection) = project_legacy_snapshot(snapshot)
        {
            self.device.set_state(projection).await;
            DeviceRefreshStatus::Fresh
        } else {
            DeviceRefreshStatus::Failed
        };
        let mut reconciler = self.reconciler.write().await;
        record_poll_result(&mut reconciler, poll_id, result);
        status
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
        control: Option<&gafctl_protocol::ControlOutcome>,
        state_error: Option<String>,
    ) -> Self {
        match (
            control.is_some_and(gafctl_protocol::ControlOutcome::is_confirmed),
            state_error,
        ) {
            (true, _) => Self::Confirmed,
            (false, Some(error)) => Self::ReadbackFailed(error),
            (false, None) => Self::Mismatch,
        }
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

fn disconnect_error(outcome: DisconnectOutcome) -> Option<String> {
    match outcome {
        DisconnectOutcome::Failed(error) => Some(error),
        DisconnectOutcome::Disconnected => None,
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

fn project_legacy_snapshot(snapshot: &DeviceSnapshot) -> Option<DeviceState> {
    snapshot.identity.decoded().ok()?;
    snapshot.mode.decoded().ok()?;
    snapshot.sensors.decoded().ok()?;
    snapshot.thresholds.decoded().ok()?;
    snapshot.timer.decoded().ok()?;
    Some(crate::legacy_projection::project_snapshot(snapshot))
}

fn probe_thresholds(
    result: &Result<ProbeResult, ProbeError>,
) -> Option<gafctl_protocol::AutomaticThresholds> {
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
        ProbeErrorKind::StaleControl => "BLE control expired before writing",
        ProbeErrorKind::Unavailable => "BLE unavailable",
        ProbeErrorKind::Authentication => "BLE permission or authentication failed",
        ProbeErrorKind::Protocol => "GAF protocol error",
    }
}

#[cfg(test)]
#[path = "tests/legacy.rs"]
mod tests;

#[cfg(test)]
#[path = "tests/state.rs"]
mod state_tests;
