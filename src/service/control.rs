use super::quickconnect::{QuickConnectControlIntent, QuickConnectControlStatus};
use super::{DeviceService, legacy::ControlAdmissionError};
use crate::model::{CommandId, is_fresh_at, unix_millis};
use crate::model::{
    ControlStatus as V2ControlStatus, DeviceControlV2Request, DeviceControlV2Response,
};
use crate::model::{DeviceBackend, DeviceCommand, DeviceId};
use std::{
    collections::{HashMap, VecDeque},
    sync::Arc,
    time::{Duration, SystemTime},
};

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
    completed: VecDeque<(CommandId, DeviceCommand, V2ControlStatus)>,
    in_flight: HashMap<
        CommandId,
        (
            DeviceCommand,
            tokio::sync::watch::Receiver<Option<V2ControlStatus>>,
        ),
    >,
}

enum V2ControlReservation {
    Execute(tokio::sync::watch::Sender<Option<V2ControlStatus>>),
    Wait(tokio::sync::watch::Receiver<Option<V2ControlStatus>>),
    Completed(V2ControlStatus),
}

impl DeviceService {
    pub(crate) async fn control(
        &self,
        id: DeviceId,
        request: DeviceControlV2Request,
    ) -> DeviceControlV2Response {
        let request_id = request.request_id.clone();
        let status = self.control_status(id, request).await;
        DeviceControlV2Response {
            request_id: request_id.as_str().to_owned(),
            status,
        }
    }

    async fn control_status(
        &self,
        id: DeviceId,
        request: DeviceControlV2Request,
    ) -> V2ControlStatus {
        if !v2_request_is_fresh(request.issued_at_unix_ms) {
            return V2ControlStatus::StaleRequest;
        }

        let registered = self.registry.read().await.descriptor(&id).is_some();
        if !registered {
            return V2ControlStatus::UnknownDevice;
        }
        let history = Arc::clone(
            self.v2_control_results
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
            history.reserve(&request.request_id, request.command)
        };
        let mut receiver = match reservation {
            V2ControlReservation::Completed(status) => return status,
            V2ControlReservation::Wait(receiver) => receiver,
            V2ControlReservation::Execute(sender) => {
                let receiver = sender.subscribe();
                let task_state = self.clone();
                drop(spawn_v2_control_execution(
                    history,
                    request.request_id.clone(),
                    request.command,
                    sender,
                    async move { execute_v2_control(&task_state, &id, &request).await },
                ));
                receiver
            }
        };
        receiver
            .wait_for(Option::is_some)
            .await
            .ok()
            .and_then(|response| response.clone())
            .unwrap_or(V2ControlStatus::ControlFailed)
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
) -> V2ControlStatus {
    let backend = state.registry.read().await.dispatch(id, request.command);
    match backend {
        Ok(DeviceBackend::LegacyBle) => execute_ble_v2_control(state, request).await,
        Ok(DeviceBackend::QuickConnect) => execute_cloud_v2_control(state, id, request).await,
        Err(crate::backend::DeviceRegistryError::UnknownDevice) => V2ControlStatus::UnknownDevice,
        Err(crate::backend::DeviceRegistryError::UnsupportedCommand) => {
            V2ControlStatus::UnsupportedCommand
        }
        Err(_) => V2ControlStatus::ControlFailed,
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
    let Some(intent) = QuickConnectControlIntent::new(request.issued_at_unix_ms, request.command)
    else {
        return V2ControlStatus::UnsupportedCommand;
    };
    quickconnect_control_status(service.execute(id, intent).await)
}

fn spawn_v2_control_execution(
    history: Arc<tokio::sync::Mutex<V2DeviceControlHistory>>,
    request_id: CommandId,
    command: DeviceCommand,
    sender: tokio::sync::watch::Sender<Option<V2ControlStatus>>,
    execution: impl std::future::Future<Output = V2ControlStatus> + Send + 'static,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let status = execution.await;
        {
            let mut history = history.lock().await;
            history.in_flight.remove(&request_id);
            history.remember(request_id, command, status.clone());
        }
        sender.send_replace(Some(status));
    })
}

impl V2DeviceControlHistory {
    fn reserve(&mut self, request_id: &CommandId, command: DeviceCommand) -> V2ControlReservation {
        self.reap_abandoned();
        if let Some((_, previous_command, status)) = self
            .completed
            .iter()
            .find(|(previous_id, _, _)| previous_id == request_id)
        {
            return V2ControlReservation::Completed(if *previous_command == command {
                status.clone()
            } else {
                V2ControlStatus::RequestIdReused
            });
        }
        if let Some((previous_command, receiver)) = self.in_flight.get(request_id) {
            if *previous_command != command {
                return V2ControlReservation::Completed(V2ControlStatus::RequestIdReused);
            }
            if receiver.has_changed().is_ok() {
                return V2ControlReservation::Wait(receiver.clone());
            }
            self.in_flight.remove(request_id);
            self.remember(request_id.clone(), command, V2ControlStatus::ControlFailed);
            return V2ControlReservation::Completed(V2ControlStatus::ControlFailed);
        }
        if self.in_flight.len() >= V2_IN_FLIGHT_CAPACITY {
            return V2ControlReservation::Completed(V2ControlStatus::Busy);
        }
        let (sender, receiver) = tokio::sync::watch::channel(None);
        self.in_flight
            .insert(request_id.clone(), (command, receiver));
        V2ControlReservation::Execute(sender)
    }

    fn reap_abandoned(&mut self) {
        let completed = &mut self.completed;
        self.in_flight
            .extract_if(|_, (_, receiver)| receiver.has_changed().is_err())
            .for_each(|(request_id, (command, _))| {
                remember_control_status(
                    completed,
                    request_id,
                    command,
                    V2ControlStatus::ControlFailed,
                );
            });
    }

    fn remember(&mut self, request_id: CommandId, command: DeviceCommand, status: V2ControlStatus) {
        remember_control_status(&mut self.completed, request_id, command, status);
    }
}

fn remember_control_status(
    completed: &mut VecDeque<(CommandId, DeviceCommand, V2ControlStatus)>,
    request_id: CommandId,
    command: DeviceCommand,
    status: V2ControlStatus,
) {
    completed.push_back((request_id, command, status));
    if completed.len() > V2_REPLAY_CAPACITY {
        completed.pop_front();
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
