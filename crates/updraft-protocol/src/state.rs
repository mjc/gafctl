//! Typed interpretations of observed replies, kept beside their wire frames.

use thiserror::Error;

use crate::{
    AutomaticThresholds, ControlCommand, Frame, HumidityTenthsPercent, ReadCommand, ReadbackError,
    TemperatureTenthsF, TimerState, parse_hex_words,
};

/// The three decimal version components at the start of an identity reply.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FirmwareVersion {
    /// The first two decimal digits.
    pub major: u8,
    /// The next two decimal digits.
    pub minor: u8,
    /// The final two decimal digits.
    pub patch: u8,
}

/// Decoded public identity information. The identity suffix stays in the frame.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Identity {
    /// The version prefix; the remaining identity stays only in the raw frame.
    pub firmware_version: FirmwareVersion,
}

impl Identity {
    fn parse(payload: &[u8]) -> Result<Self, PayloadError> {
        match payload {
            [a, b, c, d, e, f, ..] if [a, b, c, d, e, f].into_iter().all(u8::is_ascii_digit) => {
                let pair = |tens: u8, units: u8| (tens - b'0') * 10 + units - b'0';
                Ok(Self {
                    firmware_version: FirmwareVersion {
                        major: pair(*a, *b),
                        minor: pair(*c, *d),
                        patch: pair(*e, *f),
                    },
                })
            }
            _ => Err(PayloadError::InvalidIdentity),
        }
    }
}

/// Controller operating mode reported by `dmr`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OperatingMode {
    Automatic,
    Timer,
    Ota,
}

impl OperatingMode {
    fn parse(byte: u8) -> Option<Self> {
        match byte {
            b'a' => Some(Self::Automatic),
            b't' => Some(Self::Timer),
            b'o' => Some(Self::Ota),
            _ => None,
        }
    }
}

/// Controller fan flag reported by `dmr`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FanState {
    Off,
    On,
}

impl FanState {
    fn parse(byte: u8) -> Option<Self> {
        match byte {
            b'f' => Some(Self::Off),
            b'n' => Some(Self::On),
            _ => None,
        }
    }
}

/// Decoded controller mode and fan flag.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DeviceMode {
    /// Automatic, timer, or reported OTA mode.
    pub mode: OperatingMode,
    /// The controller's reported fan flag, not a physical airflow measurement.
    pub fan: FanState,
}

impl DeviceMode {
    fn parse(payload: &[u8]) -> Result<Self, PayloadError> {
        match payload {
            &[mode, fan, ..] => OperatingMode::parse(mode)
                .zip(FanState::parse(fan))
                .map(|(mode, fan)| Self { mode, fan })
                .ok_or(PayloadError::InvalidMode),
            _ => Err(PayloadError::InvalidMode),
        }
    }
}

/// Temperature and relative humidity reported by `sdr`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SensorReadings {
    /// Temperature in tenths of a degree Fahrenheit.
    pub temperature: TemperatureTenthsF,
    /// Relative humidity in tenths of a percent.
    pub humidity: HumidityTenthsPercent,
}

impl SensorReadings {
    fn parse(payload: &[u8]) -> Result<Self, PayloadError> {
        let (temperature, humidity) = parse_hex_words(payload)?;
        Ok(Self {
            temperature: TemperatureTenthsF::new(temperature),
            humidity: HumidityTenthsPercent::new(humidity),
        })
    }
}

/// A reply payload whose meaning could not be decoded.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum PayloadError {
    /// The identity version prefix is missing or nondecimal.
    #[error("identity does not begin with six decimal version digits")]
    InvalidIdentity,
    /// The mode or fan flag is missing or unrecognized.
    #[error("mode does not begin with a known mode and fan flag")]
    InvalidMode,
    /// A two-word hexadecimal payload is malformed.
    #[error(transparent)]
    Readback(#[from] ReadbackError),
}

/// One validated response frame and its decoded interpretation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Observation<T> {
    frame: Frame<'static>,
    decoded: Result<T, PayloadError>,
}

impl<T> Observation<T> {
    fn new(frame: Frame<'static>, parse: impl FnOnce(&[u8]) -> Result<T, PayloadError>) -> Self {
        let decoded = parse(frame.payload());
        Self { frame, decoded }
    }

    /// Return the complete original response frame.
    #[must_use]
    pub fn frame(&self) -> &Frame<'static> {
        &self.frame
    }

    /// Return the decoded value or its error without losing the raw frame.
    pub fn decoded(&self) -> Result<&T, &PayloadError> {
        self.decoded.as_ref()
    }
}

/// A received reply with a command identifier other than the one requested.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[error("expected response {expected:?}, received {actual:?}")]
pub struct UnexpectedResponse {
    /// Identifier expected for the request.
    pub expected: [u8; 3],
    /// Identifier carried by the received frame.
    pub actual: [u8; 3],
}

fn validate_response(frame: &Frame<'_>, expected: [u8; 3]) -> Result<(), UnexpectedResponse> {
    match frame.command() {
        actual if actual == expected => Ok(()),
        actual => Err(UnexpectedResponse { expected, actual }),
    }
}

/// One complete set of state replies, retaining every original frame.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeviceSnapshot {
    /// Identity and firmware version prefix.
    pub identity: Observation<Identity>,
    /// Controller mode and fan flag.
    pub mode: Observation<DeviceMode>,
    /// Current temperature and humidity.
    pub sensors: Observation<SensorReadings>,
    /// Automatic mode thresholds.
    pub thresholds: Observation<AutomaticThresholds>,
    /// Timer durations.
    pub timer: Observation<TimerState>,
}

impl DeviceSnapshot {
    /// Pair the five known response IDs with raw and decoded observations.
    /// Invalid payloads remain available through their observation frames.
    pub fn from_frames(
        identity: Frame<'static>,
        mode: Frame<'static>,
        sensors: Frame<'static>,
        thresholds: Frame<'static>,
        timer: Frame<'static>,
    ) -> Result<Self, UnexpectedResponse> {
        [
            (ReadCommand::Identity, &identity),
            (ReadCommand::Mode, &mode),
            (ReadCommand::Sensors, &sensors),
            (ReadCommand::AutoThresholds, &thresholds),
            (ReadCommand::Timer, &timer),
        ]
        .into_iter()
        .try_for_each(|(request, frame)| validate_response(frame, request.response_id()))?;

        Ok(Self {
            identity: Observation::new(identity, Identity::parse),
            mode: Observation::new(mode, DeviceMode::parse),
            sensors: Observation::new(sensors, SensorReadings::parse),
            thresholds: Observation::new(thresholds, |payload| {
                AutomaticThresholds::parse(payload).map_err(PayloadError::from)
            }),
            timer: Observation::new(timer, |payload| {
                TimerState::parse(payload).map_err(PayloadError::from)
            }),
        })
    }

    /// Iterate through the five retained replies in request order.
    pub fn frames(&self) -> impl Iterator<Item = (ReadCommand, &Frame<'static>)> {
        [
            (ReadCommand::Identity, self.identity.frame()),
            (ReadCommand::Mode, self.mode.frame()),
            (ReadCommand::Sensors, self.sensors.frame()),
            (ReadCommand::AutoThresholds, self.thresholds.frame()),
            (ReadCommand::Timer, self.timer.frame()),
        ]
        .into_iter()
    }
}

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
