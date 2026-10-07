use std::{error::Error as StdError, io};

use crate::protocol::{FrameError, PayloadError, ReadbackError, UnexpectedResponse};
use anyhow::Error;
use thiserror::Error as ThisError;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProbeErrorKind {
    StaleControl,
    Unavailable,
    Authentication,
    Protocol,
}

#[derive(Debug, ThisError)]
pub enum ProbeError {
    #[error("BLE control expired before writing: {0:#}")]
    StaleControl(#[source] Error),
    #[error("BLE unavailable: {0:#}")]
    Unavailable(#[source] Error),
    #[error("BLE authentication or permission failure: {0:#}")]
    Authentication(#[source] Error),
    #[error("GAF BLE protocol failure: {0:#}")]
    Protocol(#[source] Error),
}

impl ProbeError {
    pub fn classify(source: Error) -> Self {
        match Self::kind_for(&source) {
            ProbeErrorKind::StaleControl => Self::StaleControl(source),
            ProbeErrorKind::Unavailable => Self::Unavailable(source),
            ProbeErrorKind::Authentication => Self::Authentication(source),
            ProbeErrorKind::Protocol => Self::Protocol(source),
        }
    }

    pub fn kind(&self) -> ProbeErrorKind {
        match self {
            Self::StaleControl(_) => ProbeErrorKind::StaleControl,
            Self::Unavailable(_) => ProbeErrorKind::Unavailable,
            Self::Authentication(_) => ProbeErrorKind::Authentication,
            Self::Protocol(_) => ProbeErrorKind::Protocol,
        }
    }

    pub(super) fn kind_for(source: &Error) -> ProbeErrorKind {
        if source
            .chain()
            .any(|cause| cause.downcast_ref::<ControlExpired>().is_some())
        {
            ProbeErrorKind::StaleControl
        } else if source.chain().any(is_authentication_error) {
            ProbeErrorKind::Authentication
        } else if source.chain().any(is_protocol_error) {
            ProbeErrorKind::Protocol
        } else {
            ProbeErrorKind::Unavailable
        }
    }

    pub(super) fn is_transient_connect_failure(source: &Error) -> bool {
        if source
            .chain()
            .any(|cause| cause.downcast_ref::<CleanupFailed>().is_some())
        {
            return false;
        }
        source.chain().any(|cause| {
            cause
                .downcast_ref::<btleplug::Error>()
                .is_some_and(is_transient_btleplug_error)
                || cause
                    .downcast_ref::<io::Error>()
                    .is_some_and(|error| is_transient_io_error(error.kind()))
                || cause
                    .downcast_ref::<tokio::time::error::Elapsed>()
                    .is_some()
                || is_platform_transient_connect_error(cause)
        })
    }
}

pub(super) fn cleanup_is_complete(source: &Error) -> bool {
    source.chain().any(|cause| {
        cause
            .downcast_ref::<btleplug::Error>()
            .is_some_and(is_absent_peripheral)
            || is_platform_cleanup_complete(cause)
    })
}

fn is_absent_peripheral(error: &btleplug::Error) -> bool {
    if let btleplug::Error::NotConnected = error {
        return true;
    }
    if let btleplug::Error::DeviceNotFound = error {
        return true;
    }
    false
}

#[cfg(target_os = "linux")]
fn is_platform_cleanup_complete(cause: &(dyn StdError + 'static)) -> bool {
    let error = cause
        .downcast_ref::<bluez_async::BluetoothError>()
        .or_else(|| {
            let btleplug::Error::Other(source) = cause.downcast_ref::<btleplug::Error>()? else {
                return None;
            };
            source.downcast_ref::<bluez_async::BluetoothError>()
        });
    let Some(bluez_async::BluetoothError::DbusError(error)) = error else {
        return false;
    };
    dbus_cleanup_is_complete(error.name(), error.message())
}

#[cfg(not(target_os = "linux"))]
fn is_platform_cleanup_complete(_cause: &(dyn StdError + 'static)) -> bool {
    false
}

#[cfg(any(target_os = "linux", test))]
fn dbus_cleanup_is_complete(name: Option<&str>, message: Option<&str>) -> bool {
    match name {
        Some(
            "org.bluez.Error.NotConnected"
            | "org.bluez.Error.DoesNotExist"
            | "org.freedesktop.DBus.Error.UnknownObject",
        ) => true,
        Some("org.bluez.Error.Failed") => message == Some("No discovery started"),
        _ => false,
    }
}

#[derive(Debug, ThisError)]
#[error("operation failed ({operation:#}); cleanup also failed ({cleanup:#})")]
pub(super) struct CleanupFailed {
    #[source]
    pub(super) operation: Error,
    pub(super) cleanup: Error,
}

#[derive(Debug, ThisError)]
#[error("control deadline passed before writing")]
pub(super) struct ControlExpired;

#[derive(Debug, ThisError)]
#[error("device did not return a GAF identity response")]
pub(super) struct InvalidIdentityResponse;

#[derive(Debug, ThisError)]
#[error("GAF firmware before version 2 does not support ordinary controls")]
pub(super) struct UnsupportedControlFirmware;

fn is_authentication_error(cause: &(dyn StdError + 'static)) -> bool {
    cause
        .downcast_ref::<btleplug::Error>()
        .is_some_and(is_permission_denied)
        || cause
            .downcast_ref::<io::Error>()
            .is_some_and(|error| error.kind() == io::ErrorKind::PermissionDenied)
        || is_platform_authentication_error(cause)
}

fn is_protocol_error(cause: &(dyn StdError + 'static)) -> bool {
    cause.downcast_ref::<FrameError>().is_some()
        || cause.downcast_ref::<PayloadError>().is_some()
        || cause.downcast_ref::<ReadbackError>().is_some()
        || cause.downcast_ref::<UnexpectedResponse>().is_some()
        || cause.downcast_ref::<InvalidIdentityResponse>().is_some()
        || cause.downcast_ref::<UnsupportedControlFirmware>().is_some()
}

fn is_transient_io_error(kind: io::ErrorKind) -> bool {
    kind == io::ErrorKind::ConnectionAborted
        || kind == io::ErrorKind::ConnectionRefused
        || kind == io::ErrorKind::ConnectionReset
        || kind == io::ErrorKind::NotConnected
        || kind == io::ErrorKind::TimedOut
        || kind == io::ErrorKind::WouldBlock
}

fn is_transient_btleplug_error(error: &btleplug::Error) -> bool {
    if let btleplug::Error::DeviceNotFound = error {
        return true;
    }
    if let btleplug::Error::NotConnected = error {
        return true;
    }
    if let btleplug::Error::NoAdapterAvailable = error {
        return true;
    }
    if let btleplug::Error::TimedOut(_) = error {
        return true;
    }
    false
}

fn is_permission_denied(error: &btleplug::Error) -> bool {
    if let btleplug::Error::PermissionDenied = error {
        return true;
    }
    false
}

#[cfg(target_os = "linux")]
fn is_platform_authentication_error(cause: &(dyn StdError + 'static)) -> bool {
    use bluez_async::BluetoothError;

    if let Some(error) = cause.downcast_ref::<BluetoothError>() {
        return is_bluez_authentication_error(error);
    }
    if let Some(btleplug::Error::Other(source)) = cause.downcast_ref::<btleplug::Error>() {
        return source
            .downcast_ref::<BluetoothError>()
            .is_some_and(is_bluez_authentication_error);
    }
    false
}

#[cfg(target_os = "linux")]
fn is_dbus_authentication_error(name: &str) -> bool {
    name == "org.freedesktop.DBus.Error.AccessDenied"
        || name == "org.bluez.Error.NotAuthorized"
        || name == "org.bluez.Error.AuthenticationFailed"
        || name == "org.bluez.Error.AuthenticationCanceled"
        || name == "org.bluez.Error.NotPermitted"
}

#[cfg(target_os = "linux")]
fn is_bluez_authentication_error(error: &bluez_async::BluetoothError) -> bool {
    let bluez_async::BluetoothError::DbusError(error) = error else {
        return false;
    };
    error.name().is_some_and(is_dbus_authentication_error)
}

#[cfg(target_os = "linux")]
fn is_platform_transient_connect_error(cause: &(dyn StdError + 'static)) -> bool {
    if let Some(error) = cause.downcast_ref::<bluez_async::BluetoothError>() {
        return is_transient_bluez_error(error);
    }
    if let Some(btleplug::Error::Other(source)) = cause.downcast_ref::<btleplug::Error>() {
        return source
            .downcast_ref::<bluez_async::BluetoothError>()
            .is_some_and(is_transient_bluez_error);
    }
    false
}

#[cfg(target_os = "linux")]
fn is_transient_bluez_error(error: &bluez_async::BluetoothError) -> bool {
    if let bluez_async::BluetoothError::ServiceDiscoveryTimedOut = error {
        return true;
    }
    let bluez_async::BluetoothError::DbusError(error) = error else {
        return false;
    };
    error.name().is_some_and(|name| {
        name == "org.bluez.Error.NotConnected"
            || name == "org.bluez.Error.NotReady"
            || name == "org.bluez.Error.Failed"
            || name == "org.freedesktop.DBus.Error.NoReply"
    })
}

#[cfg(not(target_os = "linux"))]
fn is_platform_authentication_error(_cause: &(dyn StdError + 'static)) -> bool {
    false
}

#[cfg(not(target_os = "linux"))]
fn is_platform_transient_connect_error(_cause: &(dyn StdError + 'static)) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use anyhow::anyhow;

    use super::{ProbeError, ProbeErrorKind};

    #[cfg(target_os = "linux")]
    #[test]
    fn dbus_access_denial_is_reported_as_authentication() {
        assert!(super::is_dbus_authentication_error(
            "org.freedesktop.DBus.Error.AccessDenied"
        ));
    }

    #[test]
    fn cleanup_accepts_only_established_absence_or_stopped_state() {
        for (name, message, completed) in [
            ("org.bluez.Error.NotConnected", None, true),
            ("org.freedesktop.DBus.Error.UnknownObject", None, true),
            ("org.bluez.Error.DoesNotExist", None, true),
            ("org.bluez.Error.Failed", Some("No discovery started"), true),
            (
                "org.bluez.Error.Failed",
                Some("le-connection-abort-by-local"),
                false,
            ),
            ("org.freedesktop.DBus.Error.NoReply", None, false),
            ("org.bluez.Error.NotReady", None, false),
        ] {
            assert_eq!(
                super::dbus_cleanup_is_complete(Some(name), message),
                completed,
                "{name} {message:?}"
            );
        }
    }

    #[test]
    fn permission_denial_is_reported_as_authentication() {
        let error = ProbeError::classify(anyhow::Error::new(btleplug::Error::PermissionDenied));

        assert_eq!(error.kind(), ProbeErrorKind::Authentication);
    }

    #[test]
    fn malformed_frame_is_reported_as_protocol_failure() {
        let source = crate::protocol::Frame::parse(b"not a frame\n")
            .expect_err("malformed frame must fail parsing");
        let error = ProbeError::classify(anyhow::Error::new(source));

        assert_eq!(error.kind(), ProbeErrorKind::Protocol);
    }

    #[test]
    fn invalid_identity_response_is_reported_as_protocol_failure() {
        let error = ProbeError::classify(anyhow::Error::new(super::InvalidIdentityResponse));

        assert_eq!(error.kind(), ProbeErrorKind::Protocol);
    }

    #[test]
    fn unclassified_platform_failure_is_reported_as_unavailable() {
        let error = ProbeError::classify(anyhow!("Bluetooth adapter disappeared"));

        assert_eq!(error.kind(), ProbeErrorKind::Unavailable);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn wrapped_bluez_connection_abort_is_retryable() {
        let platform = bluez_async::BluetoothError::DbusError(dbus::Error::new_custom(
            "org.bluez.Error.Failed",
            "le-connection-abort-by-local",
        ));
        let wrapped = anyhow::Error::new(btleplug::Error::from(platform))
            .context("connect to GAF BLE peripheral");
        assert!(ProbeError::is_transient_connect_failure(&wrapped));
    }

    #[test]
    fn only_transient_connect_errors_are_retried() {
        assert!(ProbeError::is_transient_connect_failure(
            &anyhow::Error::new(btleplug::Error::TimedOut(Duration::from_secs(3)))
        ));
        assert!(!ProbeError::is_transient_connect_failure(
            &anyhow::Error::new(btleplug::Error::PermissionDenied)
        ));
        assert!(!ProbeError::is_transient_connect_failure(&anyhow!(
            "malformed GAF response"
        )));
    }
}
