use std::{
    collections::HashSet,
    fmt::{self, Write as _},
    time::Duration,
};

use anyhow::{Context, Result};
use btleplug::{
    api::{Central, Manager as _, Peripheral as _, ScanFilter},
    platform::{Adapter, Manager, Peripheral, PeripheralId},
};
use futures_util::{StreamExt, TryStreamExt, future, stream};
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

pub(super) fn summarize_scan(mut candidates: Vec<Candidate>) -> ProbeResult {
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

pub(super) async fn discover_candidates(
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
) -> Result<Vec<Candidate>> {
    let peripherals = complete_before(operation_timeout, "list BLE peripherals", async {
        adapter.peripherals().await.context("list BLE peripherals")
    })
    .await?;
    stream::iter(peripherals)
        .then(|peripheral| async move {
            if fresh_devices.is_none_or(|devices| devices.contains(&peripheral.id())) {
                read_gaf_advertisement(peripheral, operation_timeout).await
            } else {
                Ok(None)
            }
        })
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

#[cfg(test)]
mod tests {
    use super::*;
    use btleplug::api::CentralEvent;

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
