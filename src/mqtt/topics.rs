use crate::device::{DeviceId, ProxyId};

#[derive(Clone, Copy)]
pub(super) struct Topics(pub(super) ProxyId);

impl Topics {
    pub(super) fn client_id(self) -> String {
        format!("updraft-{}", self.0)
    }

    pub(super) fn process_availability(self) -> String {
        format!("updraft/{}/availability", self.0)
    }

    pub(super) fn device(self, id: &DeviceId, suffix: &str) -> String {
        format!("updraft/{}/{}/{suffix}", self.0, id.as_str())
    }

    pub(super) fn controls(self) -> String {
        format!("updraft/{}/+/control/set", self.0)
    }

    pub(super) fn control_device(self, topic: &str) -> Option<DeviceId> {
        topic
            .strip_prefix(&format!("updraft/{}/", self.0))?
            .strip_suffix("/control/set")
            .and_then(|id| DeviceId::parse(id.to_owned()))
    }

    pub(super) fn identifier(self, id: &DeviceId) -> String {
        format!("updraft_{}_{}", self.0, id.as_str())
    }

    pub(super) fn discovery(self, id: &DeviceId, domain: &str, key: &str) -> String {
        format!(
            "homeassistant/{domain}/{}/{key}/config",
            self.identifier(id)
        )
    }
}
