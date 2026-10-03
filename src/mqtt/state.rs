use std::{collections::HashSet, sync::Arc};

use crate::service::publication::StateSnapshot;
use futures_util::{Stream, StreamExt, future, stream};
use gafctl_api::DeviceStateV2Response;
use tokio::sync::watch;

use super::{connection::MqttConnection, discovery, topics::Topics};
use gafctl_api::{DeviceBackend, DeviceId};

struct StateSubscriptions {
    state: watch::Receiver<Arc<StateSnapshot>>,
    connected: watch::Receiver<bool>,
}

fn state_payloads(
    state: watch::Receiver<Arc<StateSnapshot>>,
    connected: watch::Receiver<bool>,
) -> impl Stream<Item = Arc<StateSnapshot>> {
    stream::unfold(
        StateSubscriptions { state, connected },
        receive_state_update,
    )
    .filter_map(future::ready)
}

async fn receive_state_update(
    mut subscriptions: StateSubscriptions,
) -> Option<(Option<Arc<StateSnapshot>>, StateSubscriptions)> {
    tokio::select! {
        changed = subscriptions.state.changed() => changed,
        changed = subscriptions.connected.changed() => changed,
    }
    .ok()?;
    let active = *subscriptions.connected.borrow_and_update();
    let payload = active.then(|| Arc::clone(&*subscriptions.state.borrow_and_update()));
    Some((payload, subscriptions))
}

pub(super) async fn publish_state_updates(
    connection: MqttConnection,
    state: watch::Receiver<Arc<StateSnapshot>>,
    connected: watch::Receiver<bool>,
    discovery_enabled: bool,
) {
    state_payloads(state, connected)
        .fold(HashSet::new(), |previous_topics, snapshot| {
            let connection = &connection;
            async move {
                let active =
                    publish_discovery(connection, &snapshot, discovery_enabled, previous_topics)
                        .await;
                stream::iter(snapshot.publications.iter())
                    .for_each(|publication| publish_device_state(connection, publication))
                    .await;
                active
            }
        })
        .await;
}

fn state_messages(
    topics: Topics,
    publication: &DeviceStateV2Response,
) -> serde_json::Result<[(String, Vec<u8>); 2]> {
    Ok([
        (
            topics.device(&publication.id, "state"),
            serde_json::to_vec(publication)?,
        ),
        (
            topics.device(&publication.id, "availability"),
            if publication.available {
                b"online".to_vec()
            } else {
                b"offline".to_vec()
            },
        ),
    ])
}

async fn publish_device_state(connection: &MqttConnection, publication: &DeviceStateV2Response) {
    match state_messages(connection.topics(), publication) {
        Ok(messages) => {
            stream::iter(messages)
                .for_each(|(topic, payload)| connection.publish_retained(topic, payload))
                .await
        }
        Err(error) => tracing::error!(%error, "could not serialize device state for MQTT"),
    }
}

fn inactive_discovery_topics(
    topics: Topics,
    identities: &[(DeviceId, DeviceBackend)],
    previous: HashSet<String>,
    active: &HashSet<String>,
) -> impl Iterator<Item = String> {
    previous
        .into_iter()
        .chain(discovery::candidates(topics, identities))
        .filter(|topic| !active.contains(topic))
        .collect::<HashSet<_>>()
        .into_iter()
}

async fn publish_discovery(
    connection: &MqttConnection,
    snapshot: &StateSnapshot,
    enabled: bool,
    previous: HashSet<String>,
) -> HashSet<String> {
    let configs = discovery::configs(&snapshot.descriptors)
        .filter(|_| enabled)
        .collect::<Vec<_>>();
    let active = configs
        .iter()
        .map(|(topic, _)| topic.clone())
        .collect::<HashSet<_>>();
    stream::iter(inactive_discovery_topics(
        connection.topics(),
        &snapshot.discovery_identities,
        previous,
        &active,
    ))
    .for_each(|topic| connection.publish_retained(topic, Vec::new()))
    .await;
    stream::iter(configs)
        .for_each(|(topic, config)| publish_discovery_config(connection, topic, config))
        .await;
    active
}

async fn publish_discovery_config(
    connection: &MqttConnection,
    topic: String,
    config: serde_json::Value,
) {
    match serde_json::to_vec(&config) {
        Ok(payload) => connection.publish_retained(topic, payload).await,
        Err(error) => tracing::error!(%error, "could not serialize MQTT discovery config"),
    }
}

#[cfg(test)]
mod tests {
    use super::super::{
        start,
        test_support::{
            config, mqtt_device, observed_client, receive_topic, request, snapshot,
            start_native_broker, test_connection,
        },
    };
    use super::*;
    use gafctl_api::{DeviceDescriptor, EntitySource, ProxyId};
    use rumqttc::v5::mqttbytes::QoS;
    use serde_json::Value;
    use std::time::Duration;
    use tokio::time::{sleep, timeout};

    #[test]
    fn state_publications_preserve_the_canonical_response_and_availability() {
        let mut snapshot = snapshot(mqtt_device(ProxyId::default(), "typed-state"));
        let topics = Topics(snapshot.proxy_id);
        let publication = snapshot.publications.pop().unwrap();
        [true, false].into_iter().for_each(|available| {
            let publication = DeviceStateV2Response {
                available,
                state: available.then(|| publication.state.clone()).flatten(),
                ..publication.clone()
            };
            let [(state_topic, payload), (availability_topic, availability)] =
                state_messages(topics, &publication).unwrap();
            assert_eq!(state_topic, topics.device(&publication.id, "state"));
            assert_eq!(
                serde_json::from_slice::<DeviceStateV2Response>(&payload).unwrap(),
                publication
            );
            assert_eq!(
                availability_topic,
                topics.device(&publication.id, "availability")
            );
            assert_eq!(
                availability,
                if available {
                    b"online".as_slice()
                } else {
                    b"offline".as_slice()
                }
            );
        });
    }

    #[test]
    fn restarted_http_owner_tombstones_all_prior_discovery_without_history() {
        let mut device = mqtt_device(ProxyId::default(), "qc-one");
        let previous = discovery::configs(std::slice::from_ref(&device))
            .map(|(topic, _)| topic)
            .collect::<HashSet<_>>();
        device.state_source = EntitySource::Http;
        device.command_source = EntitySource::Http;
        let inactive = inactive_discovery_topics(
            Topics(device.proxy_id),
            &[(device.id.clone(), device.backend)],
            HashSet::new(),
            &HashSet::new(),
        )
        .collect::<HashSet<_>>();
        assert!(previous.is_subset(&inactive));
        assert!(
            inactive
                .iter()
                .all(|topic| topic.contains(&device.proxy_id.to_string()))
        );
    }

    #[tokio::test]
    async fn native_broker_retains_state_and_tombstones_http_owner_after_restart() {
        let broker = start_native_broker().await;
        let device = mqtt_device(ProxyId::default(), "configured");
        let topics = Topics(device.proxy_id);
        let (observer, mut received) = observed_client("observer", broker.port);
        observer
            .subscribe(topics.process_availability(), QoS::AtLeastOnce)
            .await
            .unwrap();
        let bridge = start(config(broker.port, true), snapshot(device.clone()));
        let online = receive_topic(&mut received, &topics.process_availability()).await;
        assert_eq!(online.payload.as_ref(), b"online");
        let discovery_topic = topics.discovery(&device.id, "sensor", "temperature");
        observer
            .subscribe(&discovery_topic, QoS::AtLeastOnce)
            .await
            .unwrap();
        let discovered = receive_topic(&mut received, &discovery_topic).await;
        let config: Value = serde_json::from_slice(&discovered.payload).unwrap();
        assert_eq!(
            config["device"]["identifiers"][0],
            topics.identifier(&device.id)
        );
        observer
            .subscribe(topics.device(&device.id, "state"), QoS::AtLeastOnce)
            .await
            .unwrap();
        let state = receive_topic(&mut received, &topics.device(&device.id, "state")).await;
        // Receiving state fences the initial discovery and state publications.
        // A separate ordinary subscriber must now receive both from broker storage.
        let (late_observer, mut replayed) = observed_client("late-observer", broker.port);
        late_observer
            .subscribe(&discovery_topic, QoS::AtLeastOnce)
            .await
            .unwrap();
        let retained_discovery = receive_topic(&mut replayed, &discovery_topic).await;
        assert!(retained_discovery.retain);
        assert_eq!(retained_discovery.payload, discovered.payload);
        late_observer
            .subscribe(topics.device(&device.id, "state"), QoS::AtLeastOnce)
            .await
            .unwrap();
        let retained_state =
            receive_topic(&mut replayed, &topics.device(&device.id, "state")).await;
        assert!(retained_state.retain);
        assert_eq!(retained_state.payload, state.payload);
        assert_eq!(
            serde_json::from_slice::<DeviceStateV2Response>(&retained_state.payload).unwrap(),
            snapshot(device.clone()).publications[0]
        );
        let mut changed = device;
        changed.state_source = EntitySource::Http;
        changed.command_source = EntitySource::Http;
        // This fresh publisher has no in-memory discovery history.
        let (client, _) = observed_client("restarted-publisher", broker.port);
        let (connection, _publisher) = test_connection(client, topics);
        let mut inactive = snapshot(changed);
        inactive.descriptors.clear();
        publish_discovery(&connection, &inactive, true, HashSet::new()).await;
        let tombstone = receive_topic(&mut received, &discovery_topic).await;
        assert!(tombstone.payload.is_empty());
        drop(bridge);
    }

    #[tokio::test]
    async fn native_broker_isolates_two_proxies_with_the_same_local_device_id() {
        let broker = start_native_broker().await;
        let first = mqtt_device(ProxyId::default(), "configured");
        let second = mqtt_device(ProxyId::default(), "configured");
        let topics = Topics(first.proxy_id);
        let other = Topics(second.proxy_id);
        let (observer, mut received) = observed_client("two-proxy-observer", broker.port);
        observer
            .subscribe(topics.process_availability(), QoS::AtLeastOnce)
            .await
            .unwrap();
        let mut first_bridge = start(config(broker.port, true), snapshot(first.clone()));
        receive_topic(&mut received, &topics.process_availability()).await;
        observer
            .subscribe(other.process_availability(), QoS::AtLeastOnce)
            .await
            .unwrap();
        let mut second_bridge = start(config(broker.port, true), snapshot(second.clone()));
        receive_topic(&mut received, &other.process_availability()).await;
        observer
            .publish(
                topics.device(&first.id, "control/set"),
                QoS::AtLeastOnce,
                false,
                request("first-only"),
            )
            .await
            .unwrap();
        let work = timeout(Duration::from_secs(5), first_bridge.device_requests.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(work.request.request_id().as_str(), "first-only");
        assert!(
            timeout(
                Duration::from_millis(200),
                second_bridge.device_requests.recv()
            )
            .await
            .is_err()
        );
        drop(work);
        observer
            .subscribe(
                other.discovery(&second.id, "sensor", "temperature"),
                QoS::AtLeastOnce,
            )
            .await
            .unwrap();
        let discovered = receive_topic(
            &mut received,
            &other.discovery(&second.id, "sensor", "temperature"),
        )
        .await;
        assert!(!discovered.payload.is_empty());
        first_bridge
            .state_updates
            .send_replace(Arc::new(StateSnapshot {
                proxy_id: first.proxy_id,
                discovery_identities: vec![(first.id.clone(), first.backend)],
                descriptors: vec![DeviceDescriptor {
                    state_source: EntitySource::Http,
                    command_source: EntitySource::Http,
                    ..first
                }],
                publications: Vec::new(),
            }));
        sleep(Duration::from_millis(100)).await;
        let (late, mut retained) = observed_client("late-observer", broker.port);
        late.subscribe(
            other.discovery(&second.id, "sensor", "temperature"),
            QoS::AtLeastOnce,
        )
        .await
        .unwrap();
        assert!(
            !receive_topic(
                &mut retained,
                &other.discovery(&second.id, "sensor", "temperature")
            )
            .await
            .payload
            .is_empty()
        );
    }

    #[tokio::test]
    async fn native_broker_restart_republishes_current_state() {
        let mut broker = start_native_broker().await;
        let device = mqtt_device(ProxyId::default(), "qc-reconnect");
        let topics = Topics(device.proxy_id);
        let (observer, mut received) = observed_client("reconnect-observer", broker.port);
        observer
            .subscribe(topics.process_availability(), QoS::AtLeastOnce)
            .await
            .unwrap();
        let bridge = start(config(broker.port, true), snapshot(device.clone()));
        receive_topic(&mut received, &topics.process_availability()).await;
        let discovery_topic = topics.discovery(&device.id, "sensor", "temperature");
        let state_topic = topics.device(&device.id, "state");
        for topic in [&discovery_topic, &state_topic] {
            observer.subscribe(topic, QoS::AtLeastOnce).await.unwrap();
            assert!(!receive_topic(&mut received, topic).await.payload.is_empty());
        }
        broker.restart().await;
        let (observer, mut received) = observed_client("reconnected-observer", broker.port);
        observer
            .subscribe(topics.process_availability(), QoS::AtLeastOnce)
            .await
            .unwrap();
        receive_topic(&mut received, &topics.process_availability()).await;
        observer
            .subscribe(topics.device(&device.id, "state"), QoS::AtLeastOnce)
            .await
            .unwrap();
        assert!(
            !receive_topic(&mut received, &topics.device(&device.id, "state"))
                .await
                .payload
                .is_empty()
        );
        observer
            .subscribe(
                topics.discovery(&device.id, "sensor", "temperature"),
                QoS::AtLeastOnce,
            )
            .await
            .unwrap();
        assert!(
            !receive_topic(
                &mut received,
                &topics.discovery(&device.id, "sensor", "temperature")
            )
            .await
            .payload
            .is_empty()
        );
        drop(bridge);
    }
}
