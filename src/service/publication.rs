use std::sync::Arc;

use futures_util::{StreamExt, TryStreamExt, stream};
use gafctl_api::{DeviceBackend, DeviceDescriptor, DeviceId, DeviceStateV2Response, ProxyId};
use tokio::sync::watch;

use super::{DeviceService, ServiceError, device_state_v2_data};

#[derive(Clone)]
pub(crate) struct StateSnapshot {
    pub(crate) proxy_id: ProxyId,
    pub(crate) descriptors: Vec<DeviceDescriptor>,
    pub(crate) publications: Vec<DeviceStateV2Response>,
    pub(crate) discovery_identities: Vec<(DeviceId, DeviceBackend)>,
}

impl DeviceService {
    pub(super) async fn publish_state(&self) {
        let Some(updates) = &self.state_updates else {
            return;
        };
        let _publication = self.snapshot_publication.lock().await;
        match self.state_snapshot().await {
            Ok(snapshot) => {
                self.publish_current_snapshot(updates, snapshot).await;
            }
            Err(error) => tracing::error!(%error, "could not collect device state for publication"),
        }
    }

    pub(super) async fn publish_current_snapshot(
        &self,
        updates: &watch::Sender<Arc<StateSnapshot>>,
        snapshot: StateSnapshot,
    ) -> bool {
        let registry = self.registry.read().await;
        if !registry.descriptors().eq(snapshot.descriptors.iter()) {
            return false;
        }
        updates.send_replace(Arc::new(snapshot));
        true
    }

    pub(super) async fn state_snapshot(&self) -> Result<StateSnapshot, ServiceError> {
        let (proxy_id, descriptors, discovery_identities) = {
            let registry = self.registry.read().await;
            (
                registry.proxy_id(),
                registry.descriptors().cloned().collect::<Vec<_>>(),
                registry.discovery_identities().collect(),
            )
        };
        let publications = stream::iter(descriptors.iter())
            .then(|descriptor| device_state_v2_data(self, &descriptor.id))
            .try_collect()
            .await?;
        Ok(StateSnapshot {
            proxy_id,
            descriptors,
            publications,
            discovery_identities,
        })
    }
}
