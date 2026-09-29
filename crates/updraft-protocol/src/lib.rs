//! GAF attic fan protocol types and codecs.
//!
//! State query and ordinary control IDs and ASCII line framing are recovered
//! from the legacy `com.gaf.wifivent` app and its bundled firmware. Reply
//! payloads remain opaque unless their semantics are verified from app code or
//! device capture. Firmware update commands are not represented here.

use std::borrow::Cow;

use bytes::{Bytes, BytesMut};
use thiserror::Error;

mod state;

pub use state::{
    Acknowledgement, ControlOutcome, ControlReadback, DeviceMode, DeviceSnapshot, FanState,
    FirmwareVersion, Identity, Observation, OperatingMode, PayloadError, Readback, ReadbackMatch,
    SensorReadings, UnexpectedResponse,
};

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

/// A state read or an ordinary fan-control write.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Request {
    /// Query one state field.
    Read(ReadCommand),
    /// Change an ordinary fan setting.
    Control(ControlCommand),
}

impl From<ReadCommand> for Request {
    fn from(command: ReadCommand) -> Self {
        Self::Read(command)
    }
}

impl From<ControlCommand> for Request {
    fn from(command: ControlCommand) -> Self {
        Self::Control(command)
    }
}

impl Request {
    /// Encode a request, borrowing fixed getter frames and owning setter frames.
    #[must_use]
    pub fn frame(self) -> Cow<'static, [u8]> {
        match self {
            Self::Read(command) => Cow::Borrowed(command.frame()),
            Self::Control(command) => Cow::Owned(command.frame()),
        }
    }

    /// Return the response identifier expected for this request.
    #[must_use]
    pub const fn response_id(self) -> [u8; 3] {
        match self {
            Self::Read(command) => command.response_id(),
            Self::Control(command) => command.response_id(),
        }
    }

    /// Name the operation for transport diagnostics.
    #[must_use]
    pub const fn operation(self) -> &'static str {
        match self {
            Self::Read(_) => "state query",
            Self::Control(_) => "ordinary control command",
        }
    }
}

/// One validated `#<three-byte-id><payload>\n` protocol line.
///
/// Parsing a slice borrows it; decoding transport [`Bytes`] shares its storage.
/// Call [`Self::into_owned`] to retain a frame parsed from a borrowed slice.
#[derive(Clone, Debug)]
pub struct Frame<'a> {
    wire: FrameBytes<'a>,
}

#[derive(Clone, Debug)]
enum FrameBytes<'a> {
    Borrowed(&'a [u8]),
    Shared(Bytes),
}

impl<'a, 'b> PartialEq<Frame<'b>> for Frame<'a> {
    fn eq(&self, other: &Frame<'b>) -> bool {
        self.as_bytes() == other.as_bytes()
    }
}

impl Eq for Frame<'_> {}

impl<'a> Frame<'a> {
    /// Validate one complete line without copying its bytes.
    pub fn parse(bytes: &'a [u8]) -> Result<Self, FrameError> {
        let body = bytes.strip_prefix(b"#").ok_or(FrameError::InvalidStart)?;
        let body = body
            .strip_suffix(b"\n")
            .ok_or(FrameError::MissingLineFeed)?;

        match (body.contains(&b'\n'), body.split_at_checked(3)) {
            (true, _) => Err(FrameError::TrailingData),
            (false, Some((&[a, b, c], _))) if [a, b, c].iter().all(u8::is_ascii_alphabetic) => {
                Ok(Self {
                    wire: FrameBytes::Borrowed(bytes),
                })
            }
            _ => Err(FrameError::InvalidCommand),
        }
    }

    /// Return the three-byte command identifier.
    #[must_use]
    pub fn command(&self) -> [u8; 3] {
        let wire = self.as_bytes();
        [wire[1], wire[2], wire[3]]
    }

    /// Return the unparsed bytes between the command identifier and line feed.
    #[must_use]
    pub fn payload(&self) -> &[u8] {
        let wire = self.as_bytes();
        &wire[4..wire.len() - 1]
    }

    /// Return the complete, original wire bytes without allocating.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        match &self.wire {
            FrameBytes::Borrowed(bytes) => bytes,
            FrameBytes::Shared(bytes) => bytes,
        }
    }

    /// Copy borrowed wire bytes only when the response must be retained.
    #[must_use]
    pub fn into_owned(self) -> Frame<'static> {
        let wire = match self.wire {
            FrameBytes::Borrowed(bytes) => FrameBytes::Shared(Bytes::copy_from_slice(bytes)),
            FrameBytes::Shared(bytes) => FrameBytes::Shared(bytes),
        };
        Frame { wire }
    }
}

impl Frame<'static> {
    /// Validate and retain complete wire bytes without copying their storage.
    pub fn from_bytes(bytes: Bytes) -> Result<Self, FrameError> {
        Frame::parse(bytes.as_ref())?;
        Ok(Self {
            wire: FrameBytes::Shared(bytes),
        })
    }
}

/// Incrementally separates complete LF-terminated frames from transport data.
#[derive(Debug, Default)]
pub struct FrameDecoder {
    pending: BytesMut,
}

impl FrameDecoder {
    /// Visit each complete frame without allocating an output collection.
    ///
    /// A callback may have run before a later malformed frame returns an error.
    /// Frames share transport storage and may be retained after the callback.
    pub fn push(
        &mut self,
        bytes: Bytes,
        mut visit: impl FnMut(Frame<'static>),
    ) -> Result<(), FrameError> {
        const MAX_FRAME_LEN: usize = 1024;

        let result = bytes
            .as_ref()
            .split_inclusive(|byte| *byte == b'\n')
            .try_for_each(|chunk| {
                match (
                    chunk.len() > MAX_FRAME_LEN.saturating_sub(self.pending.len()),
                    chunk.ends_with(b"\n"),
                    self.pending.is_empty(),
                ) {
                    (true, _, _) => Err(FrameError::TooLong),
                    (false, true, true) => {
                        visit(Frame::from_bytes(bytes.slice_ref(chunk))?);
                        Ok(())
                    }
                    (false, true, false) => {
                        self.pending.extend_from_slice(chunk);
                        visit(Frame::from_bytes(self.pending.split().freeze())?);
                        Ok(())
                    }
                    (false, false, _) => {
                        self.pending.extend_from_slice(chunk);
                        Ok(())
                    }
                }
            });

        match result {
            Ok(()) => Ok(()),
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
