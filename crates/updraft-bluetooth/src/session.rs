use std::{future, pin::Pin, time::Duration};

use anyhow::{Context, Result, bail};
use btleplug::api::{CharPropFlags, Characteristic, Peripheral as _, ValueNotification, WriteType};
use btleplug::platform::Peripheral;
use bytes::Bytes;
use futures_util::{Stream, StreamExt, TryStreamExt};
use updraft_protocol::{
    ControlCommand, ControlOutcome, DeviceSnapshot, Frame, FrameDecoder, FrameError, ReadCommand,
    Request,
};

use crate::{
    DisconnectOutcome, GAF_CHARACTERISTIC_UUID, GAF_SERVICE_UUID, QueryResult,
    lifecycle::{DisconnectCleanup, complete_before, fail_with_cleanup, finish_with_cleanup},
};

pub(super) struct ConnectedPeripheral<'a> {
    peripheral: &'a Peripheral,
    cleanup: DisconnectCleanup,
}

struct ReadySession<'connected, 'device> {
    connected: &'connected ConnectedPeripheral<'device>,
    characteristic: Characteristic,
    write_type: WriteType,
    notifications: Pin<Box<dyn Stream<Item = ValueNotification> + Send>>,
    decoder: FrameDecoder,
    response_timeout: Duration,
}

pub(super) async fn query_peripheral(
    peripheral: &Peripheral,
    response_timeout: Duration,
    control_command: Option<ControlCommand>,
) -> Result<QueryResult> {
    let mut connected = ConnectedPeripheral::connect(peripheral, response_timeout).await?;
    let query_result = async {
        ReadySession::subscribe(&connected, response_timeout)
            .await?
            .query(control_command)
            .await
    }
    .await
    .map(|mut result| {
        result.disconnect = DisconnectOutcome::Disconnected;
        result
    });
    let (mut result, disconnect) = finish_with_cleanup(query_result, connected.disconnect().await)?;
    result.disconnect = disconnect;
    Ok(result)
}

impl<'a> ConnectedPeripheral<'a> {
    async fn connect(peripheral: &'a Peripheral, operation_timeout: Duration) -> Result<Self> {
        let mut cleanup = DisconnectCleanup::new(peripheral.clone(), operation_timeout);
        let connection =
            complete_before(operation_timeout, "connect to GAF BLE peripheral", async {
                peripheral
                    .connect()
                    .await
                    .context("connect to GAF BLE peripheral")
            })
            .await;
        match connection {
            Ok(()) => Ok(Self {
                peripheral,
                cleanup,
            }),
            Err(error) => fail_with_cleanup(error, cleanup.run().await),
        }
    }

    async fn disconnect(&mut self) -> Result<()> {
        self.cleanup.run().await
    }
}

async fn writable_characteristic(
    connected: &ConnectedPeripheral<'_>,
    operation_timeout: Duration,
) -> Result<(Characteristic, WriteType)> {
    complete_before(operation_timeout, "discover GAF BLE services", async {
        connected
            .peripheral
            .discover_services()
            .await
            .context("discover GAF BLE services")
    })
    .await?;
    let characteristic = connected
        .peripheral
        .services()
        .into_iter()
        .flat_map(|service| service.characteristics)
        .find(|characteristic| {
            characteristic.service_uuid == GAF_SERVICE_UUID
                && characteristic.uuid == GAF_CHARACTERISTIC_UUID
        })
        .context("GAF service 00FF has no characteristic FF01")?;

    let write_type = match (
        characteristic.properties.contains(CharPropFlags::WRITE),
        characteristic
            .properties
            .contains(CharPropFlags::WRITE_WITHOUT_RESPONSE),
    ) {
        (true, _) => WriteType::WithResponse,
        (false, true) => WriteType::WithoutResponse,
        (false, false) => bail!("GAF characteristic FF01 does not permit writes"),
    };
    Ok((characteristic, write_type))
}

impl<'connected, 'device> ReadySession<'connected, 'device> {
    async fn subscribe(
        connected: &'connected ConnectedPeripheral<'device>,
        response_timeout: Duration,
    ) -> Result<Self> {
        let (characteristic, write_type) =
            writable_characteristic(connected, response_timeout).await?;
        let notifications =
            complete_before(response_timeout, "subscribe to BLE notifications", async {
                connected
                    .peripheral
                    .notifications()
                    .await
                    .context("subscribe to BLE notifications")
            })
            .await?;
        complete_before(response_timeout, "enable GAF characteristic FF01", async {
            connected
                .peripheral
                .subscribe(&characteristic)
                .await
                .context("enable responses on GAF characteristic FF01")
        })
        .await?;

        Ok(Self {
            connected,
            characteristic,
            write_type,
            notifications,
            decoder: FrameDecoder::default(),
            response_timeout,
        })
    }

    async fn query(mut self, control_command: Option<ControlCommand>) -> Result<QueryResult> {
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
            self.connected
                .peripheral
                .write(&self.characteristic, frame.as_ref(), self.write_type)
                .await
                .with_context(|| {
                    format!(
                        "send {} {}",
                        request.operation(),
                        frame.as_ref().escape_ascii()
                    )
                })
        })
        .await?;

        let decoder = &mut self.decoder;
        let mut matching_responses = self
            .notifications
            .by_ref()
            .filter(|notification| future::ready(notification.uuid == GAF_CHARACTERISTIC_UUID))
            .filter_map(|notification| {
                future::ready(
                    decode_matching_response(
                        decoder,
                        notification.value.into(),
                        request.response_id(),
                    )
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
        let mut decoder = FrameDecoder::default();
        let error =
            decode_matching_response(&mut decoder, Bytes::from_static(b"#dmran\nx\n"), *b"dmr")
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
