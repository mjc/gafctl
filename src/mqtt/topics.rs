use super::RequestKind;
use gafctl_api::{DeviceId, ProxyId};

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

#[cfg(test)]
mod tests {
    use super::super::{discovery, test_support::mqtt_device};
    use super::*;

    #[test]
    fn discovery_topics_include_the_persisted_proxy_identity() {
        let device = mqtt_device(ProxyId::default(), "configured");
        let topics = Topics(device.proxy_id);
        let configs = discovery::configs(std::slice::from_ref(&device)).collect::<Vec<_>>();
        assert!(
            configs
                .iter()
                .any(|(_, config)| config["state_topic"] == topics.device(&device.id, "state"))
        );
        assert!(configs.iter().all(
            |(topic, config)| topic.contains(&device.proxy_id.to_string())
                && config["device"]["identifiers"][0] == topics.identifier(&device.id)
        ));
    }

    #[test]
    fn discovery_topics_use_a_broker_acl_node_owned_by_gafctl() {
        let device = mqtt_device(ProxyId::default(), "configured");
        let topics = Topics(device.proxy_id);
        assert_eq!(
            topics.discovery(&device.id, "sensor", "temperature"),
            format!(
                "homeassistant/sensor/gafctl/{}_temperature/config",
                topics.identifier(&device.id)
            )
        );
    }

    #[test]
    fn controls_only_route_within_this_proxy() {
        let topics = Topics(ProxyId::default());
        let id = DeviceId::configured_ble();
        assert_eq!(
            topics.request_device(&topics.device(&id, "control/set")),
            Some((id, RequestKind::Control))
        );
        assert_eq!(
            topics.request_device(
                &Topics(ProxyId::default()).device(&DeviceId::configured_ble(), "control/set")
            ),
            None
        );
        assert_eq!(topics.request_device("gafctl/gaf_vent/control/set"), None);
        assert_eq!(
            topics.request_device(&format!("gafctl/{}/a/b/control/set", topics.0)),
            None
        );
        assert_eq!(
            topics.request_device(&topics.device(&DeviceId::configured_ble(), "state")),
            None
        );
    }
}
