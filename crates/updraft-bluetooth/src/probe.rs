use std::{future::Future, time::Duration};

use anyhow::Context;
use anyhow::Result as AnyhowResult;
use btleplug::{
    api::Manager as _,
    platform::{Adapter, Manager},
};
use tokio::sync::Mutex;
use updraft_protocol::ControlCommand;

use crate::{
    ProbeError, ProbeMode, ProbeOptions, ProbeResult,
    discovery::{
        Candidate, CandidateSelection, DiscoveryReport, can_query_with_report, discover_candidates,
        incomplete_result, select_candidate, summarize_scan,
    },
    lifecycle::complete_before,
    session::query_peripheral,
};

/// Reusable BLE client for long-running callers.
///
/// The manager and adapter are initialized lazily and retained for the client's
/// lifetime. This is required by btleplug's Linux backend, whose manager starts
/// a detached D-Bus task that keeps its socket open after the manager is dropped.
pub struct ProbeClient {
    backend: Mutex<BleBackend>,
}

#[derive(Default)]
struct BleBackend {
    manager: Option<Manager>,
    adapter: Option<Adapter>,
}

impl ProbeClient {
    /// Create a client without opening a Bluetooth connection yet.
    #[must_use]
    pub fn new() -> Self {
        Self {
            backend: Mutex::new(BleBackend::default()),
        }
    }

    /// Discover GAF BLE peripherals and, when selected, query or control one.
    pub async fn probe(&self, options: ProbeOptions) -> Result<ProbeResult, ProbeError> {
        let mut backend = self.backend.lock().await;
        let BleBackend { manager, adapter } = &mut *backend;
        let manager = get_or_init(manager, || async {
            complete_before(
                options.response_timeout,
                "create Bluetooth manager",
                async { Manager::new().await.context("create Bluetooth manager") },
            )
            .await
        })
        .await
        .map_err(ProbeError::classify)?;

        // BlueZ can replace its adapter object after it disappears. Enumerating
        // through the retained manager refreshes that handle without opening a
        // new D-Bus session. CoreBluetooth adapters are cached because creating
        // them starts a worker thread.
        let selected_adapter = if cfg!(target_os = "linux") || adapter.is_none() {
            let current =
                complete_before(options.response_timeout, "list Bluetooth adapters", async {
                    manager.adapters().await.context("list Bluetooth adapters")
                })
                .await
                .map_err(ProbeError::classify)?
                .into_iter()
                .next()
                .context("no Bluetooth adapter is available")
                .map_err(ProbeError::classify)?;
            *adapter = Some(current);
            adapter.as_ref().expect("adapter stored above")
        } else {
            adapter.as_ref().expect("adapter initialized above")
        };

        probe_with_adapter(selected_adapter, options).await
    }
}

async fn get_or_init<T, E, F, Fut>(slot: &mut Option<T>, initialize: F) -> Result<&mut T, E>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<T, E>>,
{
    if slot.is_none() {
        *slot = Some(initialize().await?);
    }
    Ok(slot.as_mut().expect("slot initialized above"))
}

impl Default for ProbeClient {
    fn default() -> Self {
        Self::new()
    }
}

/// Discover GAF BLE peripherals and, when selected unambiguously, issue the
/// read-only queries plus an optional ordinary control-setting write.
pub async fn probe(options: ProbeOptions) -> Result<ProbeResult, ProbeError> {
    ProbeClient::new().probe(options).await
}

async fn probe_with_adapter(
    adapter: &Adapter,
    options: ProbeOptions,
) -> Result<ProbeResult, ProbeError> {
    let requested_device_id = match &options.mode {
        ProbeMode::Scan => None,
        ProbeMode::Query { device_id, .. } => device_id.as_deref(),
    };
    let discovery = discover_candidates(
        adapter,
        options.scan_duration,
        options.response_timeout,
        requested_device_id,
    )
    .await
    .map_err(ProbeError::classify)?;

    match options.mode {
        ProbeMode::Scan => Ok(summarize_scan(discovery)),
        ProbeMode::Query {
            device_id,
            control_command,
        } => query_selected_device(
            discovery,
            device_id.as_deref(),
            control_command,
            options.response_timeout,
        )
        .await
        .map_err(ProbeError::classify),
    }
}

async fn query_selected_device(
    discovery: DiscoveryReport<Candidate>,
    device_id: Option<&str>,
    control_command: Option<ControlCommand>,
    response_timeout: Duration,
) -> AnyhowResult<ProbeResult> {
    if !can_query_with_report(&discovery, device_id) {
        return Ok(incomplete_result(discovery));
    }

    let failures = discovery.failures;
    match select_candidate(discovery.candidates, device_id)? {
        CandidateSelection::NoDevices => Ok(ProbeResult::NoDevices),
        CandidateSelection::Ambiguous(devices) => Ok(ProbeResult::Ambiguous { devices }),
        CandidateSelection::Chosen { device, peripheral } => {
            let mut result =
                query_peripheral(&peripheral, response_timeout, control_command).await?;
            result.discovery_failures = failures;
            Ok(ProbeResult::Queried { device, result })
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::get_or_init;

    #[tokio::test]
    async fn manager_initialization_is_retained_across_adapter_failures() {
        let manager_creations = AtomicUsize::new(0);
        let mut manager = None;
        let mut adapter = None;

        get_or_init(&mut manager, || async {
            manager_creations.fetch_add(1, Ordering::SeqCst);
            Ok::<_, &'static str>("manager")
        })
        .await
        .unwrap();

        let failed_adapter = get_or_init(&mut adapter, || async {
            Err::<(), _>("adapter unavailable")
        })
        .await;
        assert_eq!(failed_adapter, Err("adapter unavailable"));

        get_or_init(&mut manager, || async {
            manager_creations.fetch_add(1, Ordering::SeqCst);
            Ok::<_, &'static str>("replacement manager")
        })
        .await
        .unwrap();
        get_or_init(&mut adapter, || async { Ok::<_, &'static str>(()) })
            .await
            .unwrap();

        assert_eq!(manager_creations.load(Ordering::SeqCst), 1);
        assert_eq!(manager, Some("manager"));
        assert_eq!(adapter, Some(()));
    }

    #[tokio::test]
    async fn failed_manager_initialization_can_be_retried() {
        let manager_creations = AtomicUsize::new(0);
        let mut manager = None;

        let failed = get_or_init(&mut manager, || async {
            manager_creations.fetch_add(1, Ordering::SeqCst);
            Err::<(), _>("manager unavailable")
        })
        .await;
        assert_eq!(failed, Err("manager unavailable"));

        get_or_init(&mut manager, || async {
            manager_creations.fetch_add(1, Ordering::SeqCst);
            Ok::<_, &'static str>(())
        })
        .await
        .unwrap();

        assert_eq!(manager_creations.load(Ordering::SeqCst), 2);
        assert_eq!(manager, Some(()));
    }
}
