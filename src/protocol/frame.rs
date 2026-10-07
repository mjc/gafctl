use bytes::{Bytes, BytesMut};
use thiserror::Error;

const MAX_FRAME_LEN: usize = 1024;

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
        if bytes.len() > MAX_FRAME_LEN {
            return Err(FrameError::TooLong);
        }

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
    /// Whether a notification left an incomplete frame awaiting its remaining bytes.
    #[must_use]
    pub fn has_partial_frame(&self) -> bool {
        !self.pending.is_empty()
    }

    /// Visit each complete frame without allocating an output collection.
    ///
    /// A callback may have run before a later malformed frame returns an error.
    /// Frames share transport storage and may be retained after the callback.
    pub fn push(
        &mut self,
        bytes: Bytes,
        mut visit: impl FnMut(Frame<'static>),
    ) -> Result<(), FrameError> {
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
    /// A complete or incomplete frame exceeded the maximum length.
    #[error("frame exceeds 1024 bytes")]
    TooLong,
}
