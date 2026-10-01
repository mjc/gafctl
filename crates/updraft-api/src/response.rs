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

pub(crate) fn required_option<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum ContractError {
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
        let mut ids = HashSet::new();
        if self.devices.iter().all(|device| ids.insert(&device.id)) {
            Ok(())
        } else {
            Err(ContractError::DuplicateDevice)
        }
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
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(from = "String", into = "String")]
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
    Unknown(String),
}

impl ControlStatus {
    #[must_use]
    pub fn is_confirmed(&self) -> bool {
        self == &Self::Confirmed
    }

    pub fn as_str(&self) -> &str {
        match self {
            Self::Confirmed => "confirmed",
            Self::Unconfirmed => "unconfirmed",
            Self::Rejected => "rejected",
            Self::SubmittedUnconfirmed => "submitted_unconfirmed",
            Self::ReadbackMismatch => "readback_mismatch",
            Self::ReadbackUnavailable => "readback_unavailable",
            Self::UnsupportedCommand => "unsupported_command",
            Self::StaleRequest => "stale_request",
            Self::RequestIdReused => "request_id_reused",
            Self::UnknownDevice => "unknown_device",
            Self::DeviceUnavailable => "device_unavailable",
            Self::BackendUnavailable => "backend_unavailable",
            Self::Busy => "busy",
            Self::ControlFailed => "control_failed",
            Self::InvalidRequestId => "invalid_request_id",
            Self::Unknown(value) => value,
        }
    }
}

impl From<String> for ControlStatus {
    fn from(value: String) -> Self {
        match value.as_str() {
            "confirmed" => Self::Confirmed,
            "unconfirmed" => Self::Unconfirmed,
            "rejected" => Self::Rejected,
            "submitted_unconfirmed" => Self::SubmittedUnconfirmed,
            "readback_mismatch" => Self::ReadbackMismatch,
            "readback_unavailable" => Self::ReadbackUnavailable,
            "unsupported_command" => Self::UnsupportedCommand,
            "stale_request" => Self::StaleRequest,
            "request_id_reused" => Self::RequestIdReused,
            "unknown_device" => Self::UnknownDevice,
            "device_unavailable" => Self::DeviceUnavailable,
            "backend_unavailable" => Self::BackendUnavailable,
            "busy" => Self::Busy,
            "control_failed" => Self::ControlFailed,
            "invalid_request_id" => Self::InvalidRequestId,
            _ => Self::Unknown(value),
        }
    }
}

impl From<&str> for ControlStatus {
    fn from(value: &str) -> Self {
        Self::from(value.to_owned())
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

impl std::fmt::Display for ControlStatus {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}
