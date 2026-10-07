use super::{DeviceService, ServiceError};
use crate::backend::{DeviceRuntime, RefreshReceiver, RefreshReservation};
use anyhow::Result;
use gafctl_api::{
    DeviceBackend, DeviceId, DeviceInventoryStatus, DeviceRefreshStatus, DeviceRefreshV2Response,
};
use std::{sync::Arc, time::Duration};
const DEVICE_REFRESH_TIMEOUT: Duration = Duration::from_secs(270);

impl DeviceService {
    pub(crate) async fn refresh_device(
        &self,
        id: &DeviceId,
    ) -> Result<Arc<DeviceRefreshV2Response>, ServiceError> {
        let (backend, runtime) = self.refresh_target(id).await?;
        let receiver = match runtime.reserve_refresh().await {
            RefreshReservation::Join(receiver) => receiver,
            RefreshReservation::Execute {
                receiver,
                completion,
            } => {
                let state = self.clone();
                let id = id.clone();
                tokio::spawn(async move {
                    if let Ok(response) = state.execute_device_refresh(&id, backend, &runtime).await
                    {
                        completion.send_replace(Some(Arc::new(response)));
                    }
                });
                receiver
            }
        };
        wait_for_device_refresh(receiver).await
    }

    async fn refresh_target(
        &self,
        id: &DeviceId,
    ) -> Result<(DeviceBackend, Arc<DeviceRuntime>), ServiceError> {
        let registry = self.registry.read().await;
        let descriptor = registry.descriptor(id).ok_or(ServiceError::UnknownDevice)?;
        if !descriptor.capabilities.read_state {
            return Err(ServiceError::UnsupportedRead);
        }
        let configured = match descriptor.backend {
            DeviceBackend::LegacyBle => self.ble_device.is_some(),
            DeviceBackend::QuickConnect => self.quickconnect.is_some(),
        };
        if !configured {
            return Err(ServiceError::BackendUnavailable);
        }
        Ok((
            descriptor.backend,
            registry.runtime(id).ok_or(ServiceError::UnknownDevice)?,
        ))
    }

    async fn execute_device_refresh(
        &self,
        id: &DeviceId,
        backend: DeviceBackend,
        runtime: &DeviceRuntime,
    ) -> Result<DeviceRefreshV2Response, ServiceError> {
        let generation = runtime.state_generation();
        let response = match tokio::time::timeout(
            DEVICE_REFRESH_TIMEOUT,
            self.read_device_locked(id, backend, runtime),
        )
        .await
        {
            Ok(response) => response?,
            Err(_) => {
                let status = match backend {
                    DeviceBackend::LegacyBle => {
                        if self
                            .ble_device
                            .as_ref()
                            .ok_or(ServiceError::BackendUnavailable)?
                            .record_refresh_timeout(generation)
                            .await
                        {
                            DeviceRefreshStatus::Failed
                        } else {
                            DeviceRefreshStatus::Superseded
                        }
                    }
                    DeviceBackend::QuickConnect => DeviceRefreshStatus::Failed,
                };
                let mut device = self.state(id).await?;
                if backend == DeviceBackend::QuickConnect {
                    device.last_error = Some("device refresh deadline exceeded".to_owned());
                }
                DeviceRefreshV2Response { status, device }
            }
        };
        self.publish_state().await;
        Ok(response)
    }

    async fn read_device_locked(
        &self,
        id: &DeviceId,
        backend: DeviceBackend,
        runtime: &DeviceRuntime,
    ) -> Result<DeviceRefreshV2Response, ServiceError> {
        let _transaction = runtime.acquire_transaction().await;
        let status = match backend {
            DeviceBackend::LegacyBle => {
                self.ble_device
                    .as_ref()
                    .ok_or(ServiceError::BackendUnavailable)?
                    .read_state_locked(self)
                    .await
            }
            DeviceBackend::QuickConnect => {
                self.quickconnect
                    .as_ref()
                    .ok_or(ServiceError::BackendUnavailable)?
                    .read_state_locked(id, runtime)
                    .await?
            }
        };
        self.refresh_response(id, status).await
    }

    pub(super) async fn refresh_response(
        &self,
        id: &DeviceId,
        status: DeviceRefreshStatus,
    ) -> Result<DeviceRefreshV2Response, ServiceError> {
        let device = self.state(id).await?;
        let status = if status == DeviceRefreshStatus::Fresh
            && (!device.available || device.inventory_status != DeviceInventoryStatus::Present)
        {
            DeviceRefreshStatus::Failed
        } else {
            status
        };
        Ok(DeviceRefreshV2Response { status, device })
    }
}

async fn wait_for_device_refresh(
    mut receiver: RefreshReceiver,
) -> Result<Arc<DeviceRefreshV2Response>, ServiceError> {
    receiver
        .wait_for(|result| result.is_some())
        .await
        .map_err(|_| ServiceError::WorkerUnavailable)?
        .as_ref()
        .cloned()
        .ok_or(ServiceError::WorkerUnavailable)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn refresh_waiter_reports_closed_worker_without_hanging() {
        let (sender, receiver) = tokio::sync::watch::channel(None);
        drop(sender);
        assert_eq!(
            wait_for_device_refresh(receiver).await.unwrap_err(),
            ServiceError::WorkerUnavailable
        );
    }
}
