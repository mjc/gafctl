//! Shared device state and command contract for the Gafctl v2 API.
mod control;
mod device;
mod legacy;
mod response;

pub use control::{CommandId, ControlPreset, ControlRequest, is_fresh_at, unix_millis};
pub use device::*;
pub use legacy::{AutomaticHumidityPercent, AutomaticTemperatureF, LegacyTimerMinutes};
pub use response::*;
