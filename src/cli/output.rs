use std::{fmt::Display, process::ExitCode};

use anyhow::Result;
use clap::ValueEnum;
use gafctl_api::CommandId;
use gafctl_client::ClientError;
use serde::Serialize;

#[derive(Clone, Copy, Debug, Default, ValueEnum)]
pub(super) enum OutputFormat {
    #[default]
    Text,
    Json,
}

impl OutputFormat {
    pub(super) fn write(&self, value: &impl Serialize, text: impl Display) -> Result<()> {
        match self {
            Self::Text => crate::stdout::write_stdout(|output| writeln!(output, "{text}")),
            Self::Json => {
                let mut bytes = serde_json::to_vec(value)?;
                bytes.push(b'\n');
                crate::stdout::write_stdout(|output| output.write_all(&bytes))
            }
        }
    }

    pub(super) fn error(&self, error: &ClientError) -> Result<ExitCode> {
        self.failure(
            error.kind(),
            error.to_string(),
            error.request_id(),
            error.http_status(),
        )
    }

    pub(super) fn failure(
        &self,
        kind: &'static str,
        message: String,
        request_id: Option<&CommandId>,
        http_status: Option<u16>,
    ) -> Result<ExitCode> {
        #[derive(Serialize)]
        struct ErrorDetails<'a> {
            kind: &'static str,
            message: String,
            request_id: Option<&'a CommandId>,
            http_status: Option<u16>,
        }
        #[derive(Serialize)]
        struct ErrorResponse<'a> {
            error: ErrorDetails<'a>,
        }
        match self {
            Self::Text => eprintln!("{message}"),
            Self::Json => self.write(
                &ErrorResponse {
                    error: ErrorDetails {
                        kind,
                        message,
                        request_id,
                        http_status,
                    },
                },
                "",
            )?,
        }
        Ok(ExitCode::FAILURE)
    }
}
