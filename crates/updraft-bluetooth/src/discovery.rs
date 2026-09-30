use std::{
    collections::HashSet,
    fmt::{self, Write as _},
    future::Future,
    time::Duration,
};

use anyhow::{Context, Result};
use btleplug::{
    api::{Central, Manager as _, Peripheral as _, ScanFilter},
    platform::{Adapter, Manager, Peripheral, PeripheralId},
};
use futures_util::{StreamExt, future, stream};
use tokio::time::sleep;

use crate::{
    GAF_SERVICE_UUID, ProbeResult,
    lifecycle::{ScanCleanup, complete_before, fail_with_cleanup},
};

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

/// A discovered device and its private platform handle, when a query may use it.
#[derive(Debug)]
pub struct Candidate {
    peripheral: Option<Peripheral>,
    device: DiscoveredDevice,
}

/// A peripheral whose advertisement properties could not be read.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiscoveryFailure {
    /// Peripheral ID observed during the scan.
    pub device_id: PeripheralId,
    /// Property-read error.
    pub reason: String,
}

#[derive(Debug)]
pub(super) struct DiscoveryReport<T> {
    pub(super) candidates: Vec<T>,
    pub(super) failures: Vec<DiscoveryFailure>,
}

impl<T> Default for DiscoveryReport<T> {
    fn default() -> Self {
        Self {
            candidates: Vec::new(),
            failures: Vec::new(),
        }
    }
}

impl<T> DiscoveryReport<T> {
    pub(super) const fn can_select_automatically(&self) -> bool {
        self.failures.is_empty()
    }
}

pub(super) fn can_query_with_report(
    report: &DiscoveryReport<Candidate>,
    requested_id: Option<&str>,
) -> bool {
    report.can_select_automatically()
        || requested_id.is_some_and(|expected| {
            report
                .candidates
                .iter()
                .any(|candidate| peripheral_id_matches(&candidate.device.id, expected))
        })
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

pub(super) enum CandidateSelection {
    NoDevices,
    Ambiguous(Vec<Candidate>),
    Chosen {
        device: DiscoveredDevice,
        peripheral: Peripheral,
    },
}

pub(super) fn summarize_scan(mut report: DiscoveryReport<Candidate>) -> ProbeResult {
    if !report.can_select_automatically() {
        return incomplete_result(report);
    }
    if report.candidates.is_empty() {
        ProbeResult::NoDevices
    } else {
        report
            .candidates
            .iter_mut()
            .for_each(Candidate::release_peripheral);
        ProbeResult::Discovered {
            devices: report.candidates,
        }
    }
}

pub(super) fn incomplete_result(mut report: DiscoveryReport<Candidate>) -> ProbeResult {
    report
        .candidates
        .iter_mut()
        .for_each(Candidate::release_peripheral);
    ProbeResult::DiscoveryIncomplete {
        devices: report.candidates,
        failures: report.failures,
    }
}

pub(super) async fn discover_candidates(
    scan_duration: Duration,
    operation_timeout: Duration,
) -> Result<DiscoveryReport<Candidate>> {
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

    #[cfg(target_os = "linux")]
    let existing_devices =
        complete_before(operation_timeout, "list cached BLE peripherals", async {
            adapter
                .peripherals()
                .await
                .context("list cached BLE peripherals")
        })
        .await?
        .into_iter()
        .map(|peripheral| peripheral.id())
        .collect::<HashSet<_>>();

    #[cfg(target_os = "linux")]
    let events = complete_before(operation_timeout, "subscribe to BLE scan events", async {
        adapter
            .events()
            .await
            .context("subscribe to BLE scan events")
    })
    .await?;

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
    #[cfg(target_os = "linux")]
    let fresh_devices = events
        .take_until(sleep(scan_duration))
        .filter_map(|event| future::ready(fresh_advertisement_id(event, &existing_devices)))
        .collect::<HashSet<_>>()
        .await;
    #[cfg(not(target_os = "linux"))]
    sleep(scan_duration).await;
    scan_cleanup.run().await?;

    #[cfg(target_os = "linux")]
    let fresh_devices = Some(&fresh_devices);
    #[cfg(not(target_os = "linux"))]
    let fresh_devices = None;
    collect_advertised_candidates(&adapter, fresh_devices, operation_timeout).await
}

async fn collect_advertised_candidates(
    adapter: &Adapter,
    fresh_devices: Option<&HashSet<btleplug::platform::PeripheralId>>,
    operation_timeout: Duration,
) -> Result<DiscoveryReport<Candidate>> {
    let peripherals = complete_before(operation_timeout, "list BLE peripherals", async {
        adapter.peripherals().await.context("list BLE peripherals")
    })
    .await?;
    let devices = peripherals
        .into_iter()
        .filter(|peripheral| fresh_devices.is_none_or(|devices| devices.contains(&peripheral.id())))
        .map(|peripheral| (peripheral.id(), peripheral));
    let mut report = inspect_peripherals(devices, |peripheral| {
        read_gaf_advertisement(peripheral, operation_timeout)
    })
    .await;
    report.candidates =
        report
            .candidates
            .into_iter()
            .fold(Vec::new(), |mut candidates, candidate| {
                if !already_discovered(&candidates, &candidate) {
                    candidates.push(candidate);
                }
                candidates
            });
    Ok(report)
}

async fn inspect_peripherals<I, H, C, E, F, Fut>(
    peripherals: I,
    mut inspect: F,
) -> DiscoveryReport<C>
where
    I: IntoIterator<Item = (PeripheralId, H)>,
    E: fmt::Display,
    F: FnMut(H) -> Fut,
    Fut: Future<Output = std::result::Result<Option<C>, E>>,
{
    stream::iter(peripherals)
        .then(|(device_id, peripheral)| {
            let properties = inspect(peripheral);
            async move { (device_id, properties.await) }
        })
        .fold(
            DiscoveryReport::default(),
            |mut report, (device_id, result)| {
                match result {
                    Ok(Some(candidate)) => report.candidates.push(candidate),
                    Ok(None) => {}
                    Err(error) => report.failures.push(DiscoveryFailure {
                        device_id,
                        reason: error.to_string(),
                    }),
                }
                future::ready(report)
            },
        )
        .await
}

#[cfg(any(target_os = "linux", test))]
fn fresh_advertisement_id(
    event: btleplug::api::CentralEvent,
    existing_devices: &HashSet<btleplug::platform::PeripheralId>,
) -> Option<btleplug::platform::PeripheralId> {
    use btleplug::api::CentralEvent;

    match event {
        CentralEvent::RssiUpdate { id, .. }
        | CentralEvent::ManufacturerDataAdvertisement { id, .. }
        | CentralEvent::ServiceDataAdvertisement { id, .. } => Some(id),
        CentralEvent::DeviceDiscovered(id) | CentralEvent::ServicesAdvertisement { id, .. }
            if !existing_devices.contains(&id) =>
        {
            Some(id)
        }
        CentralEvent::DeviceDiscovered(_)
        | CentralEvent::DeviceUpdated(_)
        | CentralEvent::DeviceConnected(_)
        | CentralEvent::DeviceDisconnected(_)
        | CentralEvent::DeviceServicesModified(_)
        | CentralEvent::ServicesAdvertisement { .. }
        | CentralEvent::StateUpdate(_) => None,
    }
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

pub(super) fn select_candidate(
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

pub(super) fn peripheral_id_matches(peripheral_id: &PeripheralId, expected: &str) -> bool {
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

#[cfg(test)]
mod tests {
    use super::*;
    use btleplug::api::CentralEvent;

    #[tokio::test]
    async fn property_failure_keeps_valid_candidate_and_diagnostic() {
        let report = inspect_peripherals(
            [
                (PeripheralId::from(uuid::Uuid::nil()), false),
                (PeripheralId::from(uuid::Uuid::from_u128(1)), true),
            ],
            |properties_available| async move {
                if properties_available {
                    Ok::<_, &'static str>(Some("selected-fan"))
                } else {
                    Err("property request timed out")
                }
            },
        )
        .await;

        assert_eq!(report.candidates, ["selected-fan"]);
        assert_eq!(
            report.failures,
            [DiscoveryFailure {
                device_id: PeripheralId::from(uuid::Uuid::nil()),
                reason: String::from("property request timed out"),
            }],
        );
        assert!(!report.can_select_automatically());
    }

    #[test]
    fn incomplete_scan_allows_only_explicitly_found_candidate() {
        let candidate = Candidate {
            peripheral: None,
            device: DiscoveredDevice {
                id: PeripheralId::from(uuid::Uuid::from_u128(1)),
                name: None,
                rssi: None,
            },
        };
        let id = candidate.device.id.to_string();
        let report = DiscoveryReport {
            candidates: vec![candidate],
            failures: vec![DiscoveryFailure {
                device_id: PeripheralId::from(uuid::Uuid::nil()),
                reason: String::from("property request timed out"),
            }],
        };

        assert!(can_query_with_report(&report, Some(&id)));
        assert!(!can_query_with_report(&report, None));
        assert!(!can_query_with_report(&report, Some("not-scanned")));
    }

    #[test]
    fn bluez_cached_initial_events_do_not_count_as_fresh_advertisements() {
        let stale = btleplug::platform::PeripheralId::from(uuid::Uuid::nil());
        let fresh = btleplug::platform::PeripheralId::from(uuid::Uuid::from_u128(1));
        let initial = CentralEvent::ServicesAdvertisement {
            id: stale.clone(),
            services: vec![GAF_SERVICE_UUID],
        };
        let existing = HashSet::from([stale.clone()]);
        assert!(fresh_advertisement_id(initial, &existing).is_none());

        let advertisement = CentralEvent::RssiUpdate {
            id: fresh.clone(),
            rssi: -50,
        };
        let newly_discovered = CentralEvent::DeviceDiscovered(fresh.clone());
        let observed = [
            fresh_advertisement_id(advertisement, &existing),
            fresh_advertisement_id(newly_discovered, &existing),
        ]
        .into_iter()
        .flatten()
        .collect::<HashSet<_>>();
        let current_scan = [stale, fresh.clone()]
            .into_iter()
            .filter(|id| observed.contains(id))
            .collect::<Vec<_>>();
        assert_eq!(current_scan.len(), 1);
        assert_eq!(current_scan[0], fresh);
    }

    #[test]
    fn string_match_compares_display_chunks_without_building_a_string() {
        let mut output = StringMatch {
            expected: "gaf-device-42",
            offset: 0,
        };

        write!(&mut output, "gaf-device-{}", 42).unwrap();

        assert_eq!(output.offset, output.expected.len());
    }
}
