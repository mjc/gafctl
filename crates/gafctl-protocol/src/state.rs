//! Decoded replies and their original wire frames.

use std::time::{Duration, Instant, SystemTime};

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
    /// Decode the six decimal version digits at the start of an identity reply.
    pub fn from_payload(payload: &[u8]) -> Result<Self, PayloadError> {
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
    /// The controller's reported on/off flag. Airflow is not measured.
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
    /// Wall-clock timestamp for display and logs.
    pub observed_at: SystemTime,
    freshness_started_at: Instant,
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
        Self::from_frames_at(
            identity,
            mode,
            sensors,
            thresholds,
            timer,
            SystemTime::now(),
            Instant::now(),
        )
    }

    /// Pair replies with the time at which the complete set was observed.
    pub fn from_frames_at(
        identity: Frame<'static>,
        mode: Frame<'static>,
        sensors: Frame<'static>,
        thresholds: Frame<'static>,
        timer: Frame<'static>,
        observed_at: SystemTime,
        freshness_started_at: Instant,
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
            observed_at,
            freshness_started_at,
            identity: Observation::new(identity, Identity::from_payload),
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

    /// Whether this snapshot is no older than `max_age` at `now`.
    #[must_use]
    pub fn is_fresh_at(&self, now: Instant, max_age: Duration) -> bool {
        now.checked_duration_since(self.freshness_started_at)
            .is_some_and(|age| age <= max_age)
    }

    fn decoding_error(&self) -> Option<PayloadError> {
        self.identity
            .decoded()
            .err()
            .copied()
            .or_else(|| self.mode.decoded().err().copied())
            .or_else(|| self.sensors.decoded().err().copied())
            .or_else(|| self.thresholds.decoded().err().copied())
            .or_else(|| self.timer.decoded().err().copied())
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

/// Freshness of the most recently observed snapshot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StateFreshness {
    /// No successful poll has been recorded.
    Unknown,
    /// The last snapshot is within the configured age limit.
    Fresh,
    /// The last snapshot is older than the configured age limit.
    Stale,
}

/// Reconciles completed polls without replacing a newer snapshot with a late reply.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct StateReconciler {
    latest: Option<DeviceSnapshot>,
    last_error: Option<String>,
    next_poll_id: u64,
    latest_completed_poll: Option<u64>,
}

impl StateReconciler {
    /// Reserve an identifier before starting a poll. Pass it to either completion method.
    pub fn begin_poll(&mut self) -> u64 {
        let poll_id = self.next_poll_id;
        self.next_poll_id = self.next_poll_id.saturating_add(1);
        poll_id
    }

    /// Store a completed poll unless a later-started poll already completed.
    /// Returns false when this poll result arrived out of order.
    pub fn apply_success(&mut self, poll_id: u64, snapshot: DeviceSnapshot) -> bool {
        if self
            .latest_completed_poll
            .is_some_and(|latest| poll_id < latest)
        {
            return false;
        }
        self.latest_completed_poll = Some(poll_id);
        if let Some(error) = snapshot.decoding_error() {
            self.last_error = Some(format!("poll snapshot contains invalid payload: {error}"));
            return false;
        }
        self.latest = Some(snapshot);
        self.last_error = None;
        true
    }

    /// Record a poll failure while retaining the last observed snapshot and timestamp.
    /// Returns false when this older poll finished after a newer poll.
    pub fn apply_failure(&mut self, poll_id: u64, error: impl Into<String>) -> bool {
        if self
            .latest_completed_poll
            .is_some_and(|latest| poll_id < latest)
        {
            return false;
        }
        self.last_error = Some(error.into());
        self.latest_completed_poll = Some(poll_id);
        true
    }

    /// Return the last observed snapshot, even if it is stale.
    #[must_use]
    pub fn latest_snapshot(&self) -> Option<&DeviceSnapshot> {
        self.latest.as_ref()
    }

    /// Return the last snapshot only while it is fresh at `now`.
    #[must_use]
    pub fn current_snapshot_at(&self, now: Instant, max_age: Duration) -> Option<&DeviceSnapshot> {
        self.latest
            .as_ref()
            .filter(|snapshot| snapshot.is_fresh_at(now, max_age))
    }

    /// Report whether the last successful poll is fresh, stale, or absent.
    #[must_use]
    pub fn freshness_at(&self, now: Instant, max_age: Duration) -> StateFreshness {
        match self.latest.as_ref() {
            None => StateFreshness::Unknown,
            Some(snapshot) if snapshot.is_fresh_at(now, max_age) => StateFreshness::Fresh,
            Some(_) => StateFreshness::Stale,
        }
    }

    /// Return the latest poll error without replacing observed device state.
    #[must_use]
    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }
}
