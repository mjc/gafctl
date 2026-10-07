use super::*;
use crate::model::ControlPreset;

const CLEAR: DeviceCommand = DeviceCommand::LegacyPreset {
    preset: ControlPreset::TimerClear,
};

fn reserve_pending(
    history: &mut V2DeviceControlHistory,
    prefix: &str,
) -> Vec<tokio::sync::watch::Sender<Option<V2ControlStatus>>> {
    (0..8)
        .map(|index| {
            let id = CommandId::parse(&format!("{prefix}-{index}")).unwrap();
            reservation_is_execute(history.reserve(&id, CLEAR)).unwrap()
        })
        .collect()
}

fn reservation_is_wait(reservation: V2ControlReservation) -> bool {
    match reservation {
        V2ControlReservation::Wait(_) => true,
        V2ControlReservation::Execute(_) | V2ControlReservation::Completed(_) => false,
    }
}

fn reservation_is_reused(reservation: V2ControlReservation) -> bool {
    reservation_has_status(reservation, "request_id_reused")
}

fn reservation_has_status(reservation: V2ControlReservation, status: &str) -> bool {
    match reservation {
        V2ControlReservation::Completed(response) => response.as_str() == status,
        V2ControlReservation::Execute(_) | V2ControlReservation::Wait(_) => false,
    }
}

fn reservation_is_execute(
    reservation: V2ControlReservation,
) -> Option<tokio::sync::watch::Sender<Option<V2ControlStatus>>> {
    match reservation {
        V2ControlReservation::Execute(sender) => Some(sender),
        V2ControlReservation::Wait(_) | V2ControlReservation::Completed(_) => None,
    }
}

#[test]
fn control_replay_keeps_only_the_latest_completed_requests() {
    let mut history = V2DeviceControlHistory::default();
    let command = CLEAR;
    (0..=64).for_each(|index| {
        let id = CommandId::parse(&format!("completed-{index}")).unwrap();
        history.remember(id, command, V2ControlStatus::Unconfirmed);
    });
    assert_eq!(history.completed.len(), 64);
    assert!(reservation_has_status(
        history.reserve(&CommandId::parse("completed-1").unwrap(), command),
        "unconfirmed"
    ));
    assert!(reservation_has_status(
        history.reserve(
            &CommandId::parse(&format!("completed-{V2_REPLAY_CAPACITY}")).unwrap(),
            command,
        ),
        "unconfirmed"
    ));
    assert!(
        reservation_is_execute(history.reserve(&CommandId::parse("completed-0").unwrap(), command))
            .is_some()
    );
}

#[test]
fn v2_control_admission_bounds_distinct_requests_and_keeps_duplicate_joining() {
    let mut history = V2DeviceControlHistory::default();
    let command = CLEAR;
    let senders = reserve_pending(&mut history, "pending");
    let extra = CommandId::parse("excess-request").unwrap();
    assert!(reservation_has_status(
        history.reserve(&extra, command),
        "busy"
    ));
    assert_eq!(history.in_flight.len(), 8);
    assert!(reservation_is_wait(
        history.reserve(&CommandId::parse("pending-0").unwrap(), command)
    ));
    drop(senders);
}

#[test]
fn abandoned_control_reservations_release_capacity_without_replaying_writes() {
    let mut history = V2DeviceControlHistory::default();
    let command = CLEAR;
    let senders = reserve_pending(&mut history, "abandoned");
    drop(senders);
    let new_id = CommandId::parse("new-after-abandoned").unwrap();
    let sender = reservation_is_execute(history.reserve(&new_id, command));
    assert!(sender.is_some());
    assert_eq!(history.in_flight.len(), 1);
    assert!(reservation_has_status(
        history.reserve(&CommandId::parse("abandoned-0").unwrap(), command),
        "control_failed"
    ));
    assert!(reservation_is_reused(history.reserve(
        &CommandId::parse("abandoned-0").unwrap(),
        DeviceCommand::LegacyPreset {
            preset: ControlPreset::TimerOneMinute
        }
    )));
}

#[test]
fn control_admission_prunes_only_abandoned_reservations() {
    let mut history = V2DeviceControlHistory::default();
    let command = CLEAR;
    let mut senders = reserve_pending(&mut history, "mixed");
    drop(senders.pop());
    let new_id = CommandId::parse("replacement").unwrap();
    let replacement = reservation_is_execute(history.reserve(&new_id, command));
    assert!(replacement.is_some());
    assert!(reservation_is_wait(
        history.reserve(&CommandId::parse("mixed-0").unwrap(), command)
    ));
    assert!(reservation_has_status(
        history.reserve(&CommandId::parse("still-full").unwrap(), command),
        "busy"
    ));
}

#[tokio::test]
async fn completed_execution_frees_capacity_for_a_previously_busy_id() {
    let history = Arc::new(tokio::sync::Mutex::new(V2DeviceControlHistory::default()));
    let command = CLEAR;
    let mut senders = {
        let mut history = history.lock().await;
        reserve_pending(&mut history, "complete")
    };
    let extra = CommandId::parse("try-after-completion").unwrap();
    assert!(reservation_has_status(
        history.lock().await.reserve(&extra, command),
        "busy"
    ));
    let id = CommandId::parse(&format!("complete-{}", V2_IN_FLIGHT_CAPACITY - 1)).unwrap();
    let response = V2ControlStatus::Confirmed;
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
    let sender = reservation_is_execute(history.reserve(&extra, command));
    assert!(sender.is_some());
    assert!(reservation_has_status(
        history.reserve(&id, command),
        "confirmed"
    ));
}

#[test]
fn v2_control_reservations_deduplicate_without_serializing_distinct_commands() {
    let mut history = V2DeviceControlHistory::default();
    let request_id = CommandId::parse("same-request").unwrap();
    let other_request_id = CommandId::parse("newer-request").unwrap();
    let command = DeviceCommand::QuickConnectMode {
        mode: crate::model::QuickConnectMode::Automatic,
    };
    let sender = reservation_is_execute(history.reserve(&request_id, command)).unwrap();
    assert!(reservation_is_wait(history.reserve(&request_id, command),));
    assert!(reservation_is_reused(history.reserve(
        &request_id,
        DeviceCommand::LegacyPreset {
            preset: ControlPreset::TimerClear
        }
    )));
    assert!(reservation_is_execute(history.reserve(&other_request_id, command)).is_some());
    drop(sender);
    assert!(reservation_has_status(
        history.reserve(&request_id, command),
        "control_failed"
    ));
}

#[tokio::test]
async fn v2_control_execution_survives_waiter_cancellation_and_records_result() {
    let request_id = CommandId::parse("cancelled-waiter").unwrap();
    let command = CLEAR;
    let history = Arc::new(tokio::sync::Mutex::new(V2DeviceControlHistory::default()));
    let reservation = history.lock().await.reserve(&request_id, command);
    let sender = reservation_is_execute(reservation).unwrap();
    let cancelled_waiter = sender.subscribe();
    drop(cancelled_waiter);
    let (started_sender, started_receiver) = tokio::sync::oneshot::channel();
    let (finish_sender, finish_receiver) = tokio::sync::oneshot::channel();
    let response = V2ControlStatus::Unconfirmed;
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
    assert_eq!(history.completed.front().unwrap().2.as_str(), "unconfirmed");
}
