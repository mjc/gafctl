use super::{CloudDeviceInput, DeviceRegistryError};
use gafctl_api::{
    DeviceBackend, DeviceCapabilities, DeviceDescriptor, DeviceId, EntitySource, ProxyId,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashSet},
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
};

#[derive(Clone, Eq, Hash, Ord, PartialEq, PartialOrd, Deserialize, Serialize)]
pub(super) struct ProviderIdentity {
    pub(super) account_id: String,
    pub(super) provider_id: String,
}

impl std::fmt::Debug for ProviderIdentity {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("ProviderIdentity([redacted])")
    }
}

#[derive(Clone, Deserialize, Serialize)]
pub(super) struct IdentityBinding {
    pub(super) identity: ProviderIdentity,
    pub(super) local_id: DeviceId,
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

pub(super) struct IdentityStore {
    path: Option<PathBuf>,
    pub(super) proxy_id: ProxyId,
    pub(super) sources: BTreeMap<DeviceId, EntitySource>,
    pub(super) bindings: Vec<IdentityBinding>,
}

impl IdentityStore {
    pub(super) fn load(path: PathBuf) -> Result<Self, DeviceRegistryError> {
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

    pub(super) fn in_memory() -> Self {
        Self {
            path: None,
            proxy_id: ProxyId::default(),
            sources: BTreeMap::new(),
            bindings: Vec::new(),
        }
    }

    pub(super) fn reconcile(
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

    pub(super) fn set_sources(
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
        let mut file = tempfile::NamedTempFile::new_in(parent).map_err(DeviceRegistryError::Io)?;
        file.write_all(&contents)
            .and_then(|()| file.as_file().sync_all())
            .map_err(DeviceRegistryError::Io)?;
        file.persist(path)
            .map(drop)
            .map_err(|error| DeviceRegistryError::Io(error.error))
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

#[cfg(test)]
mod tests {
    use super::super::{DeviceRegistry, test_support::*};
    use super::*;
    use crate::test_support::{cloud_device, identity_store_fixture};
    use serde_json::json;

    #[test]
    fn failed_identity_replacement_removes_staging_files() {
        let (directory, path) = identity_store_fixture();
        let store = IdentityStore::load(path.clone()).unwrap();
        fs::remove_file(&path).unwrap();
        fs::create_dir(&path).unwrap();

        assert!(store.save(&store.bindings, &store.sources).is_err());
        let entries = fs::read_dir(directory.path())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>();
        assert_eq!(entries, [path]);
    }

    #[cfg(unix)]
    #[test]
    fn identity_replacement_keeps_private_file_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let (_directory, path) = identity_store_fixture();
        let store = IdentityStore::load(path.clone()).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        store.save(&store.bindings, &store.sources).unwrap();

        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(IdentityStore::load(path).unwrap().proxy_id, store.proxy_id);
    }

    #[test]
    fn identity_store_rejects_dangling_sources_and_invalid_proxy_ids() {
        let (_directory, path) = identity_store_fixture();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        [
            (
                ProxyId::default().to_string(),
                json!({"unbound-id": "mqtt"}),
            ),
            ("not-a-uuid".to_owned(), json!({})),
            (uuid::Uuid::nil().to_string(), json!({})),
            ("550e8400-e29b-11d4-a716-446655440000".to_owned(), json!({})),
        ]
        .into_iter()
        .for_each(|(proxy_id, sources)| {
            fs::write(
                &path,
                serde_json::to_vec(&json!({
                    "version": 2, "proxy_id": proxy_id, "sources": sources, "bindings": []
                }))
                .unwrap(),
            )
            .unwrap();
            assert!(DeviceRegistry::load(&path).is_err_and(is_invalid_store));
        });
        for local_id in [
            "configured".to_owned(),
            "".to_owned(),
            "space here".to_owned(),
            "../device".to_owned(),
            "a".repeat(65),
        ] {
            fs::write(
                &path,
                serde_json::to_vec(&json!({
                    "version": 2,
                    "proxy_id": ProxyId::default(),
                    "sources": {},
                    "bindings": [{
                        "identity": {"account_id": "account", "provider_id": "provider"},
                        "local_id": local_id
                    }]
                }))
                .unwrap(),
            )
            .unwrap();
            assert!(DeviceRegistry::load(&path).is_err_and(is_invalid_store));
        }
    }

    #[test]
    fn failed_source_persistence_preserves_live_and_saved_ownership() {
        let (_directory, path) = identity_store_fixture();
        let path = path.with_file_name("store").join("identities.json");
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
    }

    #[test]
    fn entity_sources_survive_restart_for_ble_and_cloud() {
        let (_directory, path) = identity_store_fixture();
        let mut registry = DeviceRegistry::load(&path).unwrap();
        registry.register_configured_ble();
        let cloud = registry
            .reconcile_quickconnect(
                "test-account",
                &[cloud_device("private-provider", "Cloud fan")],
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
                &[cloud_device("private-provider", "Renamed cloud fan")],
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
    }

    #[test]
    fn proxy_identity_is_persisted_before_devices_are_registered() {
        let (_directory, path) = identity_store_fixture();
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
    }

    #[test]
    fn cloud_identity_survives_order_rename_restart_and_coexists_with_ble() {
        let (_directory, path) = identity_store_fixture();
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
        assert_eq!(registry.identities.bindings.len(), 2);

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
    }

    #[test]
    fn provider_identifiers_are_scoped_to_their_account() {
        let (_directory, path) = identity_store_fixture();
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
    }

    #[test]
    fn duplicate_provider_ids_reject_the_inventory_before_saving() {
        let (_directory, path) = identity_store_fixture();
        let mut registry = DeviceRegistry::load(&path).unwrap();
        let result = registry.reconcile_quickconnect(
            "account-a",
            &[cloud_device("same", "One"), cloud_device("same", "Two")],
        );
        assert!(result.err().is_some_and(is_duplicate_provider_id));
        assert_eq!(registry.identities.bindings.len(), 0);
        let stored: IdentityFile = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert!(stored.bindings.is_empty());
    }
}
