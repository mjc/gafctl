//! GAF attic fan protocol types and codecs.
//!
//! State query IDs, control IDs, and ASCII line framing come
//! from the GAF Wi-Fi Vent app (`com.gaf.wifivent`) and its bundled firmware. Reply
//! payloads are decoded only when their meaning is verified from app code or
//! device captures. Firmware update commands are unsupported.

mod command;
mod control;
mod frame;
mod state;
mod values;

pub use command::{ControlCommand, EncodedControlFrame, ReadCommand, Request, RequestFrame};
pub use control::{
    Acknowledgement, ControlOutcome, ControlReadback, ModeReadback, Readback, ReadbackMatch,
};
pub use frame::{Frame, FrameDecoder, FrameError};
pub use state::{
    DeviceMode, DeviceSnapshot, FanState, FirmwareVersion, Identity, Observation, OperatingMode,
    PayloadError, SensorReadings, StateFreshness, StateReconciler, UnexpectedResponse,
};
pub use values::{
    AutomaticThresholds, HumidityTenthsPercent, Minutes, ReadbackError, TemperatureTenthsF,
    TimerState,
};
