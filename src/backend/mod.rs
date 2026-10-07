mod identity;
mod runtime;
#[cfg(test)]
mod test_support;

pub use crate::model::DeviceInventoryStatus;
#[cfg(feature = "mqtt")]
use crate::model::ProxyId;
use crate::model::{
    DeviceBackend, DeviceCapabilities, DeviceCommand, DeviceDescriptor, DeviceId, EntitySource,
};
use futures_util::StreamExt;
use identity::IdentityStore;
pub(crate) use runtime::{DeviceRuntime, RefreshReceiver, RefreshReservation};
use std::{
    collections::{BTreeMap, HashSet, btree_map::Entry},
    io,
    path::PathBuf,
    sync::Arc,
};
use thiserror::Error;

pub struct CloudDeviceInput {
    provider_id: String,
    name: String,
}

impl CloudDeviceInput {
    pub fn new(provider_id: String, name: String) -> Self {
        Self { provider_id, name }
    }

    pub(crate) fn into_provider_id(self) -> String {
        self.provider_id
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

    #[cfg(feature = "mqtt")]
    pub(crate) fn discovery_identities_match(
        &self,
        identities: &[(DeviceId, DeviceBackend)],
    ) -> bool {
        let Some(((ble_id, ble_backend), quickconnect)) = identities.split_first() else {
            return false;
        };
        ble_id.as_str() == "configured"
            && *ble_backend == DeviceBackend::LegacyBle
            && quickconnect.len() == self.identities.bindings.len()
            && quickconnect
                .iter()
                .zip(&self.identities.bindings)
                .all(|((id, backend), binding)| {
                    id == &binding.local_id && *backend == DeviceBackend::QuickConnect
                })
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

    pub(crate) fn timer_configuration(&self) -> &crate::timed_run::TimerConfiguration {
        &self.identities.timer
    }

    pub(crate) fn set_timer_configuration(
        &mut self,
        timer: crate::timed_run::TimerConfiguration,
    ) -> Result<(), DeviceRegistryError> {
        self.identities.set_timer(timer)
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
        command: DeviceCommand,
    ) -> Result<(Arc<DeviceRuntime>, String), DeviceRegistryError> {
        self.dispatch(id, command)?;
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
        devices: &[CloudDeviceInput],
        generations: &BTreeMap<DeviceId, u64>,
    ) -> Result<Vec<DeviceId>, DeviceRegistryError> {
        let ids = match self.reconcile_quickconnect(account_id, devices) {
            Ok(ids) => ids,
            Err(error) => {
                self.mark_quickconnect_inventory_unavailable(account_id, generations)
                    .await;
                return Err(error);
            }
        };
        self.mark_missing_quickconnect_devices(account_id, &ids, generations)
            .await;
        Ok(ids)
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

    pub(crate) fn descriptor(&self, id: &DeviceId) -> Option<&DeviceDescriptor> {
        self.devices.get(id).map(|device| &device.descriptor)
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
            .descriptor(id)
            .ok_or(DeviceRegistryError::UnknownDevice)?;
        if descriptor.backend != command.required_capability().backend()
            || !descriptor.capabilities.supports(command)
        {
            return Err(DeviceRegistryError::UnsupportedCommand);
        }
        Ok(descriptor.backend)
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
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;
    use crate::model::{DeviceSettings, DeviceState, QuickConnectModeStatus};
    use crate::test_support::{cloud_device, identity_store_fixture};

    fn command_families() -> impl Iterator<Item = (DeviceCommand, DeviceBackend)> {
        [
            (
                r#"{"kind":"legacy_preset","preset":"timer_clear"}"#,
                DeviceBackend::LegacyBle,
            ),
            (
                r#"{"kind":"legacy_automatic_temperature","temperature_f":105}"#,
                DeviceBackend::LegacyBle,
            ),
            (
                r#"{"kind":"legacy_automatic_humidity","humidity_percent":40}"#,
                DeviceBackend::LegacyBle,
            ),
            (
                r#"{"kind":"legacy_timer","minutes":1}"#,
                DeviceBackend::LegacyBle,
            ),
            (
                r#"{"kind":"quick_connect_mode","mode":"automatic"}"#,
                DeviceBackend::QuickConnect,
            ),
            (
                r#"{"kind":"quick_connect_conditional_off","only_if_current":"automatic"}"#,
                DeviceBackend::QuickConnect,
            ),
            (
                r#"{"kind":"quick_connect_targets","temperature_f":105,"humidity_percent":40}"#,
                DeviceBackend::QuickConnect,
            ),
            (
                r#"{"kind":"quick_connect_automatic_temperature","temperature_f":105}"#,
                DeviceBackend::QuickConnect,
            ),
            (
                r#"{"kind":"quick_connect_automatic_humidity","humidity_percent":40}"#,
                DeviceBackend::QuickConnect,
            ),
            (
                r#"{"kind":"quick_connect_timer_duration","minutes":1}"#,
                DeviceBackend::QuickConnect,
            ),
        ]
        .into_iter()
        .map(|(payload, backend)| (serde_json::from_str(payload).unwrap(), backend))
    }

    #[test]
    fn dispatch_checks_command_family_and_enabled_capabilities() {
        let (_directory, path) = identity_store_fixture();
        let mut registry = DeviceRegistry::load(&path).unwrap();
        registry.register_configured_ble();
        let ble = DeviceId::configured_ble();
        let cloud = registry
            .reconcile_quickconnect("account-a", &[cloud_device("provider-a", "Fan")])
            .unwrap()
            .pop()
            .unwrap();
        command_families().for_each(|(command, backend)| {
            assert_eq!(
                registry.dispatch(&ble, command).ok(),
                (backend == DeviceBackend::LegacyBle).then_some(backend)
            );
            assert!(registry.dispatch(&cloud, command).is_err());
        });
        registry.set_quickconnect_writes_enabled(true);
        command_families().for_each(|(command, backend)| {
            assert_eq!(
                registry.dispatch(&cloud, command).ok(),
                (backend == DeviceBackend::QuickConnect).then_some(backend)
            );
        });

        registry
            .devices
            .get_mut(&ble)
            .unwrap()
            .descriptor
            .capabilities = DeviceCapabilities::quickconnect_with_controls();
        registry
            .devices
            .get_mut(&cloud)
            .unwrap()
            .descriptor
            .capabilities = DeviceCapabilities::legacy_ble();
        command_families().for_each(|(command, _)| {
            assert!(registry.dispatch(&ble, command).is_err(), "{command:?}");
            assert!(registry.dispatch(&cloud, command).is_err(), "{command:?}");
        });
    }

    #[test]
    fn cloud_control_targets_require_account_read_and_write_permissions() {
        let (_directory, path) = identity_store_fixture();
        let mut registry = DeviceRegistry::load(&path).unwrap();
        let id = registry
            .reconcile_quickconnect("account-a", &[cloud_device("provider-a", "Fan")])
            .unwrap()
            .pop()
            .unwrap();
        let commands = command_families()
            .filter(|(_, backend)| *backend == DeviceBackend::QuickConnect)
            .map(|(command, _)| command)
            .collect::<Vec<_>>();
        commands.iter().copied().for_each(|command| {
            assert!(
                registry
                    .quickconnect_control_target("account-a", &id, command)
                    .is_err_and(|error| {
                        std::mem::discriminant(&error)
                            == std::mem::discriminant(&DeviceRegistryError::UnsupportedCommand)
                    })
            );
        });
        registry.set_quickconnect_writes_enabled(true);
        commands.iter().copied().for_each(|command| {
            let (runtime, provider) = registry
                .quickconnect_control_target("account-a", &id, command)
                .unwrap();
            assert!(Arc::ptr_eq(&runtime, &registry.runtime(&id).unwrap()));
            assert_eq!(provider, "provider-a");
            assert!(
                registry
                    .quickconnect_control_target("account-b", &id, command)
                    .is_err_and(|error| {
                        std::mem::discriminant(&error)
                            == std::mem::discriminant(&DeviceRegistryError::UnknownDevice)
                    })
            );
        });
        registry
            .devices
            .get_mut(&id)
            .unwrap()
            .descriptor
            .capabilities
            .read_state = false;
        commands.iter().copied().for_each(|command| {
            assert!(
                registry
                    .quickconnect_control_target("account-a", &id, command)
                    .is_err_and(|error| {
                        std::mem::discriminant(&error)
                            == std::mem::discriminant(&DeviceRegistryError::UnsupportedCommand)
                    })
            );
        });
    }

    #[test]
    fn descriptors_resolve_registered_ids_and_follow_renames() {
        let (_directory, path) = identity_store_fixture();
        let mut registry = DeviceRegistry::load(&path).unwrap();
        registry.register_configured_ble();
        let id = registry
            .reconcile_quickconnect("account-a", &[cloud_device("provider-a", "Original")])
            .unwrap()
            .pop()
            .unwrap();
        assert_eq!(
            registry
                .descriptor(&DeviceId::configured_ble())
                .unwrap()
                .backend,
            DeviceBackend::LegacyBle
        );
        assert_eq!(registry.descriptor(&id).unwrap().name, "Original");
        assert!(
            registry
                .descriptor(&DeviceId::parse("missing".to_owned()).unwrap())
                .is_none()
        );
        registry
            .reconcile_quickconnect("account-a", &[cloud_device("provider-a", "Renamed")])
            .unwrap();
        assert_eq!(registry.descriptor(&id).unwrap().name, "Renamed");
    }

    #[tokio::test]
    async fn reregistering_a_device_updates_its_descriptor_and_preserves_its_runtime() {
        let (_directory, path) = identity_store_fixture();
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
            ..observed_state(Some(1_000))
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
    }

    #[test]
    fn inactive_backend_ownership_does_not_require_mqtt() {
        let (_directory, path) = identity_store_fixture();
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
    }

    #[test]
    fn state_and_command_entity_sources_are_selected_per_device() {
        let (_directory, path) = identity_store_fixture();
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
    }
}
