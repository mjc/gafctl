use crate::{
    AutomaticThresholds, ControlCommand, DeviceSnapshot, Frame, PayloadError, TimerState,
    UnexpectedResponse, state::validate_response,
};

/// Whether a decoded state readback agrees with a control request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReadbackMatch {
    Matches,
    Differs,
}

/// Decoded state after a control request, with its comparison result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Readback<T> {
    /// Decoded value read after the control command.
    pub actual: T,
    /// Comparison with the requested setting.
    pub comparison: ReadbackMatch,
}

/// The control-specific readback, including any payload decode error.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ControlReadback {
    Thresholds(Result<Readback<AutomaticThresholds>, PayloadError>),
    Timer(Result<Readback<TimerState>, PayloadError>),
}

/// An exact `0` acknowledgement or an unrecognized response payload.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Acknowledgement {
    Accepted,
    Unrecognized,
}

/// The retained control acknowledgement and comparison with later state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ControlOutcome {
    command: ControlCommand,
    frame: Frame<'static>,
    acknowledgement: Acknowledgement,
    readback: ControlReadback,
}

impl ControlOutcome {
    /// Interpret an acknowledgement and the corresponding snapshot readback.
    pub fn from_response(
        command: ControlCommand,
        frame: Frame<'static>,
        snapshot: &DeviceSnapshot,
    ) -> Result<Self, UnexpectedResponse> {
        validate_response(&frame, command.response_id())?;
        let acknowledgement = match frame.payload() {
            b"0" => Acknowledgement::Accepted,
            _ => Acknowledgement::Unrecognized,
        };
        let readback = match command {
            ControlCommand::SetAutomaticThresholds(requested) => ControlReadback::Thresholds(
                snapshot
                    .thresholds
                    .decoded()
                    .map(|actual| Readback {
                        actual: *actual,
                        comparison: if *actual == requested {
                            ReadbackMatch::Matches
                        } else {
                            ReadbackMatch::Differs
                        },
                    })
                    .map_err(|error| *error),
            ),
            ControlCommand::SetTimer(requested) => ControlReadback::Timer(
                snapshot
                    .timer
                    .decoded()
                    .map(|actual| Readback {
                        actual: *actual,
                        comparison: if actual.matches_requested_duration(requested) {
                            ReadbackMatch::Matches
                        } else {
                            ReadbackMatch::Differs
                        },
                    })
                    .map_err(|error| *error),
            ),
        };
        Ok(Self {
            command,
            frame,
            acknowledgement,
            readback,
        })
    }

    /// Return the ordinary control command that was sent.
    #[must_use]
    pub const fn command(&self) -> ControlCommand {
        self.command
    }

    /// Return the exact acknowledgement frame.
    #[must_use]
    pub fn frame(&self) -> &Frame<'static> {
        &self.frame
    }

    /// Return whether the acknowledgement payload was exactly `0`.
    #[must_use]
    pub const fn acknowledgement(&self) -> Acknowledgement {
        self.acknowledgement
    }

    /// Return the typed readback and comparison, or its parse error.
    #[must_use]
    pub const fn readback(&self) -> &ControlReadback {
        &self.readback
    }
}
