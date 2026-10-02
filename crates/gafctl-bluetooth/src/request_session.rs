use std::{future, pin::Pin, time::Duration};

use anyhow::{Context, Result};
use bytes::Bytes;
use futures_util::{Stream, StreamExt, TryStreamExt};
use gafctl_protocol::{
    ControlCommand, ControlOutcome, DeviceSnapshot, Frame, FrameDecoder, FrameError, ReadCommand,
    Request,
};

use crate::{
    DisconnectOutcome, QueryResult, error::InvalidIdentityResponse, lifecycle::complete_before,
};

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
        let identity = self.exchange(ReadCommand::Identity.into()).await?;
        if let Some(command) = control_command {
            validate_gaf_identity(&identity)?;
            let response = self
                .exchange(command.into())
                .await
                .context("wait for ordinary control acknowledgement")?;
            return match self.read_state(identity).await {
                Ok(snapshot) => Ok(QueryResult {
                    control: Some(
                        ControlOutcome::from_response(command, response, Some(&snapshot))
                            .context("validate ordinary control outcome")?,
                    ),
                    snapshot: Some(snapshot),
                    state_error: None,
                    discovery_failures: Vec::new(),
                    disconnect: DisconnectOutcome::Disconnected,
                }),
                Err(error) => Ok(QueryResult {
                    control: Some(
                        ControlOutcome::from_response(command, response, None)
                            .context("retain ordinary control acknowledgement")?,
                    ),
                    snapshot: None,
                    state_error: Some(format!("{error:#}")),
                    discovery_failures: Vec::new(),
                    disconnect: DisconnectOutcome::Disconnected,
                }),
            };
        }

        let snapshot = self.read_state(identity).await?;
        Ok(QueryResult {
            snapshot: Some(snapshot),
            state_error: None,
            discovery_failures: Vec::new(),
            control: None,
            disconnect: DisconnectOutcome::Disconnected,
        })
    }

    async fn read_state(&mut self, identity: Frame<'static>) -> Result<DeviceSnapshot> {
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

fn validate_gaf_identity(identity: &Frame<'_>) -> Result<()> {
    if identity.command() != ReadCommand::Identity.response_id() {
        anyhow::bail!(InvalidIdentityResponse);
    }
    gafctl_protocol::Identity::from_payload(identity.payload())
        .map(|_| ())
        .map_err(anyhow::Error::new)
        .context("invalid GAF identity payload")
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
    use std::sync::{Arc, Mutex};

    use futures_util::stream;
    use gafctl_protocol::{ControlCommand, Minutes};

    #[derive(Clone, Default)]
    struct RecordingTransport(Arc<Mutex<Vec<Vec<u8>>>>);

    impl GattTransport for RecordingTransport {
        async fn write(&mut self, bytes: &[u8]) -> Result<()> {
            self.0.lock().unwrap().push(bytes.to_vec());
            Ok(())
        }
    }

    fn session(
        responses: &'static [&'static [u8]],
    ) -> (RequestSession<RecordingTransport>, RecordingTransport) {
        let transport = RecordingTransport::default();
        let session = RequestSession::new(
            transport.clone(),
            Box::pin(stream::iter(responses.iter().map(|bytes| bytes.to_vec()))),
            Duration::from_secs(1),
        );
        (session, transport)
    }

    #[tokio::test]
    async fn acknowledged_timer_is_retained_when_later_state_read_fails() {
        let (session, transport) = session(&[b"#idr030000x\n", b"#tmr0\n"]);
        let result = session
            .query(Some(ControlCommand::SetTimer(Minutes::new(1))))
            .await
            .unwrap();

        let control = result.control.unwrap();
        assert_eq!(control.frame().as_bytes(), b"#tmr0\n");
        assert_eq!(
            control.acknowledgement(),
            gafctl_protocol::Acknowledgement::Accepted
        );
        assert_eq!(
            control.readback(),
            &gafctl_protocol::ControlReadback::Unavailable
        );
        assert!(result.snapshot.is_none());
        assert!(
            result
                .state_error
                .as_deref()
                .is_some_and(|error| error.contains("BLE notification stream ended"))
        );
        assert_eq!(
            transport.0.lock().unwrap().as_slice(),
            &[
                b"#idg\n".to_vec(),
                b"#tms0001\n".to_vec(),
                b"#dmg\n".to_vec()
            ]
        );
    }

    #[tokio::test]
    async fn invalid_identity_prevents_control_write() {
        let (session, transport) = session(&[b"#idrnot-gaf\n"]);
        assert!(
            session
                .query(Some(ControlCommand::SetTimer(Minutes::new(1))))
                .await
                .is_err()
        );
        assert_eq!(
            transport.0.lock().unwrap().as_slice(),
            &[b"#idg\n".to_vec()]
        );
    }

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
