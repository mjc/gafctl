mod identity;
mod runtime;
#[cfg(test)]
mod test_support;

use futures_util::StreamExt;
pub use gafctl_api::DeviceInventoryStatus;
#[cfg(feature = "mqtt")]
use gafctl_api::ProxyId;
use gafctl_api::{
    DeviceBackend, DeviceCapabilities, DeviceCommand, DeviceDescriptor, DeviceId, EntitySource,
};
use gafctl_quickconnect::QuickConnectCommand;
use identity::IdentityStore;
pub(crate) use runtime::{DeviceRuntime, RefreshReceiver, RefreshReservation};
use std::{
    collections::{BTreeMap, HashSet, btree_map::Entry},
    io,
    path::PathBuf,
    sync::Arc,
};
use thiserror::Error;

pub(crate) const QUICKCONNECT_POLL_CONCURRENCY: usize = 4;

pub(crate) struct QuickConnectReadTarget {
    pub(crate) provider_id: String,
    pub(crate) runtime: Arc<DeviceRuntime>,
    pub(crate) generation: u64,
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

struct RegisteredDevice {
    descriptor: DeviceDescriptor,
    runtime: Arc<DeviceRuntime>,
}

pub struct DeviceRegistry {
    identities: IdentityStore,
    devices: BTreeMap<DeviceId, RegisteredDevice>,
    quickconnect_writes_enabled: bool,
}

impl Default for DeviceRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl DeviceRegistry {
    pub fn new() -> Self {
        Self {
            identities: IdentityStore::in_memory(),
            devices: BTreeMap::new(),
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
            quickconnect_writes_enabled: false,
        })
    }

    #[cfg(feature = "mqtt")]
    pub fn proxy_id(&self) -> ProxyId {
        self.identities.proxy_id
    }

    #[cfg(feature = "mqtt")]
    pub fn discovery_identities(&self) -> impl Iterator<Item = (DeviceId, DeviceBackend)> + '_ {
        std::iter::once((DeviceId::configured_ble(), DeviceBackend::LegacyBle)).chain(
            self.identities
                .bindings
                .iter()
                .map(|binding| (binding.local_id.clone(), DeviceBackend::QuickConnect)),
        )
    }

    #[cfg(any(feature = "mqtt", test))]
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
            .map(|device| &mut device.descriptor)
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
            .map(|device| &device.descriptor)
            .ok_or(DeviceRegistryError::UnknownDevice)?;
        let capability = match command {
            QuickConnectCommand::SetMode { .. } | QuickConnectCommand::ClearMode { .. } => {
                gafctl_api::CommandCapability::QuickConnectMode
            }
            QuickConnectCommand::SetAutomaticTargets { .. } => {
                gafctl_api::CommandCapability::QuickConnectTargets
            }
            QuickConnectCommand::SetTimerDuration { .. } => {
                gafctl_api::CommandCapability::QuickConnectTimerDuration
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
        let device = self
            .devices
            .get(id)
            .ok_or(DeviceRegistryError::UnknownDevice)?;
        let descriptor = &device.descriptor;
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
        Ok((Arc::clone(&device.runtime), provider_id))
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

    pub async fn reconcile_quickconnect_inventory(
        &mut self,
        account_id: &str,
        inventory: Vec<gafctl_quickconnect::InventoryDevice>,
        generations: &BTreeMap<DeviceId, u64>,
    ) -> Result<Vec<QuickConnectReadTarget>, DeviceRegistryError> {
        let inputs = inventory
            .iter()
            .map(|device| {
                CloudDeviceInput::new(
                    device.provider_id().to_owned(),
                    device.name().unwrap_or("QuickConnect device").to_owned(),
                )
            })
            .collect::<Vec<_>>();
        let ids = match self.reconcile_quickconnect(account_id, &inputs) {
            Ok(ids) => ids,
            Err(error) => {
                self.mark_quickconnect_inventory_unavailable(account_id, generations)
                    .await;
                return Err(error);
            }
        };
        self.mark_missing_quickconnect_devices(account_id, &ids, generations)
            .await;
        Ok(ids
            .into_iter()
            .zip(inventory)
            .filter_map(|(id, device)| {
                self.runtime(&id).map(|runtime| {
                    let generation = generations
                        .get(&id)
                        .copied()
                        .unwrap_or_else(|| runtime.begin_state_read());
                    QuickConnectReadTarget {
                        provider_id: device.into_provider_id(),
                        runtime,
                        generation,
                    }
                })
            })
            .collect())
    }

    async fn mark_missing_quickconnect_devices(
        &self,
        account_id: &str,
        ids: &[DeviceId],
        generations: &BTreeMap<DeviceId, u64>,
    ) {
        let present = ids.iter().collect::<HashSet<_>>();
        let missing = self
            .identities
            .bindings
            .iter()
            .filter(|binding| {
                binding.identity.account_id == account_id && !present.contains(&binding.local_id)
            })
            .filter_map(|binding| {
                self.devices
                    .get(&binding.local_id)
                    .zip(generations.get(&binding.local_id))
            })
            .map(|(device, generation)| (Arc::clone(&device.runtime), *generation))
            .collect::<Vec<_>>();
        futures_util::stream::iter(missing)
            .for_each(|(runtime, generation)| async move {
                runtime.mark_missing_if_current(generation).await;
            })
            .await;
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
                self.devices
                    .get(&binding.local_id)
                    .zip(generations.get(&binding.local_id))
                    .map(|(device, generation)| (Arc::clone(&device.runtime), *generation))
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
        self.devices.values().map(|device| &device.descriptor)
    }

    pub fn runtime(&self, id: &DeviceId) -> Option<Arc<DeviceRuntime>> {
        self.devices
            .get(id)
            .map(|device| Arc::clone(&device.runtime))
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
            .map(|device| &mut device.descriptor)
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
            .map(|device| &device.descriptor)
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
        let device = match self.devices.entry(id) {
            Entry::Occupied(entry) => {
                let device = entry.into_mut();
                device.descriptor = descriptor;
                device
            }
            Entry::Vacant(entry) => entry.insert(RegisteredDevice {
                descriptor,
                runtime: Arc::new(DeviceRuntime::new()),
            }),
        };
        Arc::clone(&device.runtime)
    }

    #[cfg(test)]
    fn identity_count(&self) -> usize {
        self.identities.bindings.len()
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;
    use gafctl_api::{DeviceSettings, DeviceState, QuickConnectModeStatus, StateProvenance};
    use std::fs;

    #[tokio::test]
    async fn reregistering_a_device_updates_its_descriptor_and_preserves_its_runtime() {
        let path = registry_path();
        let mut registry = DeviceRegistry::load(&path).unwrap();
        let id = registry
            .reconcile_quickconnect("account-a", &[cloud_device("provider-a", "Original")])
            .unwrap()
            .pop()
            .unwrap();
        let runtime = registry.runtime(&id).unwrap();
        let state = DeviceState {
            temperature_f: Some(91.0),
            humidity_percent: None,
            settings: DeviceSettings::QuickConnect {
                mode: QuickConnectModeStatus::Automatic,
                automatic_temperature_f: None,
                automatic_humidity_percent: None,
                timer_duration_minutes: None,
                humidity_monitor: None,
            },
            estimated_running: None,
            diagnostics: None,
            provenance: StateProvenance {
                backend: DeviceBackend::QuickConnect,
                fetched_at_unix_ms: Some(1_000),
                observed_at_unix_ms: None,
            },
        };
        runtime.set_state(state.clone()).await;
        let transaction = runtime.acquire_transaction().await;

        let updated = registry
            .reconcile_quickconnect("account-a", &[cloud_device("provider-a", "Renamed")])
            .unwrap();
        assert_eq!(updated, vec![id.clone()]);
        let current = registry.runtime(&id).unwrap();
        assert!(Arc::ptr_eq(&runtime, &current));
        assert_eq!(current.snapshot_at(1_000).await.state, Some(state));
        assert!(current.try_acquire_transaction().is_none());
        let descriptor = registry.descriptors().next().unwrap();
        assert_eq!(descriptor.id, id);
        assert_eq!(descriptor.name, "Renamed");
        assert_eq!(registry.descriptors().count(), 1);

        drop(transaction);
        assert!(current.try_acquire_transaction().is_some());
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
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
}
