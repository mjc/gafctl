//! Bluetooth discovery, state queries, and ordinary controls for GAF attic fans.

use std::{collections::HashSet, time::Duration};

use anyhow::{Context, Result, bail};
use btleplug::{
    api::{Central, CharPropFlags, Manager as _, Peripheral as _, ScanFilter, WriteType},
    platform::{Manager, Peripheral},
};
use futures_util::{StreamExt, TryStreamExt, future, stream};
use tokio::time::{sleep, timeout};
use updraft_protocol::{ControlCommand, Frame, FrameDecoder, ReadCommand};
use uuid::Uuid;

/// GAF's observed primary BLE service UUID.
pub const GAF_SERVICE_UUID: Uuid = Uuid::from_u128(0x000000ff_0000_1000_8000_00805f9b34fb);
/// GAF's observed command/response BLE characteristic UUID.
pub const GAF_CHARACTERISTIC_UUID: Uuid = Uuid::from_u128(0x0000ff01_0000_1000_8000_00805f9b34fb);

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
    pub response: Frame,
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
        control_reply: Option<Frame>,
    },
}

struct Candidate {
    peripheral: Peripheral,
    device: DiscoveredDevice,
}

/// Discover GAF BLE peripherals and, when selected unambiguously, issue the
/// read-only queries plus an optional ordinary control-setting write.
pub async fn probe(options: ProbeOptions) -> Result<ProbeResult> {
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
    sleep(options.scan_duration).await;
    adapter.stop_scan().await.context("stop BLE scan")?;

    let candidates = stream::iter(
        adapter
            .peripherals()
            .await
            .context("list BLE peripherals")?,
    )
    .then(|peripheral| async move {
        let properties = peripheral
            .properties()
            .await
            .context("read BLE advertisement properties")?;
        Ok::<_, anyhow::Error>((peripheral, properties))
    })
    .try_filter_map(|(peripheral, properties)| async move {
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
    })
    .try_collect::<Vec<_>>()
    .await?;
    let mut seen = HashSet::new();
    let candidates: Vec<_> = candidates
        .into_iter()
        .filter(|candidate| seen.insert(candidate.device.id.clone()))
        .collect();
    let devices = candidates
        .iter()
        .map(|candidate| candidate.device.clone())
        .collect();

    let result = match options.mode {
        ProbeMode::Scan => match candidates.as_slice() {
            [] => ProbeResult::NoDevices,
            _ => ProbeResult::Discovered { devices },
        },
        ProbeMode::Query {
            device_id,
            control_command,
        } => {
            let selected = match (device_id.as_deref(), candidates.as_slice()) {
                (Some(id), _) => Some(
                    candidates
                        .iter()
                        .find(|candidate| candidate.device.id == id)
                        .with_context(|| format!("no scanned GAF peripheral has ID {id}"))?,
                ),
                (None, [candidate]) => Some(candidate),
                (None, _) => None,
            };
            match (selected, candidates.as_slice()) {
                (Some(candidate), _) => {
                    let (replies, control_reply) = query_peripheral(
                        &candidate.peripheral,
                        options.response_timeout,
                        control_command,
                    )
                    .await?;
                    ProbeResult::Queried {
                        devices,
                        replies,
                        control_reply,
                    }
                }
                (None, []) => ProbeResult::NoDevices,
                (None, _) => ProbeResult::Ambiguous { devices },
            }
        }
    };
    Ok(result)
}

async fn query_peripheral(
    peripheral: &Peripheral,
    response_timeout: Duration,
    control_command: Option<ControlCommand>,
) -> Result<(Vec<ReadReply>, Option<Frame>)> {
    peripheral
        .connect()
        .await
        .context("connect to GAF BLE peripheral")?;
    let result = query_connected(peripheral, response_timeout, control_command).await;
    let disconnect = peripheral.disconnect().await;

    match result {
        Ok(replies) => {
            disconnect.context("disconnect from GAF BLE peripheral")?;
            Ok(replies)
        }
        Err(error) => {
            let _ = disconnect;
            Err(error)
        }
    }
}

async fn query_connected(
    peripheral: &Peripheral,
    response_timeout: Duration,
    control_command: Option<ControlCommand>,
) -> Result<(Vec<ReadReply>, Option<Frame>)> {
    peripheral
        .discover_services()
        .await
        .context("discover GAF BLE services")?;
    let characteristic = peripheral
        .characteristics()
        .into_iter()
        .find(|characteristic| {
            characteristic.service_uuid == GAF_SERVICE_UUID
                && characteristic.uuid == GAF_CHARACTERISTIC_UUID
        })
        .context("GAF service 00FF has no characteristic FF01")?;

    let write_type = if characteristic.properties.contains(CharPropFlags::WRITE) {
        WriteType::WithResponse
    } else if characteristic
        .properties
        .contains(CharPropFlags::WRITE_WITHOUT_RESPONSE)
    {
        WriteType::WithoutResponse
    } else {
        bail!("GAF characteristic FF01 does not permit writes");
    };

    let mut notifications = peripheral
        .notifications()
        .await
        .context("subscribe to BLE notifications")?;
    peripheral
        .subscribe(&characteristic)
        .await
        .context("enable responses on GAF characteristic FF01")?;

    let mut decoder = FrameDecoder::default();
    let control_reply = if let Some(command) = control_command {
        let frame = command.frame();
        peripheral
            .write(&characteristic, &frame, write_type)
            .await
            .with_context(|| format!("send ordinary control command {}", frame.escape_ascii()))?;
        Some(
            await_response(
                &mut notifications,
                &mut decoder,
                command.response_id(),
                response_timeout,
            )
            .await
            .with_context(|| format!("wait for {} acknowledgement", frame.escape_ascii()))?,
        )
    } else {
        None
    };

    let characteristic = &characteristic;
    let (replies, _, _) = stream::iter([
        ReadCommand::Identity,
        ReadCommand::Mode,
        ReadCommand::Sensors,
        ReadCommand::AutoThresholds,
        ReadCommand::Timer,
    ])
    .map(Ok::<_, anyhow::Error>)
    .try_fold(
        (Vec::new(), &mut notifications, &mut decoder),
        |(mut replies, notifications, decoder), request| async move {
            peripheral
                .write(characteristic, request.frame(), write_type)
                .await
                .with_context(|| format!("send state query {}", request.frame().escape_ascii()))?;

            let response = await_response(
                notifications,
                decoder,
                request.response_id(),
                response_timeout,
            )
            .await
            .with_context(|| format!("waiting for {} response", request.frame().escape_ascii()))?;

            replies.push(ReadReply { request, response });
            Ok((replies, notifications, decoder))
        },
    )
    .await?;

    Ok((replies, control_reply))
}

async fn await_response(
    notifications: &mut (impl StreamExt<Item = btleplug::api::ValueNotification> + Unpin),
    decoder: &mut FrameDecoder,
    response_id: [u8; 3],
    response_timeout: Duration,
) -> Result<Frame> {
    timeout(
        response_timeout,
        notifications
            .by_ref()
            .filter(|notification| future::ready(notification.uuid == GAF_CHARACTERISTIC_UUID))
            .map(|notification| {
                decoder
                    .push(&notification.value)
                    .map_err(anyhow::Error::from)
            })
            .try_filter_map(|frames| {
                future::ready(Ok::<_, anyhow::Error>(
                    frames
                        .into_iter()
                        .find(|frame| frame.command() == response_id),
                ))
            })
            .try_next(),
    )
    .await
    .context("timed out waiting for matching BLE response")??
    .context("BLE notification stream ended")
}
