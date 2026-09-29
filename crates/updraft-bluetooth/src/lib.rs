//! Bluetooth discovery, state queries, and ordinary controls for GAF attic fans.

use std::{
    fmt::{self, Write as _},
    future::Future,
    pin::Pin,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use btleplug::{
    api::{
        Central, CharPropFlags, Characteristic, Manager as _, Peripheral as _, ScanFilter,
        ValueNotification, WriteType,
    },
    platform::{Adapter, Manager, Peripheral, PeripheralId},
};
use bytes::Bytes;
use futures_util::{Stream, StreamExt, TryStreamExt, future, stream};
use tokio::{
    runtime::Handle,
    time::{sleep, timeout},
};
use updraft_protocol::{
    ControlCommand, ControlOutcome, DeviceSnapshot, Frame, FrameDecoder, FrameError, ReadCommand,
    Request,
};
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
    /// Maximum time for each BLE operation and each command response.
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
    pub id: PeripheralId,
    /// BLE local name, if the device advertises one.
    pub name: Option<String>,
    /// Latest advertised RSSI, in dBm, when provided by the OS.
    pub rssi: Option<i16>,
}

/// Validated state and optional ordinary-control outcome from one device query.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QueryResult {
    /// All five mandatory device-state observations.
    pub snapshot: DeviceSnapshot,
    /// Outcome of the optional ordinary control request, interpreted with readback.
    pub control: Option<ControlOutcome>,
    /// Whether the BLE connection closed cleanly after the query.
    pub disconnect: DisconnectOutcome,
}

/// Outcome of closing the BLE connection after a query.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DisconnectOutcome {
    /// The BLE connection closed successfully.
    Disconnected,
    /// The query succeeded but the platform reported an error while disconnecting.
    Failed(String),
}

/// Result of scanning and, when selected, querying a peripheral.
// Keep the query inline to avoid a per-query heap allocation.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum ProbeResult {
    /// No peripherals advertising the service were found.
    NoDevices,
    /// Scan-only mode found one or more candidates.
    Discovered { devices: Vec<Candidate> },
    /// A query needs an exact device ID because multiple candidates were found.
    Ambiguous { devices: Vec<Candidate> },
    /// One peripheral was queried successfully.
    Queried {
        /// The selected peripheral.
        device: DiscoveredDevice,
        /// Validated snapshot and optional control outcome.
        result: QueryResult,
    },
}

/// A discovered device and its private platform handle, when a query may use it.
#[derive(Debug)]
pub struct Candidate {
    peripheral: Option<Peripheral>,
    device: DiscoveredDevice,
}

impl Candidate {
    /// Borrow the public description of this device.
    #[must_use]
    pub fn device(&self) -> &DiscoveredDevice {
        &self.device
    }

    fn release_peripheral(&mut self) {
        self.peripheral = None;
    }
}

enum CandidateSelection {
    NoDevices,
    Ambiguous(Vec<Candidate>),
    Chosen {
        device: DiscoveredDevice,
        peripheral: Peripheral,
    },
}

struct ConnectedPeripheral<'a> {
    peripheral: &'a Peripheral,
    cleanup: DisconnectCleanup,
}

struct ScanCleanup {
    adapter: Adapter,
    operation_timeout: Duration,
    runtime: Handle,
    armed: bool,
}

impl ScanCleanup {
    fn new(adapter: Adapter, operation_timeout: Duration) -> Self {
        Self {
            adapter,
            operation_timeout,
            runtime: Handle::current(),
            armed: true,
        }
    }

    async fn run(&mut self) -> Result<()> {
        stop_ble_scan(&self.adapter, self.operation_timeout).await?;
        self.armed = false;
        Ok(())
    }
}

impl Drop for ScanCleanup {
    fn drop(&mut self) {
        if self.armed {
            let adapter = self.adapter.clone();
            let operation_timeout = self.operation_timeout;
            self.runtime.spawn(async move {
                report_cleanup_failure(
                    "stop BLE scan",
                    stop_ble_scan(&adapter, operation_timeout).await,
                );
            });
        }
    }
}

struct DisconnectCleanup {
    peripheral: Peripheral,
    operation_timeout: Duration,
    runtime: Handle,
    armed: bool,
}

impl DisconnectCleanup {
    fn new(peripheral: Peripheral, operation_timeout: Duration) -> Self {
        Self {
            peripheral,
            operation_timeout,
            runtime: Handle::current(),
            armed: true,
        }
    }

    async fn run(&mut self) -> Result<()> {
        disconnect_peripheral(&self.peripheral, self.operation_timeout).await?;
        self.armed = false;
        Ok(())
    }
}

impl Drop for DisconnectCleanup {
    fn drop(&mut self) {
        if self.armed {
            let peripheral = self.peripheral.clone();
            let operation_timeout = self.operation_timeout;
            self.runtime.spawn(async move {
                report_cleanup_failure(
                    "disconnect from GAF BLE peripheral",
                    disconnect_peripheral(&peripheral, operation_timeout).await,
                );
            });
        }
    }
}

fn report_cleanup_failure(operation: &'static str, result: Result<()>) {
    if let Err(error) = result {
        tracing::warn!(operation, %error, "best-effort BLE cleanup failed");
    }
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
    let candidates = discover_candidates(options.scan_duration, options.response_timeout).await?;

    match options.mode {
        ProbeMode::Scan => Ok(summarize_scan(candidates)),
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

fn summarize_scan(mut candidates: Vec<Candidate>) -> ProbeResult {
    if candidates.is_empty() {
        ProbeResult::NoDevices
    } else {
        candidates
            .iter_mut()
            .for_each(Candidate::release_peripheral);
        ProbeResult::Discovered {
            devices: candidates,
        }
    }
}

async fn discover_candidates(
    scan_duration: Duration,
    operation_timeout: Duration,
) -> Result<Vec<Candidate>> {
    let manager = complete_before(operation_timeout, "create Bluetooth manager", async {
        Manager::new().await.context("create Bluetooth manager")
    })
    .await?;
    let adapter = complete_before(operation_timeout, "list Bluetooth adapters", async {
        manager.adapters().await.context("list Bluetooth adapters")
    })
    .await?
    .into_iter()
    .next()
    .context("no Bluetooth adapter is available")?;

    let mut scan_cleanup = ScanCleanup::new(adapter.clone(), operation_timeout);
    let scan_start = complete_before(operation_timeout, "start BLE scan", async {
        adapter
            .start_scan(ScanFilter {
                services: vec![GAF_SERVICE_UUID],
            })
            .await
            .context("start BLE scan for GAF service 00FF")
    })
    .await;
    if let Err(error) = scan_start {
        return fail_with_cleanup(error, scan_cleanup.run().await);
    }
    sleep(scan_duration).await;
    scan_cleanup.run().await?;

    collect_advertised_candidates(&adapter, operation_timeout).await
}

async fn complete_before<T>(
    duration: Duration,
    operation: &'static str,
    future: impl Future<Output = Result<T>>,
) -> Result<T> {
    timeout(duration, future)
        .await
        .with_context(|| format!("{operation} timed out"))?
}

async fn stop_ble_scan(adapter: &Adapter, operation_timeout: Duration) -> Result<()> {
    complete_before(operation_timeout, "stop BLE scan", async {
        adapter.stop_scan().await.context("stop BLE scan")
    })
    .await
}

async fn collect_advertised_candidates(
    adapter: &Adapter,
    operation_timeout: Duration,
) -> Result<Vec<Candidate>> {
    let peripherals = complete_before(operation_timeout, "list BLE peripherals", async {
        adapter.peripherals().await.context("list BLE peripherals")
    })
    .await?;
    stream::iter(peripherals)
        .then(|peripheral| read_gaf_advertisement(peripheral, operation_timeout))
        .try_fold(Vec::new(), |mut candidates, candidate| {
            if let Some(candidate) = candidate
                && !already_discovered(&candidates, &candidate)
            {
                candidates.push(candidate);
            }
            future::ready(Ok(candidates))
        })
        .await
}

fn already_discovered(candidates: &[Candidate], candidate: &Candidate) -> bool {
    candidate.peripheral.as_ref().is_some_and(|peripheral| {
        candidates.iter().any(|seen| {
            seen.peripheral
                .as_ref()
                .is_some_and(|seen| seen.id() == peripheral.id())
        })
    })
}

async fn read_gaf_advertisement(
    peripheral: Peripheral,
    operation_timeout: Duration,
) -> Result<Option<Candidate>> {
    let properties = complete_before(
        operation_timeout,
        "read BLE advertisement properties",
        async {
            peripheral
                .properties()
                .await
                .context("read BLE advertisement properties")
        },
    )
    .await?;
    Ok(properties
        .filter(|properties| properties.services.contains(&GAF_SERVICE_UUID))
        .map(|properties| Candidate {
            device: DiscoveredDevice {
                id: peripheral.id(),
                name: properties.local_name,
                rssi: properties.rssi,
            },
            peripheral: Some(peripheral),
        }))
}

fn select_candidate(
    mut candidates: Vec<Candidate>,
    device_id: Option<&str>,
) -> Result<CandidateSelection> {
    match (device_id, candidates.len()) {
        (Some(id), _) => candidates
            .iter()
            .position(|candidate| peripheral_id_matches(&candidate.device.id, id))
            .map(|index| candidates.remove(index))
            .with_context(|| format!("no scanned GAF peripheral has ID {id}"))
            .and_then(Candidate::select),
        (None, 0) => Ok(CandidateSelection::NoDevices),
        (None, 1) => Candidate::select(candidates.remove(0)),
        (None, _) => {
            candidates
                .iter_mut()
                .for_each(Candidate::release_peripheral);
            Ok(CandidateSelection::Ambiguous(candidates))
        }
    }
}

fn peripheral_id_matches(peripheral_id: &PeripheralId, expected: &str) -> bool {
    let mut output = StringMatch {
        expected,
        offset: 0,
    };
    write!(&mut output, "{peripheral_id}").is_ok() && output.offset == expected.len()
}

struct StringMatch<'a> {
    expected: &'a str,
    offset: usize,
}

impl fmt::Write for StringMatch<'_> {
    fn write_str(&mut self, output: &str) -> fmt::Result {
        let end = self.offset + output.len();
        if self.expected.get(self.offset..end) != Some(output) {
            return Err(fmt::Error);
        }
        self.offset = end;
        Ok(())
    }
}

impl Candidate {
    fn select(mut self) -> Result<CandidateSelection> {
        let peripheral = self
            .peripheral
            .take()
            .context("scanned candidate lost its BLE peripheral handle")?;
        Ok(CandidateSelection::Chosen {
            device: self.device,
            peripheral,
        })
    }
}

async fn query_selected_device(
    candidates: Vec<Candidate>,
    device_id: Option<&str>,
    control_command: Option<ControlCommand>,
    response_timeout: Duration,
) -> Result<ProbeResult> {
    match select_candidate(candidates, device_id)? {
        CandidateSelection::NoDevices => Ok(ProbeResult::NoDevices),
        CandidateSelection::Ambiguous(devices) => Ok(ProbeResult::Ambiguous { devices }),
        CandidateSelection::Chosen { device, peripheral } => {
            let result = query_peripheral(&peripheral, response_timeout, control_command).await?;
            Ok(ProbeResult::Queried { device, result })
        }
    }
}

async fn query_peripheral(
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

async fn disconnect_peripheral(peripheral: &Peripheral, operation_timeout: Duration) -> Result<()> {
    complete_before(
        operation_timeout,
        "disconnect from GAF BLE peripheral",
        async {
            peripheral
                .disconnect()
                .await
                .context("disconnect from GAF BLE peripheral")
        },
    )
    .await
}

fn finish_with_cleanup<T>(
    operation: Result<T>,
    cleanup: Result<()>,
) -> Result<(T, DisconnectOutcome)> {
    match (operation, cleanup) {
        (Ok(value), Ok(())) => Ok((value, DisconnectOutcome::Disconnected)),
        (Ok(value), Err(error)) => Ok((value, DisconnectOutcome::Failed(format!("{error:#}")))),
        (Err(error), Ok(())) => Err(error),
        (Err(error), Err(cleanup_error)) => {
            let operation = format!("{error:#}");
            Err(error.context(format!(
                "operation failed ({operation}); disconnect also failed ({cleanup_error:#})"
            )))
        }
    }
}

fn fail_with_cleanup<T>(operation: anyhow::Error, cleanup: Result<()>) -> Result<T> {
    match cleanup {
        Ok(()) => Err(operation),
        Err(cleanup_error) => {
            let operation_message = format!("{operation:#}");
            Err(operation.context(format!(
                "operation failed ({operation_message}); cleanup also failed ({cleanup_error:#})"
            )))
        }
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
    fn string_match_compares_display_chunks_without_building_a_string() {
        let mut output = StringMatch {
            expected: "gaf-device-42",
            offset: 0,
        };

        write!(&mut output, "gaf-device-{}", 42).unwrap();

        assert_eq!(output.offset, output.expected.len());
    }

    #[tokio::test]
    async fn platform_operation_deadline_names_the_timed_out_operation() {
        let error = complete_before(
            Duration::ZERO,
            "test BLE operation",
            future::pending::<Result<()>>(),
        )
        .await
        .expect_err("pending operation should time out");

        assert!(format!("{error:#}").contains("test BLE operation timed out"));
    }

    #[test]
    fn successful_operation_keeps_its_value_when_cleanup_fails() {
        let (value, disconnect) = finish_with_cleanup(Ok(42), Err(anyhow::anyhow!("disconnect")))
            .expect("query result is retained");

        assert_eq!(value, 42);
        assert_eq!(
            disconnect,
            DisconnectOutcome::Failed("disconnect".to_owned())
        );
    }

    #[test]
    fn operation_and_cleanup_errors_are_both_reported() {
        let error = finish_with_cleanup::<()>(
            Err(anyhow::anyhow!("query failed")),
            Err(anyhow::anyhow!("disconnect failed")),
        )
        .expect_err("both failures should remain visible");

        assert!(error.to_string().contains("query failed"));
        assert!(error.to_string().contains("disconnect failed"));
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
