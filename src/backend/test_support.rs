use super::*;
use crate::model::{
    DeviceBackend, DeviceSettings, DeviceState, QuickConnectModeStatus, StateProvenance,
};

pub(super) fn ids_by_name(registry: &DeviceRegistry) -> BTreeMap<String, String> {
    registry
        .descriptors()
        .map(|descriptor| (descriptor.name.clone(), descriptor.id.as_str().to_owned()))
        .collect()
}

pub(super) fn is_invalid_store(error: DeviceRegistryError) -> bool {
    std::mem::discriminant(&error) == std::mem::discriminant(&DeviceRegistryError::InvalidStore)
}

pub(super) fn is_duplicate_provider_id(error: DeviceRegistryError) -> bool {
    std::mem::discriminant(&error)
        == std::mem::discriminant(&DeviceRegistryError::DuplicateProviderId)
}

pub(super) fn observed_state(fetched_at_unix_ms: Option<u64>) -> DeviceState {
    DeviceState {
        temperature_f: Some(102.0),
        humidity_percent: Some(43.0),
        settings: DeviceSettings::QuickConnect {
            mode: QuickConnectModeStatus::Automatic,
            automatic_temperature_f: Some(105),
            automatic_humidity_percent: Some(40),
            timer_duration_minutes: None,
            humidity_monitor: Some(true),
        },
        estimated_running: Some(true),
        diagnostics: None,
        provenance: StateProvenance {
            backend: DeviceBackend::QuickConnect,
            fetched_at_unix_ms,
            observed_at_unix_ms: None,
        },
    }
}
