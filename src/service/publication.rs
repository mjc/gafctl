use std::sync::Arc;
use std::time::SystemTime;

use crate::model::{
    DeviceBackend, DeviceDescriptor, DeviceId, DeviceStateV2Response, ProxyId, unix_millis,
};
use futures_util::{StreamExt, TryStreamExt, stream};
use tokio::sync::{Mutex, watch};

use super::{DeviceService, ServiceError};

#[derive(Clone, PartialEq)]
pub(crate) struct StateSnapshot {
    pub(crate) proxy_id: ProxyId,
    pub(crate) descriptors: Vec<DeviceDescriptor>,
    pub(crate) publications: Vec<DeviceStateV2Response>,
    pub(crate) discovery_identities: Vec<(DeviceId, DeviceBackend)>,
}

pub(super) struct StatePublication {
    updates: watch::Sender<Arc<StateSnapshot>>,
    collection: Mutex<()>,
    allow_mqtt_ownership: bool,
}

impl StatePublication {
    pub(super) fn allow_mqtt_ownership(&self) -> bool {
        self.allow_mqtt_ownership
    }
}

impl DeviceService {
    pub(crate) fn attach_state_publication(
        &mut self,
        updates: watch::Sender<Arc<StateSnapshot>>,
        allow_mqtt_ownership: bool,
    ) {
        self.publication = Some(Arc::new(StatePublication {
            updates,
            collection: Mutex::new(()),
            allow_mqtt_ownership,
        }));
    }

    pub(crate) async fn publish_state(&self) {
        let Some(publication) = &self.publication else {
            return;
        };
        let _collection = publication.collection.lock().await;
        let current = {
            let current = publication.updates.borrow();
            Arc::clone(&current)
        };
        if self.published_snapshot_is_current(&current).await {
            return;
        }
        match self.state_snapshot().await {
            Ok(snapshot) => {
                self.publish_current_snapshot(&publication.updates, snapshot)
                    .await;
            }
            Err(error) => tracing::error!(%error, "could not collect device state for publication"),
        }
    }

    async fn publish_current_snapshot(
        &self,
        updates: &watch::Sender<Arc<StateSnapshot>>,
        snapshot: StateSnapshot,
    ) -> bool {
        let registry = self.registry.read().await;
        if !registry.descriptors().eq(snapshot.descriptors.iter()) {
            return false;
        }
        if **updates.borrow() == snapshot {
            return true;
        }
        updates.send_replace(Arc::new(snapshot));
        true
    }

    async fn published_snapshot_is_current(&self, snapshot: &StateSnapshot) -> bool {
        let now_unix_ms = unix_millis(SystemTime::now());
        let registry = self.registry.read().await;
        if registry.proxy_id() != snapshot.proxy_id
            || !registry.descriptors().eq(snapshot.descriptors.iter())
            || !registry.discovery_identities_match(&snapshot.discovery_identities)
        {
            return false;
        }
        let mut publications = snapshot.publications.iter();
        for descriptor in registry.descriptors() {
            let Some(response) = publications.next() else {
                return false;
            };
            if descriptor.id != response.id || descriptor.backend != response.backend {
                return false;
            }
            let Some(runtime) = registry.runtime(&descriptor.id) else {
                return false;
            };
            let current = if descriptor.backend == DeviceBackend::LegacyBle
                && descriptor.id.as_str() == "configured"
                && let Some(ble) = &self.ble_device
            {
                ble.state_response_matches(response, now_unix_ms).await
            } else {
                runtime
                    .matches_response_at(response, now_unix_ms, None, false)
                    .await
            };
            if !current {
                return false;
            }
        }
        publications.next().is_none()
    }

    pub(crate) async fn state_snapshot(&self) -> Result<StateSnapshot, ServiceError> {
        let (proxy_id, descriptors, discovery_identities) = {
            let registry = self.registry.read().await;
            (
                registry.proxy_id(),
                registry.descriptors().cloned().collect::<Vec<_>>(),
                registry.discovery_identities().collect(),
            )
        };
        let publications = stream::iter(descriptors.iter())
            .then(|descriptor| self.state(&descriptor.id))
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

#[cfg(test)]
#[path = "tests/publication.rs"]
mod tests;
