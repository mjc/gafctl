use crate::{
    AutomaticThresholds, ControlCommand, DeviceSnapshot, FanState, Frame, OperatingMode,
    PayloadError, TimerState, UnexpectedResponse, state::validate_response,
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
    /// The state request needed for verification failed after the command reply.
    Unavailable,
}

/// Operating mode observed after a control command.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModeReadback {
    Matches(OperatingMode),
    Differs(OperatingMode),
    FanFlagDiffers {
        mode: OperatingMode,
        actual: FanState,
    },
    UnverifiedTimerExpiry(OperatingMode),
    Unrecognized(PayloadError),
    Unavailable,
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
    mode_readback: ModeReadback,
}

impl ControlOutcome {
    /// Interpret an acknowledgement and the corresponding snapshot readback.
    pub fn from_response(
        command: ControlCommand,
        frame: Frame<'static>,
        snapshot: Option<&DeviceSnapshot>,
    ) -> Result<Self, UnexpectedResponse> {
        validate_response(&frame, command.response_id())?;
        let acknowledgement = match frame.payload() {
            b"0" => Acknowledgement::Accepted,
            _ => Acknowledgement::Unrecognized,
        };
        let readback = match (command, snapshot) {
            (_, None) => ControlReadback::Unavailable,
            (ControlCommand::SetAutomaticThresholds(requested), Some(snapshot)) => {
                ControlReadback::Thresholds(
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
                )
            }
            (ControlCommand::SetTimer(requested), Some(snapshot)) => snapshot
                .timer
                .as_ref()
                .map_or(ControlReadback::Unavailable, |timer| {
                    ControlReadback::Timer(
                        timer
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
                    )
                }),
        };
        let mode_readback = snapshot.map_or(ModeReadback::Unavailable, |snapshot| {
            let device_mode = match snapshot.mode.decoded() {
                Ok(mode) => mode,
                Err(error) => return ModeReadback::Unrecognized(*error),
            };
            let mode = device_mode.mode;
            match command {
                ControlCommand::SetAutomaticThresholds(_) if mode == OperatingMode::Automatic => {
                    ModeReadback::Matches(mode)
                }
                ControlCommand::SetAutomaticThresholds(_) => ModeReadback::Differs(mode),
                ControlCommand::SetTimer(requested)
                    if requested.value() > 0
                        && (mode == OperatingMode::Timer || mode == OperatingMode::Automatic)
                        && snapshot.timer.as_ref().is_some_and(|timer| {
                            timer.decoded().is_ok_and(|timer| {
                                timer.original == requested && timer.remaining.value() == 0
                            })
                        }) =>
                {
                    ModeReadback::UnverifiedTimerExpiry(mode)
                }
                ControlCommand::SetTimer(requested)
                    if mode == OperatingMode::Timer
                        && device_mode.fan
                            == if requested.value() > 0 {
                                FanState::On
                            } else {
                                FanState::Off
                            } =>
                {
                    ModeReadback::Matches(mode)
                }
                ControlCommand::SetTimer(_) if mode == OperatingMode::Timer => {
                    ModeReadback::FanFlagDiffers {
                        mode,
                        actual: device_mode.fan,
                    }
                }
                ControlCommand::SetTimer(_) => ModeReadback::Differs(mode),
            }
        });
        Ok(Self {
            command,
            frame,
            acknowledgement,
            readback,
            mode_readback,
        })
    }

    /// Return the control command that was sent.
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

    /// Return the operating-mode observation and its relation to the command.
    #[must_use]
    pub const fn mode_readback(&self) -> ModeReadback {
        self.mode_readback
    }

    /// Whether the command was acknowledged and its setting matches the request.
    #[must_use]
    pub fn is_confirmed(&self) -> bool {
        if self.acknowledgement != Acknowledgement::Accepted {
            return false;
        }
        let setting_matches = match &self.readback {
            ControlReadback::Thresholds(Ok(readback)) => {
                readback.comparison == ReadbackMatch::Matches
            }
            ControlReadback::Timer(Ok(readback)) => readback.comparison == ReadbackMatch::Matches,
            ControlReadback::Thresholds(Err(_))
            | ControlReadback::Timer(Err(_))
            | ControlReadback::Unavailable => false,
        };
        setting_matches
            && match self.mode_readback {
                ModeReadback::Matches(_) => true,
                ModeReadback::Differs(_)
                | ModeReadback::FanFlagDiffers { .. }
                | ModeReadback::UnverifiedTimerExpiry(_)
                | ModeReadback::Unrecognized(_)
                | ModeReadback::Unavailable => false,
            }
    }
}
