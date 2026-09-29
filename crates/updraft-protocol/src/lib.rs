//! GAF attic fan protocol types and codecs.
//!
//! State query and ordinary control IDs and ASCII line framing are recovered
//! from the GAF Wi-Fi Vent app (`com.gaf.wifivent`) and its bundled firmware. Reply
//! payloads remain opaque unless their semantics are verified from app code or
//! device capture. Firmware update commands are not represented here.

mod command;
mod control;
mod frame;
mod state;
mod values;

pub use command::{ControlCommand, EncodedControlFrame, ReadCommand, Request, RequestFrame};
pub use control::{Acknowledgement, ControlOutcome, ControlReadback, Readback, ReadbackMatch};
pub use frame::{Frame, FrameDecoder, FrameError};
pub use state::{
    DeviceMode, DeviceSnapshot, FanState, FirmwareVersion, Identity, Observation, OperatingMode,
    PayloadError, SensorReadings, UnexpectedResponse,
};
pub use values::{
    AutomaticThresholds, HumidityTenthsPercent, Minutes, ReadbackError, TemperatureTenthsF,
    TimerState,
};
