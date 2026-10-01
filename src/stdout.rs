use std::io::{self, Write};

#[derive(Debug, thiserror::Error)]
#[error("write stdout: {0}")]
pub(crate) struct StdoutError(#[source] io::Error);

impl StdoutError {
    pub(crate) fn is_broken_pipe(&self) -> bool {
        self.0.kind() == io::ErrorKind::BrokenPipe
    }
}

pub(crate) fn write_stdout(
    write: impl FnOnce(&mut dyn Write) -> io::Result<()>,
) -> anyhow::Result<()> {
    write(&mut io::stdout().lock()).map_err(|error| StdoutError(error).into())
}
