use std::{
    collections::{BTreeMap, HashSet},
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::Arc,
};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::{Mutex, OwnedMutexGuard, RwLock};

use crate::device::{
    DeviceBackend, DeviceCapabilities, DeviceCommand, DeviceDescriptor, DeviceId, DeviceState,
    EntitySource,
};

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

#[derive(Deserialize, Serialize)]
struct IdentityFile {
    version: u8,
    bindings: Vec<IdentityBinding>,
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
    #[error("command is not supported by this device backend")]
    UnsupportedCommand,
    #[error("could not access the device identity store")]
    Io(#[source] io::Error),
    #[error("could not encode the device identity store")]
    Encoding(#[source] serde_json::Error),
}

pub struct DeviceRegistry {
    identities: IdentityStore,
    devices: BTreeMap<DeviceId, DeviceDescriptor>,
    runtimes: BTreeMap<DeviceId, Arc<DeviceRuntime>>,
}

impl Default for DeviceRegistry {
    fn default() -> Self {
        Self::new()
    }
}

pub struct DeviceRuntime {
    state: RwLock<Option<DeviceState>>,
    transaction: Arc<Mutex<()>>,
}

impl DeviceRuntime {
    fn new() -> Self {
        Self {
            state: RwLock::new(None),
            transaction: Arc::new(Mutex::new(())),
        }
    }

    pub async fn state(&self) -> Option<DeviceState> {
        self.state.read().await.clone()
    }

    pub async fn set_state(&self, state: DeviceState) {
        *self.state.write().await = Some(state);
    }

    pub async fn acquire_transaction(&self) -> OwnedMutexGuard<()> {
        Arc::clone(&self.transaction).lock_owned().await
    }

    pub fn try_acquire_transaction(&self) -> Option<OwnedMutexGuard<()>> {
        Arc::clone(&self.transaction).try_lock_owned().ok()
    }
}

impl DeviceRegistry {
    pub fn new() -> Self {
        Self {
            identities: IdentityStore::in_memory(),
            devices: BTreeMap::new(),
            runtimes: BTreeMap::new(),
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
            (DeviceBackend::LegacyBle, DeviceCommand::LegacyPreset { .. })
                if descriptor.capabilities.supports(command) =>
            {
                Ok(DeviceBackend::LegacyBle)
            }
            (
                DeviceBackend::QuickConnect,
                DeviceCommand::QuickConnectMode { .. }
                | DeviceCommand::QuickConnectTargets { .. }
                | DeviceCommand::QuickConnectTimerDuration { .. },
            ) if descriptor.capabilities.supports(command) => Ok(DeviceBackend::QuickConnect),
            _ => Err(DeviceRegistryError::UnsupportedCommand),
        }
    }

    fn register(&mut self, mut descriptor: DeviceDescriptor) -> Arc<DeviceRuntime> {
        let id = descriptor.id.clone();
        if let Some(existing) = self.devices.get(&id)
            && existing.backend == descriptor.backend
        {
            descriptor.state_source = existing.state_source;
            descriptor.command_source = existing.command_source;
        }
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

struct IdentityStore {
    path: Option<PathBuf>,
    bindings: Vec<IdentityBinding>,
}

impl IdentityStore {
    fn load(path: PathBuf) -> Result<Self, DeviceRegistryError> {
        let bindings = match fs::read(&path) {
            Ok(bytes) => {
                let stored: IdentityFile = serde_json::from_slice(&bytes)
                    .map_err(|_| DeviceRegistryError::InvalidStore)?;
                validate_identity_file(stored)?
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(error) => return Err(DeviceRegistryError::Io(error)),
        };
        Ok(Self {
            path: Some(path),
            bindings,
        })
    }

    fn in_memory() -> Self {
        Self {
            path: None,
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
            self.save(&bindings)?;
            self.bindings = bindings;
        }
        Ok(descriptors)
    }

    fn save(&self, bindings: &[IdentityBinding]) -> Result<(), DeviceRegistryError> {
        let path = self
            .path
            .as_deref()
            .ok_or(DeviceRegistryError::PersistenceUnavailable)?;
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent).map_err(DeviceRegistryError::Io)?;
        let contents = serde_json::to_vec(&IdentityFile {
            version: 1,
            bindings: bindings.to_vec(),
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

fn validate_identity_file(file: IdentityFile) -> Result<Vec<IdentityBinding>, DeviceRegistryError> {
    let IdentityFile { version, bindings } = file;
    if version != 1 {
        return Err(DeviceRegistryError::InvalidStore);
    }
    let valid = bindings.iter().try_fold(
        (
            HashSet::with_capacity(bindings.len()),
            HashSet::with_capacity(bindings.len()),
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
    valid.map(|_| bindings)
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

    use super::*;
    use crate::device::{DeviceBackend, DeviceSettings, QuickConnectMode, StateProvenance};

    fn registry_path() -> PathBuf {
        std::env::temp_dir()
            .join(format!("updraft-identities-{}", uuid::Uuid::new_v4()))
            .join("identities.json")
    }

    fn cloud_device(provider_id: &str, name: &str) -> CloudDeviceInput {
        CloudDeviceInput::new(provider_id.to_owned(), name.to_owned())
    }

    fn ids_by_name(registry: &DeviceRegistry) -> BTreeMap<String, String> {
        registry
            .descriptors()
            .map(|descriptor| (descriptor.name.clone(), descriptor.id.as_str().to_owned()))
            .collect()
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
        registry
            .set_entity_sources(&cloud_id, EntitySource::Mqtt, EntitySource::Http)
            .unwrap();
        registry
            .reconcile_quickconnect("account-a", &[cloud_device("provider-a", "Cloud fan")])
            .unwrap();
        let cloud = registry
            .descriptors()
            .find(|descriptor| descriptor.id == cloud_id)
            .unwrap();
        assert_eq!(cloud.state_source, EntitySource::Mqtt);
        assert_eq!(cloud.command_source, EntitySource::Http);
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
            r#"{"version":1,"bindings":[{"identity":{"account_id":"account-a","provider_id":"provider-a"},"local_id":"configured"}]}"#,
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
            | DeviceRegistryError::UnsupportedCommand
            | DeviceRegistryError::Io(_)
            | DeviceRegistryError::Encoding(_) => false,
        }
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

        first
            .set_state(DeviceState {
                temperature_f: Some(102.0),
                humidity_percent: None,
                settings: DeviceSettings::QuickConnect {
                    mode: Some(QuickConnectMode::Automatic),
                    automatic_temperature_f: None,
                    automatic_humidity_percent: None,
                    timer_duration_minutes: None,
                    humidity_monitor: None,
                },
                provenance: StateProvenance {
                    backend: DeviceBackend::QuickConnect,
                    fetched_at_unix_ms: Some(1),
                    observed_at_unix_ms: None,
                },
            })
            .await;
        assert_eq!(first.state().await.unwrap().temperature_f, Some(102.0));
        assert!(second.state().await.is_none());
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
        assert!(!path.exists());
        fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    fn is_duplicate_provider_id(error: DeviceRegistryError) -> bool {
        match error {
            DeviceRegistryError::DuplicateProviderId => true,
            DeviceRegistryError::InvalidIdentity
            | DeviceRegistryError::InvalidStore
            | DeviceRegistryError::PersistenceUnavailable
            | DeviceRegistryError::UnknownDevice
            | DeviceRegistryError::UnsupportedCommand
            | DeviceRegistryError::Io(_)
            | DeviceRegistryError::Encoding(_) => false,
        }
    }
}
