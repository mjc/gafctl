use std::{future::Future, sync::Arc, time::Duration};

use anyhow::Context;
use anyhow::Result as AnyhowResult;
use btleplug::{
    api::Manager as _,
    platform::{Adapter, Manager, Peripheral},
};
use gafctl_protocol::ControlCommand;
use tokio::sync::{Mutex, OnceCell};

use crate::{
    ProbeError, ProbeMode, ProbeOptions, ProbeResult,
    discovery::{
        Candidate, CandidateSelection, DiscoveryReport, can_query_with_report, discover_candidates,
        incomplete_result, select_candidate, summarize_scan,
    },
    lifecycle::{complete_before, platform_timeout, recover_disconnect, stop_ble_scan},
    session::query_peripheral,
};

/// BLE client that retains its manager and adapter across queries.
///
/// The manager and adapter are initialized lazily and retained for the client's
/// lifetime. On Linux, btleplug's manager starts
/// a detached D-Bus task that keeps its socket open after the manager is dropped.
pub struct ProbeClient {
    backend: Arc<Mutex<BleBackend>>,
}

#[derive(Default)]
struct BleBackend {
    manager: OnceCell<Manager>,
    adapter: Option<Adapter>,
    pending_disconnect: Option<Peripheral>,
    scan_pending: bool,
}

impl ProbeClient {
    /// Create a client without opening a Bluetooth connection.
    #[must_use]
    pub fn new() -> Self {
        Self {
            backend: Arc::new(Mutex::new(BleBackend::default())),
        }
    }

    /// Wait for the current operation and its cleanup, including a detached query.
    pub async fn wait_until_idle(&self) {
        drop(self.backend.lock().await);
    }

    /// Discover GAF BLE peripherals and, when selected, query or control one.
    pub async fn probe(&self, options: ProbeOptions) -> Result<ProbeResult, ProbeError> {
        let mut backend = Arc::clone(&self.backend).lock_owned().await;
        finish_without_cancelling(async move { backend.probe(options).await })
            .await
            .map_err(ProbeError::classify)
    }
}

impl BleBackend {
    async fn probe(&mut self, options: ProbeOptions) -> AnyhowResult<ProbeResult> {
        let BleBackend {
            manager,
            adapter,
            pending_disconnect,
            scan_pending,
        } = self;
        if let Some(peripheral) = pending_disconnect.as_ref() {
            recover_disconnect(peripheral, platform_timeout(options.response_timeout)).await?;
            *pending_disconnect = None;
        }
        if *scan_pending {
            if let Some(adapter) = adapter.as_ref() {
                stop_ble_scan(adapter, platform_timeout(options.response_timeout)).await?;
            }
            *scan_pending = false;
        }
        let manager = manager
            .get_or_try_init(|| async {
                complete_before(
                    platform_timeout(options.response_timeout),
                    "create Bluetooth manager",
                    async { Manager::new().await.context("create Bluetooth manager") },
                )
                .await
            })
            .await?;

        // BlueZ can replace its adapter object after it disappears. Enumerating
        // through the retained manager refreshes that handle without opening a
        // new D-Bus session. CoreBluetooth adapters are cached because creating
        // them starts a worker thread.
        let selected_adapter = if cfg!(target_os = "linux") || adapter.is_none() {
            let current = complete_before(
                platform_timeout(options.response_timeout),
                "list Bluetooth adapters",
                async { manager.adapters().await.context("list Bluetooth adapters") },
            )
            .await?
            .into_iter()
            .next()
            .context("no Bluetooth adapter is available")?;
            *adapter = Some(current);
            adapter.as_ref().expect("adapter stored above")
        } else {
            adapter.as_ref().expect("adapter initialized above")
        };

        probe_with_adapter(selected_adapter, options, pending_disconnect, scan_pending).await
    }
}

async fn finish_without_cancelling<T: Send + 'static>(
    operation: impl Future<Output = AnyhowResult<T>> + Send + 'static,
) -> AnyhowResult<T> {
    tokio::spawn(operation)
        .await
        .context("BLE operation task failed")?
}

impl Default for ProbeClient {
    fn default() -> Self {
        Self::new()
    }
}

/// Discover GAF BLE peripherals and query one selected device, with an optional
/// threshold or timer write.
pub async fn probe(options: ProbeOptions) -> Result<ProbeResult, ProbeError> {
    ProbeClient::new().probe(options).await
}

async fn probe_with_adapter(
    adapter: &Adapter,
    options: ProbeOptions,
    pending_disconnect: &mut Option<Peripheral>,
    scan_pending: &mut bool,
) -> AnyhowResult<ProbeResult> {
    let requested_device_id = match &options.mode {
        ProbeMode::Scan => None,
        ProbeMode::Query { device_id, .. } => device_id.as_deref(),
    };
    let discovery = discover_candidates(
        adapter,
        options.scan_duration,
        platform_timeout(options.response_timeout),
        requested_device_id,
        scan_pending,
    )
    .await?;

    match options.mode {
        ProbeMode::Scan => Ok(summarize_scan(discovery)),
        ProbeMode::Query {
            device_id,
            control_command,
        } => {
            query_selected_device(
                discovery,
                device_id.as_deref(),
                control_command,
                options.response_timeout,
                options.control_deadline,
                pending_disconnect,
            )
            .await
        }
    }
}

async fn query_selected_device(
    discovery: DiscoveryReport<Candidate>,
    device_id: Option<&str>,
    control_command: Option<ControlCommand>,
    response_timeout: Duration,
    control_deadline: Option<tokio::time::Instant>,
    pending_disconnect: &mut Option<Peripheral>,
) -> AnyhowResult<ProbeResult> {
    if !can_query_with_report(&discovery, device_id) {
        return Ok(incomplete_result(discovery));
    }

    let failures = discovery.failures;
    match select_candidate(discovery.candidates, device_id)? {
        CandidateSelection::NoDevices => Ok(ProbeResult::NoDevices),
        CandidateSelection::Ambiguous(devices) => Ok(ProbeResult::Ambiguous { devices }),
        CandidateSelection::Chosen { device, peripheral } => {
            *pending_disconnect = Some(peripheral.clone());
            let mut result = query_peripheral(
                &peripheral,
                response_timeout,
                control_command,
                control_deadline,
            )
            .await?;
            if result.disconnect == crate::DisconnectOutcome::Disconnected {
                *pending_disconnect = None;
            }
            result.discovery_failures = failures;
            Ok(ProbeResult::Queried {
                device,
                result: Box::new(result),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn cancelled_caller_keeps_backend_locked_until_idle_cleanup_finishes() {
        use std::sync::Arc;
        use tokio::sync::oneshot;

        let client = Arc::new(super::ProbeClient::new());
        let guard = Arc::clone(&client.backend).lock_owned().await;
        let (started, did_start) = oneshot::channel();
        let (finish, can_finish) = oneshot::channel();
        let caller = tokio::spawn(super::finish_without_cancelling(async move {
            let _guard = guard;
            started.send(()).unwrap();
            can_finish.await.unwrap();
            Ok(())
        }));
        did_start.await.unwrap();
        caller.abort();
        assert!(caller.await.unwrap_err().is_cancelled());
        assert!(client.backend.try_lock().is_err());
        let idle_client = Arc::clone(&client);
        let idle = tokio::spawn(async move { idle_client.wait_until_idle().await });
        tokio::task::yield_now().await;
        assert!(!idle.is_finished());
        finish.send(()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(1), idle)
            .await
            .unwrap()
            .unwrap();
        assert!(client.backend.try_lock().is_ok());
    }
}
