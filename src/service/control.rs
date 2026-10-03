use super::quickconnect::{QuickConnectControlIntent, QuickConnectControlStatus};
use super::{DeviceService, legacy::ControlAdmissionError};
use gafctl_api::{CommandId, is_fresh_at, unix_millis};
use gafctl_api::{
    ControlStatus as V2ControlStatus, DeviceControlV2Request, DeviceControlV2Response,
};
use gafctl_api::{DeviceBackend, DeviceCommand, DeviceId};
use gafctl_quickconnect::{QuickConnectCommand, QuickConnectCommandMode};
use std::{
    collections::{HashMap, VecDeque},
    sync::Arc,
    time::{Duration, SystemTime},
};

#[derive(Clone)]
struct CachedV2ControlResult {
    response: DeviceControlV2Response,
}

pub(super) const V2_CONTROL_MAX_AGE: Duration = Duration::from_secs(30);

const V2_CONTROL_MAX_FUTURE_SKEW: Duration = Duration::from_secs(5);

const V2_REPLAY_CAPACITY: usize = 64;

const V2_IN_FLIGHT_CAPACITY: usize = 8;

#[derive(Default)]
pub(super) struct RecentV2ControlResults(
    HashMap<DeviceId, Arc<tokio::sync::Mutex<V2DeviceControlHistory>>>,
);

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

impl DeviceService {
    pub(crate) async fn control(
        &self,
        id: DeviceId,
        request: DeviceControlV2Request,
    ) -> DeviceControlV2Response {
        let state = self;
        if !v2_request_is_fresh(request.issued_at_unix_ms) {
            return cached_v2_result(request.request_id, V2ControlStatus::StaleRequest).response;
        }

        let registered = state
            .registry
            .read()
            .await
            .descriptors()
            .any(|descriptor| descriptor.id == id);
        if !registered {
            return cached_v2_result(request.request_id, V2ControlStatus::UnknownDevice).response;
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
            V2ControlReservation::Completed(result) => return result.response,
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
            .response
    }

    async fn execute_control(
        &self,
        issued_at_unix_ms: u64,
        command: DeviceCommand,
    ) -> Result<bool, ControlAdmissionError> {
        let device = self
            .ble_device
            .as_ref()
            .ok_or(ControlAdmissionError::BackendUnavailable)?;
        device
            .execute_control(self, issued_at_unix_ms, command)
            .await
    }
}

async fn execute_v2_control(
    state: &DeviceService,
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
    state: &DeviceService,
    request: &DeviceControlV2Request,
) -> V2ControlStatus {
    if state.ble_device.is_none() {
        return V2ControlStatus::DeviceUnavailable;
    }
    match state
        .execute_control(request.issued_at_unix_ms, request.command)
        .await
    {
        Ok(true) => V2ControlStatus::Confirmed,
        Ok(false) => V2ControlStatus::Unconfirmed,
        Err(error) => error.status(),
    }
}

async fn execute_cloud_v2_control(
    state: &DeviceService,
    id: &DeviceId,
    request: &DeviceControlV2Request,
) -> V2ControlStatus {
    let Some(service) = state.quickconnect.as_ref() else {
        return V2ControlStatus::BackendUnavailable;
    };
    let Some(command) = quickconnect_command(request.command) else {
        return V2ControlStatus::UnsupportedCommand;
    };
    let intent = QuickConnectControlIntent::new(request.issued_at_unix_ms, command);
    quickconnect_control_status(service.execute(id, intent).await)
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

pub(crate) fn v2_request_is_fresh(issued_at_unix_ms: u64) -> bool {
    v2_request_is_fresh_at(issued_at_unix_ms, unix_millis(SystemTime::now()))
}

pub(super) fn v2_request_is_fresh_at(issued_at_unix_ms: u64, now: Option<u64>) -> bool {
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

const fn cloud_mode(mode: gafctl_api::QuickConnectMode) -> QuickConnectCommandMode {
    match mode {
        gafctl_api::QuickConnectMode::Off => QuickConnectCommandMode::Off,
        gafctl_api::QuickConnectMode::Automatic => QuickConnectCommandMode::Automatic,
        gafctl_api::QuickConnectMode::Timer => QuickConnectCommandMode::Timer,
        gafctl_api::QuickConnectMode::Manual => QuickConnectCommandMode::Manual,
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

#[cfg(test)]
mod tests;
