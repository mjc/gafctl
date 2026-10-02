//! Bluetooth discovery, state queries, and ordinary controls for GAF attic fans.

use std::time::Duration;

use gafctl_protocol::{ControlCommand, ControlOutcome, DeviceSnapshot};
use uuid::Uuid;

mod discovery;
mod error;
mod lifecycle;
mod probe;
mod session;

pub use discovery::{Candidate, DiscoveredDevice, DiscoveryFailure};
pub use error::{ProbeError, ProbeErrorKind};
pub use probe::{ProbeClient, probe};

/// GAF's observed primary BLE service UUID.
pub const GAF_SERVICE_UUID: Uuid = Uuid::from_u128(0x000000ff_0000_1000_8000_00805f9b34fb);
/// GAF's observed command/response BLE characteristic UUID.
pub const GAF_CHARACTERISTIC_UUID: Uuid = Uuid::from_u128(0x0000ff01_0000_1000_8000_00805f9b34fb);

/// Settings for one GAF BLE inspection.
#[derive(Clone, Debug)]
pub struct ProbeOptions {
    /// How long to scan for the GAF service before selecting a peripheral.
    pub scan_duration: Duration,
    /// Maximum time for GATT setup, each command write, and each response.
    /// Manager setup, scanning, connection, and cleanup allow at least 40 seconds.
    pub response_timeout: Duration,
    /// Action to take after scanning.
    pub mode: ProbeMode,
}

/// Whether to inspect advertisements only or query one matching peripheral.
#[derive(Clone, Debug)]
pub enum ProbeMode {
    /// Discover candidates without connecting or sending commands.
    Scan,
    /// Query one candidate, optionally changing an ordinary control setting.
    Query {
        /// Exact peripheral ID returned by a previous scan, if needed.
        device_id: Option<String>,
        /// Ordinary control write. Firmware update commands are not represented here.
        control_command: Option<ControlCommand>,
    },
}

impl Default for ProbeOptions {
    fn default() -> Self {
        Self {
            scan_duration: Duration::from_secs(6),
            response_timeout: Duration::from_secs(3),
            mode: ProbeMode::Query {
                device_id: None,
                control_command: None,
            },
        }
    }
}

/// Validated state and optional ordinary-control outcome from one device query.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QueryResult {
    /// All five state observations when every request completed.
    pub snapshot: Option<DeviceSnapshot>,
    /// Failure to collect state after an acknowledged command.
    pub state_error: Option<String>,
    /// Advertisement property failures elsewhere in the scan.
    pub discovery_failures: Vec<DiscoveryFailure>,
    /// Outcome of the optional ordinary control request, interpreted with readback.
    pub control: Option<ControlOutcome>,
    /// Whether the BLE connection closed cleanly after the query.
    pub disconnect: DisconnectOutcome,
}

/// Outcome of closing the BLE connection after a query.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DisconnectOutcome {
    /// The BLE connection closed successfully.
    Disconnected,
    /// The query succeeded but the platform reported an error while disconnecting.
    Failed(String),
}

/// Result of scanning and, when selected, querying a peripheral.
#[derive(Debug)]
pub enum ProbeResult {
    /// No peripherals advertising the service were found.
    NoDevices,
    /// Scan-only mode found one or more candidates.
    Discovered { devices: Vec<Candidate> },
    /// A query needs an exact device ID because multiple candidates were found.
    Ambiguous { devices: Vec<Candidate> },
    /// Discovery was incomplete, so automatic selection is unsafe.
    DiscoveryIncomplete {
        devices: Vec<Candidate>,
        failures: Vec<DiscoveryFailure>,
    },
    /// One peripheral was queried successfully.
    Queried {
        device: DiscoveredDevice,
        result: Box<QueryResult>,
    },
}
