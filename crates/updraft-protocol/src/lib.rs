//! GAF attic fan protocol types and codecs.
//!
//! State query and ordinary control IDs and ASCII line framing are recovered
//! from the legacy `com.gaf.wifivent` app and its bundled firmware. Reply
//! payloads remain opaque unless their semantics are verified from app code or
//! device capture. Firmware update commands are not represented here.

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

fn parse_hex_words(payload: &[u8]) -> Result<(u16, u16), ReadbackError> {
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

/// A command that reads device state and does not intentionally change it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReadCommand {
    /// Read the device identity.
    Identity,
    /// Read the active device mode.
    Mode,
    /// Read sensor values.
    Sensors,
    /// Read automatic-mode thresholds.
    AutoThresholds,
    /// Read timer state.
    Timer,
}

impl ReadCommand {
    /// The observed request line for this read-only command.
    #[must_use]
    pub const fn frame(self) -> &'static [u8] {
        match self {
            Self::Identity => b"#idg\n",
            Self::Mode => b"#dmg\n",
            Self::Sensors => b"#sdg\n",
            Self::AutoThresholds => b"#atg\n",
            Self::Timer => b"#ttg\n",
        }
    }

    /// The observed response command identifier for this request.
    #[must_use]
    pub const fn response_id(self) -> [u8; 3] {
        match self {
            Self::Identity => *b"idr",
            Self::Mode => *b"dmr",
            Self::Sensors => *b"sdr",
            Self::AutoThresholds => *b"atr",
            Self::Timer => *b"ttr",
        }
    }
}

/// A non-firmware command that changes ordinary fan-control settings.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ControlCommand {
    /// Select automatic mode and set temperature/humidity thresholds.
    SetAutomaticThresholds(AutomaticThresholds),
    /// Start timer mode for the specified number of minutes.
    SetTimer(Minutes),
}

impl ControlCommand {
    /// Encode the observed setter frame. This command does not update firmware.
    #[must_use]
    pub fn frame(self) -> Vec<u8> {
        match self {
            Self::SetAutomaticThresholds(thresholds) => {
                let temperature = thresholds.temperature.value();
                let humidity = thresholds.humidity.value();
                format!("#ams{temperature:04X}{humidity:04X}\n").into_bytes()
            }
            Self::SetTimer(minutes) => format!("#tms{:04X}\n", minutes.value()).into_bytes(),
        }
    }

    /// The observed response identifier for this control command.
    #[must_use]
    pub const fn response_id(self) -> [u8; 3] {
        match self {
            Self::SetAutomaticThresholds(_) => *b"amr",
            Self::SetTimer(_) => *b"tmr",
        }
    }
}

/// One complete `#<three-byte-id><payload>\n` protocol line.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Frame {
    command: [u8; 3],
    payload: Vec<u8>,
}

impl Frame {
    /// Parse one complete line, retaining the payload without interpretation.
    pub fn parse(bytes: &[u8]) -> Result<Self, FrameError> {
        let body = bytes.strip_prefix(b"#").ok_or(FrameError::InvalidStart)?;
        let body = body
            .strip_suffix(b"\n")
            .ok_or(FrameError::MissingLineFeed)?;

        match (body.contains(&b'\n'), body.split_at_checked(3)) {
            (true, _) => Err(FrameError::TrailingData),
            (false, Some((&[a, b, c], payload)))
                if [a, b, c].iter().all(u8::is_ascii_alphabetic) =>
            {
                Ok(Self {
                    command: [a, b, c],
                    payload: payload.to_vec(),
                })
            }
            _ => Err(FrameError::InvalidCommand),
        }
    }

    /// Return the three-byte command identifier.
    #[must_use]
    pub const fn command(&self) -> [u8; 3] {
        self.command
    }

    /// Return the unparsed bytes between the command identifier and line feed.
    #[must_use]
    pub fn payload(&self) -> &[u8] {
        &self.payload
    }

    /// Encode this frame with the observed prefix and line terminator.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(self.payload.len() + 5);
        bytes.push(b'#');
        bytes.extend_from_slice(&self.command);
        bytes.extend_from_slice(&self.payload);
        bytes.push(b'\n');
        bytes
    }
}

/// Incrementally separates complete LF-terminated frames from transport data.
#[derive(Debug, Default)]
pub struct FrameDecoder {
    pending: Vec<u8>,
}

impl FrameDecoder {
    /// Add transport bytes and return every complete frame now available.
    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<Frame>, FrameError> {
        const MAX_FRAME_LEN: usize = 1024;

        let frames = bytes.split_inclusive(|byte| *byte == b'\n').try_fold(
            Vec::new(),
            |mut frames, chunk| {
                self.pending.extend_from_slice(chunk);
                match (self.pending.len() > MAX_FRAME_LEN, chunk.ends_with(b"\n")) {
                    (true, _) => Err(FrameError::TooLong),
                    (false, true) => {
                        frames.push(Frame::parse(&self.pending)?);
                        self.pending.clear();
                        Ok(frames)
                    }
                    (false, false) => Ok(frames),
                }
            },
        );

        match frames {
            Ok(frames) => Ok(frames),
            Err(error) => {
                self.pending.clear();
                Err(error)
            }
        }
    }
}

/// A malformed or incomplete GAF text frame.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum FrameError {
    /// The frame does not begin with `#`.
    #[error("frame must begin with '#'")]
    InvalidStart,
    /// The complete frame does not end with LF.
    #[error("frame is missing its line feed")]
    MissingLineFeed,
    /// The input contains more than one line/frame.
    #[error("input contains trailing frame data")]
    TrailingData,
    /// The frame does not contain a three-letter command identifier.
    #[error("frame command must contain three ASCII letters")]
    InvalidCommand,
    /// An incomplete frame exceeded the decoder's maximum buffered length.
    #[error("incomplete frame exceeds 1024 bytes")]
    TooLong,
}
