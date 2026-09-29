//! Bluetooth discovery, state queries, and ordinary controls for GAF attic fans.

use std::{collections::HashSet, pin::Pin, time::Duration};

use anyhow::{Context, Result, bail};
use btleplug::{
    api::{
        Central, CharPropFlags, Characteristic, Manager as _, Peripheral as _, ScanFilter,
        ValueNotification, WriteType,
    },
    platform::{Adapter, Manager, Peripheral},
};
use bytes::Bytes;
use futures_util::{Stream, StreamExt, TryStreamExt, future, stream};
use tokio::time::{sleep, timeout};
use updraft_protocol::{ControlCommand, Frame, FrameDecoder, FrameError, ReadCommand};
use uuid::Uuid;

/// GAF's observed primary BLE service UUID.
pub const GAF_SERVICE_UUID: Uuid = Uuid::from_u128(0x000000ff_0000_1000_8000_00805f9b34fb);
/// GAF's observed command/response BLE characteristic UUID.
pub const GAF_CHARACTERISTIC_UUID: Uuid = Uuid::from_u128(0x0000ff01_0000_1000_8000_00805f9b34fb);

const STATE_READS: [ReadCommand; 5] = [
    ReadCommand::Identity,
    ReadCommand::Mode,
    ReadCommand::Sensors,
    ReadCommand::AutoThresholds,
    ReadCommand::Timer,
];

/// Settings for one GAF BLE inspection.
#[derive(Clone, Debug)]
pub struct ProbeOptions {
    /// How long to scan for the GAF service before selecting a peripheral.
    pub scan_duration: Duration,
    /// Maximum time to wait for each command's response notification.
    pub response_timeout: Duration,
    /// Action to take after scanning.
    pub mode: ProbeMode,
}

/// Whether to inspect advertisements only or query one matching peripheral.
#[derive(Clone, Debug)]
pub enum ProbeMode {
    /// Discover candidates without connecting or sending commands.
    Scan,
    /// Query one candidate, optionally changing an ordinary control setting.
    Query {
        /// Exact peripheral ID returned by a previous scan, if needed.
        device_id: Option<String>,
        /// Ordinary control write. Firmware update commands are not represented here.
        control_command: Option<ControlCommand>,
    },
}

impl Default for ProbeOptions {
    fn default() -> Self {
        Self {
            scan_duration: Duration::from_secs(6),
            response_timeout: Duration::from_secs(3),
            mode: ProbeMode::Query {
                device_id: None,
                control_command: None,
            },
        }
    }
}

/// A nearby peripheral advertising GAF's service.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiscoveredDevice {
    /// Platform-specific peripheral identifier, accepted by `--device-id`.
    pub id: String,
    /// BLE local name, if the device advertises one.
    pub name: Option<String>,
    /// Latest advertised RSSI, in dBm, when provided by the OS.
    pub rssi: Option<i16>,
}

/// One request and its raw, uninterpreted protocol response.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReadReply {
    /// Read-only request sent to the device.
    pub request: ReadCommand,
    /// Matching response frame, preserving its raw payload bytes.
    pub response: Frame<'static>,
}

/// Result of scanning and, when selected, querying a peripheral.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProbeResult {
    /// No peripherals advertising the service were found.
    NoDevices,
    /// Scan-only mode found one or more candidates.
    Discovered { devices: Vec<DiscoveredDevice> },
    /// A query needs an exact device ID because multiple candidates were found.
    Ambiguous { devices: Vec<DiscoveredDevice> },
    /// One peripheral was queried successfully.
    Queried {
        /// Matching peripherals discovered during the scan.
        devices: Vec<DiscoveredDevice>,
        /// Responses to all state queries, in request order.
        replies: Vec<ReadReply>,
        /// Response to the optional ordinary control-setting write.
        control_reply: Option<Frame<'static>>,
    },
}

struct Candidate {
    peripheral: Peripheral,
    device: DiscoveredDevice,
}

enum CandidateSelection<'a> {
    NoDevices,
    Ambiguous,
    Chosen(&'a Candidate),
}

struct ConnectedPeripheral<'a> {
    peripheral: &'a Peripheral,
}

struct ReadySession<'connected, 'device> {
    connected: &'connected ConnectedPeripheral<'device>,
    characteristic: Characteristic,
    write_type: WriteType,
    notifications: Pin<Box<dyn Stream<Item = ValueNotification> + Send>>,
    decoder: FrameDecoder,
    response_timeout: Duration,
}

/// Discover GAF BLE peripherals and, when selected unambiguously, issue the
/// read-only queries plus an optional ordinary control-setting write.
pub async fn probe(options: ProbeOptions) -> Result<ProbeResult> {
    let candidates = discover_candidates(options.scan_duration).await?;

    match options.mode {
        ProbeMode::Scan => Ok(summarize_scan(
            candidates
                .into_iter()
                .map(|candidate| candidate.device)
                .collect(),
        )),
        ProbeMode::Query {
            device_id,
            control_command,
        } => {
            query_selected_device(
                candidates,
                device_id.as_deref(),
                control_command,
                options.response_timeout,
            )
            .await
        }
    }
}

fn summarize_scan(devices: Vec<DiscoveredDevice>) -> ProbeResult {
    match devices.as_slice() {
        [] => ProbeResult::NoDevices,
        _ => ProbeResult::Discovered { devices },
    }
}

async fn discover_candidates(scan_duration: Duration) -> Result<Vec<Candidate>> {
    let manager = Manager::new().await.context("create Bluetooth manager")?;
    let adapter = manager
        .adapters()
        .await
        .context("list Bluetooth adapters")?
        .into_iter()
        .next()
        .context("no Bluetooth adapter is available")?;

    adapter
        .start_scan(ScanFilter {
            services: vec![GAF_SERVICE_UUID],
        })
        .await
        .context("start BLE scan for GAF service 00FF")?;
    sleep(scan_duration).await;
    adapter.stop_scan().await.context("stop BLE scan")?;

    collect_advertised_candidates(&adapter).await
}

async fn collect_advertised_candidates(adapter: &Adapter) -> Result<Vec<Candidate>> {
    let mut seen = HashSet::new();
    stream::iter(
        adapter
            .peripherals()
            .await
            .context("list BLE peripherals")?,
    )
    .then(read_gaf_advertisement)
    .try_filter_map(|candidate| {
        future::ready(Ok(
            candidate.filter(|candidate| seen.insert(candidate.peripheral.id()))
        ))
    })
    .try_collect::<Vec<_>>()
    .await
}

async fn read_gaf_advertisement(peripheral: Peripheral) -> Result<Option<Candidate>> {
    let properties = peripheral
        .properties()
        .await
        .context("read BLE advertisement properties")?;
    Ok(properties
        .filter(|properties| properties.services.contains(&GAF_SERVICE_UUID))
        .map(|properties| Candidate {
            device: DiscoveredDevice {
                id: peripheral.id().to_string(),
                name: properties.local_name,
                rssi: properties.rssi,
            },
            peripheral,
        }))
}

fn select_candidate<'a>(
    candidates: &'a [Candidate],
    device_id: Option<&str>,
) -> Result<CandidateSelection<'a>> {
    match (device_id, candidates) {
        (Some(id), _) => candidates
            .iter()
            .find(|candidate| candidate.device.id == id)
            .map(CandidateSelection::Chosen)
            .with_context(|| format!("no scanned GAF peripheral has ID {id}")),
        (None, []) => Ok(CandidateSelection::NoDevices),
        (None, [candidate]) => Ok(CandidateSelection::Chosen(candidate)),
        (None, _) => Ok(CandidateSelection::Ambiguous),
    }
}

async fn query_selected_device(
    candidates: Vec<Candidate>,
    device_id: Option<&str>,
    control_command: Option<ControlCommand>,
    response_timeout: Duration,
) -> Result<ProbeResult> {
    match select_candidate(&candidates, device_id)? {
        CandidateSelection::NoDevices => Ok(ProbeResult::NoDevices),
        CandidateSelection::Ambiguous => Ok(ProbeResult::Ambiguous {
            devices: candidates
                .into_iter()
                .map(|candidate| candidate.device)
                .collect(),
        }),
        CandidateSelection::Chosen(candidate) => {
            let (replies, control_reply) =
                query_peripheral(&candidate.peripheral, response_timeout, control_command).await?;
            Ok(ProbeResult::Queried {
                devices: candidates
                    .into_iter()
                    .map(|candidate| candidate.device)
                    .collect(),
                replies,
                control_reply,
            })
        }
    }
}

async fn query_peripheral(
    peripheral: &Peripheral,
    response_timeout: Duration,
    control_command: Option<ControlCommand>,
) -> Result<(Vec<ReadReply>, Option<Frame<'static>>)> {
    let connected = ConnectedPeripheral::connect(peripheral).await?;
    let query_result = async {
        ReadySession::subscribe(&connected, response_timeout)
            .await?
            .query(control_command)
            .await
    }
    .await;
    let disconnect_result = connected.disconnect().await;
    query_result.and_then(|replies| disconnect_result.map(|()| replies))
}

impl<'a> ConnectedPeripheral<'a> {
    async fn connect(peripheral: &'a Peripheral) -> Result<Self> {
        peripheral
            .connect()
            .await
            .context("connect to GAF BLE peripheral")?;
        Ok(Self { peripheral })
    }

    async fn disconnect(self) -> Result<()> {
        self.peripheral
            .disconnect()
            .await
            .context("disconnect from GAF BLE peripheral")
    }
}

async fn writable_characteristic(
    connected: &ConnectedPeripheral<'_>,
) -> Result<(Characteristic, WriteType)> {
    connected
        .peripheral
        .discover_services()
        .await
        .context("discover GAF BLE services")?;
    let characteristic = connected
        .peripheral
        .characteristics()
        .into_iter()
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
        let (characteristic, write_type) = writable_characteristic(connected).await?;
        let notifications = connected
            .peripheral
            .notifications()
            .await
            .context("subscribe to BLE notifications")?;
        connected
            .peripheral
            .subscribe(&characteristic)
            .await
            .context("enable responses on GAF characteristic FF01")?;

        Ok(Self {
            connected,
            characteristic,
            write_type,
            notifications,
            decoder: FrameDecoder::default(),
            response_timeout,
        })
    }

    async fn query(
        mut self,
        control_command: Option<ControlCommand>,
    ) -> Result<(Vec<ReadReply>, Option<Frame<'static>>)> {
        let control_reply = match control_command {
            Some(command) => {
                let frame = command.frame();
                Some(
                    self.exchange(&frame, command.response_id(), "ordinary control command")
                        .await
                        .with_context(|| {
                            format!("wait for {} acknowledgement", frame.escape_ascii())
                        })?,
                )
            }
            None => None,
        };
        let replies = self.read_state().await?;
        Ok((replies, control_reply))
    }

    async fn read_state(self) -> Result<Vec<ReadReply>> {
        let (_, replies) = stream::iter(STATE_READS)
            .map(Ok::<_, anyhow::Error>)
            .try_fold(
                (self, Vec::with_capacity(STATE_READS.len())),
                |(mut session, mut replies), request| async move {
                    let response = session
                        .exchange(request.frame(), request.response_id(), "state query")
                        .await
                        .with_context(|| {
                            format!("waiting for {} response", request.frame().escape_ascii())
                        })?;
                    replies.push(ReadReply { request, response });
                    Ok((session, replies))
                },
            )
            .await?;
        Ok(replies)
    }

    async fn exchange(
        &mut self,
        frame: &[u8],
        response_id: [u8; 3],
        operation: &str,
    ) -> Result<Frame<'static>> {
        self.connected
            .peripheral
            .write(&self.characteristic, frame, self.write_type)
            .await
            .with_context(|| format!("send {operation} {}", frame.escape_ascii()))?;

        let decoder = &mut self.decoder;
        let mut matching_responses = self
            .notifications
            .by_ref()
            .filter(|notification| future::ready(notification.uuid == GAF_CHARACTERISTIC_UUID))
            .filter_map(|notification| {
                future::ready(
                    decode_matching_response(decoder, notification.value.into(), response_id)
                        .transpose(),
                )
            });
        let response = timeout(self.response_timeout, matching_responses.try_next())
            .await
            .context("timed out waiting for matching BLE response")?;

        response?.context("BLE notification stream ended")
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
