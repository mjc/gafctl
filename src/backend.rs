use std::{
    collections::{BTreeMap, HashSet},
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::Arc,
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::{Mutex, OwnedMutexGuard, OwnedSemaphorePermit, RwLock, Semaphore, watch};

use crate::device::{
    DeviceBackend, DeviceCapabilities, DeviceCommand, DeviceDescriptor, DeviceDiagnostics,
    DeviceId, DeviceSettings, DeviceState, EntitySource, ProxyId, QuickConnectModeStatus,
    StateProvenance,
};
use updraft_quickconnect::QuickConnectCommand;

const DEVICE_STATE_FRESHNESS_LIMIT_MS: u64 = 90_000;
const CONTROL_QUEUE_CAPACITY: usize = 8;

#[derive(Clone, Eq, Hash, Ord, PartialEq, PartialOrd, Deserialize, Serialize)]
struct ProviderIdentity {
    account_id: String,
    provider_id: String,
}

impl std::fmt::Debug for ProviderIdentity {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("ProviderIdentity([redacted])")
    }
}

#[derive(Clone, Deserialize, Serialize)]
struct IdentityBinding {
    identity: ProviderIdentity,
    local_id: DeviceId,
}

#[derive(Deserialize)]
struct IdentityFile {
    version: u8,
    proxy_id: ProxyId,
    sources: BTreeMap<DeviceId, EntitySource>,
    bindings: Vec<IdentityBinding>,
}

#[derive(Serialize)]
struct IdentityFileContents<'a> {
    version: u8,
    proxy_id: ProxyId,
    sources: &'a BTreeMap<DeviceId, EntitySource>,
    bindings: &'a [IdentityBinding],
}

pub struct CloudDeviceInput {
    provider_id: String,
    name: String,
}

impl CloudDeviceInput {
    pub fn new(provider_id: String, name: String) -> Self {
        Self { provider_id, name }
    }
}

#[derive(Debug, Error)]
pub enum DeviceRegistryError {
    #[error("cloud device identity is invalid")]
    InvalidIdentity,
    #[error("cloud inventory contains duplicate device identifiers")]
    DuplicateProviderId,
    #[error("identity store is invalid")]
    InvalidStore,
    #[error("cloud identities require a configured persistent store")]
    PersistenceUnavailable,
    #[error("device is not registered")]
    UnknownDevice,
    #[error("a device must have one Home Assistant entity source")]
    MixedSources,
    #[error("command is not supported by this device backend")]
    UnsupportedCommand,
    #[error("could not access the device identity store")]
    Io(#[source] io::Error),
    #[error("could not encode the device identity store")]
    Encoding(#[source] serde_json::Error),
}

#[derive(Debug, Error)]
pub enum QuickConnectPollingError {
    #[error(transparent)]
    Client(#[from] updraft_quickconnect::ClientError),
    #[error(transparent)]
    Registry(#[from] DeviceRegistryError),
}

pub struct DeviceRegistry {
    identities: IdentityStore,
    devices: BTreeMap<DeviceId, DeviceDescriptor>,
    runtimes: BTreeMap<DeviceId, Arc<DeviceRuntime>>,
    quickconnect_writes_enabled: bool,
}

impl Default for DeviceRegistry {
    fn default() -> Self {
        Self::new()
    }
}

pub struct DeviceRuntime {
    snapshot: RwLock<DeviceRuntimeSnapshot>,
    transaction: Arc<Mutex<()>>,
    state_generation: AtomicU64,
    control_generation: AtomicU64,
    control_changed: watch::Sender<u64>,
    control_slots: Arc<Semaphore>,
    refresh: Mutex<Option<RefreshReceiver>>,
}

pub(crate) type RefreshReceiver =
    watch::Receiver<Option<Arc<updraft_api::DeviceRefreshV2Response>>>;

pub(crate) enum RefreshReservation {
    Join(RefreshReceiver),
    Execute {
        receiver: RefreshReceiver,
        completion: watch::Sender<Option<Arc<updraft_api::DeviceRefreshV2Response>>>,
    },
}

pub use updraft_api::DeviceInventoryStatus;

#[derive(Clone, Debug, Default)]
pub struct DeviceRuntimeSnapshot {
    pub state: Option<DeviceState>,
    pub last_successful_state: Option<DeviceState>,
    pub inventory_status: DeviceInventoryStatus,
    pub last_error: Option<String>,
}

impl DeviceRuntime {
    fn new() -> Self {
        let (control_changed, _) = watch::channel(0);
        Self {
            snapshot: RwLock::new(DeviceRuntimeSnapshot::default()),
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

    pub async fn state(&self) -> Option<DeviceState> {
        self.snapshot().await.state
    }

    pub async fn snapshot(&self) -> DeviceRuntimeSnapshot {
        self.snapshot_at_option(unix_millis(SystemTime::now()))
            .await
    }

    pub async fn snapshot_at(&self, now_unix_ms: u64) -> DeviceRuntimeSnapshot {
        self.snapshot_at_option(Some(now_unix_ms)).await
    }

    async fn snapshot_at_option(&self, now_unix_ms: Option<u64>) -> DeviceRuntimeSnapshot {
        let mut snapshot = self.snapshot.read().await.clone();
        if snapshot
            .state
            .as_ref()
            .is_some_and(|state| now_unix_ms.is_none_or(|now| !state_is_fresh(state, now)))
        {
            snapshot.state = None;
            snapshot.last_error = Some("device state expired".to_owned());
        }
        snapshot
    }

    pub async fn set_state(&self, state: DeviceState) {
        let mut snapshot = self.snapshot.write().await;
        self.state_generation.fetch_add(1, Ordering::AcqRel);
        snapshot.state = Some(state.clone());
        snapshot.last_successful_state = Some(state);
        snapshot.inventory_status = DeviceInventoryStatus::Present;
        snapshot.last_error = None;
    }

    pub fn begin_state_read(&self) -> u64 {
        self.state_generation
            .fetch_add(1, Ordering::AcqRel)
            .saturating_add(1)
    }

    pub async fn set_state_if_current(&self, generation: u64, state: DeviceState) -> bool {
        let mut snapshot = self.snapshot.write().await;
        if self.state_generation.load(Ordering::Acquire) != generation {
            return false;
        }
        snapshot.state = Some(state.clone());
        snapshot.last_successful_state = Some(state);
        snapshot.inventory_status = DeviceInventoryStatus::Present;
        snapshot.last_error = None;
        true
    }

    pub async fn mark_detail_unavailable_if_current(&self, generation: u64) -> bool {
        self.update_error_if_current(
            generation,
            DeviceInventoryStatus::Present,
            "QuickConnect device detail unavailable",
        )
        .await
    }

    async fn mark_missing_if_current(&self, generation: u64) -> bool {
        self.update_error_if_current(
            generation,
            DeviceInventoryStatus::Missing,
            "QuickConnect device absent from inventory",
        )
        .await
    }

    async fn mark_inventory_unavailable_if_current(&self, generation: u64) -> bool {
        self.update_error_if_current(
            generation,
            DeviceInventoryStatus::Unavailable,
            "QuickConnect inventory unavailable",
        )
        .await
    }

    async fn update_error_if_current(
        &self,
        generation: u64,
        inventory_status: DeviceInventoryStatus,
        message: &'static str,
    ) -> bool {
        let mut snapshot = self.snapshot.write().await;
        if self.state_generation.load(Ordering::Acquire) != generation {
            return false;
        }
        snapshot.state = None;
        snapshot.inventory_status = inventory_status;
        snapshot.last_error = Some(message.to_owned());
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
        if *changed.borrow_and_update() == generation {
            let _ = changed.changed().await;
        }
    }

    pub async fn set_control_state_if_current(&self, generation: u64, state: DeviceState) -> bool {
        let mut snapshot = self.snapshot.write().await;
        if !self.is_current_control_intent(generation) {
            return false;
        }
        self.state_generation.fetch_add(1, Ordering::AcqRel);
        snapshot.state = Some(state.clone());
        snapshot.last_successful_state = Some(state);
        snapshot.inventory_status = DeviceInventoryStatus::Present;
        snapshot.last_error = None;
        true
    }

    pub async fn mark_control_state_unavailable_if_current(&self, generation: u64) -> bool {
        let mut snapshot = self.snapshot.write().await;
        if !self.is_current_control_intent(generation) {
            return false;
        }
        self.state_generation.fetch_add(1, Ordering::AcqRel);
        snapshot.state = None;
        snapshot.inventory_status = DeviceInventoryStatus::Present;
        snapshot.last_error = Some("QuickConnect control readback unavailable".to_owned());
        true
    }

    pub async fn acquire_transaction(&self) -> OwnedMutexGuard<()> {
        Arc::clone(&self.transaction).lock_owned().await
    }

    pub fn try_acquire_transaction(&self) -> Option<OwnedMutexGuard<()>> {
        Arc::clone(&self.transaction).try_lock_owned().ok()
    }
}

fn state_is_fresh(state: &DeviceState, now_unix_ms: u64) -> bool {
    state
        .provenance
        .fetched_at_unix_ms
        .and_then(|fetched_at| now_unix_ms.checked_sub(fetched_at))
        .is_some_and(|age| age <= DEVICE_STATE_FRESHNESS_LIMIT_MS)
}

fn unix_millis(time: SystemTime) -> Option<u64> {
    time.duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| u64::try_from(duration.as_millis()).ok())
}

impl DeviceRegistry {
    pub fn new() -> Self {
        Self {
            identities: IdentityStore::in_memory(),
            devices: BTreeMap::new(),
            runtimes: BTreeMap::new(),
            quickconnect_writes_enabled: false,
        }
    }

    pub fn load_optional(path: Option<PathBuf>) -> Result<Self, DeviceRegistryError> {
        path.map_or_else(|| Ok(Self::new()), Self::load)
    }

    pub fn load(path: impl Into<PathBuf>) -> Result<Self, DeviceRegistryError> {
        Ok(Self {
            identities: IdentityStore::load(path.into())?,
            devices: BTreeMap::new(),
            runtimes: BTreeMap::new(),
            quickconnect_writes_enabled: false,
        })
    }

    pub fn proxy_id(&self) -> ProxyId {
        self.identities.proxy_id
    }

    pub fn discovery_identities(&self) -> impl Iterator<Item = (DeviceId, DeviceBackend)> + '_ {
        std::iter::once((DeviceId::configured_ble(), DeviceBackend::LegacyBle)).chain(
            self.identities
                .bindings
                .iter()
                .map(|binding| (binding.local_id.clone(), DeviceBackend::QuickConnect)),
        )
    }

    pub fn mqtt_ownership_required(&self, ble_enabled: bool, account_id: Option<&str>) -> bool {
        self.identities.sources.iter().any(|(id, source)| {
            *source == EntitySource::Mqtt
                && if *id == DeviceId::configured_ble() {
                    ble_enabled
                } else {
                    self.identities.bindings.iter().any(|binding| {
                        binding.local_id == *id
                            && Some(binding.identity.account_id.as_str()) == account_id
                    })
                }
        })
    }

    pub fn register_configured_ble(&mut self) -> Arc<DeviceRuntime> {
        let descriptor = DeviceDescriptor::configured_ble();
        self.register(descriptor)
    }

    pub fn reconcile_quickconnect(
        &mut self,
        account_id: &str,
        devices: &[CloudDeviceInput],
    ) -> Result<Vec<DeviceId>, DeviceRegistryError> {
        let descriptors = self.identities.reconcile(account_id, devices)?;
        let ids = descriptors
            .iter()
            .map(|descriptor| descriptor.id.clone())
            .collect();
        descriptors.into_iter().for_each(|descriptor| {
            self.register(descriptor);
        });
        Ok(ids)
    }

    pub fn set_quickconnect_writes_enabled(&mut self, enabled: bool) {
        self.quickconnect_writes_enabled = enabled;
        self.devices
            .values_mut()
            .filter(|descriptor| descriptor.backend == DeviceBackend::QuickConnect)
            .for_each(|descriptor| {
                descriptor.capabilities = if enabled {
                    DeviceCapabilities::quickconnect_with_controls()
                } else {
                    DeviceCapabilities::quickconnect_read_only()
                };
            });
    }

    pub(crate) fn quickconnect_control_target(
        &self,
        account_id: &str,
        id: &DeviceId,
        command: QuickConnectCommand,
    ) -> Result<(Arc<DeviceRuntime>, String), DeviceRegistryError> {
        let descriptor = self
            .devices
            .get(id)
            .ok_or(DeviceRegistryError::UnknownDevice)?;
        let capability = match command {
            QuickConnectCommand::SetMode { .. } | QuickConnectCommand::ClearMode { .. } => {
                crate::device::CommandCapability::QuickConnectMode
            }
            QuickConnectCommand::SetAutomaticTargets { .. } => {
                crate::device::CommandCapability::QuickConnectTargets
            }
            QuickConnectCommand::SetTimerDuration { .. } => {
                crate::device::CommandCapability::QuickConnectTimerDuration
            }
        };
        if descriptor.backend != DeviceBackend::QuickConnect
            || !descriptor.capabilities.commands.contains(&capability)
        {
            return Err(DeviceRegistryError::UnsupportedCommand);
        }
        self.quickconnect_read_target(account_id, id)
    }

    pub(crate) fn quickconnect_read_target(
        &self,
        account_id: &str,
        id: &DeviceId,
    ) -> Result<(Arc<DeviceRuntime>, String), DeviceRegistryError> {
        let descriptor = self
            .devices
            .get(id)
            .ok_or(DeviceRegistryError::UnknownDevice)?;
        if descriptor.backend != DeviceBackend::QuickConnect || !descriptor.capabilities.read_state
        {
            return Err(DeviceRegistryError::UnsupportedCommand);
        }
        let provider_id = self
            .identities
            .bindings
            .iter()
            .find(|binding| binding.local_id == *id && binding.identity.account_id == account_id)
            .map(|binding| binding.identity.provider_id.clone())
            .ok_or(DeviceRegistryError::UnknownDevice)?;
        let runtime = self
            .runtimes
            .get(id)
            .cloned()
            .ok_or(DeviceRegistryError::UnknownDevice)?;
        Ok((runtime, provider_id))
    }

    pub async fn poll_quickconnect(
        &mut self,
        account_id: &str,
        client: &updraft_quickconnect::QuickConnectClient,
    ) -> Result<Vec<DeviceId>, QuickConnectPollingError> {
        let generations = self.begin_quickconnect_poll(account_id);
        let polls = match client.poll_devices().await {
            Ok(polls) => polls,
            Err(error) => {
                self.mark_quickconnect_inventory_unavailable(account_id, &generations)
                    .await;
                return Err(error.into());
            }
        };
        self.reconcile_quickconnect_polls(account_id, polls, &generations)
            .await
    }

    pub fn begin_quickconnect_poll(&self, account_id: &str) -> BTreeMap<DeviceId, u64> {
        self.identities
            .bindings
            .iter()
            .filter(|binding| binding.identity.account_id == account_id)
            .filter_map(|binding| {
                self.runtime(&binding.local_id)
                    .map(|runtime| (binding.local_id.clone(), runtime.begin_state_read()))
            })
            .collect()
    }

    pub async fn reconcile_quickconnect_polls(
        &mut self,
        account_id: &str,
        polls: Vec<updraft_quickconnect::QuickConnectDevicePoll>,
        generations: &BTreeMap<DeviceId, u64>,
    ) -> Result<Vec<DeviceId>, QuickConnectPollingError> {
        let inputs = polls
            .iter()
            .map(|poll| {
                CloudDeviceInput::new(
                    poll.inventory.provider_id().to_owned(),
                    poll.inventory
                        .name()
                        .unwrap_or("QuickConnect device")
                        .to_owned(),
                )
            })
            .collect::<Vec<_>>();
        let ids = match self.reconcile_quickconnect(account_id, &inputs) {
            Ok(ids) => ids,
            Err(error) => {
                self.mark_quickconnect_inventory_unavailable(account_id, generations)
                    .await;
                return Err(error.into());
            }
        };
        let present = polls
            .iter()
            .map(|poll| poll.inventory.provider_id().to_owned())
            .collect::<HashSet<_>>();
        let runtimes = ids
            .iter()
            .cloned()
            .zip(polls)
            .filter_map(|(id, poll)| {
                self.runtime(&id).map(|runtime| {
                    let generation = generations
                        .get(&id)
                        .copied()
                        .unwrap_or_else(|| runtime.begin_state_read());
                    (runtime, generation, poll)
                })
            })
            .collect::<Vec<_>>();
        let missing = self
            .identities
            .bindings
            .iter()
            .filter(|binding| binding.identity.account_id == account_id)
            .filter(|binding| !present.contains(&binding.identity.provider_id))
            .filter_map(|binding| {
                self.runtimes
                    .get(&binding.local_id)
                    .zip(generations.get(&binding.local_id))
                    .map(|(runtime, generation)| (Arc::clone(runtime), *generation))
            })
            .collect::<Vec<_>>();
        futures_util::stream::iter(missing)
            .for_each(|(runtime, generation)| async move {
                runtime.mark_missing_if_current(generation).await;
            })
            .await;
        futures_util::stream::iter(runtimes)
            .for_each(|(runtime, generation, poll)| async move {
                match poll.detail {
                    Ok(state) => {
                        runtime
                            .set_state_if_current(generation, common_state(state))
                            .await;
                    }
                    Err(_) => {
                        runtime.mark_detail_unavailable_if_current(generation).await;
                    }
                }
            })
            .await;
        Ok(ids)
    }

    pub async fn mark_quickconnect_inventory_unavailable(
        &self,
        account_id: &str,
        generations: &BTreeMap<DeviceId, u64>,
    ) {
        let unavailable = self
            .identities
            .bindings
            .iter()
            .filter(|binding| binding.identity.account_id == account_id)
            .filter_map(|binding| {
                self.runtimes
                    .get(&binding.local_id)
                    .zip(generations.get(&binding.local_id))
                    .map(|(runtime, generation)| (Arc::clone(runtime), *generation))
            })
            .collect::<Vec<_>>();
        futures_util::stream::iter(unavailable)
            .for_each(|(runtime, generation)| async move {
                runtime
                    .mark_inventory_unavailable_if_current(generation)
                    .await;
            })
            .await;
    }

    pub fn descriptors(&self) -> impl Iterator<Item = &DeviceDescriptor> {
        self.devices.values()
    }

    pub fn runtime(&self, id: &DeviceId) -> Option<Arc<DeviceRuntime>> {
        self.runtimes.get(id).cloned()
    }

    pub fn set_entity_sources(
        &mut self,
        id: &DeviceId,
        state: EntitySource,
        command: EntitySource,
    ) -> Result<(), DeviceRegistryError> {
        if state != command {
            return Err(DeviceRegistryError::MixedSources);
        }
        if !self.devices.contains_key(id) {
            return Err(DeviceRegistryError::UnknownDevice);
        }
        self.identities.set_sources(id, state)?;
        let descriptor = self
            .devices
            .get_mut(id)
            .ok_or(DeviceRegistryError::UnknownDevice)?;
        descriptor.state_source = state;
        descriptor.command_source = command;
        Ok(())
    }

    pub fn dispatch(
        &self,
        id: &DeviceId,
        command: DeviceCommand,
    ) -> Result<DeviceBackend, DeviceRegistryError> {
        let descriptor = self
            .devices
            .get(id)
            .ok_or(DeviceRegistryError::UnknownDevice)?;
        match (descriptor.backend, command) {
            (
                DeviceBackend::LegacyBle,
                DeviceCommand::LegacyPreset { .. }
                | DeviceCommand::LegacyAutomaticTemperature { .. }
                | DeviceCommand::LegacyAutomaticHumidity { .. }
                | DeviceCommand::LegacyTimer { .. },
            ) if descriptor.capabilities.supports(command) => Ok(DeviceBackend::LegacyBle),
            (
                DeviceBackend::QuickConnect,
                DeviceCommand::QuickConnectMode { .. }
                | DeviceCommand::QuickConnectConditionalOff { .. }
                | DeviceCommand::QuickConnectTargets { .. }
                | DeviceCommand::QuickConnectAutomaticTemperature { .. }
                | DeviceCommand::QuickConnectAutomaticHumidity { .. }
                | DeviceCommand::QuickConnectTimerDuration { .. },
            ) if descriptor.capabilities.supports(command) => Ok(DeviceBackend::QuickConnect),
            _ => Err(DeviceRegistryError::UnsupportedCommand),
        }
    }

    fn register(&mut self, mut descriptor: DeviceDescriptor) -> Arc<DeviceRuntime> {
        descriptor.proxy_id = self.identities.proxy_id;
        if self.quickconnect_writes_enabled && descriptor.backend == DeviceBackend::QuickConnect {
            descriptor.capabilities = DeviceCapabilities::quickconnect_with_controls();
        }
        let id = descriptor.id.clone();
        let source = self
            .identities
            .sources
            .get(&id)
            .copied()
            .unwrap_or(EntitySource::Http);
        descriptor.state_source = source;
        descriptor.command_source = source;
        self.devices.insert(id.clone(), descriptor);
        Arc::clone(
            self.runtimes
                .entry(id)
                .or_insert_with(|| Arc::new(DeviceRuntime::new())),
        )
    }

    #[cfg(test)]
    fn identity_count(&self) -> usize {
        self.identities.bindings.len()
    }
}

pub(crate) fn common_state(state: updraft_quickconnect::QuickConnectDeviceState) -> DeviceState {
    use updraft_quickconnect::DeviceModeStatus;

    DeviceState {
        temperature_f: state.temperature_f,
        humidity_percent: state.humidity_percent,
        settings: DeviceSettings::QuickConnect {
            mode: match state.settings.mode {
                DeviceModeStatus::Off => QuickConnectModeStatus::Off,
                DeviceModeStatus::Automatic => QuickConnectModeStatus::Automatic,
                DeviceModeStatus::Timer => QuickConnectModeStatus::Timer,
                DeviceModeStatus::Manual => QuickConnectModeStatus::Manual,
                DeviceModeStatus::Unknown => QuickConnectModeStatus::Unknown,
                DeviceModeStatus::Conflicting => QuickConnectModeStatus::Conflicting,
            },
            automatic_temperature_f: state.settings.automatic_temperature_f,
            automatic_humidity_percent: state.settings.automatic_humidity_percent,
            timer_duration_minutes: state.settings.timer_duration_minutes,
            humidity_monitor: state.settings.humidity_monitor,
        },
        estimated_running: state.estimated_running,
        diagnostics: Some(DeviceDiagnostics {
            firmware_version: state.diagnostics.firmware_version,
            signal_strength_raw: state.diagnostics.signal_strength_raw,
            verified_raw: state.diagnostics.verified_raw,
            ota_in_progress: state.diagnostics.ota_in_progress,
        }),
        provenance: StateProvenance {
            backend: DeviceBackend::QuickConnect,
            fetched_at_unix_ms: state.fetched_at_unix_ms,
            observed_at_unix_ms: state.observed_at_unix_ms,
        },
    }
}

struct IdentityStore {
    path: Option<PathBuf>,
    proxy_id: ProxyId,
    sources: BTreeMap<DeviceId, EntitySource>,
    bindings: Vec<IdentityBinding>,
}

impl IdentityStore {
    fn load(path: PathBuf) -> Result<Self, DeviceRegistryError> {
        match fs::read(&path) {
            Ok(bytes) => {
                let stored: IdentityFile = serde_json::from_slice(&bytes)
                    .map_err(|_| DeviceRegistryError::InvalidStore)?;
                let stored = validate_identity_file(stored)?;
                Ok(Self {
                    path: Some(path),
                    proxy_id: stored.proxy_id,
                    sources: stored.sources,
                    bindings: stored.bindings,
                })
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let store = Self {
                    path: Some(path),
                    ..Self::in_memory()
                };
                store.save(&store.bindings, &store.sources)?;
                Ok(store)
            }
            Err(error) => Err(DeviceRegistryError::Io(error)),
        }
    }

    fn in_memory() -> Self {
        Self {
            path: None,
            proxy_id: ProxyId::default(),
            sources: BTreeMap::new(),
            bindings: Vec::new(),
        }
    }

    fn reconcile(
        &mut self,
        account_id: &str,
        devices: &[CloudDeviceInput],
    ) -> Result<Vec<DeviceDescriptor>, DeviceRegistryError> {
        validate_inventory(account_id, devices)?;
        let mut bindings = self.bindings.clone();
        let (descriptors, changed) = devices.iter().try_fold(
            (Vec::with_capacity(devices.len()), false),
            |(mut descriptors, mut changed), device| {
                let identity = ProviderIdentity {
                    account_id: account_id.to_owned(),
                    provider_id: device.provider_id.clone(),
                };
                let local_id = match bindings.iter().find(|binding| binding.identity == identity) {
                    Some(binding) => binding.local_id.clone(),
                    None => {
                        let local_id = DeviceId::quickconnect();
                        bindings.push(IdentityBinding {
                            identity,
                            local_id: local_id.clone(),
                        });
                        changed = true;
                        local_id
                    }
                };
                descriptors.push(DeviceDescriptor {
                    proxy_id: self.proxy_id,
                    id: local_id,
                    name: device.name.clone(),
                    backend: DeviceBackend::QuickConnect,
                    capabilities: DeviceCapabilities::quickconnect_read_only(),
                    state_source: EntitySource::Http,
                    command_source: EntitySource::Http,
                });
                Ok::<_, DeviceRegistryError>((descriptors, changed))
            },
        )?;
        if changed {
            self.save(&bindings, &self.sources)?;
            self.bindings = bindings;
        }
        Ok(descriptors)
    }

    fn set_sources(
        &mut self,
        id: &DeviceId,
        value: EntitySource,
    ) -> Result<(), DeviceRegistryError> {
        if self.sources.get(id) == Some(&value) {
            return Ok(());
        }
        let previous = self.sources.insert(id.clone(), value);
        if self.path.is_some()
            && let Err(error) = self.save(&self.bindings, &self.sources)
        {
            match previous {
                Some(source) => {
                    self.sources.insert(id.clone(), source);
                }
                None => {
                    self.sources.remove(id);
                }
            }
            return Err(error);
        }
        Ok(())
    }

    fn save(
        &self,
        bindings: &[IdentityBinding],
        sources: &BTreeMap<DeviceId, EntitySource>,
    ) -> Result<(), DeviceRegistryError> {
        let path = self
            .path
            .as_deref()
            .ok_or(DeviceRegistryError::PersistenceUnavailable)?;
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent).map_err(DeviceRegistryError::Io)?;
        let contents = serde_json::to_vec(&IdentityFileContents {
            version: 2,
            proxy_id: self.proxy_id,
            sources,
            bindings,
        })
        .map_err(DeviceRegistryError::Encoding)?;
        let temporary_path = temporary_path(path);
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = options
            .open(&temporary_path)
            .map_err(DeviceRegistryError::Io)?;
        file.write_all(&contents)
            .and_then(|()| file.sync_all())
            .map_err(DeviceRegistryError::Io)?;
        fs::rename(temporary_path, path).map_err(DeviceRegistryError::Io)
    }
}

fn validate_inventory(
    account_id: &str,
    devices: &[CloudDeviceInput],
) -> Result<(), DeviceRegistryError> {
    if !valid_identity(account_id) {
        return Err(DeviceRegistryError::InvalidIdentity);
    }
    devices
        .iter()
        .map(|device| device.provider_id.as_str())
        .try_fold(
            HashSet::with_capacity(devices.len()),
            |mut seen, provider_id| {
                if !valid_identity(provider_id) {
                    return Err(DeviceRegistryError::InvalidIdentity);
                }
                if !seen.insert(provider_id) {
                    return Err(DeviceRegistryError::DuplicateProviderId);
                }
                Ok(seen)
            },
        )
        .map(|_| ())
}

fn validate_identity_file(file: IdentityFile) -> Result<IdentityFile, DeviceRegistryError> {
    if file.version != 2 {
        return Err(DeviceRegistryError::InvalidStore);
    }
    let valid = file.bindings.iter().try_fold(
        (
            HashSet::with_capacity(file.bindings.len()),
            HashSet::with_capacity(file.bindings.len()),
        ),
        |(mut identities, mut local_ids), binding| {
            if !valid_identity(&binding.identity.account_id)
                || !valid_identity(&binding.identity.provider_id)
                || DeviceId::parse(binding.local_id.as_str().to_owned()).is_none()
                || binding.local_id == DeviceId::configured_ble()
                || !identities.insert(binding.identity.clone())
                || !local_ids.insert(binding.local_id.clone())
            {
                return Err(DeviceRegistryError::InvalidStore);
            }
            Ok((identities, local_ids))
        },
    );
    let (_, local_ids) = valid?;
    if file
        .sources
        .keys()
        .any(|id| *id != DeviceId::configured_ble() && !local_ids.contains(id))
    {
        return Err(DeviceRegistryError::InvalidStore);
    }
    Ok(file)
}

fn valid_identity(value: &str) -> bool {
    !value.is_empty() && value.len() <= 256 && !value.chars().any(char::is_control)
}

fn temporary_path(path: &Path) -> PathBuf {
    path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4().simple()))
}

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, fs, path::PathBuf};

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
    fn identity_store_rejects_dangling_sources_and_invalid_proxy_ids() {
        let path = registry_path();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        for (proxy_id, sources) in [
            (
                ProxyId::default().to_string(),
                json!({"unbound-id": "mqtt"}),
            ),
            ("not-a-uuid".to_owned(), json!({})),
            (uuid::Uuid::nil().to_string(), json!({})),
            ("550e8400-e29b-11d4-a716-446655440000".to_owned(), json!({})),
        ] {
            fs::write(
                &path,
                serde_json::to_vec(&json!({
                    "version": 2, "proxy_id": proxy_id, "sources": sources, "bindings": []
                }))
                .unwrap(),
            )
            .unwrap();
            assert!(DeviceRegistry::load(&path).is_err_and(is_invalid_store));
        }
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn failed_source_persistence_preserves_live_and_saved_ownership() {
        let path = registry_path();
        let mut registry = DeviceRegistry::load(&path).unwrap();
        registry.register_configured_ble();
        let parent = path.parent().unwrap();
        let backup = parent.with_extension("backup");
        fs::rename(parent, &backup).unwrap();
        fs::write(parent, b"block directory creation").unwrap();
        assert!(
            registry
                .set_entity_sources(
                    &DeviceId::configured_ble(),
                    EntitySource::Mqtt,
                    EntitySource::Mqtt
                )
                .is_err()
        );
        assert_eq!(
            registry.descriptors().next().unwrap().state_source,
            EntitySource::Http
        );
        assert!(
            !registry
                .identities
                .sources
                .contains_key(&DeviceId::configured_ble())
        );
        fs::remove_file(parent).unwrap();
        fs::rename(backup, parent).unwrap();
        let mut restored = DeviceRegistry::load(&path).unwrap();
        restored.register_configured_ble();
        assert_eq!(
            restored.descriptors().next().unwrap().state_source,
            EntitySource::Http
        );
        fs::remove_dir_all(parent).unwrap();
    }

    #[test]
    fn inactive_backend_ownership_does_not_require_mqtt() {
        let path = registry_path();
        let mut registry = DeviceRegistry::load(&path).unwrap();
        registry.register_configured_ble();
        let cloud = registry
            .reconcile_quickconnect("account-a", &[cloud_device("provider-a", "Fan")])
            .unwrap()
            .pop()
            .unwrap();
        registry
            .set_entity_sources(&cloud, EntitySource::Mqtt, EntitySource::Mqtt)
            .unwrap();
        assert!(!registry.mqtt_ownership_required(true, None));
        assert!(!registry.mqtt_ownership_required(false, Some("account-b")));
        assert!(registry.mqtt_ownership_required(false, Some("account-a")));
        registry
            .set_entity_sources(
                &DeviceId::configured_ble(),
                EntitySource::Mqtt,
                EntitySource::Mqtt,
            )
            .unwrap();
        assert!(!registry.mqtt_ownership_required(false, None));
        assert!(registry.mqtt_ownership_required(true, None));
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn entity_sources_survive_restart_for_ble_and_cloud() {
        let path = registry_path();
        let mut registry = DeviceRegistry::load(&path).unwrap();
        registry.register_configured_ble();
        let cloud = registry
            .reconcile_quickconnect(
                "test-account",
                &[CloudDeviceInput::new(
                    "private-provider".to_owned(),
                    "Cloud fan".to_owned(),
                )],
            )
            .unwrap()
            .pop()
            .unwrap();
        registry
            .set_entity_sources(
                &DeviceId::configured_ble(),
                EntitySource::Mqtt,
                EntitySource::Mqtt,
            )
            .unwrap();
        registry
            .set_entity_sources(&cloud, EntitySource::Http, EntitySource::Http)
            .unwrap();
        drop(registry);
        let mut restored = DeviceRegistry::load(&path).unwrap();
        restored.register_configured_ble();
        restored
            .reconcile_quickconnect(
                "test-account",
                &[CloudDeviceInput::new(
                    "private-provider".to_owned(),
                    "Renamed cloud fan".to_owned(),
                )],
            )
            .unwrap();
        let ble = restored
            .descriptors()
            .find(|d| d.id == DeviceId::configured_ble())
            .unwrap();
        assert_eq!(ble.state_source, EntitySource::Mqtt);
        assert_eq!(ble.command_source, EntitySource::Mqtt);
        let cloud = restored.descriptors().find(|d| d.id == cloud).unwrap();
        assert_eq!(cloud.command_source, EntitySource::Http);
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn proxy_identity_is_persisted_before_devices_are_registered() {
        let path = registry_path();
        let mut registry = DeviceRegistry::load(&path).unwrap();
        let first: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        registry.register_configured_ble();
        let descriptor = serde_json::to_value(registry.descriptors().next().unwrap()).unwrap();
        assert_eq!(descriptor["proxy_id"], first["proxy_id"]);
        assert!(
            first["proxy_id"]
                .as_str()
                .is_some_and(|id| uuid::Uuid::parse_str(id).is_ok())
        );
        drop(registry);
        let mut restored = DeviceRegistry::load(&path).unwrap();
        restored.register_configured_ble();
        let restored = serde_json::to_value(restored.descriptors().next().unwrap()).unwrap();
        assert_eq!(restored["proxy_id"], descriptor["proxy_id"]);
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    use super::*;
    use crate::device::{DeviceBackend, DeviceSettings, StateProvenance};
    use axum::{Json, Router, http::Uri, routing::get, routing::post};
    use serde_json::{Value, json};

    fn registry_path() -> PathBuf {
        std::env::temp_dir()
            .join(format!("updraft-identities-{}", uuid::Uuid::new_v4()))
            .join("identities.json")
    }

    fn cloud_device(provider_id: &str, name: &str) -> CloudDeviceInput {
        CloudDeviceInput::new(provider_id.to_owned(), name.to_owned())
    }

    async fn mock_quickconnect_client() -> (
        updraft_quickconnect::QuickConnectClient,
        tokio::task::JoinHandle<()>,
    ) {
        let app = Router::new()
            .route("/cognito/login", post(mock_login))
            .route("/gaf/device/deviceList", get(mock_inventory))
            .route("/gaf/device", get(mock_detail));
        mock_client(app).await
    }

    async fn mock_duplicate_inventory_client() -> (
        updraft_quickconnect::QuickConnectClient,
        tokio::task::JoinHandle<()>,
    ) {
        let app = Router::new()
            .route("/cognito/login", post(mock_login))
            .route("/gaf/device/deviceList", get(mock_duplicate_inventory))
            .route("/gaf/device", get(mock_detail));
        mock_client(app).await
    }

    async fn mock_client(
        app: Router,
    ) -> (
        updraft_quickconnect::QuickConnectClient,
        tokio::task::JoinHandle<()>,
    ) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let base = reqwest::Url::parse(&format!("http://{address}/")).unwrap();
        let client = updraft_quickconnect::QuickConnectClient::new(
            updraft_quickconnect::Credentials::new(
                "synthetic-user",
                "synthetic-password",
                updraft_quickconnect::AccountRole::Contractor,
            ),
            updraft_quickconnect::QuickConnectConfig::new(
                base.join("cognito/").unwrap(),
                base.join("gaf/").unwrap(),
            ),
        )
        .unwrap();
        (client, server)
    }

    async fn mock_login() -> Json<Value> {
        Json(json!({"responseData": {"idToken": "SYNTHETIC_TOKEN_DO_NOT_USE"}}))
    }

    async fn mock_inventory() -> Json<Value> {
        Json(json!({
            "responseData": {"devices": [
                {"deviceId": "synthetic-failed-detail", "name": "Failed detail"},
                {"deviceId": "synthetic-live-detail", "name": "Live detail"}
            ]}
        }))
    }

    async fn mock_duplicate_inventory() -> Json<Value> {
        Json(json!({
            "responseData": [
                {"deviceId": "synthetic-device", "name": "Duplicate one"},
                {"deviceId": "synthetic-device", "name": "Duplicate two"}
            ]
        }))
    }

    async fn mock_detail(uri: Uri) -> (axum::http::StatusCode, Json<Value>) {
        if uri
            .query()
            .is_some_and(|query| query.contains("synthetic-failed-detail"))
        {
            return (
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"message": "synthetic failure"})),
            );
        }
        (
            axum::http::StatusCode::OK,
            Json(json!({
                "responseData": {
                    "deviceConfig": {"setTemperature": 78, "setHumidity": 44},
                    "deviceSettings": {
                        "automaticMode": true,
                        "timerMode": false,
                        "fanMode": false,
                        "setTemperature": 105,
                        "setHumidity": 40,
                        "humidityMonitor": true
                    }
                }
            })),
        )
    }

    fn ids_by_name(registry: &DeviceRegistry) -> BTreeMap<String, String> {
        registry
            .descriptors()
            .map(|descriptor| (descriptor.name.clone(), descriptor.id.as_str().to_owned()))
            .collect()
    }

    #[tokio::test]
    async fn detail_failure_keeps_device_registered_while_other_state_and_ble_remain_available() {
        let path = registry_path();
        let mut registry = DeviceRegistry::load(&path).unwrap();
        let ble_runtime = registry.register_configured_ble();
        let (client, server) = mock_quickconnect_client().await;

        let ids = registry
            .poll_quickconnect("synthetic-account", &client)
            .await
            .unwrap();

        assert_eq!(ids.len(), 2);
        let descriptors = registry
            .descriptors()
            .map(|descriptor| (descriptor.name.as_str(), descriptor.id.clone()))
            .collect::<BTreeMap<_, _>>();
        let failed = registry.runtime(&descriptors["Failed detail"]).unwrap();
        let live = registry.runtime(&descriptors["Live detail"]).unwrap();
        let failed_snapshot = failed.snapshot().await;
        assert!(failed_snapshot.state.is_none());
        assert_eq!(
            failed_snapshot.inventory_status,
            DeviceInventoryStatus::Present
        );
        assert_eq!(failed_snapshot.last_successful_state, None);
        assert_eq!(live.state().await.unwrap().temperature_f, Some(78.0));
        assert_eq!(
            live.snapshot().await.inventory_status,
            DeviceInventoryStatus::Present
        );
        assert!(ble_runtime.state().await.is_none());
        assert!(
            registry
                .descriptors()
                .any(|descriptor| descriptor.id == DeviceId::configured_ble())
        );

        server.abort();
        fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[tokio::test]
    async fn invalid_inventory_marks_only_that_accounts_current_state_unavailable() {
        let path = registry_path();
        let mut registry = DeviceRegistry::load(&path).unwrap();
        let first = registry
            .reconcile_quickconnect("account-a", &[cloud_device("provider-a", "One")])
            .unwrap();
        let second = registry
            .reconcile_quickconnect("account-b", &[cloud_device("provider-a", "Two")])
            .unwrap();
        let first_runtime = registry.runtime(&first[0]).unwrap();
        let second_runtime = registry.runtime(&second[0]).unwrap();
        let state = || DeviceState {
            temperature_f: Some(78.0),
            humidity_percent: Some(40.0),
            settings: DeviceSettings::QuickConnect {
                mode: QuickConnectModeStatus::Automatic,
                automatic_temperature_f: Some(100),
                automatic_humidity_percent: Some(40),
                timer_duration_minutes: None,
                humidity_monitor: Some(true),
            },
            estimated_running: Some(false),
            diagnostics: None,
            provenance: StateProvenance {
                backend: DeviceBackend::QuickConnect,
                fetched_at_unix_ms: unix_millis(SystemTime::now()),
                observed_at_unix_ms: None,
            },
        };
        first_runtime.set_state(state()).await;
        second_runtime.set_state(state()).await;
        let (client, server) = mock_duplicate_inventory_client().await;

        assert!(is_duplicate_poll_error(
            registry.poll_quickconnect("account-a", &client).await
        ));

        let failed = first_runtime.snapshot().await;
        assert_eq!(failed.inventory_status, DeviceInventoryStatus::Unavailable);
        assert!(failed.state.is_none());
        assert!(failed.last_successful_state.is_some());
        let unaffected = second_runtime.snapshot().await;
        assert_eq!(unaffected.inventory_status, DeviceInventoryStatus::Present);
        assert_eq!(unaffected.state.unwrap().temperature_f, Some(78.0));

        server.abort();
        fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn cloud_identity_survives_order_rename_restart_and_coexists_with_ble() {
        let path = registry_path();
        let mut registry = DeviceRegistry::load(&path).unwrap();
        registry.register_configured_ble();
        registry
            .reconcile_quickconnect(
                "account-private-a",
                &[
                    cloud_device("provider-private-a", "Attic fan"),
                    cloud_device("provider-private-b", "Guest fan"),
                ],
            )
            .unwrap();
        let original = ids_by_name(&registry);
        let attic_id = original["Attic fan"].clone();
        let guest_id = original["Guest fan"].clone();
        assert_ne!(attic_id, guest_id);
        assert_eq!(registry.identity_count(), 2);

        drop(registry);
        let mut restored = DeviceRegistry::load(&path).unwrap();
        restored.register_configured_ble();
        restored
            .reconcile_quickconnect(
                "account-private-a",
                &[
                    cloud_device("provider-private-b", "Renamed guest"),
                    cloud_device("provider-private-a", "Renamed attic"),
                ],
            )
            .unwrap();
        let renamed = ids_by_name(&restored);
        assert_eq!(renamed["Renamed attic"], attic_id);
        assert_eq!(renamed["Renamed guest"], guest_id);
        assert_eq!(renamed["GAF Wi-Fi Vent"], "configured");
        assert!(restored.descriptors().all(|descriptor| {
            serde_json::to_string(descriptor).is_ok_and(|payload| {
                !payload.contains("provider-private") && !payload.contains("account-private")
            })
        }));
        fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn provider_identifiers_are_scoped_to_their_account() {
        let path = registry_path();
        let mut registry = DeviceRegistry::load(&path).unwrap();
        let first = registry
            .reconcile_quickconnect("account-a", &[cloud_device("same-provider-id", "One")])
            .unwrap();
        let second = registry
            .reconcile_quickconnect("account-b", &[cloud_device("same-provider-id", "Two")])
            .unwrap();
        assert_ne!(first[0], second[0]);
        let zero = registry
            .reconcile_quickconnect("account-c", &[cloud_device("0", "Zero ID")])
            .unwrap();
        assert!(zero[0].as_str().starts_with("qc-"));
        assert!(
            !format!(
                "{:?}",
                ProviderIdentity {
                    account_id: "account-private".to_owned(),
                    provider_id: "provider-private".to_owned(),
                }
            )
            .contains("provider-private")
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn state_and_command_entity_sources_are_selected_per_device() {
        let path = registry_path();
        let mut registry = DeviceRegistry::load(&path).unwrap();
        registry.register_configured_ble();
        let cloud_id = registry
            .reconcile_quickconnect("account-a", &[cloud_device("provider-a", "Cloud fan")])
            .unwrap()[0]
            .clone();
        assert!(
            registry
                .set_entity_sources(&cloud_id, EntitySource::Mqtt, EntitySource::Http)
                .is_err()
        );
        registry
            .set_entity_sources(&cloud_id, EntitySource::Mqtt, EntitySource::Mqtt)
            .unwrap();
        registry
            .reconcile_quickconnect("account-a", &[cloud_device("provider-a", "Cloud fan")])
            .unwrap();
        let cloud = registry
            .descriptors()
            .find(|descriptor| descriptor.id == cloud_id)
            .unwrap();
        assert_eq!(cloud.state_source, EntitySource::Mqtt);
        assert_eq!(cloud.command_source, EntitySource::Mqtt);
        assert_eq!(
            registry
                .descriptors()
                .find(|descriptor| descriptor.id == DeviceId::configured_ble())
                .unwrap()
                .state_source,
            EntitySource::Http
        );
        fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn identity_store_rejects_the_reserved_configured_device_id() {
        let path = registry_path();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            serde_json::to_vec(&json!({"version": 2, "proxy_id": ProxyId::default(), "sources": {}, "bindings": [{"identity": {"account_id": "account-a", "provider_id": "provider-a"}, "local_id": "configured"}]})).unwrap(),
        )
        .unwrap();

        assert!(DeviceRegistry::load(&path).is_err_and(is_invalid_store));
        fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    fn is_invalid_store(error: DeviceRegistryError) -> bool {
        match error {
            DeviceRegistryError::InvalidStore => true,
            DeviceRegistryError::InvalidIdentity
            | DeviceRegistryError::DuplicateProviderId
            | DeviceRegistryError::PersistenceUnavailable
            | DeviceRegistryError::UnknownDevice
            | DeviceRegistryError::MixedSources
            | DeviceRegistryError::UnsupportedCommand
            | DeviceRegistryError::Io(_)
            | DeviceRegistryError::Encoding(_) => false,
        }
    }

    fn is_duplicate_poll_error(result: Result<Vec<DeviceId>, QuickConnectPollingError>) -> bool {
        match result {
            Err(QuickConnectPollingError::Registry(DeviceRegistryError::DuplicateProviderId)) => {
                true
            }
            Err(QuickConnectPollingError::Client(_) | QuickConnectPollingError::Registry(_))
            | Ok(_) => false,
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
        let path = registry_path();
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
            temperature_f: Some(102.0),
            humidity_percent: None,
            settings: DeviceSettings::QuickConnect {
                mode: QuickConnectModeStatus::Automatic,
                automatic_temperature_f: None,
                automatic_humidity_percent: None,
                timer_duration_minutes: None,
                humidity_monitor: None,
            },
            estimated_running: Some(true),
            diagnostics: None,
            provenance: StateProvenance {
                backend: DeviceBackend::QuickConnect,
                fetched_at_unix_ms: Some(1),
                observed_at_unix_ms: None,
            },
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
        assert_eq!(
            expired.last_successful_state.unwrap().estimated_running,
            Some(true)
        );
        assert!(first.snapshot_at(0).await.state.is_none());
        assert!(first.snapshot_at_option(None).await.state.is_none());
        assert!(second.state().await.is_none());
        let permits = std::iter::repeat_with(|| first.try_reserve_control().unwrap())
            .take(CONTROL_QUEUE_CAPACITY)
            .collect::<Vec<_>>();
        assert!(first.try_reserve_control().is_none());
        drop(permits);
        fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn duplicate_provider_ids_reject_the_inventory_before_saving() {
        let path = registry_path();
        let mut registry = DeviceRegistry::load(&path).unwrap();
        let result = registry.reconcile_quickconnect(
            "account-a",
            &[cloud_device("same", "One"), cloud_device("same", "Two")],
        );
        assert!(result.err().is_some_and(is_duplicate_provider_id));
        assert_eq!(registry.identity_count(), 0);
        let stored: IdentityFile = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert!(stored.bindings.is_empty());
        fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    fn is_duplicate_provider_id(error: DeviceRegistryError) -> bool {
        match error {
            DeviceRegistryError::DuplicateProviderId => true,
            DeviceRegistryError::InvalidIdentity
            | DeviceRegistryError::InvalidStore
            | DeviceRegistryError::PersistenceUnavailable
            | DeviceRegistryError::UnknownDevice
            | DeviceRegistryError::MixedSources
            | DeviceRegistryError::UnsupportedCommand
            | DeviceRegistryError::Io(_)
            | DeviceRegistryError::Encoding(_) => false,
        }
    }
}
