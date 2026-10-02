use crate::values::{AutomaticThresholds, Minutes};

/// A command that reads device state.
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

/// A command that changes automatic thresholds or timer settings.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ControlCommand {
    /// Select automatic mode and set temperature/humidity thresholds.
    SetAutomaticThresholds(AutomaticThresholds),
    /// Start timer mode for the specified number of minutes.
    SetTimer(Minutes),
}

impl ControlCommand {
    /// Encode the setter frame observed in the manufacturer app.
    #[must_use]
    pub fn frame(self) -> EncodedControlFrame {
        match self {
            Self::SetAutomaticThresholds(thresholds) => {
                let mut bytes = [0; 13];
                bytes[..4].copy_from_slice(b"#ams");
                encode_hex_word(&mut bytes, 4, thresholds.temperature.value());
                encode_hex_word(&mut bytes, 8, thresholds.humidity.value());
                bytes[12] = b'\n';
                EncodedControlFrame { bytes, len: 13 }
            }
            Self::SetTimer(minutes) => {
                let mut bytes = [0; 13];
                bytes[..4].copy_from_slice(b"#tms");
                encode_hex_word(&mut bytes, 4, minutes.value());
                bytes[8] = b'\n';
                EncodedControlFrame { bytes, len: 9 }
            }
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

/// A setter command encoded inline without a heap allocation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EncodedControlFrame {
    bytes: [u8; 13],
    len: usize,
}

impl EncodedControlFrame {
    /// Borrow the complete wire frame.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes[..self.len]
    }
}

impl AsRef<[u8]> for EncodedControlFrame {
    fn as_ref(&self) -> &[u8] {
        self.as_bytes()
    }
}

fn encode_hex_word(bytes: &mut [u8], offset: usize, value: u16) {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    [12, 8, 4, 0]
        .into_iter()
        .enumerate()
        .for_each(|(index, shift)| {
            bytes[offset + index] = HEX[((value >> shift) & 0x0f) as usize];
        });
}

/// A state read or fan-control write.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Request {
    /// Query one state field.
    Read(ReadCommand),
    /// Change a fan setting.
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
    /// Encode a request, borrowing fixed getter frames and storing setter frames inline.
    #[must_use]
    pub fn frame(self) -> RequestFrame {
        match self {
            Self::Read(command) => RequestFrame::Read(command),
            Self::Control(command) => RequestFrame::Control(command.frame()),
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

/// A request frame whose bytes are borrowed from a static getter or inline setter storage.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RequestFrame {
    /// A fixed read-only getter frame.
    Read(ReadCommand),
    /// A setting frame stored inline.
    Control(EncodedControlFrame),
}

impl RequestFrame {
    /// Borrow the complete wire frame.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        match self {
            Self::Read(command) => command.frame(),
            Self::Control(frame) => frame.as_bytes(),
        }
    }
}

impl AsRef<[u8]> for RequestFrame {
    fn as_ref(&self) -> &[u8] {
        self.as_bytes()
    }
}
