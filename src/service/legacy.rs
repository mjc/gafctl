use super::DeviceService;
use super::control::{V2_CONTROL_MAX_AGE, v2_request_is_fresh_at};
use crate::backend::DeviceRuntime;
use crate::timed_run::{TimedRun, TimerAction, TimerConfiguration, TimerObservation};
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
    timer: RwLock<TimerConfiguration>,
    timer_error: RwLock<Option<TimerFailure>>,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum TimerFailure {
    Persistence,
    RestoreUnconfirmed,
}

impl TimerFailure {
    const fn message(self) -> &'static str {
        match self {
            Self::Persistence => {
                "could not save timer settings; automatic restoration is not armed"
            }
            Self::RestoreUnconfirmed => {
                "timer ended; restoring the previous mode was not confirmed"
            }
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
pub(super) enum ControlAdmissionError {
    BackendUnavailable,
    StaleRequest,
    Busy,
    ReadbackUnavailable,
    Persistence,
}

impl ControlAdmissionError {
    pub(super) const fn status(self) -> V2ControlStatus {
        match self {
            Self::BackendUnavailable => V2ControlStatus::BackendUnavailable,
            Self::StaleRequest => V2ControlStatus::StaleRequest,
            Self::Busy => V2ControlStatus::Busy,
            Self::ReadbackUnavailable => V2ControlStatus::ReadbackUnavailable,
            Self::Persistence => V2ControlStatus::ControlFailed,
        }
    }
}

impl LegacyBleRuntime {
    pub(super) fn begin_shutdown(&self) {
        self.ble_client.begin_shutdown();
    }

    pub(super) async fn wait_until_idle(&self) -> Result<(), gafctl_bluetooth::ProbeError> {
        self.ble_client.wait_until_idle().await
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
        response.timer_duration_minutes = Some(self.timer.read().await.duration_minutes);
        response.last_error = poll_error
            .or(self
                .timer_error
                .read()
                .await
                .map(|error| error.message().to_owned()))
            .or(response.last_error.take());
    }

    #[cfg(feature = "mqtt")]
    pub(super) async fn state_response_matches(
        &self,
        response: &gafctl_api::DeviceStateV2Response,
        now_unix_ms: Option<u64>,
    ) -> bool {
        let reconciler = self.reconciler.read().await;
        let timer_error = *self.timer_error.read().await;
        let poll_error = reconciler.last_error();
        let inventory_unavailable = poll_error.is_some() && reconciler.latest_snapshot().is_none();
        if response.timer_duration_minutes != Some(self.timer.read().await.duration_minutes) {
            return false;
        }
        self.device
            .matches_response_at(
                response,
                now_unix_ms,
                poll_error.or(timer_error.map(TimerFailure::message)),
                inventory_unavailable,
            )
            .await
    }

    pub(super) fn new(
        peripheral_id: String,
        device: Arc<DeviceRuntime>,
        timer: TimerConfiguration,
    ) -> Self {
        Self {
            reconciler: Arc::new(RwLock::new(StateReconciler::default())),
            device,
            ble_client: Arc::new(ProbeClient::new()),
            peripheral_id: Arc::from(peripheral_id),
            timer: RwLock::new(timer),
            timer_error: RwLock::new(None),
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
        if let DeviceCommand::LegacyTimerDuration { minutes } = command {
            let mut timer = self.timer.read().await.clone();
            timer.duration_minutes = minutes;
            self.save_timer(state, timer).await?;
            state.publish_state().await;
            return Ok(true);
        }
        let (prepared, observation) = self.prepare_control_locked(state, command).await?;
        let now = unix_millis(SystemTime::now()).ok_or(ControlAdmissionError::StaleRequest)?;
        if !v2_request_is_fresh_at(issued_at_unix_ms, Some(now)) {
            return Err(ControlAdmissionError::StaleRequest);
        }
        let run = self
            .prepare_timed_run(prepared, observation.as_ref(), now)
            .await?;
        let remaining = issued_at_unix_ms
            .saturating_add(u64::try_from(V2_CONTROL_MAX_AGE.as_millis()).unwrap_or(u64::MAX))
            .saturating_sub(now);
        let deadline = tokio::time::Instant::now() + Duration::from_millis(remaining);
        self.cancel_timed_run(state).await?;
        let confirmed = self
            .execute_control_locked(state, prepared, deadline)
            .await?;
        self.finish_control(state, confirmed, run).await
    }

    async fn finish_control(
        &self,
        state: &DeviceService,
        confirmed: bool,
        run: Option<TimedRun>,
    ) -> Result<bool, ControlAdmissionError> {
        if confirmed {
            if let Some(run) = run {
                let mut timer = self.timer.read().await.clone();
                timer.run = Some(run);
                self.save_timer(state, timer).await?;
            }
            *self.timer_error.write().await = None;
            state.publish_state().await;
        }
        Ok(confirmed)
    }

    async fn prepare_control_locked(
        &self,
        state: &DeviceService,
        command: DeviceCommand,
    ) -> Result<(gafctl_protocol::ControlCommand, Option<TimerObservation>), ControlAdmissionError>
    {
        let settings = if crate::legacy_control::needs_state_read(command) {
            let poll_id = self.reconciler.write().await.begin_poll();
            let result = self.probe(None).await;
            let settings = probe_control_settings(&result);
            self.reconcile_poll_result(poll_id, result).await;
            state.publish_state().await;
            settings
        } else {
            None
        };
        let timer = self.timer.read().await.duration_minutes;
        let prepared = crate::legacy_control::prepare_control(
            command,
            settings.as_ref().map(|(thresholds, _)| *thresholds),
            Some(gafctl_protocol::Minutes::new(timer.value())),
        )
        .ok_or(ControlAdmissionError::ReadbackUnavailable)?;
        Ok((prepared, settings.map(|(_, observation)| observation)))
    }

    async fn prepare_timed_run(
        &self,
        command: gafctl_protocol::ControlCommand,
        observation: Option<&TimerObservation>,
        now_ms: u64,
    ) -> Result<Option<TimedRun>, ControlAdmissionError> {
        let gafctl_protocol::ControlCommand::SetTimer(minutes) = command else {
            return Ok(None);
        };
        if minutes.value() == 0 {
            return Ok(None);
        }
        let observation = observation.ok_or(ControlAdmissionError::ReadbackUnavailable)?;
        let minutes = minutes
            .value()
            .try_into()
            .map_err(|_| ControlAdmissionError::ReadbackUnavailable)?;
        TimedRun::start(
            &self.peripheral_id,
            minutes,
            observation,
            self.timer.read().await.run.as_ref(),
            now_ms,
        )
        .map(Some)
        .ok_or(ControlAdmissionError::ReadbackUnavailable)
    }

    async fn save_timer(
        &self,
        state: &DeviceService,
        timer: TimerConfiguration,
    ) -> Result<(), ControlAdmissionError> {
        let result = state
            .registry
            .write()
            .await
            .set_timer_configuration(timer.clone());
        if let Err(error) = result {
            tracing::error!(%error, "could not save timer settings");
            *self.timer_error.write().await = Some(TimerFailure::Persistence);
            state.publish_state().await;
            return Err(ControlAdmissionError::Persistence);
        }
        *self.timer.write().await = timer;
        let mut error = self.timer_error.write().await;
        if *error == Some(TimerFailure::Persistence) {
            *error = None;
        }
        Ok(())
    }

    async fn cancel_timed_run(&self, state: &DeviceService) -> Result<(), ControlAdmissionError> {
        if self.timer.read().await.run.is_some() {
            let mut timer = self.timer.read().await.clone();
            timer.run = None;
            self.save_timer(state, timer).await?;
        }
        Ok(())
    }

    async fn restore_timed_run(&self, state: &DeviceService, observation: &TimerObservation) {
        let Some(now) = unix_millis(SystemTime::now()) else {
            return;
        };
        let action = {
            let mut timer = self.timer.write().await;
            timer.run.as_mut().map_or(TimerAction::Wait, |run| {
                run.observe(&self.peripheral_id, observation, now)
            })
        };
        let command = match action {
            TimerAction::Wait => return,
            TimerAction::Cancel => None,
            TimerAction::Restore(command) => Some(command),
        };
        if self.cancel_timed_run(state).await.is_err() {
            return;
        }
        if let Some(command) = command {
            let deadline = tokio::time::Instant::now() + V2_CONTROL_MAX_AGE;
            if self.execute_control_locked(state, command, deadline).await != Ok(true) {
                *self.timer_error.write().await = Some(TimerFailure::RestoreUnconfirmed);
            }
        }
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
            scan_duration: Duration::from_secs(5),
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
                } else {
                    self.device.mark_ble_state_unavailable().await;
                }
                reconciler.apply_success(poll_id, snapshot);
            }
            None => {
                self.device.mark_ble_state_unavailable().await;
                reconciler.apply_failure(poll_id, message);
            }
        }
    }

    pub(super) async fn record_refresh_timeout(&self, generation: u64) -> bool {
        let mut reconciler = self.reconciler.write().await;
        if !self
            .device
            .mark_refresh_unavailable_if_current(generation)
            .await
        {
            return false;
        }
        let poll_id = reconciler.begin_poll();
        reconciler.apply_failure(poll_id, "device refresh deadline exceeded");
        true
    }

    pub(super) async fn read_state_locked(&self, state: &DeviceService) -> DeviceRefreshStatus {
        let poll_id = self.reconciler.write().await.begin_poll();
        let result = self.probe(None).await;
        let observation = probe_control_settings(&result).map(|(_, observation)| observation);
        let status = self.reconcile_poll_result(poll_id, result).await;
        if status == DeviceRefreshStatus::Fresh
            && let Some(observation) = observation
        {
            self.restore_timed_run(state, &observation).await;
        }
        status
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
            self.device.mark_ble_state_unavailable().await;
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
        DisconnectOutcome::Disconnected | DisconnectOutcome::Retained => None,
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
    if let Some(timer) = &snapshot.timer {
        timer.decoded().ok()?;
    }
    Some(crate::legacy_projection::project_snapshot(snapshot))
}

fn probe_control_settings(
    result: &Result<ProbeResult, ProbeError>,
) -> Option<(gafctl_protocol::AutomaticThresholds, TimerObservation)> {
    let Ok(ProbeResult::Queried { result, .. }) = result else {
        return None;
    };
    if result.state_error.is_some() {
        return None;
    }
    let snapshot = result.snapshot.as_ref()?;
    project_legacy_snapshot(snapshot)?;
    Some((
        *snapshot.thresholds.decoded().ok()?,
        TimerObservation::from_snapshot(snapshot)?,
    ))
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
