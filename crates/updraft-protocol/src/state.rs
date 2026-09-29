//! Typed interpretations of observed replies, kept beside their wire frames.

use thiserror::Error;

use crate::{
    AutomaticThresholds, Frame, HumidityTenthsPercent, ReadCommand, ReadbackError,
    TemperatureTenthsF, TimerState, values::parse_hex_words,
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

pub(super) fn validate_response(
    frame: &Frame<'_>,
    expected: [u8; 3],
) -> Result<(), UnexpectedResponse> {
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
