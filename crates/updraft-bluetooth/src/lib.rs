//! Bluetooth discovery, state queries, and ordinary controls for GAF attic fans.

use std::{collections::HashSet, time::Duration};

use anyhow::{Context, Result, bail};
use btleplug::{
    api::{Central, CharPropFlags, Manager as _, Peripheral as _, ScanFilter, WriteType},
    platform::{Manager, Peripheral},
};
use futures_util::StreamExt;
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
    /// Optional exact peripheral ID returned by a previous scan.
    pub device_id: Option<String>,
    /// Discover candidates only; do not connect or send any command.
    pub scan_only: bool,
    /// Optional ordinary control-setting write. Firmware update commands are
    /// not represented by this API.
    pub control_command: Option<ControlCommand>,
}

impl Default for ProbeOptions {
    fn default() -> Self {
        Self {
            scan_duration: Duration::from_secs(6),
            response_timeout: Duration::from_secs(3),
            device_id: None,
            scan_only: false,
            control_command: None,
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

/// Result of scanning, optionally followed by read-only queries.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ProbeResult {
    /// Matching GAF peripherals discovered during this run.
    pub devices: Vec<DiscoveredDevice>,
    /// Raw responses. Empty when scanning only or when device selection is ambiguous.
    pub replies: Vec<ReadReply>,
    /// Response to the optional ordinary control-setting write.
    pub control_reply: Option<Frame>,
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

    let mut candidates = Vec::new();
    let mut devices = Vec::new();
    let mut seen = HashSet::new();

    for peripheral in adapter
        .peripherals()
        .await
        .context("list BLE peripherals")?
    {
        let properties = peripheral
            .properties()
            .await
            .context("read BLE advertisement properties")?;
        let Some(properties) = properties else {
            continue;
        };
        if !properties.services.contains(&GAF_SERVICE_UUID) {
            continue;
        }

        let id = peripheral.id().to_string();
        if !seen.insert(id.clone()) {
            continue;
        }

        devices.push(DiscoveredDevice {
            id,
            name: properties.local_name,
            rssi: properties.rssi,
        });
        candidates.push(peripheral);
    }

    if options.scan_only {
        return Ok(ProbeResult {
            devices,
            replies: Vec::new(),
            control_reply: None,
        });
    }

    let selected = match options.device_id.as_deref() {
        Some(id) => candidates
            .iter()
            .find(|peripheral| peripheral.id().to_string() == id)
            .with_context(|| format!("no scanned GAF peripheral has ID {id}"))?,
        None if candidates.len() == 1 => &candidates[0],
        None if candidates.is_empty() => bail!(
            "no BLE peripheral advertising GAF service 00FF was found; verify Bluetooth is on and the fan is nearby"
        ),
        None => {
            return Ok(ProbeResult {
                devices,
                replies: Vec::new(),
                control_reply: None,
            });
        }
    };

    let (replies, control_reply) =
        query_peripheral(selected, options.response_timeout, options.control_command).await?;
    Ok(ProbeResult {
        devices,
        replies,
        control_reply,
    })
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
    let mut replies = Vec::new();
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

    for request in [
        ReadCommand::Identity,
        ReadCommand::Mode,
        ReadCommand::Sensors,
        ReadCommand::AutoThresholds,
        ReadCommand::Timer,
    ] {
        peripheral
            .write(&characteristic, request.frame(), write_type)
            .await
            .with_context(|| format!("send state query {}", request.frame().escape_ascii()))?;

        let response = await_response(
            &mut notifications,
            &mut decoder,
            request.response_id(),
            response_timeout,
        )
        .await
        .with_context(|| format!("waiting for {} response", request.frame().escape_ascii()))?;

        replies.push(ReadReply { request, response });
    }

    Ok((replies, control_reply))
}

async fn await_response(
    notifications: &mut (impl StreamExt<Item = btleplug::api::ValueNotification> + Unpin),
    decoder: &mut FrameDecoder,
    response_id: [u8; 3],
    response_timeout: Duration,
) -> Result<Frame> {
    timeout(response_timeout, async {
        loop {
            let notification = notifications
                .next()
                .await
                .context("BLE notification stream ended")?;
            if notification.uuid != GAF_CHARACTERISTIC_UUID {
                continue;
            }
            for frame in decoder.push(&notification.value)? {
                if frame.command() == response_id {
                    return Ok::<Frame, anyhow::Error>(frame);
                }
            }
        }
    })
    .await
    .context("timed out waiting for matching BLE response")?
}
