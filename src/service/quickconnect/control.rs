use std::{
    future::Future,
    sync::Arc,
    time::{Duration, SystemTime},
};

use futures_util::{StreamExt, TryStreamExt, stream};
use gafctl_api::{DeviceCommand, DeviceId, is_fresh_at, unix_millis};
use gafctl_quickconnect::{
    ClientError, QuickConnectCommand, QuickConnectCommandMode, QuickConnectSettings,
    QuickConnectSettingsBody,
};
use tokio::time::{Instant, sleep, timeout};

use super::QuickConnectBackend;
use crate::backend::DeviceRuntime;

const DEFAULT_MAX_COMMAND_AGE: Duration = Duration::from_secs(30);
const DEFAULT_MAX_FUTURE_SKEW: Duration = Duration::from_secs(5);
const DEFAULT_READBACK_TIMEOUT: Duration = Duration::from_secs(60);
const DEFAULT_READBACK_INTERVAL: Duration = Duration::from_secs(2);
const DEFAULT_READBACK_ATTEMPTS: u16 = 31;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::service) enum QuickConnectControlStatus {
    Rejected,
    SubmittedUnconfirmed,
    ReadbackMismatch,
    ReadbackUnavailable,
    Confirmed,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct QuickConnectControlPolicy {
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
    #[cfg(test)]
    pub fn with_command_freshness(mut self, max_age: Duration, max_future_skew: Duration) -> Self {
        self.max_command_age = max_age;
        self.max_future_skew = max_future_skew;
        self
    }

    #[cfg(test)]
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
pub(in crate::service) struct QuickConnectControlIntent {
    issued_at_unix_ms: u64,
    requested_command: DeviceCommand,
    command: QuickConnectCommand,
}

impl QuickConnectControlIntent {
    pub(in crate::service) fn new(
        issued_at_unix_ms: u64,
        requested_command: DeviceCommand,
    ) -> Option<Self> {
        let command = quickconnect_command(requested_command)?;
        Some(Self {
            issued_at_unix_ms,
            requested_command,
            command,
        })
    }

    fn is_fresh_at(&self, now_unix_ms: u64, policy: QuickConnectControlPolicy) -> bool {
        is_fresh_at(
            self.issued_at_unix_ms,
            now_unix_ms,
            policy.max_command_age,
            policy.max_future_skew,
        )
    }
}

impl QuickConnectBackend {
    pub(in crate::service) async fn execute(
        &self,
        id: &DeviceId,
        intent: QuickConnectControlIntent,
    ) -> QuickConnectControlStatus {
        if !self.is_fresh(&intent) {
            return QuickConnectControlStatus::Rejected;
        }
        let target = self.control_target(id, intent.requested_command).await;
        let Ok((runtime, provider_id)) = target else {
            return QuickConnectControlStatus::Rejected;
        };
        let Some(_queue_permit) = runtime.try_reserve_control() else {
            return QuickConnectControlStatus::Rejected;
        };
        let generation = runtime.begin_control_intent();
        let _transaction = runtime.acquire_transaction().await;
        if !self
            .is_current(&intent, id, &runtime, &provider_id, generation)
            .await
        {
            return QuickConnectControlStatus::Rejected;
        }

        self.execute_locked(id, &intent, &runtime, &provider_id, generation)
            .await
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
        let body = match gafctl_quickconnect::build_settings_body(&intent.command, &before.settings)
        {
            Ok(body) => body,
            Err(gafctl_quickconnect::QuickConnectCommandError::ModeAlreadyInactive) => {
                return if runtime
                    .set_control_state_if_current(generation, super::common_state(before))
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
    ) -> Option<gafctl_quickconnect::QuickConnectDeviceState> {
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
        command: DeviceCommand,
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
            .control_target_matches(id, intent.requested_command, runtime, provider_id)
            .await;
        matches_target && self.is_fresh(intent) && runtime.is_current_control_intent(generation)
    }

    async fn control_target_matches(
        &self,
        id: &DeviceId,
        command: DeviceCommand,
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
        before: &gafctl_quickconnect::QuickConnectDeviceState,
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
        if !Self::publish_readback(runtime, generation, result.state).await {
            return QuickConnectControlStatus::SubmittedUnconfirmed;
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
        stream::iter(0..self.policy.readback_attempts.max(1))
            .map(Ok)
            .try_fold(
                ReadbackProgress::default(),
                |progress, attempt| async move {
                    if !runtime.is_current_control_intent(generation) {
                        return Err(progress);
                    }
                    let poll = async move {
                        let remaining = deadline.saturating_duration_since(Instant::now());
                        if attempt > 0 && !self.policy.readback_interval.is_zero() {
                            sleep(self.policy.readback_interval.min(remaining)).await;
                        }
                        let remaining = deadline.saturating_duration_since(Instant::now());
                        if remaining.is_zero() {
                            None
                        } else {
                            Some(
                                timeout(remaining, self.client.read_device_state(provider_id))
                                    .await,
                            )
                        }
                    };
                    let result = tokio::select! {
                        _ = runtime.wait_for_control_change(generation) => None,
                        result = poll => result,
                    };
                    match result {
                        Some(Ok(Ok(state))) => {
                            let matched = body.matches_readback(before, &state.settings);
                            let progress = ReadbackProgress {
                                matched,
                                state: Some(state),
                            };
                            if matched { Err(progress) } else { Ok(progress) }
                        }
                        Some(Ok(Err(_)) | Err(_)) | None => Ok(progress),
                    }
                },
            )
            .await
            .unwrap_or_else(std::convert::identity)
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
        Self::publish_readback(runtime, generation, result.ok().and_then(Result::ok)).await;
    }

    async fn publish_readback(
        runtime: &DeviceRuntime,
        generation: u64,
        state: Option<gafctl_quickconnect::QuickConnectDeviceState>,
    ) -> bool {
        match state {
            Some(state) => {
                runtime
                    .set_control_state_if_current(generation, super::common_state(state))
                    .await
            }
            None => {
                runtime
                    .mark_control_state_unavailable_if_current(generation)
                    .await
            }
        }
    }
}

#[derive(Default)]
struct ReadbackProgress {
    matched: bool,
    state: Option<gafctl_quickconnect::QuickConnectDeviceState>,
}

fn quickconnect_command(command: DeviceCommand) -> Option<QuickConnectCommand> {
    match command {
        DeviceCommand::QuickConnectMode { mode } => Some(QuickConnectCommand::SetMode {
            mode: cloud_mode(mode),
        }),
        DeviceCommand::QuickConnectConditionalOff { only_if_current } => {
            Some(QuickConnectCommand::ClearMode {
                mode: cloud_mode(only_if_current),
            })
        }
        DeviceCommand::QuickConnectTargets {
            temperature_f,
            humidity_percent,
        } => Some(QuickConnectCommand::SetAutomaticTargets {
            temperature_f: Some(temperature_f),
            humidity_percent: Some(humidity_percent),
        }),
        DeviceCommand::QuickConnectAutomaticTemperature { temperature_f } => {
            Some(QuickConnectCommand::SetAutomaticTargets {
                temperature_f: Some(temperature_f.value()),
                humidity_percent: None,
            })
        }
        DeviceCommand::QuickConnectAutomaticHumidity { humidity_percent } => {
            Some(QuickConnectCommand::SetAutomaticTargets {
                temperature_f: None,
                humidity_percent: Some(humidity_percent.value()),
            })
        }
        DeviceCommand::QuickConnectTimerDuration { minutes } => {
            Some(QuickConnectCommand::SetTimerDuration {
                duration_minutes: minutes,
            })
        }
        DeviceCommand::LegacyMode { .. }
        | DeviceCommand::LegacyPreset { .. }
        | DeviceCommand::LegacyAutomaticTemperature { .. }
        | DeviceCommand::LegacyAutomaticHumidity { .. }
        | DeviceCommand::LegacyTimer { .. } => None,
    }
}

const fn cloud_mode(mode: gafctl_api::QuickConnectMode) -> QuickConnectCommandMode {
    match mode {
        gafctl_api::QuickConnectMode::Off => QuickConnectCommandMode::Off,
        gafctl_api::QuickConnectMode::Automatic => QuickConnectCommandMode::Automatic,
        gafctl_api::QuickConnectMode::Timer => QuickConnectCommandMode::Timer,
        gafctl_api::QuickConnectMode::Manual => QuickConnectCommandMode::Manual,
    }
}

fn fresh_state(state: &gafctl_quickconnect::QuickConnectDeviceState) -> bool {
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
        time::SystemTime,
    };

    use axum::{
        Json, Router, extract::State, http::StatusCode, response::IntoResponse, routing::get,
        routing::post,
    };
    use serde_json::{Value, json};
    use tokio::{sync::RwLock, time::sleep};

    use super::*;
    use crate::backend::DeviceRegistry;
    use crate::test_support::cloud_device;
    use gafctl_api::DeviceId;

    #[test]
    fn control_intents_preserve_domain_commands_and_translate_cloud_payloads() {
        let temperature_f = gafctl_api::AutomaticTemperatureF::try_from(110).unwrap();
        let humidity_percent = gafctl_api::AutomaticHumidityPercent::try_from(42).unwrap();
        let cases = [
            (
                DeviceCommand::QuickConnectMode {
                    mode: gafctl_api::QuickConnectMode::Manual,
                },
                QuickConnectCommand::SetMode {
                    mode: QuickConnectCommandMode::Manual,
                },
            ),
            (
                DeviceCommand::QuickConnectConditionalOff {
                    only_if_current: gafctl_api::QuickConnectMode::Timer,
                },
                QuickConnectCommand::ClearMode {
                    mode: QuickConnectCommandMode::Timer,
                },
            ),
            (
                DeviceCommand::QuickConnectTargets {
                    temperature_f: 110,
                    humidity_percent: 42,
                },
                QuickConnectCommand::SetAutomaticTargets {
                    temperature_f: Some(110),
                    humidity_percent: Some(42),
                },
            ),
            (
                DeviceCommand::QuickConnectAutomaticTemperature { temperature_f },
                QuickConnectCommand::SetAutomaticTargets {
                    temperature_f: Some(110),
                    humidity_percent: None,
                },
            ),
            (
                DeviceCommand::QuickConnectAutomaticHumidity { humidity_percent },
                QuickConnectCommand::SetAutomaticTargets {
                    temperature_f: None,
                    humidity_percent: Some(42),
                },
            ),
            (
                DeviceCommand::QuickConnectTimerDuration { minutes: 90 },
                QuickConnectCommand::SetTimerDuration {
                    duration_minutes: 90,
                },
            ),
        ];
        cases.into_iter().for_each(|(requested, translated)| {
            let intent = QuickConnectControlIntent::new(123, requested).unwrap();
            assert_eq!(intent.issued_at_unix_ms, 123);
            assert_eq!(intent.requested_command, requested);
            assert_eq!(intent.command, translated);
        });
        [
            DeviceCommand::LegacyPreset {
                preset: gafctl_api::ControlPreset::TimerClear,
            },
            DeviceCommand::LegacyAutomaticTemperature { temperature_f },
            DeviceCommand::LegacyAutomaticHumidity { humidity_percent },
            DeviceCommand::LegacyTimer {
                minutes: gafctl_api::LegacyTimerMinutes::try_from(1).unwrap(),
            },
        ]
        .into_iter()
        .for_each(|command| assert!(QuickConnectControlIntent::new(123, command).is_none()));
    }

    #[derive(Clone, Default)]
    struct MockState {
        detail_reads: Arc<AtomicUsize>,
        settings_writes: Arc<AtomicUsize>,
        login_delay_ms: Arc<AtomicUsize>,
        delay_pre_once: Arc<AtomicBool>,
        delay_post_once: Arc<AtomicBool>,
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
        service: QuickConnectBackend,
        device_id: DeviceId,
        registry: Arc<RwLock<DeviceRegistry>>,
        mock: MockState,
        _server: tokio_util::task::AbortOnDropHandle<()>,
        _directory: tempfile::TempDir,
    }

    async fn control_fixture(
        policy: QuickConnectControlPolicy,
        writes_enabled: bool,
    ) -> ControlFixture {
        let mock = MockState {
            post_status: Arc::new(AtomicUsize::new(200)),
            ..MockState::default()
        };
        let app = Router::new()
            .route("/cognito/login", post(login))
            .route("/gaf/device", get(detail))
            .route("/gaf/deviceMode/provider-fan", post(save_settings))
            .with_state(mock.clone());
        let (client, server) = crate::test_support::mock_client(app).await;
        let (directory, store_path) = crate::test_support::identity_store_fixture();
        let mut registry = DeviceRegistry::load(&store_path).unwrap();
        let device_id = registry
            .reconcile_quickconnect(
                "synthetic-account",
                &[cloud_device("provider-fan", "Synthetic fan")],
            )
            .unwrap()
            .into_iter()
            .next()
            .unwrap();
        registry.set_quickconnect_writes_enabled(writes_enabled);
        let registry = Arc::new(RwLock::new(registry));
        let service = QuickConnectBackend {
            policy,
            ..QuickConnectBackend::new(Arc::clone(&registry), client, "synthetic-account")
        };
        ControlFixture {
            service,
            device_id,
            registry,
            mock,
            _server: server,
            _directory: directory,
        }
    }

    impl ControlFixture {
        fn execute(
            &self,
            command: DeviceCommand,
        ) -> impl Future<Output = QuickConnectControlStatus> + '_ {
            self.service.execute(&self.device_id, fresh_intent(command))
        }
    }

    fn fresh_intent(command: DeviceCommand) -> QuickConnectControlIntent {
        let now_unix_ms = unix_millis(SystemTime::now()).unwrap();
        QuickConnectControlIntent::new(now_unix_ms, command).unwrap()
    }

    fn automatic_target_change() -> DeviceCommand {
        DeviceCommand::QuickConnectAutomaticTemperature {
            temperature_f: gafctl_api::AutomaticTemperatureF::try_from(110).unwrap(),
        }
    }

    #[tokio::test]
    async fn conditional_off_of_inactive_mode_confirms_without_writing() {
        let fixture = control_fixture(QuickConnectControlPolicy::for_test(), true).await;
        let status = fixture
            .execute(DeviceCommand::QuickConnectConditionalOff {
                only_if_current: gafctl_api::QuickConnectMode::Timer,
            })
            .await;
        assert_eq!(status, QuickConnectControlStatus::Confirmed);
        assert_eq!(fixture.mock.settings_writes.load(Ordering::SeqCst), 0);
        assert_eq!(fixture.mock.detail_reads.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn changed_preserved_value_after_successful_post_is_a_readback_mismatch() {
        let fixture = control_fixture(QuickConnectControlPolicy::for_test(), true).await;
        fixture
            .mock
            .mismatch_preserved_humidity
            .store(true, Ordering::SeqCst);

        let status = fixture.execute(automatic_target_change()).await;

        assert_eq!(status, QuickConnectControlStatus::ReadbackMismatch);
        assert_eq!(fixture.mock.settings_writes.load(Ordering::SeqCst), 1);
        assert_eq!(fixture.mock.detail_reads.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn disabled_write_capability_rejects_before_cloud_io() {
        let fixture = control_fixture(QuickConnectControlPolicy::for_test(), false).await;
        let status = fixture.execute(automatic_target_change()).await;

        assert_eq!(status, QuickConnectControlStatus::Rejected);
        assert_eq!(fixture.mock.settings_writes.load(Ordering::SeqCst), 0);
        assert_eq!(fixture.mock.detail_reads.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn stale_command_after_authentication_never_posts() {
        let policy = QuickConnectControlPolicy::for_test()
            .with_command_freshness(Duration::from_millis(25), Duration::ZERO);
        let fixture = control_fixture(policy, true).await;
        fixture.mock.login_delay_ms.store(60, Ordering::SeqCst);

        let status = fixture
            .service
            .execute(&fixture.device_id, fresh_intent(automatic_target_change()))
            .await;

        assert_eq!(status, QuickConnectControlStatus::Rejected);
        assert_eq!(fixture.mock.settings_writes.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn disabled_gate_immediately_before_post_rejects_without_writing() {
        let fixture = control_fixture(QuickConnectControlPolicy::for_test(), true).await;
        fixture.mock.login_delay_ms.store(60, Ordering::SeqCst);
        let login_started = Arc::clone(&fixture.mock.login_started);
        let registry = Arc::clone(&fixture.registry);
        let status = fixture
            .service
            .execute(&fixture.device_id, fresh_intent(automatic_target_change()));
        let disable_gate = async move {
            login_started.notified().await;
            registry
                .write()
                .await
                .set_quickconnect_writes_enabled(false);
        };
        let (status, ()) = tokio::join!(status, disable_gate);

        assert_eq!(status, QuickConnectControlStatus::Rejected);
        assert_eq!(fixture.mock.settings_writes.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn matching_readback_confirms_only_after_the_single_post() {
        let fixture = control_fixture(QuickConnectControlPolicy::for_test(), true).await;

        let status = fixture
            .execute(DeviceCommand::QuickConnectTimerDuration { minutes: 90 })
            .await;

        assert_eq!(status, QuickConnectControlStatus::Confirmed);
        assert_eq!(fixture.mock.settings_writes.load(Ordering::SeqCst), 1);
        assert_eq!(fixture.mock.detail_reads.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn ambiguous_post_response_is_unconfirmed_and_never_replayed() {
        let fixture = control_fixture(QuickConnectControlPolicy::for_test(), true).await;
        fixture.mock.post_status.store(500, Ordering::SeqCst);

        let status = fixture.execute(automatic_target_change()).await;

        assert_eq!(status, QuickConnectControlStatus::SubmittedUnconfirmed);
        assert_eq!(fixture.mock.settings_writes.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn accepted_post_without_readback_is_reported_unavailable() {
        let fixture = control_fixture(QuickConnectControlPolicy::for_test(), true).await;
        fixture
            .mock
            .fail_detail_after_write
            .store(true, Ordering::SeqCst);

        let status = fixture.execute(automatic_target_change()).await;

        assert_eq!(status, QuickConnectControlStatus::ReadbackUnavailable);
        assert_eq!(fixture.mock.settings_writes.load(Ordering::SeqCst), 1);
        assert!(fixture.mock.detail_reads.load(Ordering::SeqCst) >= 2);
    }

    #[derive(Clone, Copy, Debug)]
    enum SupersededStage {
        Readback,
        AmbiguousRefresh,
        PreRead,
    }

    impl ControlFixture {
        fn spawn(
            &self,
            command: DeviceCommand,
        ) -> tokio::task::JoinHandle<QuickConnectControlStatus> {
            let service = self.service.clone();
            let id = self.device_id.clone();
            tokio::spawn(async move { service.execute(&id, fresh_intent(command)).await })
        }
    }

    #[tokio::test]
    async fn replacement_intent_cancels_each_superseded_io_stage() {
        use QuickConnectControlStatus::{Confirmed, Rejected, SubmittedUnconfirmed};
        for (stage, first_status, second_status, writes) in [
            (
                SupersededStage::Readback,
                SubmittedUnconfirmed,
                Confirmed,
                2,
            ),
            (
                SupersededStage::AmbiguousRefresh,
                SubmittedUnconfirmed,
                SubmittedUnconfirmed,
                2,
            ),
            (SupersededStage::PreRead, Rejected, Confirmed, 1),
        ] {
            let policy = QuickConnectControlPolicy::for_test().with_readback(
                Duration::from_secs(10),
                Duration::from_secs(5),
                10,
            );
            let fixture = control_fixture(policy, true).await;
            let started = match stage {
                SupersededStage::Readback => {
                    fixture.mock.mismatch_once.store(true, Ordering::SeqCst);
                    &fixture.mock.post_readback_started
                }
                SupersededStage::AmbiguousRefresh => {
                    fixture.mock.post_status.store(500, Ordering::SeqCst);
                    fixture.mock.delay_post_once.store(true, Ordering::SeqCst);
                    &fixture.mock.post_readback_started
                }
                SupersededStage::PreRead => {
                    fixture.mock.delay_pre_once.store(true, Ordering::SeqCst);
                    &fixture.mock.pre_read_started
                }
            };
            let first = fixture.spawn(automatic_target_change());
            started.notified().await;
            let second = fixture.spawn(DeviceCommand::QuickConnectTargets {
                temperature_f: 111,
                humidity_percent: 42,
            });
            if let SupersededStage::Readback = stage {
                tokio::time::sleep(Duration::from_millis(100)).await;
                let runtime = fixture
                    .registry
                    .read()
                    .await
                    .runtime(&fixture.device_id)
                    .unwrap();
                assert!(!runtime.is_current_control_intent(1), "{stage:?}");
            }
            let second = tokio::time::timeout(Duration::from_secs(2), second)
                .await
                .expect("replacement must cancel old IO before its 5s wait")
                .unwrap();
            assert_eq!(first.await.unwrap(), first_status, "{stage:?}");
            assert_eq!(second, second_status, "{stage:?}");
            assert_eq!(
                fixture.mock.settings_writes.load(Ordering::SeqCst),
                writes,
                "{stage:?}"
            );
        }
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
        let delayed = (post_seen && state.delay_post_once.swap(false, Ordering::SeqCst))
            || (!post_seen && state.delay_pre_once.swap(false, Ordering::SeqCst));
        sleep(if delayed {
            Duration::from_secs(5)
        } else {
            Duration::ZERO
        })
        .await;
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
