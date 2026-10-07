use std::fmt;

use crate::protocol::{ControlReadback, FanState, ModeReadback, OperatingMode, ReadbackMatch};

pub(crate) struct ControlReadbackDisplay<'a>(pub(crate) &'a ControlReadback);

impl fmt::Display for ControlReadbackDisplay<'_> {
    fn fmt(&self, output: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            ControlReadback::Unavailable => output.write_str("control readback: unavailable"),
            ControlReadback::Thresholds(Ok(readback)) => write!(
                output,
                "automatic threshold readback: {}",
                describe_readback_match(readback.comparison),
            ),
            ControlReadback::Thresholds(Err(_)) => {
                output.write_str("automatic threshold readback: unrecognized payload")
            }
            ControlReadback::Timer(Ok(readback)) => write!(
                output,
                "timer readback: remaining={} minute(s), original={} minute(s); {}",
                readback.actual.remaining.value(),
                readback.actual.original.value(),
                describe_readback_match(readback.comparison),
            ),
            ControlReadback::Timer(Err(_)) => {
                output.write_str("timer readback: unrecognized payload")
            }
        }
    }
}

pub(crate) struct ModeReadbackDisplay(pub(crate) ModeReadback);

impl fmt::Display for ModeReadbackDisplay {
    fn fmt(&self, output: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            ModeReadback::Matches(mode) => write!(
                output,
                "mode readback: {}; matches request",
                mode_name(mode)
            ),
            ModeReadback::Differs(mode) => write!(
                output,
                "mode readback: {}; differs from request",
                mode_name(mode)
            ),
            ModeReadback::FanFlagDiffers { mode, actual } => write!(
                output,
                "mode readback: {}; controller fan flag is {}, but timer clear expects off",
                mode_name(mode),
                fan_state_name(actual),
            ),
            ModeReadback::UnverifiedTimerExpiry(mode) => write!(
                output,
                "mode readback: {}; timer-expiry pattern is unverified; control is not confirmed",
                mode_name(mode)
            ),
            ModeReadback::Unrecognized(error) => {
                write!(output, "mode readback: unrecognized payload ({error})")
            }
            ModeReadback::Unavailable => output.write_str("mode readback: unavailable"),
        }
    }
}

fn mode_name(mode: OperatingMode) -> &'static str {
    match mode {
        OperatingMode::Automatic => "automatic",
        OperatingMode::Timer => "timer",
        OperatingMode::Ota => "OTA",
    }
}

fn fan_state_name(fan: FanState) -> &'static str {
    match fan {
        FanState::Off => "off",
        FanState::On => "on",
    }
}

fn describe_readback_match(comparison: ReadbackMatch) -> &'static str {
    match comparison {
        ReadbackMatch::Matches => "matches request",
        ReadbackMatch::Differs => "differs from request",
    }
}
