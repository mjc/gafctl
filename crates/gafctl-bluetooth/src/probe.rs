#[cfg(any(target_os = "linux", test))]
use std::ops::AsyncFnOnce;
use std::{
    future::Future,
    sync::{Arc, OnceLock},
    time::Duration,
};

use anyhow::Context;
use anyhow::Result as AnyhowResult;
#[cfg(target_os = "linux")]
use btleplug::api::Central as _;
use btleplug::{
    api::{Manager as _, Peripheral as _},
    platform::{Adapter, Manager, Peripheral},
};
use gafctl_protocol::ControlCommand;
use tokio::sync::{Mutex, OnceCell};
use tokio::time::{Instant, timeout_at};
use tokio_util::sync::CancellationToken;

use crate::{
    ProbeError, ProbeMode, ProbeOptions, ProbeResult, SHUTDOWN_CLEANUP_TIMEOUT,
    discovery::{
        Candidate, CandidateSelection, DiscoveryReport, can_query_with_report, discover_candidates,
        incomplete_result, select_candidate, summarize_scan,
    },
    lifecycle::{complete_before, disconnect_peripheral, platform_timeout, stop_ble_scan},
    session::{BtleplugSession, open_session},
};

/// BLE client that retains its manager, adapter, and configured-device session.
///
/// The manager and adapter are initialized lazily and retained for the client's
/// lifetime. On Linux, btleplug's manager starts
/// a detached D-Bus task that keeps its socket open after the manager is dropped.
pub struct ProbeClient {
    backend: Arc<Mutex<BleBackend>>,
    shutdown: Arc<ShutdownState>,
}

struct ShutdownState {
    token: CancellationToken,
    cleanup_deadline: OnceLock<Instant>,
}

impl ShutdownState {
    fn new() -> Self {
        Self {
            token: CancellationToken::new(),
            cleanup_deadline: OnceLock::new(),
        }
    }

    fn begin(&self) {
        let _ = self
            .cleanup_deadline
            .set(Instant::now() + SHUTDOWN_CLEANUP_TIMEOUT);
        self.token.cancel();
    }

    fn cleanup_deadline(&self) -> AnyhowResult<Instant> {
        self.cleanup_deadline
            .get()
            .copied()
            .context("BLE shutdown cleanup deadline was not initialized")
    }

    fn ensure_open(&self) -> AnyhowResult<()> {
        if self.token.is_cancelled() {
            anyhow::bail!("BLE client is shutting down");
        }
        Ok(())
    }
}

struct ActiveSession {
    device: crate::DiscoveredDevice,
    requests: BtleplugSession,
}

struct BleBackend {
    active_session: Option<ActiveSession>,
    manager: OnceCell<Manager>,
    adapter: Option<Adapter>,
    pending_disconnect: Option<Peripheral>,
    scan_pending: bool,
    #[cfg(any(target_os = "linux", test))]
    startup_disconnect_pending: bool,
}

impl Default for BleBackend {
    fn default() -> Self {
        Self {
            active_session: None,
            manager: OnceCell::new(),
            adapter: None,
            pending_disconnect: None,
            scan_pending: false,
            #[cfg(any(target_os = "linux", test))]
            startup_disconnect_pending: true,
        }
    }
}

impl ProbeClient {
    /// Create a client without opening a Bluetooth connection.
    #[must_use]
    pub fn new() -> Self {
        Self {
            backend: Arc::new(Mutex::new(BleBackend::default())),
            shutdown: Arc::new(ShutdownState::new()),
        }
    }

    /// Reject new operations and cancel an active probe so it can release BLE resources.
    pub fn begin_shutdown(&self) {
        self.shutdown.begin();
    }

    /// Wait for the current operation and its cleanup, including a detached query.
    pub async fn wait_until_idle(&self) -> Result<(), ProbeError> {
        let backend = Arc::clone(&self.backend);
        let shutdown = Arc::clone(&self.shutdown);
        finish_without_cancelling(async move {
            let mut backend = backend.lock_owned().await;
            if shutdown.token.is_cancelled() {
                backend
                    .cleanup_pending_until(shutdown.cleanup_deadline()?)
                    .await
            } else {
                backend
                    .cleanup_pending(platform_timeout(Duration::from_secs(3)))
                    .await
            }
        })
        .await
        .map_err(ProbeError::classify)
    }

    /// Discover GAF BLE peripherals and, when selected, query or control one.
    pub async fn probe(&self, options: ProbeOptions) -> Result<ProbeResult, ProbeError> {
        let shutdown = Arc::clone(&self.shutdown);
        if let Err(error) = shutdown.ensure_open() {
            return Err(ProbeError::classify(error));
        }
        let backend = Arc::clone(&self.backend);
        let mut backend = tokio::select! {
            biased;
            () = shutdown.token.cancelled() => {
                return Err(ProbeError::classify(anyhow::anyhow!("BLE client is shutting down")));
            }
            backend = backend.lock_owned() => backend,
        };
        if let Err(error) = shutdown.ensure_open() {
            return Err(ProbeError::classify(error));
        }
        finish_without_cancelling(async move {
            run_cancellable_operation(&mut *backend, &shutdown, options).await
        })
        .await
        .map_err(ProbeError::classify)
    }
}

impl BleBackend {
    async fn cleanup_pending_until(&mut self, deadline: Instant) -> AnyhowResult<()> {
        self.active_session = None;
        let adapter = self.adapter.clone();
        drain_until_deadline(
            &mut self.pending_disconnect,
            &mut self.scan_pending,
            deadline,
            |peripheral| async move {
                disconnect_peripheral(&peripheral, platform_timeout(Duration::from_secs(3))).await
            },
            || async move {
                let adapter = adapter.context("pending BLE scan cleanup has no adapter")?;
                stop_ble_scan(&adapter, platform_timeout(Duration::from_secs(3))).await
            },
        )
        .await
    }

    fn can_reuse_session(&self, mode: &ProbeMode) -> bool {
        match (self.active_session.as_ref(), mode) {
            (
                Some(session),
                ProbeMode::Query {
                    device_id: Some(id),
                    ..
                },
            ) => crate::discovery::peripheral_id_matches(&session.device.id, id),
            _ => false,
        }
    }

    async fn query_active_session(&mut self, options: ProbeOptions) -> AnyhowResult<ProbeResult> {
        let ProbeMode::Query {
            control_command, ..
        } = options.mode
        else {
            anyhow::bail!("a retained BLE session requires a device query");
        };
        let query = self
            .query_connected_session(
                options.response_timeout,
                control_command,
                options.control_deadline,
                options.refresh_settings,
            )
            .await;
        self.finish_session_query(query, options.response_timeout)
            .await
    }

    async fn query_connected_session(
        &mut self,
        timeout: Duration,
        command: Option<ControlCommand>,
        deadline: Option<Instant>,
        refresh_settings: bool,
    ) -> AnyhowResult<crate::QueryResult> {
        let peripheral = self
            .pending_disconnect
            .as_ref()
            .context("retained BLE link is not tracked")?;
        if !complete_before(
            platform_timeout(timeout),
            "check retained BLE connection",
            async {
                peripheral
                    .is_connected()
                    .await
                    .context("check retained BLE connection")
            },
        )
        .await?
        {
            anyhow::bail!("retained GAF BLE connection was lost");
        }
        let session = self
            .active_session
            .as_mut()
            .context("BLE session is not initialized")?;
        session.requests.set_response_timeout(timeout);
        session
            .requests
            .query(command, deadline, refresh_settings)
            .await
    }

    async fn finish_session_query(
        &mut self,
        query: AnyhowResult<crate::QueryResult>,
        timeout: Duration,
    ) -> AnyhowResult<ProbeResult> {
        let mut query = query?;
        let device = self
            .active_session
            .as_ref()
            .context("BLE session is not initialized")?
            .device
            .clone();
        if query.state_error.is_some() {
            self.active_session = None;
            let cleanup = self.cleanup_pending(platform_timeout(timeout)).await;
            query.disconnect = crate::lifecycle::finish_with_cleanup(Ok(()), cleanup)?.1;
        }
        Ok(ProbeResult::Queried {
            device,
            result: Box::new(query),
        })
    }

    async fn cleanup_pending(&mut self, operation_timeout: Duration) -> AnyhowResult<()> {
        let adapter = self.adapter.clone();
        drain_pending_cleanup(
            &mut self.pending_disconnect,
            &mut self.scan_pending,
            |peripheral| async move { disconnect_peripheral(&peripheral, operation_timeout).await },
            || async move {
                let adapter = adapter.context("pending BLE scan cleanup has no adapter")?;
                stop_ble_scan(&adapter, operation_timeout).await
            },
        )
        .await
    }

    async fn probe(&mut self, options: ProbeOptions) -> AnyhowResult<ProbeResult> {
        if self.can_reuse_session(&options.mode) {
            return self.query_active_session(options).await;
        }
        self.active_session = None;
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

        let adapter_timeout = platform_timeout(options.response_timeout);
        #[cfg(target_os = "linux")]
        {
            // A successful Adapter1 GetAll validates the cached BlueZ object.
            // Re-enumerate only when that object is stale or the check fails.
            reuse_or_refresh_adapter(
                adapter,
                adapter_timeout,
                async |cached| {
                    cached
                        .adapter_address()
                        .await
                        .context("validate cached Bluetooth adapter")
                },
                || async { list_first_adapter(manager).await },
            )
            .await?;
        }
        #[cfg(not(target_os = "linux"))]
        if adapter.is_none() {
            *adapter = Some(
                complete_before(adapter_timeout, "list Bluetooth adapters", async {
                    list_first_adapter(manager).await
                })
                .await?,
            );
        }
        let selected_adapter = adapter.as_ref().expect("adapter initialized above");

        probe_with_adapter(
            selected_adapter,
            options,
            &mut self.pending_disconnect,
            &mut self.scan_pending,
            &mut self.active_session,
            #[cfg(any(target_os = "linux", test))]
            &mut self.startup_disconnect_pending,
        )
        .await
    }
}

async fn list_first_adapter(manager: &Manager) -> AnyhowResult<Adapter> {
    manager
        .adapters()
        .await
        .context("list Bluetooth adapters")?
        .into_iter()
        .next()
        .context("no Bluetooth adapter is available")
}

#[cfg(any(target_os = "linux", test))]
async fn reuse_or_refresh_adapter<T, A, Validate, Refresh, RefreshFuture>(
    cached: &mut Option<T>,
    timeout: Duration,
    validate: Validate,
    refresh: Refresh,
) -> AnyhowResult<()>
where
    Validate: for<'a> AsyncFnOnce(&'a T) -> AnyhowResult<Option<A>>,
    Refresh: FnOnce() -> RefreshFuture,
    RefreshFuture: Future<Output = AnyhowResult<T>>,
{
    let deadline = Instant::now() + timeout;
    if let Some(cached_adapter) = cached.as_ref()
        && let Ok(Ok(_)) = timeout_at(deadline, validate(cached_adapter)).await
    {
        return Ok(());
    }

    if Instant::now() >= deadline {
        anyhow::bail!("list Bluetooth adapters timed out");
    }

    let current = timeout_at(deadline, refresh())
        .await
        .context("list Bluetooth adapters timed out")??;
    *cached = Some(current);
    Ok(())
}

async fn finish_without_cancelling<T: Send + 'static>(
    operation: impl Future<Output = AnyhowResult<T>> + Send + 'static,
) -> AnyhowResult<T> {
    tokio::spawn(operation)
        .await
        .context("BLE operation task failed")?
}

async fn run_until_shutdown<T>(
    shutdown: &CancellationToken,
    operation: impl Future<Output = AnyhowResult<T>>,
) -> Result<AnyhowResult<T>, ()> {
    tokio::select! {
        biased;
        () = shutdown.cancelled() => Err(()),
        result = operation => Ok(result),
    }
}

trait CancellableBackend {
    type Output;

    fn has_retained_session(&self, _options: &ProbeOptions) -> bool {
        false
    }

    async fn run_operation(&mut self, options: ProbeOptions) -> AnyhowResult<Self::Output>;
    async fn cleanup_until(&mut self, deadline: Instant) -> AnyhowResult<()>;
}

impl CancellableBackend for BleBackend {
    type Output = ProbeResult;

    fn has_retained_session(&self, _options: &ProbeOptions) -> bool {
        self.can_reuse_session(&_options.mode)
            && self
                .active_session
                .as_ref()
                .is_some_and(|session| session.requests.is_initialized())
    }

    async fn run_operation(&mut self, options: ProbeOptions) -> AnyhowResult<Self::Output> {
        self.probe(options).await
    }

    async fn cleanup_until(&mut self, deadline: Instant) -> AnyhowResult<()> {
        self.cleanup_pending_until(deadline).await
    }
}

async fn run_cancellable_operation<B: CancellableBackend>(
    backend: &mut B,
    shutdown: &ShutdownState,
    options: ProbeOptions,
) -> AnyhowResult<B::Output> {
    let retained = backend.has_retained_session(&options);
    let cleanup_timeout = platform_timeout(options.response_timeout);
    match run_until_shutdown(&shutdown.token, backend.run_operation(options)).await {
        Ok(Err(error))
            if retained && ProbeError::kind_for(&error) == crate::ProbeErrorKind::StaleControl =>
        {
            Err(error)
        }
        Ok(Err(error)) => {
            let cleanup = backend
                .cleanup_until(Instant::now() + cleanup_timeout)
                .await;
            crate::lifecycle::fail_with_cleanup(error, cleanup)
        }
        Ok(Ok(result)) => Ok(result),
        Err(()) => {
            backend
                .cleanup_until(shutdown.cleanup_deadline()?)
                .await
                .context("BLE shutdown cleanup after cancelled operation")?;
            anyhow::bail!("BLE operation cancelled during shutdown");
        }
    }
}

async fn drain_until_deadline<T, D, DFut, S, SFut>(
    pending_disconnect: &mut Option<T>,
    scan_pending: &mut bool,
    deadline: Instant,
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
    drain_pending_cleanup(
        pending_disconnect,
        scan_pending,
        |peripheral| async move {
            timeout_at(deadline, disconnect(peripheral))
                .await
                .context("disconnect BLE peripheral before shutdown deadline")?
        },
        || async move {
            timeout_at(deadline, stop_scan())
                .await
                .context("stop BLE scan before shutdown deadline")?
        },
    )
    .await
}

#[cfg(any(target_os = "linux", test))]
async fn disconnect_before_first_query<T, D, DFut>(
    peripheral: &T,
    startup_disconnect_pending: &mut bool,
    pending_disconnect: &mut Option<T>,
    disconnect: D,
) -> AnyhowResult<()>
where
    T: Clone,
    D: FnOnce(T) -> DFut,
    DFut: Future<Output = AnyhowResult<()>>,
{
    if !*startup_disconnect_pending {
        return Ok(());
    }

    *pending_disconnect = Some(peripheral.clone());
    match disconnect(peripheral.clone()).await {
        Ok(()) => {
            *pending_disconnect = None;
            *startup_disconnect_pending = false;
            Ok(())
        }
        Err(error) => Err(error).context("disconnect stale configured BLE link before query"),
    }
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
    let client = ProbeClient::new();
    let result = client.probe(options).await;
    client.begin_shutdown();
    let cleanup = client.wait_until_idle().await.map_err(anyhow::Error::new);
    let (mut result, disconnect) =
        crate::lifecycle::finish_with_cleanup(result.map_err(anyhow::Error::new), cleanup)
            .map_err(ProbeError::classify)?;
    if let ProbeResult::Queried { result, .. } = &mut result {
        result.disconnect = disconnect;
    }
    Ok(result)
}

async fn probe_with_adapter(
    adapter: &Adapter,
    options: ProbeOptions,
    pending_disconnect: &mut Option<Peripheral>,
    scan_pending: &mut bool,
    active_session: &mut Option<ActiveSession>,
    #[cfg(any(target_os = "linux", test))] startup_disconnect_pending: &mut bool,
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

    match &options.mode {
        ProbeMode::Scan => Ok(summarize_scan(discovery)),
        ProbeMode::Query { .. } => {
            query_selected_device(
                discovery,
                &options,
                pending_disconnect,
                active_session,
                #[cfg(any(target_os = "linux", test))]
                startup_disconnect_pending,
            )
            .await
        }
    }
}

async fn query_selected_device(
    discovery: DiscoveryReport<Candidate>,
    options: &ProbeOptions,
    pending_disconnect: &mut Option<Peripheral>,
    active_session: &mut Option<ActiveSession>,
    #[cfg(any(target_os = "linux", test))] startup_disconnect_pending: &mut bool,
) -> AnyhowResult<ProbeResult> {
    let ProbeMode::Query {
        device_id,
        control_command,
    } = &options.mode
    else {
        anyhow::bail!("selected BLE device requires a query");
    };
    let device_id = device_id.as_deref();
    let response_timeout = options.response_timeout;
    if !can_query_with_report(&discovery, device_id) {
        return Ok(incomplete_result(discovery));
    }

    let failures = discovery.failures;
    match select_candidate(discovery.candidates, device_id)? {
        CandidateSelection::NoDevices => Ok(ProbeResult::NoDevices),
        CandidateSelection::Ambiguous(devices) => Ok(ProbeResult::Ambiguous { devices }),
        CandidateSelection::Chosen { device, peripheral } => {
            #[cfg(any(target_os = "linux", test))]
            if device_id.is_some() {
                disconnect_before_first_query(
                    &peripheral,
                    startup_disconnect_pending,
                    pending_disconnect,
                    |peripheral| async move {
                        disconnect_peripheral(&peripheral, platform_timeout(response_timeout)).await
                    },
                )
                .await?;
            }
            *pending_disconnect = Some(peripheral.clone());
            let requests = open_session(&peripheral, response_timeout).await?;
            *active_session = Some(ActiveSession {
                device: device.clone(),
                requests,
            });
            let session = active_session
                .as_mut()
                .context("BLE session is not initialized")?;
            let mut result = session
                .requests
                .query(
                    *control_command,
                    options.control_deadline,
                    options.refresh_settings,
                )
                .await?;
            if result.state_error.is_some() {
                *active_session = None;
                let cleanup =
                    disconnect_peripheral(&peripheral, platform_timeout(response_timeout)).await;
                if cleanup.is_ok() {
                    *pending_disconnect = None;
                }
                result.disconnect = crate::lifecycle::finish_with_cleanup(Ok(()), cleanup)?.1;
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
    use std::future;

    #[tokio::test]
    async fn closed_client_rejects_admission_without_opening_backend() {
        let client = super::ProbeClient::new();
        client.begin_shutdown();

        let error = client
            .probe(crate::ProbeOptions::default())
            .await
            .expect_err("shutdown must close probe admission");

        assert!(format!("{error:#}").contains("shutting down"));
        let backend = client.backend.lock().await;
        assert!(backend.manager.get().is_none());
    }

    #[tokio::test]
    async fn shutdown_cancels_active_operation_and_drains_tracked_link_and_scan() {
        struct PendingBackend {
            pending_disconnect: Option<(&'static str, bool)>,
            scan_pending: bool,
            disconnected: bool,
            scan_stopped: bool,
            disconnect_attempts: Vec<bool>,
            fail_first_disconnect: bool,
        }

        impl super::CancellableBackend for PendingBackend {
            type Output = ();

            async fn run_operation(
                &mut self,
                _options: crate::ProbeOptions,
            ) -> anyhow::Result<Self::Output> {
                future::pending().await
            }

            async fn cleanup_until(
                &mut self,
                deadline: tokio::time::Instant,
            ) -> anyhow::Result<()> {
                let fail_disconnect = std::mem::replace(&mut self.fail_first_disconnect, false);
                let disconnect_attempts = &mut self.disconnect_attempts;
                let disconnected = &mut self.disconnected;
                let scan_stopped = &mut self.scan_stopped;
                super::drain_until_deadline(
                    &mut self.pending_disconnect,
                    &mut self.scan_pending,
                    deadline,
                    |(id, connected)| {
                        disconnect_attempts.push(connected);
                        *disconnected = true;
                        async move {
                            assert_eq!(id, "selected peripheral");
                            assert!(!connected, "disconnect runs before Connected is true");
                            if fail_disconnect {
                                Err(anyhow::anyhow!("simulated disconnect timeout"))
                            } else {
                                Ok(())
                            }
                        }
                    },
                    || {
                        *scan_stopped = true;
                        future::ready(Ok(()))
                    },
                )
                .await
            }
        }

        let shutdown = super::ShutdownState::new();
        let mut backend = PendingBackend {
            pending_disconnect: Some(("selected peripheral", false)),
            scan_pending: true,
            disconnected: false,
            scan_stopped: false,
            disconnect_attempts: Vec::new(),
            fail_first_disconnect: true,
        };
        let result = {
            let run = super::run_cancellable_operation(
                &mut backend,
                &shutdown,
                crate::ProbeOptions::default(),
            );
            tokio::pin!(run);
            tokio::select! {
                biased;
                result = &mut run => panic!("operation unexpectedly finished: {result:?}"),
                () = tokio::task::yield_now() => shutdown.begin(),
            }
            run.await
        };

        assert!(result.is_err(), "failed disconnect must remain a failure");
        assert!(backend.disconnected);
        assert!(backend.scan_stopped);
        assert_eq!(
            backend.pending_disconnect,
            Some(("selected peripheral", false))
        );
        assert!(!backend.scan_pending);
        assert_eq!(backend.disconnect_attempts, [false]);

        super::CancellableBackend::cleanup_until(
            &mut backend,
            shutdown.cleanup_deadline().unwrap(),
        )
        .await
        .expect("waiter retries the same tracked pending connection");
        assert!(backend.pending_disconnect.is_none());
        assert_eq!(backend.disconnect_attempts, [false, false]);
    }

    #[tokio::test]
    async fn expired_controls_clean_cold_sessions_and_preserve_reusable_sessions() {
        struct ExpiringBackend {
            reusable: bool,
            cleaned: bool,
        }
        impl super::CancellableBackend for ExpiringBackend {
            type Output = ();
            fn has_retained_session(&self, _options: &crate::ProbeOptions) -> bool {
                self.reusable
            }
            async fn run_operation(&mut self, _options: crate::ProbeOptions) -> anyhow::Result<()> {
                Err(crate::error::ControlExpired.into())
            }
            async fn cleanup_until(
                &mut self,
                _deadline: tokio::time::Instant,
            ) -> anyhow::Result<()> {
                self.cleaned = true;
                Ok(())
            }
        }
        for reusable in [false, true] {
            let mut backend = ExpiringBackend {
                reusable,
                cleaned: false,
            };
            let error = super::run_cancellable_operation(
                &mut backend,
                &super::ShutdownState::new(),
                crate::ProbeOptions::default(),
            )
            .await
            .unwrap_err();
            assert_eq!(
                crate::ProbeError::classify(error).kind(),
                crate::ProbeErrorKind::StaleControl
            );
            assert_eq!(backend.cleaned, !reusable);
        }
    }

    #[tokio::test]
    async fn shutdown_drain_reuses_one_absolute_deadline() {
        let shutdown = super::ShutdownState::new();
        shutdown.begin();
        let deadline = shutdown.cleanup_deadline().unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        assert_eq!(shutdown.cleanup_deadline().unwrap(), deadline);
    }

    #[tokio::test]
    async fn queued_probe_is_rejected_when_shutdown_closes_admission() {
        use std::sync::Arc;

        let client = Arc::new(super::ProbeClient::new());
        let guard = Arc::clone(&client.backend).lock_owned().await;
        let queued_client = Arc::clone(&client);
        let queued =
            tokio::spawn(async move { queued_client.probe(crate::ProbeOptions::default()).await });
        tokio::task::yield_now().await;
        client.begin_shutdown();
        drop(guard);

        let error = queued
            .await
            .unwrap()
            .expect_err("queued work must be rejected");
        assert!(format!("{error:#}").contains("shutting down"));
        assert!(client.backend.lock().await.manager.get().is_none());
    }

    #[tokio::test]
    async fn admission_is_rechecked_after_backend_lock_before_detaching() {
        let client = super::ProbeClient::new();
        let backend = client.backend.clone().lock_owned().await;

        client.shutdown.ensure_open().expect("client starts open");
        client.begin_shutdown();
        assert!(client.shutdown.ensure_open().is_err());
        assert!(backend.manager.get().is_none());
    }

    #[tokio::test]
    async fn configured_first_query_disconnect_retries_before_connecting() {
        use std::cell::Cell;

        let disconnect_attempts = Cell::new(0);
        let mut startup_disconnect_pending = true;
        let mut pending_disconnect = None;

        let error = super::disconnect_before_first_query(
            &"selected configured peripheral",
            &mut startup_disconnect_pending,
            &mut pending_disconnect,
            |_| {
                disconnect_attempts.set(disconnect_attempts.get() + 1);
                future::ready(Err(anyhow::anyhow!("stale local link could not be closed")))
            },
        )
        .await
        .expect_err("failed stale-link cleanup must block the query");

        assert!(format!("{error:#}").contains("stale local link"));
        assert_eq!(disconnect_attempts.get(), 1);
        assert!(startup_disconnect_pending);
        assert_eq!(pending_disconnect, Some("selected configured peripheral"));

        super::disconnect_before_first_query(
            &"selected configured peripheral",
            &mut startup_disconnect_pending,
            &mut pending_disconnect,
            |_| {
                disconnect_attempts.set(disconnect_attempts.get() + 1);
                future::ready(Ok(()))
            },
        )
        .await
        .expect("a later query retries the exact tracked peripheral");

        assert_eq!(disconnect_attempts.get(), 2);
        assert!(!startup_disconnect_pending);
        assert!(pending_disconnect.is_none());
    }

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

    #[tokio::test]
    async fn cached_adapter_validation_reuses_adapter_even_without_address() {
        use std::cell::Cell;

        let validations = Cell::new(0);
        let enumerations = Cell::new(0);
        let mut cached = Some("cached");
        super::reuse_or_refresh_adapter(
            &mut cached,
            std::time::Duration::from_secs(1),
            async |_| {
                validations.set(validations.get() + 1);
                Ok(None::<()>)
            },
            || {
                enumerations.set(enumerations.get() + 1);
                async { Ok("replacement") }
            },
        )
        .await
        .expect("an adapter that answers GetAll is still live");

        assert_eq!(cached, Some("cached"));
        assert_eq!(validations.get(), 1);
        assert_eq!(enumerations.get(), 0);
    }

    #[tokio::test]
    async fn missing_cached_adapter_is_enumerated() {
        use std::cell::Cell;

        let validations = Cell::new(0);
        let enumerations = Cell::new(0);
        let mut cached = None;
        super::reuse_or_refresh_adapter(
            &mut cached,
            std::time::Duration::from_secs(1),
            async |_| {
                validations.set(validations.get() + 1);
                Ok(None::<()>)
            },
            || {
                enumerations.set(enumerations.get() + 1);
                async { Ok("first adapter") }
            },
        )
        .await
        .expect("the first probe should enumerate adapters");

        assert_eq!(cached, Some("first adapter"));
        assert_eq!(validations.get(), 0);
        assert_eq!(enumerations.get(), 1);
    }

    #[tokio::test]
    async fn failed_adapter_validation_replaces_and_then_reuses_adapter() {
        use std::cell::Cell;

        let enumerations = Cell::new(0);
        let mut cached = Some("stale");
        super::reuse_or_refresh_adapter(
            &mut cached,
            std::time::Duration::from_secs(1),
            async |_| Err::<Option<()>, _>(anyhow::anyhow!("adapter object disappeared")),
            || {
                enumerations.set(enumerations.get() + 1);
                async { Ok("replacement") }
            },
        )
        .await
        .expect("a stale adapter should be refreshed");
        assert_eq!(cached, Some("replacement"));
        assert_eq!(enumerations.get(), 1);

        super::reuse_or_refresh_adapter(
            &mut cached,
            std::time::Duration::from_secs(1),
            async |_| Ok(Some(())),
            || {
                enumerations.set(enumerations.get() + 1);
                async { Ok("unexpected second replacement") }
            },
        )
        .await
        .expect("the replacement should be cached on the next probe");
        assert_eq!(cached, Some("replacement"));
        assert_eq!(enumerations.get(), 1);
    }

    #[tokio::test]
    async fn adapter_enumeration_failure_is_returned() {
        let mut cached = Some("stale");
        let error = super::reuse_or_refresh_adapter(
            &mut cached,
            std::time::Duration::from_secs(1),
            async |_| Err::<Option<()>, _>(anyhow::anyhow!("adapter object disappeared")),
            || async { Err(anyhow::anyhow!("D-Bus enumeration failed")) },
        )
        .await
        .expect_err("refresh failure must be returned");

        assert!(format!("{error:#}").contains("D-Bus enumeration failed"));
        assert_eq!(cached, Some("stale"));
    }

    #[tokio::test(start_paused = true)]
    async fn adapter_validation_and_enumeration_share_one_timeout() {
        let mut cached = Some("stale");
        let error = super::reuse_or_refresh_adapter(
            &mut cached,
            std::time::Duration::from_secs(1),
            async |_| {
                tokio::time::sleep(std::time::Duration::from_millis(750)).await;
                Err::<Option<()>, _>(anyhow::anyhow!("adapter object disappeared"))
            },
            || async {
                tokio::time::sleep(std::time::Duration::from_millis(750)).await;
                Ok("replacement")
            },
        )
        .await
        .expect_err("validation and enumeration must share one deadline");

        assert!(format!("{error:#}").contains("list Bluetooth adapters timed out"));
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
