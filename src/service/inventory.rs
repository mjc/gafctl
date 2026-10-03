use super::{DeviceService, ServiceError};
use anyhow::Result;
use gafctl_api::{
    DeviceBackend, DeviceDescriptor, DeviceId, DeviceListV2Response, DeviceStateV2Response,
    EntitySource, EntitySources,
};
impl DeviceService {
    pub(crate) async fn inventory(&self) -> DeviceListV2Response {
        DeviceListV2Response {
            devices: self.registry.read().await.descriptors().cloned().collect(),
        }
    }

    pub(crate) async fn state(&self, id: &DeviceId) -> Result<DeviceStateV2Response, ServiceError> {
        device_state_v2_data(self, id).await
    }

    pub(crate) async fn set_sources(
        &self,
        id: &DeviceId,
        sources: EntitySources,
    ) -> Result<DeviceDescriptor, ServiceError> {
        if sources.state_source != sources.command_source {
            return Err(ServiceError::InvalidSources);
        }
        let descriptor = {
            let mut registry = self.registry.write().await;
            if registry.runtime(id).is_none() {
                return Err(ServiceError::UnknownDevice);
            }
            if sources.state_source == EntitySource::Mqtt && !mqtt_ownership_available(self) {
                return Err(ServiceError::OwnershipUnavailable);
            }
            registry
                .set_entity_sources(id, sources.state_source, sources.command_source)
                .map_err(|error| {
                    tracing::error!(%error, "could not persist entity ownership");
                    ServiceError::Persistence
                })?;
            registry
                .descriptors()
                .find(|descriptor| &descriptor.id == id)
                .cloned()
                .ok_or(ServiceError::UnknownDevice)?
        };
        self.publish_state().await;
        Ok(descriptor)
    }
}

async fn device_state_v2_data(
    state: &DeviceService,
    id: &DeviceId,
) -> Result<DeviceStateV2Response, ServiceError> {
    let registry = state.registry.read().await;
    let descriptor = registry
        .descriptors()
        .find(|descriptor| descriptor.id == *id)
        .ok_or(ServiceError::UnknownDevice)?;
    let runtime = registry.runtime(id).ok_or(ServiceError::UnknownDevice)?;
    let snapshot = runtime.snapshot().await;
    let mut response = DeviceStateV2Response {
        id: id.clone(),
        backend: descriptor.backend,
        available: snapshot.state.is_some(),
        inventory_status: snapshot.inventory_status,
        last_error: snapshot.last_error,
        state: snapshot.state,
    };
    if descriptor.backend == DeviceBackend::LegacyBle
        && *id == DeviceId::configured_ble()
        && let Some(ble) = &state.ble_device
    {
        ble.decorate_state_response(&mut response).await;
    }
    Ok(response)
}
#[cfg(feature = "mqtt")]
fn mqtt_ownership_available(state: &DeviceService) -> bool {
    state
        .publication
        .as_ref()
        .is_some_and(|publication| publication.allow_mqtt_ownership())
}
#[cfg(not(feature = "mqtt"))]
fn mqtt_ownership_available(_: &DeviceService) -> bool {
    false
}

#[cfg(test)]
#[path = "tests/inventory.rs"]
mod tests;
