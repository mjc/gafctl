use std::collections::HashSet;

use serde::{Deserialize, Deserializer, Serialize};
use thiserror::Error;

use crate::{
    CommandId, DeviceBackend, DeviceCommand, DeviceDescriptor, DeviceId, DeviceSettings,
    DeviceState,
};

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceInventoryStatus {
    #[default]
    Unknown,
    Present,
    Missing,
    Unavailable,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct DeviceListV2Response {
    pub devices: Vec<DeviceDescriptor>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct DeviceStateV2Response {
    pub id: DeviceId,
    pub backend: DeviceBackend,
    pub available: bool,
    pub inventory_status: DeviceInventoryStatus,
    #[serde(deserialize_with = "required_option")]
    pub last_error: Option<String>,
    #[serde(deserialize_with = "required_option")]
    pub state: Option<DeviceState>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceRefreshStatus {
    Fresh,
    Failed,
    Superseded,
}

impl DeviceRefreshStatus {
    pub const fn http_status(self) -> u16 {
        match self {
            Self::Fresh => 200,
            Self::Failed => 502,
            Self::Superseded => 409,
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct DeviceRefreshV2Response {
    pub status: DeviceRefreshStatus,
    #[serde(flatten)]
    pub device: DeviceStateV2Response,
}

impl DeviceRefreshV2Response {
    pub fn validate(&self) -> Result<(), ContractError> {
        self.device.validate()?;
        if self.status == DeviceRefreshStatus::Fresh
            && (!self.device.available
                || self.device.inventory_status != DeviceInventoryStatus::Present)
        {
            return Err(ContractError::Refresh);
        }
        Ok(())
    }
}

pub(crate) fn required_option<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum ContractError {
    #[error("fresh refresh requires an available current device snapshot")]
    Refresh,
    #[error("device inventory contains inconsistent proxy identities")]
    ProxyIdentity,
    #[error("a device must have one Home Assistant entity source")]
    EntityOwnership,
    #[error("device inventory contains duplicate local IDs")]
    DuplicateDevice,
    #[error("state availability does not match the snapshot")]
    Availability,
    #[error("state settings or provenance do not match the backend")]
    Backend,
    #[error("state contains invalid measurements")]
    Measurement,
}

impl DeviceListV2Response {
    pub fn validate(&self) -> Result<(), ContractError> {
        let proxy_id = self.devices.first().map(|device| device.proxy_id);
        self.devices
            .iter()
            .try_fold(HashSet::new(), |mut ids, device| {
                if Some(device.proxy_id) != proxy_id {
                    return Err(ContractError::ProxyIdentity);
                }
                if device.state_source != device.command_source {
                    return Err(ContractError::EntityOwnership);
                }
                if !ids.insert(&device.id) {
                    return Err(ContractError::DuplicateDevice);
                }
                Ok(ids)
            })
            .map(|_| ())
    }
}

impl DeviceStateV2Response {
    pub fn validate(&self) -> Result<(), ContractError> {
        if self.available != self.state.is_some() {
            return Err(ContractError::Availability);
        }
        let Some(state) = &self.state else {
            return Ok(());
        };
        let settings_backend = match state.settings {
            DeviceSettings::LegacyBle { .. } => DeviceBackend::LegacyBle,
            DeviceSettings::QuickConnect { .. } => DeviceBackend::QuickConnect,
        };
        if settings_backend != self.backend || state.provenance.backend != self.backend {
            return Err(ContractError::Backend);
        }
        if state.temperature_f.is_some_and(|value| !value.is_finite())
            || state
                .humidity_percent
                .is_some_and(|value| !value.is_finite() || !(0.0..=100.0).contains(&value))
        {
            return Err(ContractError::Measurement);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DeviceControlV2Request {
    pub request_id: CommandId,
    pub issued_at_unix_ms: u64,
    pub command: DeviceCommand,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DeviceControlV2Response {
    pub request_id: String,
    pub status: ControlStatus,
}

/// Backend outcome. Unrecognized future statuses retain their wire value.
#[derive(
    Clone,
    Debug,
    Deserialize,
    Eq,
    PartialEq,
    Serialize,
    strum::AsRefStr,
    strum::Display,
    strum::EnumString,
    strum::EnumIter,
)]
#[serde(from = "String", into = "String")]
#[strum(serialize_all = "snake_case")]
pub enum ControlStatus {
    Confirmed,
    Unconfirmed,
    Rejected,
    SubmittedUnconfirmed,
    ReadbackMismatch,
    ReadbackUnavailable,
    UnsupportedCommand,
    StaleRequest,
    RequestIdReused,
    UnknownDevice,
    DeviceUnavailable,
    BackendUnavailable,
    Busy,
    ControlFailed,
    InvalidRequestId,
    #[strum(default, transparent)]
    Unknown(String),
}

impl ControlStatus {
    #[must_use]
    pub fn is_confirmed(&self) -> bool {
        self == &Self::Confirmed
    }

    pub fn as_str(&self) -> &str {
        self.as_ref()
    }
}

impl From<String> for ControlStatus {
    fn from(value: String) -> Self {
        <Self as strum::IntoEnumIterator>::iter()
            .find(|status| match status {
                Self::Unknown(_) => false,
                status => status.as_ref() == value,
            })
            .unwrap_or(Self::Unknown(value))
    }
}

impl From<ControlStatus> for String {
    fn from(value: ControlStatus) -> Self {
        value.as_str().to_owned()
    }
}

impl PartialEq<&str> for ControlStatus {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn refresh_status_has_one_http_contract() {
        use super::DeviceRefreshStatus;
        for (status, expected) in [
            (DeviceRefreshStatus::Fresh, 200),
            (DeviceRefreshStatus::Failed, 502),
            (DeviceRefreshStatus::Superseded, 409),
        ] {
            assert_eq!(status.http_status(), expected);
        }
    }

    use super::*;
    use crate::EntitySource;

    #[test]
    fn fresh_refresh_requires_an_available_present_snapshot() {
        let response = DeviceRefreshV2Response {
            status: DeviceRefreshStatus::Fresh,
            device: DeviceStateV2Response {
                id: DeviceId::configured_ble(),
                backend: DeviceBackend::LegacyBle,
                available: false,
                inventory_status: DeviceInventoryStatus::Unknown,
                last_error: None,
                state: None,
            },
        };
        assert!(response.validate().is_err());
        let failed = DeviceRefreshV2Response {
            status: DeviceRefreshStatus::Failed,
            ..response
        };
        assert!(failed.validate().is_ok());
    }

    #[test]
    fn inventory_rejects_mixed_proxy_identity_and_split_ownership() {
        let first = DeviceDescriptor::configured_ble();
        let mut second = DeviceDescriptor::configured_ble();
        second.id = "another".parse().unwrap();
        let mut inventory = DeviceListV2Response {
            devices: vec![first, second],
        };
        assert!(inventory.validate().is_err());
        inventory.devices[1].proxy_id = inventory.devices[0].proxy_id;
        assert!(inventory.validate().is_ok());
        inventory.devices[1].state_source = EntitySource::Mqtt;
        assert!(inventory.validate().is_err());
    }
}
