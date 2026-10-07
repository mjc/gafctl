#[cfg(feature = "mqtt")]
use crate::model::DeviceStateV2Response;
use crate::model::{DeviceInventoryStatus, DeviceState, unix_millis};
use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::SystemTime,
};
use tokio::sync::{Mutex, OwnedMutexGuard, OwnedSemaphorePermit, RwLock, Semaphore, watch};

const DEVICE_STATE_FRESHNESS_LIMIT_MS: u64 = 90_000;
const CONTROL_QUEUE_CAPACITY: usize = 8;

pub struct DeviceRuntime {
    observation: RwLock<RuntimeObservation>,
    transaction: Arc<Mutex<()>>,
    state_generation: AtomicU64,
    control_generation: AtomicU64,
    control_changed: watch::Sender<u64>,
    control_slots: Arc<Semaphore>,
    refresh: Mutex<Option<RefreshReceiver>>,
}

pub(crate) type RefreshReceiver =
    watch::Receiver<Option<Arc<crate::model::DeviceRefreshV2Response>>>;

pub(crate) enum RefreshReservation {
    Join(RefreshReceiver),
    Execute {
        receiver: RefreshReceiver,
        completion: watch::Sender<Option<Arc<crate::model::DeviceRefreshV2Response>>>,
    },
}

#[derive(Clone, Debug, Default)]
pub struct DeviceRuntimeSnapshot {
    pub state: Option<DeviceState>,
    pub inventory_status: DeviceInventoryStatus,
    pub last_error: Option<String>,
}

#[derive(Default)]
enum RuntimeObservation {
    #[default]
    Unknown,
    Available(DeviceState),
    Unavailable(RuntimeUnavailableReason),
}

impl RuntimeObservation {
    fn projection_at(
        &self,
        now_unix_ms: Option<u64>,
    ) -> (
        Option<&DeviceState>,
        DeviceInventoryStatus,
        Option<&'static str>,
    ) {
        match self {
            Self::Unknown => (None, DeviceInventoryStatus::Unknown, None),
            Self::Available(state) => {
                let fresh = now_unix_ms.is_some_and(|now| state_is_fresh(state, now));
                (
                    fresh.then_some(state),
                    DeviceInventoryStatus::Present,
                    (!fresh).then_some("device state expired"),
                )
            }
            Self::Unavailable(reason) => {
                let (inventory_status, message) = reason.status_message();
                (None, inventory_status, Some(message))
            }
        }
    }

    fn snapshot_at(&self, now_unix_ms: Option<u64>) -> DeviceRuntimeSnapshot {
        let (state, inventory_status, last_error) = self.projection_at(now_unix_ms);
        DeviceRuntimeSnapshot {
            state: state.cloned(),
            inventory_status,
            last_error: last_error.map(str::to_owned),
        }
    }

    #[cfg(feature = "mqtt")]
    fn matches_response_at(
        &self,
        response: &DeviceStateV2Response,
        now_unix_ms: Option<u64>,
        error_override: Option<&str>,
        inventory_unavailable: bool,
    ) -> bool {
        let (state, inventory_status, last_error) = self.projection_at(now_unix_ms);
        let inventory_status = if inventory_unavailable {
            DeviceInventoryStatus::Unavailable
        } else {
            inventory_status
        };
        response.available == state.is_some()
            && response.state.as_ref() == state
            && response.inventory_status == inventory_status
            && response.last_error.as_deref() == error_override.or(last_error)
    }
}

#[derive(Clone, Copy)]
enum RuntimeUnavailableReason {
    Detail,
    Missing,
    Inventory,
    ControlReadback,
    Bluetooth,
    RefreshDeadline,
}

impl RuntimeUnavailableReason {
    fn status_message(self) -> (DeviceInventoryStatus, &'static str) {
        match self {
            Self::RefreshDeadline => (
                DeviceInventoryStatus::Unavailable,
                "device refresh deadline exceeded",
            ),
            Self::Bluetooth => (
                DeviceInventoryStatus::Unavailable,
                "Bluetooth state unavailable",
            ),
            Self::Detail => (
                DeviceInventoryStatus::Present,
                "QuickConnect device detail unavailable",
            ),
            Self::Missing => (
                DeviceInventoryStatus::Missing,
                "QuickConnect device absent from inventory",
            ),
            Self::Inventory => (
                DeviceInventoryStatus::Unavailable,
                "QuickConnect inventory unavailable",
            ),
            Self::ControlReadback => (
                DeviceInventoryStatus::Present,
                "QuickConnect control readback unavailable",
            ),
        }
    }
}

impl DeviceRuntime {
    pub(super) fn new() -> Self {
        let (control_changed, _) = watch::channel(0);
        Self {
            observation: RwLock::new(RuntimeObservation::default()),
            transaction: Arc::new(Mutex::new(())),
            state_generation: AtomicU64::new(0),
            control_generation: AtomicU64::new(0),
            control_changed,
            control_slots: Arc::new(Semaphore::new(CONTROL_QUEUE_CAPACITY)),
            refresh: Mutex::new(None),
        }
    }

    pub(crate) async fn reserve_refresh(&self) -> RefreshReservation {
        let mut pending = self.refresh.lock().await;
        if let Some(receiver) = pending.as_ref()
            && receiver.borrow().is_none()
            && receiver.has_changed().is_ok()
        {
            return RefreshReservation::Join(receiver.clone());
        }
        let (completion, receiver) = watch::channel(None);
        *pending = Some(receiver.clone());
        RefreshReservation::Execute {
            receiver,
            completion,
        }
    }

    #[cfg(test)]
    pub async fn state(&self) -> Option<DeviceState> {
        self.snapshot().await.state
    }

    pub async fn snapshot(&self) -> DeviceRuntimeSnapshot {
        self.snapshot_at_option(unix_millis(SystemTime::now()))
            .await
    }

    #[cfg(test)]
    pub async fn snapshot_at(&self, now_unix_ms: u64) -> DeviceRuntimeSnapshot {
        self.snapshot_at_option(Some(now_unix_ms)).await
    }

    async fn snapshot_at_option(&self, now_unix_ms: Option<u64>) -> DeviceRuntimeSnapshot {
        self.observation.read().await.snapshot_at(now_unix_ms)
    }

    #[cfg(feature = "mqtt")]
    pub(crate) async fn matches_response_at(
        &self,
        response: &crate::model::DeviceStateV2Response,
        now_unix_ms: Option<u64>,
        error_override: Option<&str>,
        inventory_unavailable: bool,
    ) -> bool {
        self.observation.read().await.matches_response_at(
            response,
            now_unix_ms,
            error_override,
            inventory_unavailable,
        )
    }

    #[cfg(all(test, feature = "mqtt"))]
    pub(crate) async fn block_snapshot_for_test(&self) -> impl Send + '_ {
        self.observation.write().await
    }

    pub async fn set_state(&self, state: DeviceState) {
        let mut observation = self.observation.write().await;
        self.state_generation.fetch_add(1, Ordering::AcqRel);
        *observation = RuntimeObservation::Available(state);
    }

    pub(crate) async fn mark_ble_state_unavailable(&self) {
        let mut observation = self.observation.write().await;
        self.state_generation.fetch_add(1, Ordering::AcqRel);
        *observation = RuntimeObservation::Unavailable(RuntimeUnavailableReason::Bluetooth);
    }

    pub(crate) fn state_generation(&self) -> u64 {
        self.state_generation.load(Ordering::Acquire)
    }

    pub(crate) async fn mark_refresh_unavailable_if_current(&self, generation: u64) -> bool {
        self.mark_unavailable_if_current(generation, RuntimeUnavailableReason::RefreshDeadline)
            .await
    }

    pub(crate) fn is_current_state_read(&self, generation: u64) -> bool {
        self.state_generation.load(Ordering::Acquire) == generation
    }

    pub fn begin_state_read(&self) -> u64 {
        self.state_generation
            .fetch_add(1, Ordering::AcqRel)
            .saturating_add(1)
    }

    pub async fn set_state_if_current(&self, generation: u64, state: DeviceState) -> bool {
        let mut observation = self.observation.write().await;
        if !self.is_current_state_read(generation) {
            return false;
        }
        *observation = RuntimeObservation::Available(state);
        true
    }

    pub async fn mark_detail_unavailable_if_current(&self, generation: u64) -> bool {
        self.mark_unavailable_if_current(generation, RuntimeUnavailableReason::Detail)
            .await
    }

    pub(super) async fn mark_missing_if_current(&self, generation: u64) -> bool {
        self.mark_unavailable_if_current(generation, RuntimeUnavailableReason::Missing)
            .await
    }

    pub(super) async fn mark_inventory_unavailable_if_current(&self, generation: u64) -> bool {
        self.mark_unavailable_if_current(generation, RuntimeUnavailableReason::Inventory)
            .await
    }

    async fn mark_unavailable_if_current(
        &self,
        generation: u64,
        reason: RuntimeUnavailableReason,
    ) -> bool {
        let mut observation = self.observation.write().await;
        if !self.is_current_state_read(generation) {
            return false;
        }
        *observation = RuntimeObservation::Unavailable(reason);
        true
    }

    pub fn try_reserve_control(&self) -> Option<OwnedSemaphorePermit> {
        Arc::clone(&self.control_slots).try_acquire_owned().ok()
    }

    pub fn begin_control_intent(&self) -> u64 {
        self.state_generation.fetch_add(1, Ordering::AcqRel);
        let generation = self.next_control_generation();
        self.publish_control_generation(generation);
        generation
    }

    fn next_control_generation(&self) -> u64 {
        self.control_generation
            .fetch_add(1, Ordering::AcqRel)
            .saturating_add(1)
    }

    fn publish_control_generation(&self, generation: u64) {
        self.control_changed.send_if_modified(|published| {
            if *published < generation {
                *published = generation;
                true
            } else {
                false
            }
        });
    }

    pub fn is_current_control_intent(&self, generation: u64) -> bool {
        self.control_generation.load(Ordering::Acquire) == generation
    }

    pub async fn wait_for_control_change(&self, generation: u64) {
        let mut changed = self.control_changed.subscribe();
        let _ = changed.wait_for(|current| *current != generation).await;
    }

    pub async fn set_control_state_if_current(&self, generation: u64, state: DeviceState) -> bool {
        let mut observation = self.observation.write().await;
        if !self.is_current_control_intent(generation) {
            return false;
        }
        self.state_generation.fetch_add(1, Ordering::AcqRel);
        *observation = RuntimeObservation::Available(state);
        true
    }

    pub async fn mark_control_state_unavailable_if_current(&self, generation: u64) -> bool {
        let mut observation = self.observation.write().await;
        if !self.is_current_control_intent(generation) {
            return false;
        }
        self.state_generation.fetch_add(1, Ordering::AcqRel);
        *observation = RuntimeObservation::Unavailable(RuntimeUnavailableReason::ControlReadback);
        true
    }

    pub async fn acquire_transaction(&self) -> OwnedMutexGuard<()> {
        Arc::clone(&self.transaction).lock_owned().await
    }

    #[cfg(test)]
    pub fn try_acquire_transaction(&self) -> Option<OwnedMutexGuard<()>> {
        Arc::clone(&self.transaction).try_lock_owned().ok()
    }
}

fn state_is_fresh(state: &DeviceState, now_unix_ms: u64) -> bool {
    let provenance = &state.provenance;
    provenance.fetched_at_unix_ms.is_some_and(|fetched| {
        [Some(fetched), provenance.observed_at_unix_ms]
            .into_iter()
            .flatten()
            .all(|time| {
                now_unix_ms
                    .checked_sub(time)
                    .is_some_and(|age| age <= DEVICE_STATE_FRESHNESS_LIMIT_MS)
            })
    })
}

#[cfg(test)]
mod tests {
    use super::super::{DeviceRegistry, test_support::*};
    use super::*;
    use crate::model::{DeviceSettings, QuickConnectModeStatus};
    use crate::test_support::{cloud_device, identity_store_fixture};
    async fn assert_observation(
        runtime: &DeviceRuntime,
        state: Option<DeviceState>,
        inventory_status: DeviceInventoryStatus,
        last_error: Option<&str>,
    ) {
        let snapshot = runtime.snapshot_at(50_000).await;
        assert_eq!(snapshot.state, state);
        assert_eq!(snapshot.inventory_status, inventory_status);
        assert_eq!(snapshot.last_error.as_deref(), last_error);
    }

    #[tokio::test]
    async fn runtime_observation_transitions_report_failures_and_recover() {
        let runtime = DeviceRuntime::new();
        assert_observation(&runtime, None, DeviceInventoryStatus::Unknown, None).await;
        let state = observed_state(Some(1));
        runtime.set_state(state.clone()).await;
        assert_observation(
            &runtime,
            Some(state.clone()),
            DeviceInventoryStatus::Present,
            None,
        )
        .await;

        for (reason, status, error) in [
            (
                RuntimeUnavailableReason::Detail,
                DeviceInventoryStatus::Present,
                "QuickConnect device detail unavailable",
            ),
            (
                RuntimeUnavailableReason::Missing,
                DeviceInventoryStatus::Missing,
                "QuickConnect device absent from inventory",
            ),
            (
                RuntimeUnavailableReason::Inventory,
                DeviceInventoryStatus::Unavailable,
                "QuickConnect inventory unavailable",
            ),
            (
                RuntimeUnavailableReason::ControlReadback,
                DeviceInventoryStatus::Present,
                "QuickConnect control readback unavailable",
            ),
        ] {
            let generation = match reason {
                RuntimeUnavailableReason::ControlReadback => runtime.begin_control_intent(),
                _ => runtime.begin_state_read(),
            };
            for _ in 0..2 {
                assert!(match reason {
                    RuntimeUnavailableReason::RefreshDeadline =>
                        runtime
                            .mark_refresh_unavailable_if_current(generation)
                            .await,
                    RuntimeUnavailableReason::Bluetooth => {
                        runtime.mark_ble_state_unavailable().await;
                        true
                    }
                    RuntimeUnavailableReason::Detail =>
                        runtime.mark_detail_unavailable_if_current(generation).await,
                    RuntimeUnavailableReason::Missing =>
                        runtime.mark_missing_if_current(generation).await,
                    RuntimeUnavailableReason::Inventory =>
                        runtime
                            .mark_inventory_unavailable_if_current(generation)
                            .await,
                    RuntimeUnavailableReason::ControlReadback =>
                        runtime
                            .mark_control_state_unavailable_if_current(generation)
                            .await,
                });
            }
            assert_observation(&runtime, None, status, Some(error)).await;
            match reason {
                RuntimeUnavailableReason::ControlReadback => assert!(
                    runtime
                        .set_control_state_if_current(generation, state.clone())
                        .await
                ),
                _ => runtime.set_state(state.clone()).await,
            }
            assert_observation(
                &runtime,
                Some(state.clone()),
                DeviceInventoryStatus::Present,
                None,
            )
            .await;
        }
    }

    #[tokio::test]
    async fn runtime_observation_freshness_is_projected_without_changing_stored_state() {
        let runtime = DeviceRuntime::new();
        let state = observed_state(Some(1));
        runtime.set_state(state.clone()).await;
        assert_eq!(runtime.snapshot_at(90_001).await.state, Some(state.clone()));
        let expired = runtime.snapshot_at(90_002).await;
        assert_eq!(expired.state, None);
        assert_eq!(expired.inventory_status, DeviceInventoryStatus::Present);
        assert_eq!(expired.last_error.as_deref(), Some("device state expired"));
        assert_eq!(runtime.snapshot_at(0).await.state, None);
        assert_eq!(runtime.snapshot_at_option(None).await.state, None);
        assert_eq!(runtime.snapshot_at(50_000).await.state, Some(state));

        runtime.set_state(observed_state(None)).await;
        assert_observation(
            &runtime,
            None,
            DeviceInventoryStatus::Present,
            Some("device state expired"),
        )
        .await;
    }

    #[tokio::test]
    async fn overlapping_refreshes_share_one_worker_and_abandoned_workers_are_replaced() {
        let runtime = DeviceRuntime::new();
        let first = runtime.reserve_refresh().await;
        let completion = match first {
            RefreshReservation::Execute { completion, .. } => Some(completion),
            RefreshReservation::Join(_) => None,
        }
        .expect("first request starts a worker");
        assert!(refresh_joins(runtime.reserve_refresh().await));
        drop(completion);
        assert!(!refresh_joins(runtime.reserve_refresh().await));
    }

    fn refresh_joins(reservation: RefreshReservation) -> bool {
        match reservation {
            RefreshReservation::Join(_) => true,
            RefreshReservation::Execute { .. } => false,
        }
    }

    #[test]
    fn control_generation_publication_stays_monotonic_when_calls_reorder() {
        let runtime = Arc::new(DeviceRuntime::new());
        let first_generation = runtime.next_control_generation();
        let later_generation = runtime.next_control_generation();
        let barrier = Arc::new(std::sync::Barrier::new(3));
        let later_runtime = Arc::clone(&runtime);
        let later_barrier = Arc::clone(&barrier);
        let later = std::thread::spawn(move || {
            later_barrier.wait();
            later_runtime.publish_control_generation(later_generation);
        });
        let first_runtime = Arc::clone(&runtime);
        let first_barrier = Arc::clone(&barrier);
        let first = std::thread::spawn(move || {
            first_barrier.wait();
            first_runtime.publish_control_generation(first_generation);
        });

        barrier.wait();
        later.join().unwrap();
        first.join().unwrap();

        assert_eq!(*runtime.control_changed.borrow(), later_generation);
    }

    #[tokio::test]
    async fn each_device_owns_independent_state_and_transaction_lock() {
        let (_directory, path) = identity_store_fixture();
        let mut registry = DeviceRegistry::load(&path).unwrap();
        let ids = registry
            .reconcile_quickconnect(
                "account-a",
                &[
                    cloud_device("provider-a", "One"),
                    cloud_device("provider-b", "Two"),
                ],
            )
            .unwrap();
        let first = registry.runtime(&ids[0]).unwrap();
        let second = registry.runtime(&ids[1]).unwrap();
        let first_transaction = first.acquire_transaction().await;
        assert!(first.try_acquire_transaction().is_none());
        assert!(second.try_acquire_transaction().is_some());
        drop(first_transaction);

        let stale_poll = first.begin_state_read();
        let control_generation = first.begin_control_intent();
        let confirmed = DeviceState {
            humidity_percent: None,
            settings: DeviceSettings::QuickConnect {
                mode: QuickConnectModeStatus::Automatic,
                automatic_temperature_f: None,
                automatic_humidity_percent: None,
                timer_duration_minutes: None,
                humidity_monitor: None,
            },
            ..observed_state(Some(1))
        };
        assert!(
            first
                .set_control_state_if_current(control_generation, confirmed.clone())
                .await
        );
        assert!(!first.set_state_if_current(stale_poll, confirmed).await);
        assert_eq!(
            first.snapshot_at(50_000).await.state.unwrap().temperature_f,
            Some(102.0)
        );
        let expired = first.snapshot_at(90_002).await;
        assert!(expired.state.is_none());
        assert!(first.snapshot_at(0).await.state.is_none());
        assert!(first.snapshot_at_option(None).await.state.is_none());
        assert!(second.state().await.is_none());
        let permits = std::iter::repeat_with(|| first.try_reserve_control().unwrap())
            .take(CONTROL_QUEUE_CAPACITY)
            .collect::<Vec<_>>();
        assert!(first.try_reserve_control().is_none());
        drop(permits);
    }
}
