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
    pub async fn wait_until_idle(&self) -> Result<(), ProbeError> {
        let backend = Arc::clone(&self.backend);
        finish_without_cancelling(async move {
            let mut backend = backend.lock_owned().await;
            backend
                .cleanup_pending(platform_timeout(Duration::from_secs(3)))
                .await
        })
        .await
        .map_err(ProbeError::classify)
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
    async fn cleanup_pending(&mut self, operation_timeout: Duration) -> AnyhowResult<()> {
        let adapter = self.adapter.clone();
        drain_pending_cleanup(
            &mut self.pending_disconnect,
            &mut self.scan_pending,
            |peripheral| async move { recover_disconnect(&peripheral, operation_timeout).await },
            || async move {
                let adapter = adapter.context("pending BLE scan cleanup has no adapter")?;
                stop_ble_scan(&adapter, operation_timeout).await
            },
        )
        .await
    }

    async fn probe(&mut self, options: ProbeOptions) -> AnyhowResult<ProbeResult> {
        self.cleanup_pending(platform_timeout(options.response_timeout))
            .await?;
        let Self {
            manager, adapter, ..
        } = self;
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

        probe_with_adapter(
            selected_adapter,
            options,
            &mut self.pending_disconnect,
            &mut self.scan_pending,
        )
        .await
    }
}

async fn finish_without_cancelling<T: Send + 'static>(
    operation: impl Future<Output = AnyhowResult<T>> + Send + 'static,
) -> AnyhowResult<T> {
    tokio::spawn(operation)
        .await
        .context("BLE operation task failed")?
}

async fn drain_pending_cleanup<T, D, DFut, S, SFut>(
    pending_disconnect: &mut Option<T>,
    scan_pending: &mut bool,
    disconnect: D,
    stop_scan: S,
) -> AnyhowResult<()>
where
    T: Clone,
    D: FnOnce(T) -> DFut,
    DFut: Future<Output = AnyhowResult<()>>,
    S: FnOnce() -> SFut,
    SFut: Future<Output = AnyhowResult<()>>,
{
    let disconnect = if let Some(peripheral) = pending_disconnect.as_ref() {
        match disconnect(peripheral.clone()).await {
            Ok(()) => {
                *pending_disconnect = None;
                Ok(())
            }
            Err(error) => Err(error),
        }
    } else {
        Ok(())
    };
    let scan = if *scan_pending {
        match stop_scan().await {
            Ok(()) => {
                *scan_pending = false;
                Ok(())
            }
            Err(error) => Err(error),
        }
    } else {
        Ok(())
    };
    match (disconnect, scan) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(disconnect), Ok(())) => Err(disconnect).context("retry pending BLE disconnect"),
        (Ok(()), Err(scan)) => Err(scan).context("retry pending BLE scan stop"),
        (Err(disconnect), Err(scan)) => Err(anyhow::anyhow!(
            "disconnect cleanup failed ({disconnect:#}); scan cleanup failed ({scan:#})"
        )),
    }
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
            .unwrap()
            .expect("idle cleanup succeeds");
        assert!(client.backend.try_lock().is_ok());
    }

    #[tokio::test]
    async fn wait_until_idle_reports_pending_scan_without_adapter_and_preserves_it() {
        let client = super::ProbeClient::new();
        client.backend.lock().await.scan_pending = true;

        let error = client
            .wait_until_idle()
            .await
            .expect_err("pending scan cleanup without an adapter must fail");

        assert!(format!("{error:#}").contains("pending BLE scan cleanup has no adapter"));
        let backend = client.backend.lock().await;
        assert!(backend.scan_pending);
        assert!(backend.adapter.is_none());
    }

    #[tokio::test]
    async fn pending_cleanup_retries_only_pending_resources_and_retains_failures() {
        use std::{cell::Cell, future};

        let disconnect_attempts = Cell::new(0);
        let mut pending_disconnect = Some("peripheral");
        let scan_attempts = Cell::new(0);
        let mut scan_pending = true;
        let error = super::drain_pending_cleanup(
            &mut pending_disconnect,
            &mut scan_pending,
            |peripheral| {
                disconnect_attempts.set(disconnect_attempts.get() + 1);
                async move {
                    assert_eq!(peripheral, "peripheral");
                    Err(anyhow::anyhow!("disconnect failed"))
                }
            },
            || {
                scan_attempts.set(scan_attempts.get() + 1);
                future::ready(Err(anyhow::anyhow!("stop scan failed")))
            },
        )
        .await
        .expect_err("both failed cleanup operations must be reported");
        assert!(format!("{error:#}").contains("disconnect failed"));
        assert!(format!("{error:#}").contains("stop scan failed"));
        assert_eq!(disconnect_attempts.get(), 1);
        assert_eq!(pending_disconnect, Some("peripheral"));
        assert!(scan_pending);
        assert_eq!(scan_attempts.get(), 1);

        super::drain_pending_cleanup(
            &mut pending_disconnect,
            &mut scan_pending,
            |_| async { Ok(()) },
            || {
                scan_attempts.set(scan_attempts.get() + 1);
                future::ready(Ok(()))
            },
        )
        .await
        .expect("pending cleanup retries succeed");
        assert_eq!(pending_disconnect, None);
        assert!(!scan_pending);
        assert_eq!(scan_attempts.get(), 2);

        let disconnect_attempts_when_empty = disconnect_attempts.get();
        let scan_attempts_when_empty = scan_attempts.get();
        super::drain_pending_cleanup(
            &mut pending_disconnect,
            &mut scan_pending,
            |_| async {
                disconnect_attempts.set(disconnect_attempts.get() + 1);
                Ok(())
            },
            || {
                scan_attempts.set(scan_attempts.get() + 1);
                future::ready(Ok(()))
            },
        )
        .await
        .expect("empty state needs no cleanup");
        assert_eq!(disconnect_attempts.get(), disconnect_attempts_when_empty);
        assert_eq!(scan_attempts.get(), scan_attempts_when_empty);
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_timeout_keeps_pending_cleanup_serialized_and_retryable() {
        use std::sync::Arc;
        use tokio::sync::{Mutex, oneshot};

        #[derive(Default)]
        struct Pending {
            disconnect: Option<u8>,
            scan: bool,
        }

        let pending = Arc::new(Mutex::new(Pending {
            disconnect: Some(1),
            scan: true,
        }));
        let guard = Arc::clone(&pending).lock_owned().await;
        let (started, did_start) = oneshot::channel();
        let (finish, can_finish) = oneshot::channel();
        let drain = super::finish_without_cancelling(async move {
            let mut guard = guard;
            let Pending { disconnect, scan } = &mut *guard;
            super::drain_pending_cleanup(
                disconnect,
                scan,
                |_| async {
                    started.send(()).unwrap();
                    can_finish.await.unwrap();
                    Err(anyhow::anyhow!("disconnect still failed"))
                },
                || async { Err(anyhow::anyhow!("scan stop failed")) },
            )
            .await
        });
        let shutdown = tokio::spawn(async move {
            tokio::time::timeout(std::time::Duration::from_secs(5), drain).await
        });

        did_start.await.unwrap();
        tokio::time::advance(std::time::Duration::from_secs(5)).await;
        assert!(shutdown.await.unwrap().is_err());
        assert!(pending.try_lock().is_err());

        finish.send(()).unwrap();
        let pending = tokio::time::timeout(std::time::Duration::from_secs(1), pending.lock())
            .await
            .expect("detached cleanup eventually releases backend");
        assert_eq!(pending.disconnect, Some(1));
        assert!(pending.scan);
    }
}
