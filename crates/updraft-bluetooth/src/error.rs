use std::{error::Error as StdError, fmt, io};

use anyhow::Error;
use thiserror::Error as ThisError;
use updraft_protocol::{FrameError, PayloadError, ReadbackError, UnexpectedResponse};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProbeErrorKind {
    Unavailable,
    Authentication,
    Protocol,
}

#[derive(Debug)]
pub enum ProbeError {
    Unavailable(Error),
    Authentication(Error),
    Protocol(Error),
}

impl ProbeError {
    pub fn classify(source: Error) -> Self {
        match Self::kind_for(&source) {
            ProbeErrorKind::Unavailable => Self::Unavailable(source),
            ProbeErrorKind::Authentication => Self::Authentication(source),
            ProbeErrorKind::Protocol => Self::Protocol(source),
        }
    }

    pub fn kind(&self) -> ProbeErrorKind {
        match self {
            Self::Unavailable(_) => ProbeErrorKind::Unavailable,
            Self::Authentication(_) => ProbeErrorKind::Authentication,
            Self::Protocol(_) => ProbeErrorKind::Protocol,
        }
    }

    fn source_error(&self) -> &Error {
        match self {
            Self::Unavailable(source) | Self::Authentication(source) | Self::Protocol(source) => {
                source
            }
        }
    }

    pub(super) fn kind_for(source: &Error) -> ProbeErrorKind {
        if source.chain().any(is_authentication_error) {
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

#[derive(Debug)]
pub(super) struct CleanupFailed {
    pub(super) operation: Error,
    pub(super) cleanup: Error,
}

impl fmt::Display for CleanupFailed {
    fn fmt(&self, output: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            output,
            "operation failed ({:#}); cleanup also failed ({:#})",
            self.operation, self.cleanup
        )
    }
}

impl StdError for CleanupFailed {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        self.operation.chain().next()
    }
}

impl fmt::Display for ProbeError {
    fn fmt(&self, output: &mut fmt::Formatter<'_>) -> fmt::Result {
        let category = match self {
            Self::Unavailable(_) => "BLE unavailable",
            Self::Authentication(_) => "BLE authentication or permission failure",
            Self::Protocol(_) => "GAF BLE protocol failure",
        };
        write!(output, "{category}: {:#}", self.source_error())
    }
}

impl StdError for ProbeError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        self.source_error().chain().next()
    }
}

#[derive(Debug, ThisError)]
#[error("device did not return a GAF identity response")]
pub(super) struct InvalidIdentityResponse;

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
    fn permission_denial_is_reported_as_authentication() {
        let error = ProbeError::classify(anyhow::Error::new(btleplug::Error::PermissionDenied));

        assert_eq!(error.kind(), ProbeErrorKind::Authentication);
    }

    #[test]
    fn malformed_frame_is_reported_as_protocol_failure() {
        let source = updraft_protocol::Frame::parse(b"not a frame\n")
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
