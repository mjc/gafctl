use thiserror::Error;

/// Temperature in tenths of a degree Fahrenheit, as carried on the wire.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct TemperatureTenthsF(u16);

impl TemperatureTenthsF {
    /// Preserve any four-digit wire value without imposing a device limit.
    #[must_use]
    pub const fn new(value: u16) -> Self {
        Self(value)
    }

    /// Return the value used by the command frame.
    #[must_use]
    pub const fn value(self) -> u16 {
        self.0
    }
}

/// Relative humidity in tenths of a percent, as carried on the wire.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct HumidityTenthsPercent(u16);

impl HumidityTenthsPercent {
    /// Preserve any four-digit wire value without imposing a device limit.
    #[must_use]
    pub const fn new(value: u16) -> Self {
        Self(value)
    }

    /// Return the value used by the command frame.
    #[must_use]
    pub const fn value(self) -> u16 {
        self.0
    }
}

/// Minutes in a timer command or readback, as carried on the wire.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct Minutes(u16);

impl Minutes {
    /// Preserve any four-digit wire value without imposing a device limit.
    #[must_use]
    pub const fn new(value: u16) -> Self {
        Self(value)
    }

    /// Return the value used by the command frame.
    #[must_use]
    pub const fn value(self) -> u16 {
        self.0
    }
}

/// The automatic-mode temperature and humidity thresholds.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AutomaticThresholds {
    /// Temperature threshold in tenths of a degree Fahrenheit.
    pub temperature: TemperatureTenthsF,
    /// Humidity threshold in tenths of a percent.
    pub humidity: HumidityTenthsPercent,
}

impl AutomaticThresholds {
    /// Parse the two four-digit hexadecimal fields of an `atr` reply.
    pub fn parse(payload: &[u8]) -> Result<Self, ReadbackError> {
        let (temperature, humidity) = parse_hex_words(payload)?;
        Ok(Self {
            temperature: TemperatureTenthsF::new(temperature),
            humidity: HumidityTenthsPercent::new(humidity),
        })
    }
}

/// Remaining and originally requested timer durations.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TimerState {
    /// Minutes left on the running timer.
    pub remaining: Minutes,
    /// Minutes originally requested for the timer.
    pub original: Minutes,
}

impl TimerState {
    /// Parse the two four-digit hexadecimal fields of a `ttr` reply.
    pub fn parse(payload: &[u8]) -> Result<Self, ReadbackError> {
        let (remaining, original) = parse_hex_words(payload)?;
        Ok(Self {
            remaining: Minutes::new(remaining),
            original: Minutes::new(original),
        })
    }

    /// Compare with the requested duration while allowing elapsed time.
    #[must_use]
    pub fn matches_requested_duration(self, requested: Minutes) -> bool {
        self.original == requested && self.remaining <= requested
    }
}

/// An automatic-threshold or timer readback with invalid wire fields.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum ReadbackError {
    /// The reply payload is shorter or longer than two four-digit fields.
    #[error("readback requires exactly eight ASCII hexadecimal bytes")]
    InvalidLength,
    /// At least one payload byte is outside `0-9`, `a-f`, and `A-F`.
    #[error("readback contains a non-hexadecimal byte")]
    InvalidHex,
}

pub(super) fn parse_hex_words(payload: &[u8]) -> Result<(u16, u16), ReadbackError> {
    let payload: &[u8; 8] = payload
        .try_into()
        .map_err(|_| ReadbackError::InvalidLength)?;
    let (first, second) = payload.split_at(4);
    Ok((parse_hex_word(first)?, parse_hex_word(second)?))
}

fn parse_hex_word(digits: &[u8]) -> Result<u16, ReadbackError> {
    std::str::from_utf8(digits)
        .ok()
        .filter(|digits| digits.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .and_then(|digits| u16::from_str_radix(digits, 16).ok())
        .ok_or(ReadbackError::InvalidHex)
}
