use super::*;
use std::path::PathBuf;
pub(super) fn registry_fixture() -> (tempfile::TempDir, PathBuf) {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/backend-tests");
    std::fs::create_dir_all(&root).unwrap();
    let directory = tempfile::Builder::new()
        .prefix("gafctl-identities-")
        .tempdir_in(root)
        .unwrap();
    let path = directory.path().join("identities.json");
    (directory, path)
}

pub(super) fn cloud_device(provider_id: &str, name: &str) -> CloudDeviceInput {
    CloudDeviceInput::new(provider_id.to_owned(), name.to_owned())
}

pub(super) fn ids_by_name(registry: &DeviceRegistry) -> BTreeMap<String, String> {
    registry
        .descriptors()
        .map(|descriptor| (descriptor.name.clone(), descriptor.id.as_str().to_owned()))
        .collect()
}

pub(super) fn is_invalid_store(error: DeviceRegistryError) -> bool {
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

pub(super) fn is_duplicate_provider_id(error: DeviceRegistryError) -> bool {
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
