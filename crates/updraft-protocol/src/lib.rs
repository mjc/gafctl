//! GAF attic fan protocol types and codecs.
//!
//! State query and ordinary control IDs and ASCII line framing are recovered
//! from the legacy `com.gaf.wifivent` app and its bundled firmware. Reply
//! payloads remain opaque unless their semantics are verified from app code or
//! device capture. Firmware update commands are not represented here.

use thiserror::Error;

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
    SetAutomaticThresholds {
        /// Target temperature in tenths of a degree Fahrenheit.
        temperature_tenths_f: u16,
        /// Humidity threshold in tenths of a percent.
        humidity_tenths_percent: u16,
    },
    /// Start timer mode for the specified number of minutes.
    SetTimer {
        /// Duration in minutes.
        duration_minutes: u16,
    },
}

impl ControlCommand {
    /// Encode the observed setter frame. This command does not update firmware.
    #[must_use]
    pub fn frame(self) -> Vec<u8> {
        match self {
            Self::SetAutomaticThresholds {
                temperature_tenths_f,
                humidity_tenths_percent,
            } => format!("#ams{temperature_tenths_f:04X}{humidity_tenths_percent:04X}\n")
                .into_bytes(),
            Self::SetTimer { duration_minutes } => {
                format!("#tms{duration_minutes:04X}\n").into_bytes()
            }
        }
    }

    /// The observed response identifier for this control command.
    #[must_use]
    pub const fn response_id(self) -> [u8; 3] {
        match self {
            Self::SetAutomaticThresholds { .. } => *b"amr",
            Self::SetTimer { .. } => *b"tmr",
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
