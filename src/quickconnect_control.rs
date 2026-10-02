use std::{
    future::{Future, ready},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, SystemTime},
};

use futures_util::{StreamExt, stream};
use tokio::{
    sync::RwLock,
    time::{Instant, sleep, timeout},
};
use updraft_quickconnect::{
    ClientError, QuickConnectClient, QuickConnectCommand, QuickConnectSettings,
    QuickConnectSettingsBody,
};

use crate::{
    backend::{DeviceRegistry, DeviceRuntime},
    control::unix_millis,
    device::DeviceId,
};

const DEFAULT_MAX_COMMAND_AGE: Duration = Duration::from_secs(30);
const DEFAULT_MAX_FUTURE_SKEW: Duration = Duration::from_secs(5);
const DEFAULT_READBACK_TIMEOUT: Duration = Duration::from_secs(60);
const DEFAULT_READBACK_INTERVAL: Duration = Duration::from_secs(2);
const DEFAULT_READBACK_ATTEMPTS: u16 = 31;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QuickConnectControlStatus {
    Rejected,
    SubmittedUnconfirmed,
    ReadbackMismatch,
    ReadbackUnavailable,
    Confirmed,
}

#[derive(Clone, Debug)]
pub struct QuickConnectControlOutcome {
    request_id: Arc<str>,
    status: QuickConnectControlStatus,
}

impl QuickConnectControlOutcome {
    pub fn request_id(&self) -> &str {
        &self.request_id
    }

    pub const fn status(&self) -> QuickConnectControlStatus {
        self.status
    }

    fn new(request_id: &Arc<str>, status: QuickConnectControlStatus) -> Self {
        Self {
            request_id: Arc::clone(request_id),
            status,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct QuickConnectControlPolicy {
    max_command_age: Duration,
    max_future_skew: Duration,
    readback_timeout: Duration,
    readback_interval: Duration,
    readback_attempts: u16,
}

impl Default for QuickConnectControlPolicy {
    fn default() -> Self {
        Self {
            max_command_age: DEFAULT_MAX_COMMAND_AGE,
            max_future_skew: DEFAULT_MAX_FUTURE_SKEW,
            readback_timeout: DEFAULT_READBACK_TIMEOUT,
            readback_interval: DEFAULT_READBACK_INTERVAL,
            readback_attempts: DEFAULT_READBACK_ATTEMPTS,
        }
    }
}

impl QuickConnectControlPolicy {
    pub fn with_command_freshness(mut self, max_age: Duration, max_future_skew: Duration) -> Self {
        self.max_command_age = max_age;
        self.max_future_skew = max_future_skew;
        self
    }

    pub fn with_readback(mut self, timeout: Duration, interval: Duration, attempts: u16) -> Self {
        self.readback_timeout = timeout;
        self.readback_interval = interval;
        self.readback_attempts = attempts.max(1);
        self
    }

    #[cfg(test)]
    fn for_test() -> Self {
        Self::default()
            .with_command_freshness(Duration::from_secs(60), Duration::ZERO)
            .with_readback(Duration::from_secs(2), Duration::ZERO, 1)
    }
}

#[derive(Clone, Debug)]
pub struct QuickConnectControlIntent {
    request_id: Arc<str>,
    issued_at_unix_ms: u64,
    command: QuickConnectCommand,
}

impl QuickConnectControlIntent {
    pub fn new(
        request_id: &str,
        issued_at_unix_ms: u64,
        command: QuickConnectCommand,
    ) -> Option<Self> {
        let valid_id = !request_id.is_empty()
            && request_id.len() <= 64
            && request_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-');
        valid_id.then(|| Self {
            request_id: Arc::from(request_id),
            issued_at_unix_ms,
            command,
        })
    }

    fn is_fresh_at(&self, now_unix_ms: u64, policy: QuickConnectControlPolicy) -> bool {
        let age = policy
            .max_command_age
            .as_millis()
            .try_into()
            .unwrap_or(u64::MAX);
        let skew = policy
            .max_future_skew
            .as_millis()
            .try_into()
            .unwrap_or(u64::MAX);
        match self.issued_at_unix_ms.checked_sub(now_unix_ms) {
            Some(future_ms) => future_ms <= skew,
            None => now_unix_ms - self.issued_at_unix_ms <= age,
        }
    }
}

#[derive(Clone)]
pub struct QuickConnectControlService {
    registry: Arc<RwLock<DeviceRegistry>>,
    client: QuickConnectClient,
    account_id: Arc<str>,
    policy: QuickConnectControlPolicy,
}

impl QuickConnectControlService {
    pub fn new(
        registry: Arc<RwLock<DeviceRegistry>>,
        client: QuickConnectClient,
        account_id: impl Into<Arc<str>>,
        policy: QuickConnectControlPolicy,
    ) -> Self {
        Self {
            registry,
            client,
            account_id: account_id.into(),
            policy,
        }
    }

    pub async fn execute(
        &self,
        id: &DeviceId,
        intent: QuickConnectControlIntent,
    ) -> QuickConnectControlOutcome {
        if !self.is_fresh(&intent) {
            return QuickConnectControlOutcome::new(
                &intent.request_id,
                QuickConnectControlStatus::Rejected,
            );
        }
        let target = self.control_target(id, intent.command).await;
        let Ok((runtime, provider_id)) = target else {
            return QuickConnectControlOutcome::new(
                &intent.request_id,
                QuickConnectControlStatus::Rejected,
            );
        };
        let Some(_queue_permit) = runtime.try_reserve_control() else {
            return QuickConnectControlOutcome::new(
                &intent.request_id,
                QuickConnectControlStatus::Rejected,
            );
        };
        let generation = runtime.begin_control_intent();
        let _transaction = runtime.acquire_transaction().await;
        if !self
            .is_current(&intent, id, &runtime, &provider_id, generation)
            .await
        {
            return QuickConnectControlOutcome::new(
                &intent.request_id,
                QuickConnectControlStatus::Rejected,
            );
        }

        let status = self
            .execute_locked(id, &intent, &runtime, &provider_id, generation)
            .await;
        QuickConnectControlOutcome::new(&intent.request_id, status)
    }

    async fn execute_locked(
        &self,
        id: &DeviceId,
        intent: &QuickConnectControlIntent,
        runtime: &Arc<DeviceRuntime>,
        provider_id: &str,
        generation: u64,
    ) -> QuickConnectControlStatus {
        let Some(before) = self
            .read_current_state(id, intent, runtime, provider_id, generation)
            .await
        else {
            return QuickConnectControlStatus::Rejected;
        };
        let body =
            match updraft_quickconnect::build_settings_body(&intent.command, &before.settings) {
                Ok(body) => body,
                Err(updraft_quickconnect::QuickConnectCommandError::ModeAlreadyInactive) => {
                    return if runtime
                        .set_control_state_if_current(
                            generation,
                            crate::backend::common_state(before),
                        )
                        .await
                    {
                        QuickConnectControlStatus::Confirmed
                    } else {
                        QuickConnectControlStatus::Rejected
                    };
                }
                Err(_) => return QuickConnectControlStatus::Rejected,
            };
        let Some(Ok(prepared)) = self
            .while_current(runtime, generation, self.client.prepare_settings_write())
            .await
        else {
            return QuickConnectControlStatus::Rejected;
        };
        if !self
            .is_current(intent, id, runtime, provider_id, generation)
            .await
        {
            return QuickConnectControlStatus::Rejected;
        }
        match self
            .client
            .save_device_settings_prepared(provider_id, &body, prepared)
            .await
        {
            Ok(_) => {
                self.confirm_readback(runtime, generation, provider_id, &before, &body)
                    .await
            }
            Err(error) if is_rejected_write(&error) => QuickConnectControlStatus::Rejected,
            Err(_) => {
                self.read_and_publish(runtime, generation, provider_id)
                    .await;
                QuickConnectControlStatus::SubmittedUnconfirmed
            }
        }
    }

    async fn read_current_state(
        &self,
        id: &DeviceId,
        intent: &QuickConnectControlIntent,
        runtime: &Arc<DeviceRuntime>,
        provider_id: &str,
        generation: u64,
    ) -> Option<updraft_quickconnect::QuickConnectDeviceState> {
        let before = self
            .while_current(
                runtime,
                generation,
                self.client.read_device_state(provider_id),
            )
            .await?
            .ok()?;
        (self
            .is_current(intent, id, runtime, provider_id, generation)
            .await
            && fresh_state(&before))
        .then_some(before)
    }

    async fn control_target(
        &self,
        id: &DeviceId,
        command: QuickConnectCommand,
    ) -> Result<(Arc<DeviceRuntime>, String), ()> {
        self.registry
            .read()
            .await
            .quickconnect_control_target(&self.account_id, id, command)
            .map_err(|_| ())
    }

    fn is_fresh(&self, intent: &QuickConnectControlIntent) -> bool {
        unix_millis(SystemTime::now()).is_some_and(|now| intent.is_fresh_at(now, self.policy))
    }

    async fn while_current<T>(
        &self,
        runtime: &DeviceRuntime,
        generation: u64,
        operation: impl Future<Output = T>,
    ) -> Option<T> {
        tokio::select! {
            _ = runtime.wait_for_control_change(generation) => None,
            result = timeout(self.policy.readback_timeout, operation) => result.ok(),
        }
    }

    async fn is_current(
        &self,
        intent: &QuickConnectControlIntent,
        id: &DeviceId,
        runtime: &Arc<DeviceRuntime>,
        provider_id: &str,
        generation: u64,
    ) -> bool {
        let matches_target = self
            .control_target_matches(id, intent.command, runtime, provider_id)
            .await;
        matches_target && self.is_fresh(intent) && runtime.is_current_control_intent(generation)
    }

    async fn control_target_matches(
        &self,
        id: &DeviceId,
        command: QuickConnectCommand,
        runtime: &Arc<DeviceRuntime>,
        provider_id: &str,
    ) -> bool {
        let target =
            self.registry
                .read()
                .await
                .quickconnect_control_target(&self.account_id, id, command);
        target.is_ok_and(|(current_runtime, current_provider_id)| {
            Arc::ptr_eq(&current_runtime, runtime) && current_provider_id == provider_id
        })
    }

    async fn confirm_readback(
        &self,
        runtime: &DeviceRuntime,
        generation: u64,
        provider_id: &str,
        before: &updraft_quickconnect::QuickConnectDeviceState,
        body: &QuickConnectSettingsBody,
    ) -> QuickConnectControlStatus {
        let result = self
            .readback(provider_id, &before.settings, body, runtime, generation)
            .await;
        if !runtime.is_current_control_intent(generation) {
            return QuickConnectControlStatus::SubmittedUnconfirmed;
        }
        let status = if result.matched {
            QuickConnectControlStatus::Confirmed
        } else if result.state.is_some() {
            QuickConnectControlStatus::ReadbackMismatch
        } else {
            QuickConnectControlStatus::ReadbackUnavailable
        };
        match result.state {
            Some(state) => {
                if !runtime
                    .set_control_state_if_current(generation, crate::backend::common_state(state))
                    .await
                {
                    return QuickConnectControlStatus::SubmittedUnconfirmed;
                }
            }
            None => {
                if !runtime
                    .mark_control_state_unavailable_if_current(generation)
                    .await
                {
                    return QuickConnectControlStatus::SubmittedUnconfirmed;
                }
            }
        }
        status
    }

    async fn readback(
        &self,
        provider_id: &str,
        before: &QuickConnectSettings,
        body: &QuickConnectSettingsBody,
        runtime: &DeviceRuntime,
        generation: u64,
    ) -> ReadbackProgress {
        let deadline = Instant::now() + self.policy.readback_timeout;
        let matched = Arc::new(AtomicBool::new(false));
        let continue_polling = Arc::clone(&matched);
        stream::iter(0..self.policy.readback_attempts.max(1))
            .take_while(move |_| {
                ready(
                    !continue_polling.load(Ordering::Acquire)
                        && runtime.is_current_control_intent(generation),
                )
            })
            .then(|attempt| async move {
                let poll = async move {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if attempt > 0 && !self.policy.readback_interval.is_zero() {
                        sleep(self.policy.readback_interval.min(remaining)).await;
                    }
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        None
                    } else {
                        Some(timeout(remaining, self.client.read_device_state(provider_id)).await)
                    }
                };
                tokio::select! {
                    _ = runtime.wait_for_control_change(generation) => None,
                    result = poll => result,
                }
            })
            .fold(ReadbackProgress::default(), |progress, result| {
                let matched = Arc::clone(&matched);
                async move {
                    match result {
                        Some(Ok(Ok(state))) => {
                            let is_match = body.matches_readback(before, &state.settings);
                            matched.store(is_match, Ordering::Release);
                            ReadbackProgress {
                                matched: is_match,
                                state: Some(state),
                            }
                        }
                        Some(Ok(Err(_)) | Err(_)) | None => progress,
                    }
                }
            })
            .await
    }

    async fn read_and_publish(&self, runtime: &DeviceRuntime, generation: u64, provider_id: &str) {
        let read = timeout(
            self.policy.readback_timeout,
            self.client.read_device_state(provider_id),
        );
        let result = tokio::select! {
            _ = runtime.wait_for_control_change(generation) => return,
            result = read => result,
        };
        match result {
            Ok(Ok(state)) => {
                runtime
                    .set_control_state_if_current(generation, crate::backend::common_state(state))
                    .await;
            }
            Ok(Err(_)) | Err(_) => {
                runtime
                    .mark_control_state_unavailable_if_current(generation)
                    .await;
            }
        }
    }
}

#[derive(Default)]
struct ReadbackProgress {
    matched: bool,
    state: Option<updraft_quickconnect::QuickConnectDeviceState>,
}

fn fresh_state(state: &updraft_quickconnect::QuickConnectDeviceState) -> bool {
    state.fetched_at_unix_ms.is_some_and(|fetched_at| {
        unix_millis(SystemTime::now())
            .and_then(|now| now.checked_sub(fetched_at))
            .is_some_and(|age| age <= 90_000)
    })
}

fn is_rejected_write(error: &ClientError) -> bool {
    match error {
        ClientError::Authentication | ClientError::ServiceStatus(_) => true,
        ClientError::HttpStatus(status) => (400..500).contains(status),
        ClientError::Transport
        | ClientError::ResponseTooLarge(_)
        | ClientError::InvalidJson
        | ClientError::InvalidEnvelope
        | ClientError::InvalidEndpoint => false,
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Arc,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
        time::Duration,
        time::{SystemTime, UNIX_EPOCH},
    };

    use axum::{
        Json, Router, extract::State, http::StatusCode, response::IntoResponse, routing::get,
        routing::post,
    };
    use serde_json::{Value, json};
    use tokio::{sync::RwLock, time::sleep};

    use crate::{
        backend::{CloudDeviceInput, DeviceRegistry},
        device::DeviceId,
        quickconnect_control::{
            QuickConnectControlIntent, QuickConnectControlPolicy, QuickConnectControlService,
            QuickConnectControlStatus,
        },
    };
    use updraft_quickconnect::{
        AccountRole, Credentials, QuickConnectClient, QuickConnectCommand, QuickConnectConfig,
    };

    #[derive(Clone)]
    struct MockState {
        detail_reads: Arc<AtomicUsize>,
        settings_writes: Arc<AtomicUsize>,
        login_delay_ms: Arc<AtomicUsize>,
        detail_delay_ms: Arc<AtomicUsize>,
        pre_detail_delay_ms: Arc<AtomicUsize>,
        delayed_pre_details: Arc<AtomicUsize>,
        post_detail_delay_ms: Arc<AtomicUsize>,
        delayed_post_details: Arc<AtomicUsize>,
        login_started: Arc<tokio::sync::Notify>,
        mismatch_preserved_humidity: Arc<AtomicBool>,
        mismatch_once: Arc<AtomicBool>,
        fail_detail_after_write: Arc<AtomicBool>,
        post_status: Arc<AtomicUsize>,
        saved_body: Arc<tokio::sync::Mutex<Option<Value>>>,
        post_readback_started: Arc<tokio::sync::Notify>,
        pre_read_started: Arc<tokio::sync::Notify>,
    }

    struct ControlFixture {
        service: QuickConnectControlService,
        device_id: DeviceId,
        registry: Arc<RwLock<DeviceRegistry>>,
        mock: MockState,
        server: tokio::task::JoinHandle<()>,
        store_path: std::path::PathBuf,
    }

    async fn control_fixture(
        policy: QuickConnectControlPolicy,
        writes_enabled: bool,
    ) -> ControlFixture {
        let mock = MockState {
            detail_reads: Arc::new(AtomicUsize::new(0)),
            settings_writes: Arc::new(AtomicUsize::new(0)),
            login_delay_ms: Arc::new(AtomicUsize::new(0)),
            detail_delay_ms: Arc::new(AtomicUsize::new(0)),
            pre_detail_delay_ms: Arc::new(AtomicUsize::new(0)),
            delayed_pre_details: Arc::new(AtomicUsize::new(0)),
            post_detail_delay_ms: Arc::new(AtomicUsize::new(0)),
            delayed_post_details: Arc::new(AtomicUsize::new(0)),
            login_started: Arc::new(tokio::sync::Notify::new()),
            mismatch_preserved_humidity: Arc::new(AtomicBool::new(false)),
            mismatch_once: Arc::new(AtomicBool::new(false)),
            fail_detail_after_write: Arc::new(AtomicBool::new(false)),
            post_status: Arc::new(AtomicUsize::new(200)),
            saved_body: Arc::new(tokio::sync::Mutex::new(None)),
            post_readback_started: Arc::new(tokio::sync::Notify::new()),
            pre_read_started: Arc::new(tokio::sync::Notify::new()),
        };
        let app = Router::new()
            .route("/cognito/login", post(login))
            .route("/gaf/device", get(detail))
            .route("/gaf/deviceMode/provider-fan", post(save_settings))
            .with_state(mock.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let base = reqwest::Url::parse(&format!("http://{address}/")).unwrap();
        let client = QuickConnectClient::new(
            Credentials::new(
                "synthetic-user",
                "synthetic-password",
                AccountRole::Contractor,
            ),
            QuickConnectConfig::new(base.join("cognito/").unwrap(), base.join("gaf/").unwrap()),
        )
        .unwrap();
        let store_path = std::env::temp_dir()
            .join(format!("updraft-control-{}", uuid::Uuid::new_v4()))
            .join("identities.json");
        let mut registry = DeviceRegistry::load(&store_path).unwrap();
        let device_id = registry
            .reconcile_quickconnect(
                "synthetic-account",
                &[CloudDeviceInput::new(
                    "provider-fan".to_owned(),
                    "Synthetic fan".to_owned(),
                )],
            )
            .unwrap()
            .into_iter()
            .next()
            .unwrap();
        registry.set_quickconnect_writes_enabled(writes_enabled);
        let registry = Arc::new(RwLock::new(registry));
        let service = QuickConnectControlService::new(
            Arc::clone(&registry),
            client,
            "synthetic-account",
            policy,
        );
        ControlFixture {
            service,
            device_id,
            registry,
            mock,
            server,
            store_path,
        }
    }

    fn fresh_intent(request_id: &str, command: QuickConnectCommand) -> QuickConnectControlIntent {
        let now_unix_ms = u64::try_from(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis(),
        )
        .unwrap();
        QuickConnectControlIntent::new(request_id, now_unix_ms, command).unwrap()
    }

    fn automatic_target_change() -> QuickConnectCommand {
        QuickConnectCommand::SetAutomaticTargets {
            temperature_f: Some(110),
            humidity_percent: None,
        }
    }

    fn clean_up(fixture: ControlFixture) {
        fixture.server.abort();
        std::fs::remove_dir_all(fixture.store_path.parent().unwrap()).ok();
    }

    #[tokio::test]
    async fn conditional_off_of_inactive_mode_confirms_without_writing() {
        let fixture = control_fixture(QuickConnectControlPolicy::for_test(), true).await;
        let outcome = fixture
            .service
            .execute(
                &fixture.device_id,
                fresh_intent(
                    "inactive-timer-off",
                    QuickConnectCommand::ClearMode {
                        mode: updraft_quickconnect::QuickConnectCommandMode::Timer,
                    },
                ),
            )
            .await;
        assert_eq!(outcome.status(), QuickConnectControlStatus::Confirmed);
        assert_eq!(fixture.mock.settings_writes.load(Ordering::SeqCst), 0);
        assert_eq!(fixture.mock.detail_reads.load(Ordering::SeqCst), 1);
        clean_up(fixture);
    }

    #[tokio::test]
    async fn changed_preserved_value_after_successful_post_is_a_readback_mismatch() {
        let fixture = control_fixture(QuickConnectControlPolicy::for_test(), true).await;
        fixture
            .mock
            .mismatch_preserved_humidity
            .store(true, Ordering::SeqCst);

        let outcome = fixture
            .service
            .execute(
                &fixture.device_id,
                fresh_intent("request-1", automatic_target_change()),
            )
            .await;

        assert_eq!(
            outcome.status(),
            QuickConnectControlStatus::ReadbackMismatch
        );
        assert_eq!(outcome.request_id(), "request-1");
        assert_eq!(fixture.mock.settings_writes.load(Ordering::SeqCst), 1);
        assert_eq!(fixture.mock.detail_reads.load(Ordering::SeqCst), 2);
        clean_up(fixture);
    }

    #[tokio::test]
    async fn disabled_write_capability_rejects_before_cloud_io() {
        let fixture = control_fixture(QuickConnectControlPolicy::for_test(), false).await;
        let outcome = fixture
            .service
            .execute(
                &fixture.device_id,
                fresh_intent("request-2", automatic_target_change()),
            )
            .await;

        assert_eq!(outcome.status(), QuickConnectControlStatus::Rejected);
        assert_eq!(fixture.mock.settings_writes.load(Ordering::SeqCst), 0);
        assert_eq!(fixture.mock.detail_reads.load(Ordering::SeqCst), 0);
        clean_up(fixture);
    }

    #[tokio::test]
    async fn stale_command_after_authentication_never_posts() {
        let policy = QuickConnectControlPolicy::for_test()
            .with_command_freshness(Duration::from_millis(25), Duration::ZERO);
        let fixture = control_fixture(policy, true).await;
        fixture.mock.login_delay_ms.store(60, Ordering::SeqCst);

        let outcome = fixture
            .service
            .execute(
                &fixture.device_id,
                fresh_intent("request-3", automatic_target_change()),
            )
            .await;

        assert_eq!(outcome.status(), QuickConnectControlStatus::Rejected);
        assert_eq!(fixture.mock.settings_writes.load(Ordering::SeqCst), 0);
        clean_up(fixture);
    }

    #[tokio::test]
    async fn disabled_gate_immediately_before_post_rejects_without_writing() {
        let fixture = control_fixture(QuickConnectControlPolicy::for_test(), true).await;
        fixture.mock.login_delay_ms.store(60, Ordering::SeqCst);
        let login_started = Arc::clone(&fixture.mock.login_started);
        let registry = Arc::clone(&fixture.registry);
        let outcome = fixture.service.execute(
            &fixture.device_id,
            fresh_intent("request-gate", automatic_target_change()),
        );
        let disable_gate = async move {
            login_started.notified().await;
            registry
                .write()
                .await
                .set_quickconnect_writes_enabled(false);
        };
        let (outcome, ()) = tokio::join!(outcome, disable_gate);

        assert_eq!(outcome.status(), QuickConnectControlStatus::Rejected);
        assert_eq!(fixture.mock.settings_writes.load(Ordering::SeqCst), 0);
        clean_up(fixture);
    }

    #[tokio::test]
    async fn matching_readback_confirms_only_after_the_single_post() {
        let fixture = control_fixture(QuickConnectControlPolicy::for_test(), true).await;

        let outcome = fixture
            .service
            .execute(
                &fixture.device_id,
                fresh_intent(
                    "request-4",
                    QuickConnectCommand::SetTimerDuration {
                        duration_minutes: 90,
                    },
                ),
            )
            .await;

        assert_eq!(outcome.status(), QuickConnectControlStatus::Confirmed);
        assert_eq!(fixture.mock.settings_writes.load(Ordering::SeqCst), 1);
        assert_eq!(fixture.mock.detail_reads.load(Ordering::SeqCst), 2);
        clean_up(fixture);
    }

    #[tokio::test]
    async fn ambiguous_post_response_is_unconfirmed_and_never_replayed() {
        let fixture = control_fixture(QuickConnectControlPolicy::for_test(), true).await;
        fixture.mock.post_status.store(500, Ordering::SeqCst);

        let outcome = fixture
            .service
            .execute(
                &fixture.device_id,
                fresh_intent("request-ambiguous", automatic_target_change()),
            )
            .await;

        assert_eq!(
            outcome.status(),
            QuickConnectControlStatus::SubmittedUnconfirmed
        );
        assert_eq!(fixture.mock.settings_writes.load(Ordering::SeqCst), 1);
        clean_up(fixture);
    }

    #[tokio::test]
    async fn accepted_post_without_readback_is_reported_unavailable() {
        let fixture = control_fixture(QuickConnectControlPolicy::for_test(), true).await;
        fixture
            .mock
            .fail_detail_after_write
            .store(true, Ordering::SeqCst);

        let outcome = fixture
            .service
            .execute(
                &fixture.device_id,
                fresh_intent("request-unavailable", automatic_target_change()),
            )
            .await;

        assert_eq!(
            outcome.status(),
            QuickConnectControlStatus::ReadbackUnavailable
        );
        assert_eq!(fixture.mock.settings_writes.load(Ordering::SeqCst), 1);
        assert!(fixture.mock.detail_reads.load(Ordering::SeqCst) >= 2);
        clean_up(fixture);
    }

    #[tokio::test]
    async fn newer_intent_cancels_superseded_readback_and_runs_before_expiring() {
        let policy = QuickConnectControlPolicy::for_test().with_readback(
            Duration::from_secs(10),
            Duration::from_secs(5),
            10,
        );
        let fixture = control_fixture(policy, true).await;
        fixture.mock.mismatch_once.store(true, Ordering::SeqCst);
        let first_service = fixture.service.clone();
        let first_device = fixture.device_id.clone();
        let wait_for_readback = Arc::clone(&fixture.mock.post_readback_started);
        let first = tokio::spawn(async move {
            first_service
                .execute(
                    &first_device,
                    fresh_intent("request-old", automatic_target_change()),
                )
                .await
        });
        wait_for_readback.notified().await;

        let second_service = fixture.service.clone();
        let second_device = fixture.device_id.clone();
        let second = tokio::spawn(async move {
            second_service
                .execute(
                    &second_device,
                    fresh_intent(
                        "request-new",
                        QuickConnectCommand::SetAutomaticTargets {
                            temperature_f: Some(111),
                            humidity_percent: Some(42),
                        },
                    ),
                )
                .await
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        let runtime = fixture
            .registry
            .read()
            .await
            .runtime(&fixture.device_id)
            .unwrap();
        assert!(
            !runtime.is_current_control_intent(1),
            "replacement intent should advance the generation"
        );
        let second = tokio::time::timeout(Duration::from_secs(2), second)
            .await
            .expect("new intent should not wait for the old readback deadline");

        assert_eq!(
            first.await.unwrap().status(),
            QuickConnectControlStatus::SubmittedUnconfirmed
        );
        assert_eq!(
            second.unwrap().status(),
            QuickConnectControlStatus::Confirmed
        );
        assert_eq!(fixture.mock.settings_writes.load(Ordering::SeqCst), 2);
        clean_up(fixture);
    }

    #[tokio::test]
    async fn newer_intent_cancels_ambiguous_post_refresh() {
        let policy = QuickConnectControlPolicy::for_test().with_readback(
            Duration::from_secs(10),
            Duration::from_secs(5),
            10,
        );
        let fixture = control_fixture(policy, true).await;
        fixture.mock.post_status.store(500, Ordering::SeqCst);
        fixture
            .mock
            .post_detail_delay_ms
            .store(5_000, Ordering::SeqCst);
        fixture.mock.delayed_post_details.store(1, Ordering::SeqCst);
        let first_service = fixture.service.clone();
        let first_device = fixture.device_id.clone();
        let wait_for_refresh = Arc::clone(&fixture.mock.post_readback_started);
        let first = tokio::spawn(async move {
            first_service
                .execute(
                    &first_device,
                    fresh_intent("request-ambiguous-old", automatic_target_change()),
                )
                .await
        });
        wait_for_refresh.notified().await;

        let second_service = fixture.service.clone();
        let second_device = fixture.device_id.clone();
        let second = tokio::spawn(async move {
            second_service
                .execute(
                    &second_device,
                    fresh_intent(
                        "request-ambiguous-new",
                        QuickConnectCommand::SetAutomaticTargets {
                            temperature_f: Some(111),
                            humidity_percent: Some(42),
                        },
                    ),
                )
                .await
        });
        let second = tokio::time::timeout(Duration::from_secs(2), second)
            .await
            .expect("new intent should cancel the ambiguous refresh")
            .unwrap();

        assert_eq!(
            first.await.unwrap().status(),
            QuickConnectControlStatus::SubmittedUnconfirmed
        );
        assert_eq!(
            second.status(),
            QuickConnectControlStatus::SubmittedUnconfirmed
        );
        assert_eq!(fixture.mock.settings_writes.load(Ordering::SeqCst), 2);
        clean_up(fixture);
    }

    #[tokio::test]
    async fn newer_intent_cancels_superseded_pre_read() {
        let policy = QuickConnectControlPolicy::for_test().with_readback(
            Duration::from_secs(10),
            Duration::from_secs(5),
            10,
        );
        let fixture = control_fixture(policy, true).await;
        fixture
            .mock
            .pre_detail_delay_ms
            .store(5_000, Ordering::SeqCst);
        fixture.mock.delayed_pre_details.store(1, Ordering::SeqCst);
        let first_service = fixture.service.clone();
        let first_device = fixture.device_id.clone();
        let wait_for_pre_read = Arc::clone(&fixture.mock.pre_read_started);
        let first = tokio::spawn(async move {
            first_service
                .execute(
                    &first_device,
                    fresh_intent("request-pre-read-old", automatic_target_change()),
                )
                .await
        });
        wait_for_pre_read.notified().await;

        let second_service = fixture.service.clone();
        let second_device = fixture.device_id.clone();
        let second = tokio::spawn(async move {
            second_service
                .execute(
                    &second_device,
                    fresh_intent(
                        "request-pre-read-new",
                        QuickConnectCommand::SetAutomaticTargets {
                            temperature_f: Some(111),
                            humidity_percent: Some(42),
                        },
                    ),
                )
                .await
        });
        let second = tokio::time::timeout(Duration::from_secs(2), second)
            .await
            .expect("new intent should cancel the stale pre-read")
            .unwrap();

        assert_eq!(
            first.await.unwrap().status(),
            QuickConnectControlStatus::Rejected
        );
        assert_eq!(second.status(), QuickConnectControlStatus::Confirmed);
        assert_eq!(fixture.mock.settings_writes.load(Ordering::SeqCst), 1);
        clean_up(fixture);
    }

    async fn login(State(state): State<MockState>) -> Json<Value> {
        state.login_started.notify_one();
        sleep(Duration::from_millis(
            state.login_delay_ms.load(Ordering::SeqCst) as u64,
        ))
        .await;
        Json(json!({"responseData":{"idToken":"SYNTHETIC_TOKEN_DO_NOT_USE"}}))
    }

    async fn detail(State(state): State<MockState>) -> impl IntoResponse {
        let post_seen = state.settings_writes.load(Ordering::SeqCst) > 0;
        if post_seen {
            state.post_readback_started.notify_one();
        } else {
            state.pre_read_started.notify_one();
        }
        let delayed_post = post_seen
            && state
                .delayed_post_details
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                    remaining.checked_sub(1)
                })
                .is_ok();
        let delayed_pre = !post_seen
            && state
                .delayed_pre_details
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                    remaining.checked_sub(1)
                })
                .is_ok();
        let delay_ms = if delayed_post {
            state.post_detail_delay_ms.load(Ordering::SeqCst)
        } else if delayed_pre {
            state.pre_detail_delay_ms.load(Ordering::SeqCst)
        } else {
            state.detail_delay_ms.load(Ordering::SeqCst)
        };
        sleep(Duration::from_millis(delay_ms as u64)).await;
        state.detail_reads.fetch_add(1, Ordering::SeqCst);
        if post_seen && state.fail_detail_after_write.load(Ordering::SeqCst) {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error":"synthetic readback failure"})),
            )
                .into_response();
        }
        let body = state.saved_body.lock().await.clone().unwrap_or_default();
        let target_temperature = body["desiredTemp"].as_u64().unwrap_or(105);
        let target_humidity = body["desiredHumidity"].as_u64().unwrap_or(40);
        let mismatch = post_seen
            && (state.mismatch_preserved_humidity.load(Ordering::SeqCst)
                || state.mismatch_once.swap(false, Ordering::SeqCst));
        let humidity_target = if mismatch {
            target_humidity + 1
        } else {
            target_humidity
        };
        let timer_value = body["timerValue"].as_u64().unwrap_or(60);
        let automatic_mode = body["automaticMode"].as_bool().unwrap_or(true);
        let timer_mode = body["timerMode"].as_bool().unwrap_or(false);
        let fan_mode = body["fanMode"].as_bool().unwrap_or(false);
        Json(json!({
            "responseData": {
                "deviceConfig": {"setTemperature": 78, "setHumidity": 44},
                "deviceSettings": {
                    "automaticMode": automatic_mode,
                    "timerMode": timer_mode,
                    "fanMode": fan_mode,
                    "setTemperature": target_temperature,
                    "setHumidity": humidity_target,
                    "timerValue": timer_value,
                    "humidityMonitor": true
                }
            }
        }))
        .into_response()
    }

    async fn save_settings(
        State(state): State<MockState>,
        Json(body): Json<Value>,
    ) -> impl IntoResponse {
        *state.saved_body.lock().await = Some(body);
        state.settings_writes.fetch_add(1, Ordering::SeqCst);
        if state.post_status.load(Ordering::SeqCst) == 500 {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error":"ambiguous"})),
            )
                .into_response();
        }
        Json(json!({"responseData": {"accepted": true}})).into_response()
    }
}
