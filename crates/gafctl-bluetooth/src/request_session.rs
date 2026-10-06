use std::{
    future,
    time::{Duration, Instant, SystemTime},
};

use anyhow::{Context, Result};
use bytes::Bytes;
use futures_util::{FutureExt, Stream, StreamExt, TryStreamExt};
use gafctl_protocol::{
    ControlCommand, ControlOutcome, DeviceSnapshot, Frame, FrameDecoder, ReadCommand, Request,
};

use crate::{
    DisconnectOutcome, QueryResult, error::InvalidIdentityResponse, lifecycle::complete_before,
};

pub(crate) trait GattTransport {
    async fn write(&mut self, bytes: &[u8]) -> Result<()>;
}

pub(crate) struct RequestSession<T, S> {
    transport: T,
    notifications: S,
    decoder: FrameDecoder,
    response_timeout: Duration,
    snapshot: Option<DeviceSnapshot>,
}

impl<T, S> RequestSession<T, S>
where
    T: GattTransport,
    S: Stream<Item = Vec<u8>> + Send + Unpin,
{
    pub(crate) fn new(transport: T, notifications: S, response_timeout: Duration) -> Self {
        Self {
            transport,
            notifications,
            decoder: FrameDecoder::default(),
            response_timeout,
            snapshot: None,
        }
    }

    pub(crate) fn is_initialized(&self) -> bool {
        self.snapshot.is_some()
    }

    pub(crate) fn set_response_timeout(&mut self, timeout: Duration) {
        self.response_timeout = timeout;
    }

    pub(crate) async fn query(
        &mut self,
        control_command: Option<ControlCommand>,
        control_deadline: Option<tokio::time::Instant>,
    ) -> Result<QueryResult> {
        if control_command.is_some() {
            check_control_deadline(control_deadline)?;
        }
        let identity = self.identity().await?;
        if let Some(command) = control_command {
            self.query_control(identity, command, control_deadline)
                .await
        } else {
            let snapshot = self.query_state(identity).await?;
            let state_error = snapshot.decoding_error().map(|error| error.to_string());
            Ok(QueryResult {
                snapshot: Some(snapshot),
                state_error,
                discovery_failures: Vec::new(),
                control: None,
                disconnect: DisconnectOutcome::Retained,
            })
        }
    }

    async fn identity(&mut self) -> Result<Frame<'static>> {
        match &self.snapshot {
            Some(snapshot) => Ok(snapshot.identity.frame().clone()),
            None => self.exchange(ReadCommand::Identity.into(), None).await,
        }
    }

    async fn query_control(
        &mut self,
        identity: Frame<'static>,
        command: ControlCommand,
        deadline: Option<tokio::time::Instant>,
    ) -> Result<QueryResult> {
        validate_gaf_identity(&identity)?;
        check_control_deadline(deadline)?;
        if self.snapshot.is_none() {
            let initialized = self.query_state(identity.clone()).await?;
            if let Some(error) = initialized.decoding_error() {
                return Err(error.into());
            }
        }
        check_control_deadline(deadline)?;
        let response = self
            .exchange(command.into(), deadline)
            .await
            .context("wait for ordinary control acknowledgement")?;
        let (snapshot, state_error) = match self.read_state(identity, true).await {
            Ok(snapshot) => {
                let error = snapshot.decoding_error().map(|error| error.to_string());
                (Some(snapshot), error)
            }
            Err(error) => (None, Some(format!("{error:#}"))),
        };
        let control = ControlOutcome::from_response(command, response, snapshot.as_ref())
            .context("validate ordinary control outcome")?;
        self.snapshot = snapshot
            .as_ref()
            .filter(|snapshot| snapshot.decoding_error().is_none())
            .cloned();
        Ok(QueryResult {
            snapshot,
            state_error,
            discovery_failures: Vec::new(),
            control: Some(control),
            disconnect: DisconnectOutcome::Retained,
        })
    }

    async fn query_state(&mut self, identity: Frame<'static>) -> Result<DeviceSnapshot> {
        let snapshot = self.read_state(identity, false).await?;
        self.snapshot = snapshot
            .decoding_error()
            .is_none()
            .then(|| snapshot.clone());
        Ok(snapshot)
    }

    async fn read_state(
        &mut self,
        identity: Frame<'static>,
        read_timer: bool,
    ) -> Result<DeviceSnapshot> {
        let sensors = self.exchange(ReadCommand::Sensors.into(), None).await?;
        let observed_at = SystemTime::now();
        let freshness_started_at = Instant::now();
        let thresholds = self
            .exchange(ReadCommand::AutoThresholds.into(), None)
            .await?;
        let mode = self.exchange(ReadCommand::Mode.into(), None).await?;
        let timer = if read_timer || mode.payload().starts_with(b"tn") {
            Some(self.exchange(ReadCommand::Timer.into(), None).await?)
        } else {
            None
        };
        let snapshot = DeviceSnapshot::from_frames_at(
            identity,
            mode,
            sensors,
            thresholds,
            timer,
            observed_at,
            freshness_started_at,
        )
        .context("validate device snapshot")?;
        Ok(snapshot)
    }

    async fn drain_idle_notifications(&mut self) -> Result<()> {
        complete_before(self.response_timeout, "drain queued BLE responses", async {
            for _ in 0..64 {
                let notification = match self.notifications.next().now_or_never() {
                    Some(notification) => notification.context("BLE notification stream ended")?,
                    None if self.decoder.has_partial_frame() => self
                        .notifications
                        .next()
                        .await
                        .context("BLE notification stream ended")?,
                    None => return Ok(()),
                };
                decode_matching_response(
                    &mut self.decoder,
                    notification.into(),
                    *b"---",
                    self.snapshot.as_mut(),
                )?;
            }
            anyhow::bail!("BLE notification stream flooded between requests")
        })
        .await
    }

    async fn exchange(
        &mut self,
        request: Request,
        deadline: Option<tokio::time::Instant>,
    ) -> Result<Frame<'static>> {
        self.drain_idle_notifications().await?;
        check_control_deadline(deadline)?;
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
        let snapshot = &mut self.snapshot;
        let mut matching_responses = self.notifications.by_ref().filter_map(|notification| {
            future::ready(
                decode_matching_response(
                    decoder,
                    notification.into(),
                    request.response_id(),
                    snapshot.as_mut(),
                )
                .transpose(),
            )
        });
        complete_before(
            self.response_timeout,
            "waiting for matching BLE response",
            async { matching_responses.try_next().await },
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

fn check_control_deadline(deadline: Option<tokio::time::Instant>) -> Result<()> {
    if deadline.is_some_and(|deadline| tokio::time::Instant::now() >= deadline) {
        anyhow::bail!(crate::error::ControlExpired);
    }
    Ok(())
}

fn validate_gaf_identity(identity: &Frame<'_>) -> Result<()> {
    if identity.command() != ReadCommand::Identity.response_id() {
        anyhow::bail!(InvalidIdentityResponse);
    }
    let identity = gafctl_protocol::Identity::from_payload(identity.payload())
        .context("invalid GAF identity payload")?;
    if identity.firmware_version.major < 2 {
        anyhow::bail!(crate::error::UnsupportedControlFirmware);
    }
    Ok(())
}

fn decode_matching_response(
    decoder: &mut FrameDecoder,
    bytes: Bytes,
    response_id: [u8; 3],
    mut snapshot: Option<&mut DeviceSnapshot>,
) -> Result<Option<Frame<'static>>> {
    let mut matching = None;
    let mut payload_error = None;
    decoder.push(bytes, |frame| {
        if frame.command() == response_id {
            if matching.is_none() {
                matching = Some(frame);
            }
        } else if let Some(snapshot) = snapshot.as_deref_mut()
            && let Err(error) = snapshot.observe_frame(frame)
        {
            payload_error = Some(error);
        }
    })?;
    match (matching, payload_error) {
        (Some(response), _) => Ok(Some(response)),
        (None, Some(error)) => Err(error.into()),
        (None, None) => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    use futures_util::stream;
    use gafctl_protocol::{ControlCommand, FrameError, Minutes};

    #[derive(Clone, Default)]
    struct RecordingTransport {
        writes: Arc<Mutex<Vec<Vec<u8>>>>,
        responses: Arc<Mutex<std::collections::VecDeque<&'static [u8]>>>,
        sender: Arc<Mutex<Option<tokio::sync::mpsc::UnboundedSender<Vec<u8>>>>>,
    }

    impl GattTransport for RecordingTransport {
        async fn write(&mut self, bytes: &[u8]) -> Result<()> {
            self.writes.lock().unwrap().push(bytes.to_vec());
            let response = self.responses.lock().unwrap().pop_front();
            if let Some(sender) = self.sender.lock().unwrap().as_ref()
                && let Some(response) = response
            {
                sender.send(response.to_vec()).unwrap();
            }
            if response.is_none() {
                self.sender.lock().unwrap().take();
            }
            Ok(())
        }
    }

    type TestSession =
        RequestSession<RecordingTransport, std::pin::Pin<Box<dyn Stream<Item = Vec<u8>> + Send>>>;

    fn session(responses: &[&'static [u8]]) -> (TestSession, RecordingTransport) {
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        let transport = RecordingTransport {
            responses: Arc::new(Mutex::new(responses.iter().copied().collect())),
            sender: Arc::new(Mutex::new(Some(sender))),
            ..RecordingTransport::default()
        };
        let notifications = Box::pin(stream::unfold(receiver, |mut receiver| async move {
            receiver.recv().await.map(|bytes| (bytes, receiver))
        }));
        (
            RequestSession::new(transport.clone(), notifications, Duration::from_secs(1)),
            transport,
        )
    }

    #[tokio::test]
    async fn repeated_polls_refresh_all_readings_without_repeating_identity() {
        let (mut session, transport) = session(&[
            b"#idr030000x\n",
            b"#sdr03ca00aa\n",
            b"#atr041a012c\n",
            b"#dmraf\n",
            b"#sdr03cb00ab\n",
            b"#atr041b012d\n",
            b"#dmran\n",
        ]);
        session.query(None, None).await.unwrap();
        let second = session.query(None, None).await.unwrap().snapshot.unwrap();
        assert_eq!(
            second.thresholds.decoded().unwrap().temperature.value(),
            1051
        );
        assert_eq!(
            second.mode.decoded().unwrap().fan,
            gafctl_protocol::FanState::On
        );
        assert_eq!(second.sensors.decoded().unwrap().temperature.value(), 971);
        assert_eq!(
            transport.writes.lock().unwrap().as_slice(),
            &[
                b"#idg\n".to_vec(),
                b"#sdg\n".to_vec(),
                b"#atg\n".to_vec(),
                b"#dmg\n".to_vec(),
                b"#sdg\n".to_vec(),
                b"#atg\n".to_vec(),
                b"#dmg\n".to_vec(),
            ]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn timer_expiry_is_observed_on_the_next_poll() {
        let (mut session, transport) = session(&[
            b"#idr030000x\n",
            b"#sdr03ca00aa\n",
            b"#atr041a012c\n",
            b"#dmraf\n",
            b"#sdr03cb00ab\n",
            b"#atr041a012c\n",
            b"#dmrtn\n",
            b"#ttr00010002\n",
            b"#sdr03cc00ac\n",
            b"#atr041a012c\n",
            b"#dmrtf\n",
        ]);
        session.query(None, None).await.unwrap();
        tokio::time::sleep(Duration::from_secs(3)).await;
        let running = session.query(None, None).await.unwrap().snapshot.unwrap();
        assert_eq!(
            running.mode.decoded().unwrap().fan,
            gafctl_protocol::FanState::On
        );
        assert!(running.timer.is_some());
        tokio::time::sleep(Duration::from_secs(3)).await;
        let stopped = session.query(None, None).await.unwrap().snapshot.unwrap();
        assert_eq!(
            stopped.mode.decoded().unwrap().fan,
            gafctl_protocol::FanState::Off
        );
        assert!(stopped.timer.is_none());
        assert_eq!(
            transport.writes.lock().unwrap().as_slice(),
            &[
                b"#idg\n".to_vec(),
                b"#sdg\n".to_vec(),
                b"#atg\n".to_vec(),
                b"#dmg\n".to_vec(),
                b"#sdg\n".to_vec(),
                b"#atg\n".to_vec(),
                b"#dmg\n".to_vec(),
                b"#ttg\n".to_vec(),
                b"#sdg\n".to_vec(),
                b"#atg\n".to_vec(),
                b"#dmg\n".to_vec(),
            ]
        );
    }

    #[tokio::test]
    async fn delayed_settings_do_not_renew_sensor_observation_time() {
        struct DelayedSettings(RecordingTransport);
        impl GattTransport for DelayedSettings {
            async fn write(&mut self, bytes: &[u8]) -> Result<()> {
                if bytes == b"#atg\n" {
                    tokio::time::sleep(Duration::from_millis(40)).await;
                }
                self.0.write(bytes).await
            }
        }
        let (session, transport) = session(&[
            b"#idr030000x\n",
            b"#sdr03ca00aa\n",
            b"#atr041a012c\n",
            b"#dmraf\n",
        ]);
        let mut session = RequestSession::new(
            DelayedSettings(transport),
            session.notifications,
            Duration::from_secs(1),
        );
        let snapshot = session.query(None, None).await.unwrap().snapshot.unwrap();
        assert!(
            std::time::SystemTime::now()
                .duration_since(snapshot.observed_at)
                .unwrap()
                >= Duration::from_millis(40)
        );
        assert!(!snapshot.is_fresh_at(std::time::Instant::now(), Duration::from_millis(10)));
    }

    #[tokio::test]
    async fn queued_acknowledgement_cannot_confirm_a_new_control() {
        let (mut session, transport) = session(&[
            b"#idr030000x\n",
            b"#sdr03ca00aa\n",
            b"#atr041a012c\n",
            b"#dmraf\n",
        ]);
        session.query(None, None).await.unwrap();
        transport
            .sender
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .send(b"#tmr0\n".to_vec())
            .unwrap();
        let result = session
            .query(Some(ControlCommand::SetTimer(Minutes::new(1))), None)
            .await;
        assert!(result.is_err());
        assert_eq!(
            transport.writes.lock().unwrap().last().unwrap(),
            b"#tms0001\n"
        );
    }

    #[tokio::test]
    async fn accepted_control_survives_coalesced_invalid_unsolicited_settings() {
        let (mut session, _) = session(&[
            b"#idr030000x\n",
            b"#sdr03ca00aa\n",
            b"#atr041a012c\n",
            b"#dmraf\n",
            b"#tmr0\n#atrbad\n",
        ]);
        session.query(None, None).await.unwrap();
        let result = session
            .query(Some(ControlCommand::SetTimer(Minutes::new(1))), None)
            .await
            .unwrap();
        assert_eq!(
            result.control.unwrap().acknowledgement(),
            gafctl_protocol::Acknowledgement::Accepted
        );
        assert!(result.state_error.is_some());
        assert!(!session.is_initialized());
    }

    #[tokio::test]
    async fn queued_settings_reply_is_replaced_by_current_readback() {
        let (mut session, transport) = session(&[
            b"#idr030000x\n",
            b"#sdr03ca00aa\n",
            b"#atr041a012c\n",
            b"#dmraf\n",
            b"#sdr03cb00ab\n",
            b"#atr041a012c\n",
            b"#dmraf\n",
        ]);
        session.query(None, None).await.unwrap();
        transport
            .sender
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .send(b"#dmran\n".to_vec())
            .unwrap();
        let result = session.query(None, None).await.unwrap();
        assert_eq!(
            result.snapshot.unwrap().mode.decoded().unwrap().fan,
            gafctl_protocol::FanState::Off
        );
        assert_eq!(transport.writes.lock().unwrap().len(), 7);
    }

    #[tokio::test(start_paused = true)]
    async fn control_expiring_during_idle_fragment_drain_is_never_written() {
        let (mut session, transport) = session(&[
            b"#idr030000x\n",
            b"#sdr03ca00aa\n",
            b"#atr041a012c\n",
            b"#dmraf\n",
        ]);
        session.query(None, None).await.unwrap();
        let sender = transport.sender.lock().unwrap().as_ref().unwrap().clone();
        sender.send(b"#tm".to_vec()).unwrap();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            sender.send(b"r0\n".to_vec()).unwrap();
        });
        let error = session
            .query(
                Some(ControlCommand::SetTimer(Minutes::new(1))),
                Some(tokio::time::Instant::now() + Duration::from_millis(100)),
            )
            .await
            .unwrap_err();
        assert_eq!(
            crate::ProbeError::classify(error).kind(),
            crate::ProbeErrorKind::StaleControl
        );
        assert_eq!(transport.writes.lock().unwrap().len(), 4);
    }

    #[tokio::test]
    async fn initialization_reads_timer_only_when_timer_mode_is_running() {
        let (mut session, transport) = session(&[
            b"#idr030000x\n",
            b"#sdr03ca00aa\n",
            b"#atr041a012c\n",
            b"#dmrtn\n",
            b"#ttr00010002\n",
        ]);
        let result = session.query(None, None).await.unwrap();
        assert!(result.snapshot.unwrap().timer.is_some());
        assert_eq!(transport.writes.lock().unwrap().last().unwrap(), b"#ttg\n");
    }

    #[tokio::test]
    async fn malformed_settings_do_not_initialize_the_retained_cache() {
        let (mut session, _) = session(&[
            b"#idr030000x\n",
            b"#sdr03ca00aa\n",
            b"#atrgarbage\n",
            b"#dmraf\n",
        ]);
        let result = session.query(None, None).await.unwrap();
        assert!(result.state_error.is_some());
        assert_eq!(
            result.snapshot.unwrap().thresholds.frame().payload(),
            b"garbage"
        );
        assert!(!session.is_initialized());
    }

    #[tokio::test]
    async fn old_firmware_is_readable_but_cannot_receive_control_writes() {
        let (mut session, transport) = session(&[
            b"#idr010000x\n",
            b"#sdr03ca00aa\n",
            b"#atr041a012c\n",
            b"#dmraf\n",
        ]);
        session.query(None, None).await.unwrap();
        assert!(
            session
                .query(Some(ControlCommand::SetTimer(Minutes::new(1))), None)
                .await
                .is_err()
        );
        assert_eq!(transport.writes.lock().unwrap().len(), 4);
    }

    #[tokio::test(start_paused = true)]
    async fn control_expiring_during_identity_read_is_never_written() {
        let transport = RecordingTransport::default();
        let responses = stream::once(async {
            tokio::time::sleep(Duration::from_millis(200)).await;
            b"#idr030000x\n".to_vec()
        });
        let mut session = RequestSession::new(
            transport.clone(),
            Box::pin(responses),
            Duration::from_secs(1),
        );
        let result = session
            .query(
                Some(ControlCommand::SetTimer(Minutes::new(1))),
                Some(tokio::time::Instant::now() + Duration::from_millis(100)),
            )
            .await;
        let error = crate::ProbeError::classify(result.unwrap_err());
        assert_eq!(error.kind(), crate::ProbeErrorKind::StaleControl);
        assert_eq!(
            transport.writes.lock().unwrap().as_slice(),
            &[b"#idg\n".to_vec()]
        );
    }

    #[tokio::test]
    async fn ordinary_state_read_failure_is_returned_as_error() {
        let (mut session, transport) = session(&[b"#idr030000x\n"]);
        let error = session.query(None, None).await.unwrap_err();

        assert!(format!("{error:#}").contains("BLE notification stream ended"));
        assert_eq!(
            transport.writes.lock().unwrap().as_slice(),
            &[b"#idg\n".to_vec(), b"#sdg\n".to_vec()]
        );
    }

    #[tokio::test]
    async fn acknowledged_timer_is_retained_when_later_state_read_fails() {
        let (mut session, transport) = session(&[
            b"#idr030000x\n",
            b"#sdr03ca00aa\n",
            b"#atr041a012c\n",
            b"#dmraf\n",
            b"#tmr0\n",
        ]);
        let result = session
            .query(Some(ControlCommand::SetTimer(Minutes::new(1))), None)
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
            transport.writes.lock().unwrap().as_slice(),
            &[
                b"#idg\n".to_vec(),
                b"#sdg\n".to_vec(),
                b"#atg\n".to_vec(),
                b"#dmg\n".to_vec(),
                b"#tms0001\n".to_vec(),
                b"#sdg\n".to_vec()
            ]
        );
    }

    #[tokio::test]
    async fn invalid_identity_prevents_control_write() {
        let (mut session, transport) = session(&[b"#idrnot-gaf\n"]);
        assert!(
            session
                .query(Some(ControlCommand::SetTimer(Minutes::new(1))), None)
                .await
                .is_err()
        );
        assert_eq!(
            transport.writes.lock().unwrap().as_slice(),
            &[b"#idg\n".to_vec()]
        );
    }

    #[test]
    fn first_matching_response_survives_later_notifications() {
        let mut decoder = FrameDecoder::default();
        assert_eq!(
            decode_matching_response(&mut decoder, Bytes::from_static(b"#dm"), *b"dmr", None)
                .unwrap(),
            None
        );
        let response = decode_matching_response(
            &mut decoder,
            Bytes::from_static(b"ran\n#dmraf\n"),
            *b"dmr",
            None,
        )
        .unwrap()
        .unwrap();
        decode_matching_response(&mut decoder, Bytes::from_static(b"#atr"), *b"dmr", None).unwrap();
        assert_eq!(
            decode_matching_response(
                &mut decoder,
                Bytes::from_static(b"041a012c\n"),
                *b"dmr",
                None
            )
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
            None,
        )
        .unwrap_err();
        assert_eq!(
            error.downcast_ref::<FrameError>(),
            Some(&FrameError::InvalidStart)
        );
    }

    #[test]
    fn matching_response_shares_notification_storage() {
        let notification = b"#amr0\n#dmran\n".to_vec();
        let response_pointer = notification[6..].as_ptr();
        let response = decode_matching_response(
            &mut FrameDecoder::default(),
            notification.into(),
            *b"dmr",
            None,
        )
        .unwrap()
        .unwrap();

        assert_eq!(response.as_bytes(), b"#dmran\n");
        assert_eq!(response.as_bytes().as_ptr(), response_pointer);
    }
}
