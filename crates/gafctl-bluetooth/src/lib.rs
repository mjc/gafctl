//! Bluetooth discovery, state queries, and controls for GAF attic fans.

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

/// Maximum time allowed to cancel an in-flight operation and close tracked BLE resources.
pub const SHUTDOWN_CLEANUP_TIMEOUT: Duration = Duration::from_secs(80);
/// Maximum time the server waits for bounded BLE shutdown cleanup.
pub const SHUTDOWN_DRAIN_TIMEOUT: Duration = Duration::from_secs(85);

/// Settings for one GAF BLE inspection.
#[derive(Clone, Debug)]
pub struct ProbeOptions {
    /// How long to scan for GAFVent advertisements before selecting a peripheral.
    pub scan_duration: Duration,
    /// Maximum time for GATT setup, each command write, and each response.
    /// Manager setup, scanning, connection, and cleanup allow at least 40 seconds.
    pub response_timeout: Duration,
    /// Latest instant a control may be written; reads and CLI controls have no deadline.
    pub control_deadline: Option<tokio::time::Instant>,
    /// Action to take after scanning.
    pub mode: ProbeMode,
}

/// Whether to inspect advertisements only or query one matching peripheral.
#[derive(Clone, Debug)]
pub enum ProbeMode {
    /// Discover candidates without connecting or sending commands.
    Scan,
    /// Query one candidate, optionally changing a threshold or timer setting.
    Query {
        /// Exact peripheral ID returned by a previous scan, if needed.
        device_id: Option<String>,
        /// Threshold or timer write.
        control_command: Option<ControlCommand>,
    },
}

impl Default for ProbeOptions {
    fn default() -> Self {
        Self {
            scan_duration: Duration::from_secs(5),
            response_timeout: Duration::from_secs(3),
            control_deadline: None,
            mode: ProbeMode::Query {
                device_id: None,
                control_command: None,
            },
        }
    }
}

/// State and optional control result from one device query.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QueryResult {
    /// Sensor readings and settings, with timer observations when queried.
    pub snapshot: Option<DeviceSnapshot>,
    /// Failure to collect state after an acknowledged command.
    pub state_error: Option<String>,
    /// Advertisement property failures elsewhere in the scan.
    pub discovery_failures: Vec<DiscoveryFailure>,
    /// Control acknowledgement and readback, when a control was requested.
    pub control: Option<ControlOutcome>,
    /// Whether the BLE connection is retained, closed, or failed to close.
    pub disconnect: DisconnectOutcome,
}

/// Connection ownership after a query.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DisconnectOutcome {
    /// The service retains the connection for subsequent polls and controls.
    Retained,
    /// The BLE connection closed successfully.
    Disconnected,
    /// The query succeeded but the platform reported an error while disconnecting.
    Failed(String),
}

/// Result of scanning and, when selected, querying a peripheral.
#[derive(Debug)]
pub enum ProbeResult {
    /// No matching GAFVent peripherals were found.
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
