use super::RequestKind;
use crate::device::{DeviceId, ProxyId};

#[derive(Clone, Copy)]
pub(super) struct Topics(pub(super) ProxyId);

impl Topics {
    pub(super) fn request_device(self, topic: &str) -> Option<(DeviceId, RequestKind)> {
        let scoped = topic.strip_prefix(&format!("gafctl/{}/", self.0))?;
        [
            ("/control/set", RequestKind::Control),
            ("/refresh/set", RequestKind::Refresh),
        ]
        .into_iter()
        .find_map(|(suffix, kind)| {
            DeviceId::parse(scoped.strip_suffix(suffix)?.to_owned()).map(|id| (id, kind))
        })
    }

    pub(super) fn refreshes(self) -> String {
        format!("gafctl/{}/+/refresh/set", self.0)
    }
    pub(super) fn client_id(self) -> String {
        format!("gafctl-{}", self.0)
    }

    pub(super) fn process_availability(self) -> String {
        format!("gafctl/{}/availability", self.0)
    }

    pub(super) fn device(self, id: &DeviceId, suffix: &str) -> String {
        format!("gafctl/{}/{}/{suffix}", self.0, id.as_str())
    }

    pub(super) fn controls(self) -> String {
        format!("gafctl/{}/+/control/set", self.0)
    }

    pub(super) fn identifier(self, id: &DeviceId) -> String {
        format!("gafctl_{}_{}", self.0, id.as_str())
    }

    pub(super) fn discovery(self, id: &DeviceId, domain: &str, key: &str) -> String {
        format!(
            "homeassistant/{domain}/gafctl/{}_{key}/config",
            self.identifier(id)
        )
    }
}
