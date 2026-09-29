use std::{future, pin::Pin, time::Duration};

use anyhow::{Context, Result};
use bytes::Bytes;
use futures_util::{Stream, StreamExt, TryStreamExt};
use updraft_protocol::{
    ControlCommand, ControlOutcome, DeviceSnapshot, Frame, FrameDecoder, FrameError, ReadCommand,
    Request,
};

use crate::{DisconnectOutcome, QueryResult, lifecycle::complete_before};

pub(super) trait GattTransport {
    async fn write(&mut self, bytes: &[u8]) -> Result<()>;
}

pub(super) struct RequestSession<T> {
    transport: T,
    notifications: Pin<Box<dyn Stream<Item = Vec<u8>> + Send>>,
    decoder: FrameDecoder,
    response_timeout: Duration,
}

impl<T: GattTransport> RequestSession<T> {
    pub(super) fn new(
        transport: T,
        notifications: Pin<Box<dyn Stream<Item = Vec<u8>> + Send>>,
        response_timeout: Duration,
    ) -> Self {
        Self {
            transport,
            notifications,
            decoder: FrameDecoder::default(),
            response_timeout,
        }
    }

    pub(super) async fn query(
        mut self,
        control_command: Option<ControlCommand>,
    ) -> Result<QueryResult> {
        let control_response = match control_command {
            Some(command) => Some((
                command,
                self.exchange(command.into())
                    .await
                    .context("wait for ordinary control acknowledgement")?,
            )),
            None => None,
        };
        let snapshot = self.read_state().await?;
        let control = control_response
            .map(|(command, response)| ControlOutcome::from_response(command, response, &snapshot))
            .transpose()
            .context("validate ordinary control outcome")?;
        Ok(QueryResult {
            snapshot,
            control,
            disconnect: DisconnectOutcome::Disconnected,
        })
    }

    async fn read_state(&mut self) -> Result<DeviceSnapshot> {
        let identity = self.exchange(ReadCommand::Identity.into()).await?;
        let mode = self.exchange(ReadCommand::Mode.into()).await?;
        let sensors = self.exchange(ReadCommand::Sensors.into()).await?;
        let thresholds = self.exchange(ReadCommand::AutoThresholds.into()).await?;
        let timer = self.exchange(ReadCommand::Timer.into()).await?;
        DeviceSnapshot::from_frames(identity, mode, sensors, thresholds, timer)
            .context("validate device snapshot")
    }

    async fn exchange(&mut self, request: Request) -> Result<Frame<'static>> {
        let frame = request.frame();
        complete_before(self.response_timeout, "write GAF BLE request", async {
            self.transport.write(frame.as_ref()).await.with_context(|| {
                format!(
                    "send {} {}",
                    request.operation(),
                    frame.as_ref().escape_ascii()
                )
            })
        })
        .await?;

        let decoder = &mut self.decoder;
        let mut matching_responses = self.notifications.by_ref().filter_map(|notification| {
            future::ready(
                decode_matching_response(decoder, notification.into(), request.response_id())
                    .transpose(),
            )
        });
        complete_before(
            self.response_timeout,
            "waiting for matching BLE response",
            async {
                matching_responses
                    .try_next()
                    .await
                    .map_err(anyhow::Error::from)
            },
        )
        .await
        .and_then(|response| response.context("BLE notification stream ended"))
        .with_context(|| {
            format!(
                "waiting for {} response to {}",
                request.response_id().escape_ascii(),
                frame.as_ref().escape_ascii()
            )
        })
    }
}

fn decode_matching_response(
    decoder: &mut FrameDecoder,
    bytes: Bytes,
    response_id: [u8; 3],
) -> Result<Option<Frame<'static>>, FrameError> {
    let mut matching = None;
    decoder.push(bytes, |frame| {
        if matching.is_none() && frame.command() == response_id {
            matching = Some(frame);
        }
    })?;
    Ok(matching)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_matching_response_survives_later_notifications() {
        let mut decoder = FrameDecoder::default();
        assert_eq!(
            decode_matching_response(&mut decoder, Bytes::from_static(b"#dm"), *b"dmr").unwrap(),
            None
        );
        let response =
            decode_matching_response(&mut decoder, Bytes::from_static(b"ran\n#dmraf\n"), *b"dmr")
                .unwrap()
                .unwrap();
        decode_matching_response(&mut decoder, Bytes::from_static(b"#atr"), *b"dmr").unwrap();
        assert_eq!(
            decode_matching_response(&mut decoder, Bytes::from_static(b"041a012c\n"), *b"dmr")
                .unwrap(),
            None
        );
        assert_eq!(response.as_bytes(), b"#dmran\n");
    }

    #[test]
    fn malformed_trailing_frame_invalidates_matching_response() {
        let error = decode_matching_response(
            &mut FrameDecoder::default(),
            Bytes::from_static(b"#dmran\nx\n"),
            *b"dmr",
        )
        .unwrap_err();
        assert_eq!(error, FrameError::InvalidStart);
    }

    #[test]
    fn matching_response_shares_notification_storage() {
        let notification = b"#amr0\n#dmran\n".to_vec();
        let response_pointer = notification[6..].as_ptr();
        let response =
            decode_matching_response(&mut FrameDecoder::default(), notification.into(), *b"dmr")
                .unwrap()
                .unwrap();

        assert_eq!(response.as_bytes(), b"#dmran\n");
        assert_eq!(response.as_bytes().as_ptr(), response_pointer);
    }
}
