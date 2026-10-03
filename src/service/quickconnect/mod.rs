use self::control::QuickConnectControlPolicy;
pub(super) use self::control::{QuickConnectControlIntent, QuickConnectControlStatus};
use super::{DeviceService, ServiceError};
use crate::backend::DeviceRegistry;
use crate::backend::{DeviceRuntime, QuickConnectReadTarget};
use tokio::sync::RwLock;
mod control;
use anyhow::{Context, Result};
use futures_util::{StreamExt, stream};
use gafctl_api::DeviceRefreshStatus;
use gafctl_api::{
    DeviceBackend, DeviceDiagnostics, DeviceId, DeviceSettings, DeviceState,
    QuickConnectModeStatus, StateProvenance,
};
use gafctl_quickconnect::{Credentials, QuickConnectClient, QuickConnectConfig};
use std::{sync::Arc, time::Duration};

const QUICKCONNECT_DETAIL_TIMEOUT: Duration = Duration::from_secs(270);

pub(crate) struct QuickConnectRuntimeConfig {
    pub(crate) credentials: Credentials,
    pub(crate) account_id: String,
    pub(crate) writes_enabled: bool,
}

#[derive(Clone)]
pub(super) struct QuickConnectBackend {
    registry: Arc<RwLock<DeviceRegistry>>,
    client: QuickConnectClient,
    account_id: Arc<str>,
    policy: QuickConnectControlPolicy,
}

impl QuickConnectBackend {
    pub(super) fn new(
        registry: Arc<RwLock<DeviceRegistry>>,
        client: QuickConnectClient,
        account_id: impl Into<Arc<str>>,
    ) -> Self {
        Self {
            registry,
            client,
            account_id: account_id.into(),
            policy: QuickConnectControlPolicy::default(),
        }
    }
    pub(super) async fn read_state_locked(
        &self,
        id: &DeviceId,
        runtime: &DeviceRuntime,
    ) -> Result<DeviceRefreshStatus, ServiceError> {
        let (_, provider_id) = self
            .registry
            .read()
            .await
            .quickconnect_read_target(&self.account_id, id)
            .map_err(|_| ServiceError::UnknownDevice)?;
        let generation = runtime.begin_state_read();
        match self.client.read_device_state(&provider_id).await {
            Ok(state) => Ok(
                if runtime
                    .set_state_if_current(generation, common_state(state))
                    .await
                {
                    DeviceRefreshStatus::Fresh
                } else {
                    DeviceRefreshStatus::Superseded
                },
            ),
            Err(error) => {
                tracing::warn!(%error, "QuickConnect device refresh failed");
                Ok(
                    if runtime.mark_detail_unavailable_if_current(generation).await {
                        DeviceRefreshStatus::Failed
                    } else {
                        DeviceRefreshStatus::Superseded
                    },
                )
            }
        }
    }
}

impl DeviceService {
    pub(crate) async fn start_quickconnect(
        &mut self,
        config: QuickConnectRuntimeConfig,
    ) -> Result<()> {
        let client = QuickConnectClient::new(config.credentials, QuickConnectConfig::production()?)
            .context("could not configure QuickConnect client")?;
        self.registry
            .write()
            .await
            .set_quickconnect_writes_enabled(config.writes_enabled);
        self.quickconnect = Some(QuickConnectBackend::new(
            Arc::clone(&self.registry),
            client,
            config.account_id,
        ));
        Ok(())
    }

    pub(crate) async fn poll_quickconnect(&self) {
        let Some(cloud) = self.quickconnect.as_ref() else {
            return;
        };
        let generations = self
            .registry
            .read()
            .await
            .begin_quickconnect_poll(&cloud.account_id);
        let targets: Result<_, anyhow::Error> = match cloud.client.read_inventory().await {
            Ok(inventory) => self
                .registry
                .write()
                .await
                .reconcile_quickconnect_inventory(&cloud.account_id, inventory, &generations)
                .await
                .map_err(Into::into),
            Err(error) => {
                self.registry
                    .read()
                    .await
                    .mark_quickconnect_inventory_unavailable(&cloud.account_id, &generations)
                    .await;
                Err(error.into())
            }
        };
        self.publish_state().await;
        match targets {
            Ok(targets) => {
                stream::iter(targets)
                    .for_each_concurrent(
                        Some(crate::backend::QUICKCONNECT_POLL_CONCURRENCY),
                        |target| async move {
                            target.read(&cloud.client).await;
                            self.publish_state().await;
                        },
                    )
                    .await
            }
            Err(error) => tracing::warn!(%error, "QuickConnect inventory polling failed"),
        }
    }
}
impl QuickConnectReadTarget {
    pub(crate) async fn read(&self, client: &gafctl_quickconnect::QuickConnectClient) {
        if tokio::time::timeout(QUICKCONNECT_DETAIL_TIMEOUT, self.read_locked(client))
            .await
            .is_err()
        {
            self.runtime
                .mark_detail_unavailable_if_current(self.generation)
                .await;
        }
    }

    async fn read_locked(&self, client: &gafctl_quickconnect::QuickConnectClient) {
        let _transaction = self.runtime.acquire_transaction().await;
        if !self.runtime.is_current_state_read(self.generation) {
            return;
        }
        match client.read_device_state(&self.provider_id).await {
            Ok(state) => {
                self.runtime
                    .set_state_if_current(self.generation, common_state(state))
                    .await;
            }
            Err(_) => {
                self.runtime
                    .mark_detail_unavailable_if_current(self.generation)
                    .await;
            }
        }
    }
}

fn common_state(state: gafctl_quickconnect::QuickConnectDeviceState) -> DeviceState {
    use gafctl_quickconnect::DeviceModeStatus;

    DeviceState {
        temperature_f: state.temperature_f,
        humidity_percent: state.humidity_percent,
        settings: DeviceSettings::QuickConnect {
            mode: match state.settings.mode {
                DeviceModeStatus::Off => QuickConnectModeStatus::Off,
                DeviceModeStatus::Automatic => QuickConnectModeStatus::Automatic,
                DeviceModeStatus::Timer => QuickConnectModeStatus::Timer,
                DeviceModeStatus::Manual => QuickConnectModeStatus::Manual,
                DeviceModeStatus::Unknown => QuickConnectModeStatus::Unknown,
                DeviceModeStatus::Conflicting => QuickConnectModeStatus::Conflicting,
            },
            automatic_temperature_f: state.settings.automatic_temperature_f,
            automatic_humidity_percent: state.settings.automatic_humidity_percent,
            timer_duration_minutes: state.settings.timer_duration_minutes,
            humidity_monitor: state.settings.humidity_monitor,
        },
        estimated_running: state.estimated_running,
        diagnostics: Some(DeviceDiagnostics {
            firmware_version: state.diagnostics.firmware_version,
            signal_strength_raw: state.diagnostics.signal_strength_raw,
            verified_raw: state.diagnostics.verified_raw,
            ota_in_progress: state.diagnostics.ota_in_progress,
        }),
        provenance: StateProvenance {
            backend: DeviceBackend::QuickConnect,
            fetched_at_unix_ms: state.fetched_at_unix_ms,
            observed_at_unix_ms: state.observed_at_unix_ms,
        },
    }
}

#[cfg(test)]
mod tests;
