use std::{collections::HashSet, sync::Arc};

use crate::service::publication::StateSnapshot;
use futures_util::{Stream, StreamExt, TryStreamExt, future, stream};
use gafctl_api::DeviceStateV2Response;
use rumqttc_next::{AsyncClient, PublishOptions};
use tokio::sync::watch;

use super::{discovery, topics::Topics};
use crate::service::DeviceService;
use gafctl_api::{DeviceBackend, DeviceId};

struct StateSubscriptions {
    state: watch::Receiver<Arc<StateSnapshot>>,
    connected: watch::Receiver<bool>,
    service: Option<DeviceService>,
}

fn state_payloads(
    state: watch::Receiver<Arc<StateSnapshot>>,
    connected: watch::Receiver<bool>,
    service: Option<DeviceService>,
) -> impl Stream<Item = Arc<StateSnapshot>> {
    stream::unfold(
        StateSubscriptions {
            state,
            connected,
            service,
        },
        receive_state_update,
    )
    .filter_map(future::ready)
}

async fn receive_state_update(
    mut subscriptions: StateSubscriptions,
) -> Option<(Option<Arc<StateSnapshot>>, StateSubscriptions)> {
    tokio::select! {
        changed = subscriptions.state.changed() => changed.ok()?,
        changed = subscriptions.connected.changed() => changed.ok()?,
    }
    let active = *subscriptions.connected.borrow_and_update();
    if active && let Some(service) = &subscriptions.service {
        service.publish_state().await;
    }
    let payload = active.then(|| Arc::clone(&*subscriptions.state.borrow_and_update()));
    Some((payload, subscriptions))
}

pub(super) async fn publish_state_updates(
    client: AsyncClient,
    topics: Topics,
    state: watch::Receiver<Arc<StateSnapshot>>,
    connected: watch::Receiver<bool>,
    discovery_enabled: bool,
    service: Option<DeviceService>,
) {
    state_payloads(state, connected, service)
        .fold(HashSet::new(), |previous_topics, snapshot| {
            let client = &client;
            async move {
                let active = publish_discovery(
                    client,
                    topics,
                    &snapshot,
                    discovery_enabled,
                    previous_topics,
                )
                .await;
                stream::iter(snapshot.publications.iter())
                    .for_each(|publication| publish_device_state(client, topics, publication))
                    .await;
                active
            }
        })
        .await;
}

async fn publish_retained(client: &AsyncClient, topic: String, payload: Vec<u8>) {
    if let Err(error) = client
        .publish(topic, payload, PublishOptions::at_least_once().retained())
        .await
    {
        tracing::warn!(%error, "could not publish retained MQTT message");
    }
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

async fn publish_device_state(
    client: &AsyncClient,
    topics: Topics,
    publication: &DeviceStateV2Response,
) {
    match state_messages(topics, publication) {
        Ok(messages) => {
            stream::iter(messages)
                .for_each(|(topic, payload)| publish_retained(client, topic, payload))
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
        .chain(identities.iter().map(|(id, _)| topics.discovery(id)))
        .filter(|topic| !active.contains(topic))
        .collect::<HashSet<_>>()
        .into_iter()
}

async fn publish_discovery(
    client: &AsyncClient,
    topics: Topics,
    snapshot: &StateSnapshot,
    enabled: bool,
    previous: HashSet<String>,
) -> HashSet<String> {
    let configs = snapshot
        .descriptors
        .iter()
        .filter(|_| enabled)
        .flat_map(|device| {
            discovery::configs(std::slice::from_ref(device))
                .map(move |(topic, config)| (device, topic, config))
        })
        .collect::<Vec<_>>();
    let desired = configs
        .iter()
        .map(|(_, topic, _)| topic.clone())
        .collect::<HashSet<_>>();
    let mut active = previous
        .intersection(&desired)
        .cloned()
        .collect::<HashSet<_>>();
    let inactive_identities = snapshot
        .discovery_identities
        .iter()
        .filter(|(id, _)| !desired.contains(&topics.discovery(id)))
        .cloned()
        .collect::<Vec<_>>();
    let inactive_topics = inactive_discovery_topics(
        topics,
        &snapshot.discovery_identities,
        previous.clone(),
        &desired,
    )
    .chain(discovery::component_topics(topics, &inactive_identities));
    if !publish_discovery_messages(
        client,
        inactive_topics,
        b"",
        PublishOptions::at_least_once().retained(),
    )
    .await
    {
        return previous;
    }
    for (device, topic, config) in configs {
        let identity = (device.id.clone(), device.backend);
        if publish_device_discovery(
            client,
            topics,
            &identity,
            topic.clone(),
            config,
            !previous.contains(&topic),
        )
        .await
        {
            active.insert(topic);
        } else {
            break;
        }
    }
    active
}

async fn publish_device_discovery(
    client: &AsyncClient,
    topics: Topics,
    identity: &(DeviceId, DeviceBackend),
    topic: String,
    config: serde_json::Value,
    migrating: bool,
) -> bool {
    if migrating
        && !publish_component_messages(
            client,
            topics,
            identity,
            br#"{"migrate_discovery":true}"#,
            PublishOptions::at_least_once(),
        )
        .await
    {
        return false;
    }
    let payload = match serde_json::to_vec(&config) {
        Ok(payload) => payload,
        Err(error) => {
            tracing::error!(%error, "could not serialize MQTT discovery config");
            return false;
        }
    };
    if !publish_discovery_message(
        client,
        topic,
        payload,
        PublishOptions::at_least_once().retained(),
    )
    .await
    {
        return false;
    }
    !migrating
        || publish_component_messages(
            client,
            topics,
            identity,
            b"",
            PublishOptions::at_least_once().retained(),
        )
        .await
}

async fn publish_component_messages(
    client: &AsyncClient,
    topics: Topics,
    identity: &(DeviceId, DeviceBackend),
    payload: &[u8],
    options: PublishOptions,
) -> bool {
    publish_discovery_messages(
        client,
        discovery::component_topics(topics, std::slice::from_ref(identity)),
        payload,
        options,
    )
    .await
}

async fn publish_discovery_messages(
    client: &AsyncClient,
    topics: impl IntoIterator<Item = String>,
    payload: &[u8],
    options: PublishOptions,
) -> bool {
    stream::iter(topics)
        .map(Ok::<_, ()>)
        .try_fold((), |(), topic| {
            let options = options.clone();
            async move {
                publish_discovery_message(client, topic, payload.to_vec(), options)
                    .await
                    .then_some(())
                    .ok_or(())
            }
        })
        .await
        .is_ok()
}

async fn publish_discovery_message(
    client: &AsyncClient,
    topic: String,
    payload: Vec<u8>,
    options: PublishOptions,
) -> bool {
    let completion = tokio::time::timeout(std::time::Duration::from_secs(30), async {
        let notice = client.publish_tracked(topic, payload, options).await?;
        notice.wait_completion_async().await?;
        Ok::<(), anyhow::Error>(())
    })
    .await;
    match completion {
        Ok(Ok(())) => true,
        Ok(Err(error)) => {
            tracing::warn!(%error, "could not complete MQTT discovery publication");
            false
        }
        Err(error) => {
            tracing::warn!(%error, "MQTT discovery publication timed out");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::{
        start_for_test,
        test_support::{
            config, mqtt_device, observed_client, receive_topic, request, snapshot,
            start_native_broker,
        },
    };
    use super::*;
    use gafctl_api::{DeviceDescriptor, EntitySource, ProxyId};
    use rumqttc_next::QoS;
    use serde_json::Value;
    use std::time::Duration;
    use tokio::time::{sleep, timeout};

    #[tokio::test]
    async fn server_mqtt_start_rechecks_retained_state_after_broker_reconnect() {
        let (mut service, id, fixture, _server, _directory) =
            crate::service::test_support::refresh_fixture().await;
        fixture.release.notify_one();
        service.refresh_device(&id).await.unwrap();
        fixture.entered.notified().await;
        let snapshot = service.state_snapshot().await.unwrap();
        let device = snapshot.descriptors[0].clone();
        let topics = Topics(snapshot.proxy_id);
        let mut broker = start_native_broker().await;
        let mut mqtt = crate::server::mqtt::start(&mut service, config(broker.port, true))
            .await
            .unwrap();
        let (observer, mut received) = observed_client("startup-reconnect-observer", broker.port);
        observer
            .subscribe(topics.process_availability(), QoS::AtLeastOnce)
            .await
            .unwrap();
        assert_eq!(
            receive_topic(&mut received, &topics.process_availability())
                .await
                .payload
                .as_ref(),
            b"online"
        );
        observer
            .subscribe(topics.device(&device.id, "state"), QoS::AtLeastOnce)
            .await
            .unwrap();
        let initial = receive_topic(&mut received, &topics.device(&device.id, "state")).await;
        assert!(
            serde_json::from_slice::<DeviceStateV2Response>(&initial.payload)
                .unwrap()
                .available
        );
        crate::service::test_support::age_state_for_test(&service, &id, Duration::from_secs(91))
            .await;

        broker.restart().await;
        let (reconnected, mut replayed) = observed_client("startup-reconnect-reader", broker.port);
        reconnected
            .subscribe(topics.process_availability(), QoS::AtLeastOnce)
            .await
            .unwrap();
        assert_eq!(
            receive_topic(&mut replayed, &topics.process_availability())
                .await
                .payload
                .as_ref(),
            b"online"
        );
        reconnected
            .subscribe(topics.device(&device.id, "state"), QoS::AtLeastOnce)
            .await
            .unwrap();
        let retained = receive_topic(&mut replayed, &topics.device(&device.id, "state")).await;
        let retained: DeviceStateV2Response = serde_json::from_slice(&retained.payload).unwrap();
        assert!(!retained.available);
        reconnected
            .subscribe(topics.device(&device.id, "availability"), QoS::AtLeastOnce)
            .await
            .unwrap();
        assert_eq!(
            receive_topic(&mut replayed, &topics.device(&device.id, "availability"))
                .await
                .payload
                .as_ref(),
            b"offline"
        );
        mqtt.drain(tokio::time::Instant::now() + Duration::from_secs(5))
            .await;
    }

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
    async fn native_broker_migrates_existing_component_discovery_before_clearing_it() {
        let broker = start_native_broker().await;
        let device = mqtt_device(ProxyId::default(), "migration");
        let topics = Topics(device.proxy_id);
        let identities = [(device.id.clone(), device.backend)];
        let old_topics = discovery::component_topics(topics, &identities).collect::<HashSet<_>>();
        let old_topic = format!(
            "homeassistant/sensor/gafctl/{}_temperature/config",
            topics.identifier(&device.id)
        );
        let grouped_topic = format!(
            "homeassistant/device/gafctl/{}/config",
            topics.identifier(&device.id)
        );
        let (observer, mut received) = observed_client("migration-observer", broker.port);
        observer
            .subscribe("homeassistant/#", QoS::AtLeastOnce)
            .await
            .unwrap();
        observer
            .publish(
                &old_topic,
                br#"{"unique_id":"existing"}"#.to_vec(),
                PublishOptions::at_least_once().retained(),
            )
            .await
            .unwrap();
        receive_topic(&mut received, &old_topic).await;
        let (client, _) = observed_client("migration-publisher", broker.port);
        publish_discovery(&client, topics, &snapshot(device), true, HashSet::new()).await;
        let first = timeout(Duration::from_secs(15), received.next())
            .await
            .unwrap()
            .unwrap();
        assert!(old_topics.contains(std::str::from_utf8(&first.topic).unwrap()));
        assert_eq!(
            serde_json::from_slice::<Value>(&first.payload).unwrap()["migrate_discovery"],
            true
        );
        let rest = timeout(
            Duration::from_secs(15),
            received
                .by_ref()
                .take(old_topics.len() * 2)
                .collect::<Vec<_>>(),
        )
        .await
        .unwrap();
        let messages = std::iter::once(first).chain(rest).collect::<Vec<_>>();
        assert!(messages[..old_topics.len()].iter().all(|message| {
            old_topics.contains(std::str::from_utf8(&message.topic).unwrap())
                && serde_json::from_slice::<Value>(&message.payload).unwrap()["migrate_discovery"]
                    == true
        }));
        assert_eq!(messages[old_topics.len()].topic, grouped_topic);
        assert!(messages[old_topics.len() + 1..].iter().all(|message| {
            old_topics.contains(std::str::from_utf8(&message.topic).unwrap())
                && message.payload.is_empty()
        }));
        let (late, mut replayed) = observed_client("migration-late-observer", broker.port);
        late.subscribe("homeassistant/#", QoS::AtLeastOnce)
            .await
            .unwrap();
        let retained = timeout(Duration::from_secs(15), replayed.next())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(retained.topic, grouped_topic);
        assert!(retained.retain);
        assert!(
            timeout(Duration::from_millis(200), replayed.next())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn native_broker_replaces_lost_capabilities_without_repeating_migration() {
        let broker = start_native_broker().await;
        let mut device = mqtt_device(ProxyId::default(), "capabilities");
        let topics = Topics(device.proxy_id);
        let grouped_topic = topics.discovery(&device.id);
        let (observer, mut received) = observed_client("capability-observer", broker.port);
        observer
            .subscribe("homeassistant/#", QoS::AtLeastOnce)
            .await
            .unwrap();
        let (client, _) = observed_client("capability-publisher", broker.port);
        let previous = publish_discovery(
            &client,
            topics,
            &snapshot(device.clone()),
            true,
            HashSet::new(),
        )
        .await;
        receive_topic(&mut received, &grouped_topic).await;
        // Fence all initial migration clears before observing the next update.
        client
            .publish(
                "homeassistant/fence",
                b"ready".to_vec(),
                PublishOptions::at_least_once(),
            )
            .await
            .unwrap();
        receive_topic(&mut received, "homeassistant/fence").await;
        device.capabilities.commands.clear();
        publish_discovery(&client, topics, &snapshot(device), true, previous).await;
        let update = timeout(Duration::from_secs(15), received.next())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(update.topic, grouped_topic);
        let config: Value = serde_json::from_slice(&update.payload).unwrap();
        assert_eq!(
            config["components"]["select_mode"],
            serde_json::json!({"platform": "select"})
        );
        assert_eq!(
            config["components"]["switch_automatic_mode"],
            serde_json::json!({"platform": "switch"})
        );
        assert!(config["components"]["sensor_temperature"]["unique_id"].is_string());
        assert!(
            timeout(Duration::from_millis(200), received.next())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn native_broker_rejected_grouped_discovery_preserves_old_migration_configs() {
        let mut broker =
            super::super::test_support::start_native_broker_with_packet_limit(2_048).await;
        let device = mqtt_device(ProxyId::default(), "packet-limit");
        let topics = Topics(device.proxy_id);
        let identities = [(device.id.clone(), device.backend)];
        let old_topics = discovery::component_topics(topics, &identities).collect::<HashSet<_>>();
        let old_topic = format!(
            "homeassistant/sensor/gafctl/{}_temperature/config",
            topics.identifier(&device.id)
        );
        let original = br#"{"unique_id":"existing","state_topic":"working/state"}"#;
        let (observer, mut received) = observed_client("rejected-group-observer", broker.port);
        observer
            .subscribe("homeassistant/#", QoS::AtLeastOnce)
            .await
            .unwrap();
        observer
            .publish(
                &old_topic,
                original.to_vec(),
                PublishOptions::at_least_once().retained(),
            )
            .await
            .unwrap();
        receive_topic(&mut received, &old_topic).await;
        let (client, _) = observed_client("rejected-group-publisher", broker.port);
        let previous = publish_discovery(
            &client,
            topics,
            &snapshot(device.clone()),
            true,
            HashSet::new(),
        )
        .await;
        assert!(previous.is_empty());
        let markers = timeout(
            Duration::from_secs(15),
            received.by_ref().take(old_topics.len()).collect::<Vec<_>>(),
        )
        .await
        .unwrap();
        assert!(markers.iter().all(|message| {
            old_topics.contains(std::str::from_utf8(&message.topic).unwrap())
                && serde_json::from_slice::<Value>(&message.payload).unwrap()["migrate_discovery"]
                    == true
        }));
        assert!(
            timeout(Duration::from_millis(200), received.next())
                .await
                .is_err()
        );
        let (late, mut retained) = observed_client("rejected-group-late", broker.port);
        late.subscribe(&old_topic, QoS::AtLeastOnce).await.unwrap();
        assert_eq!(
            receive_topic(&mut retained, &old_topic)
                .await
                .payload
                .as_ref(),
            original
        );
        broker.restart().await;
        assert_successful_migration_retry(broker.port, device, previous).await;
    }

    async fn assert_successful_migration_retry(
        port: u16,
        device: DeviceDescriptor,
        previous: HashSet<String>,
    ) {
        let topics = Topics(device.proxy_id);
        let old_topics =
            discovery::component_topics(topics, &[(device.id.clone(), device.backend)])
                .collect::<HashSet<_>>();
        let (observer, mut received) = observed_client("retry-observer", port);
        observer
            .subscribe("homeassistant/#", QoS::AtLeastOnce)
            .await
            .unwrap();
        let (client, _) = observed_client("retry-publisher", port);
        let active =
            publish_discovery(&client, topics, &snapshot(device.clone()), true, previous).await;
        assert_eq!(active, HashSet::from([topics.discovery(&device.id)]));
        let messages = timeout(
            Duration::from_secs(15),
            received
                .by_ref()
                .take(old_topics.len() * 2 + 1)
                .collect::<Vec<_>>(),
        )
        .await
        .unwrap();
        assert!(messages[..old_topics.len()].iter().all(|message| {
            old_topics.contains(std::str::from_utf8(&message.topic).unwrap())
                && message.payload.as_ref() == br#"{"migrate_discovery":true}"#
        }));
        assert_eq!(
            messages[old_topics.len()].topic,
            topics.discovery(&device.id)
        );
        assert!(messages[old_topics.len() + 1..].iter().all(|message| {
            old_topics.contains(std::str::from_utf8(&message.topic).unwrap())
                && message.payload.is_empty()
        }));
        let (late, mut retained) = observed_client("retry-late", port);
        late.subscribe("homeassistant/#", QoS::AtLeastOnce)
            .await
            .unwrap();
        assert_eq!(
            timeout(Duration::from_secs(15), retained.next())
                .await
                .unwrap()
                .unwrap()
                .topic,
            topics.discovery(&device.id)
        );
        assert!(
            timeout(Duration::from_millis(200), retained.next())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn native_broker_partial_marker_rejection_preserves_configs_and_retries() {
        let device = mqtt_device(ProxyId::default(), "partial-marker");
        let topics = Topics(device.proxy_id);
        let identities = [(device.id.clone(), device.backend)];
        let old_topics = discovery::component_topics(topics, &identities)
            .take(2)
            .collect::<Vec<_>>();
        let mut broker =
            super::super::test_support::start_native_broker_with_acl(&old_topics[0]).await;
        let (observer, mut received) = observed_client("partial-observer", broker.port);
        observer
            .subscribe("homeassistant/#", QoS::AtLeastOnce)
            .await
            .unwrap();
        let original = br#"{"unique_id":"existing","state_topic":"working/state"}"#;
        for topic in &old_topics {
            observer
                .publish(
                    topic,
                    original.to_vec(),
                    PublishOptions::at_least_once().retained(),
                )
                .await
                .unwrap();
            receive_topic(&mut received, topic).await;
        }
        let mut options =
            super::super::test_support::test_mqtt_options("partial-publisher", broker.port);
        options.set_credentials("partial-migration", "unused");
        let (client, eventloop) = AsyncClient::builder(options).capacity(16).build();
        let _events = tokio_util::task::AbortOnDropHandle::new(tokio::spawn(
            eventloop.into_stream().for_each(|_| future::ready(())),
        ));
        let previous = publish_discovery(
            &client,
            topics,
            &snapshot(device.clone()),
            true,
            HashSet::new(),
        )
        .await;
        assert!(previous.is_empty());
        let marker = timeout(Duration::from_secs(15), received.next())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(marker.topic, old_topics[0]);
        assert_eq!(marker.payload.as_ref(), br#"{"migrate_discovery":true}"#);
        assert!(
            timeout(Duration::from_millis(200), received.next())
                .await
                .is_err()
        );
        let (late, mut retained) = observed_client("partial-late", broker.port);
        late.subscribe("homeassistant/#", QoS::AtLeastOnce)
            .await
            .unwrap();
        let replayed = timeout(
            Duration::from_secs(15),
            retained.by_ref().take(2).collect::<Vec<_>>(),
        )
        .await
        .unwrap();
        assert_eq!(replayed.len(), 2);
        assert_eq!(
            replayed
                .iter()
                .map(|message| std::str::from_utf8(&message.topic).unwrap().to_owned())
                .collect::<HashSet<_>>(),
            old_topics.into_iter().collect::<HashSet<_>>()
        );
        assert!(
            replayed
                .iter()
                .all(|message| message.payload.as_ref() == original)
        );
        broker.restart().await;
        assert_successful_migration_retry(broker.port, device, previous).await;
    }

    #[tokio::test]
    async fn rejected_discovery_admission_is_not_remembered_as_a_completed_migration() {
        let device = mqtt_device(ProxyId::default(), "rejected-migration");
        let topics = Topics(device.proxy_id);
        let (client, eventloop) =
            AsyncClient::builder(super::super::test_support::test_mqtt_options("closed", 1))
                .build();
        drop(eventloop);
        let snapshot = snapshot(device.clone());
        assert!(
            publish_discovery(&client, topics, &snapshot, true, HashSet::new())
                .await
                .is_empty()
        );
        let previous = HashSet::from([topics.discovery(&device.id)]);
        assert_eq!(
            publish_discovery(&client, topics, &snapshot, false, previous.clone()).await,
            previous
        );
    }

    #[tokio::test]
    async fn native_broker_disabled_discovery_clears_grouped_and_old_retained_configs() {
        let broker = start_native_broker().await;
        let device = mqtt_device(ProxyId::default(), "disabled");
        let topics = Topics(device.proxy_id);
        let grouped_topic = topics.discovery(&device.id);
        let old_topic = format!(
            "homeassistant/sensor/gafctl/{}_temperature/config",
            topics.identifier(&device.id)
        );
        let (observer, mut received) = observed_client("disabled-observer", broker.port);
        observer
            .subscribe("homeassistant/#", QoS::AtLeastOnce)
            .await
            .unwrap();
        for topic in [&grouped_topic, &old_topic] {
            observer
                .publish(
                    topic,
                    b"existing".to_vec(),
                    PublishOptions::at_least_once().retained(),
                )
                .await
                .unwrap();
            receive_topic(&mut received, topic).await;
        }
        let (client, _) = observed_client("disabled-publisher", broker.port);
        assert!(
            publish_discovery(&client, topics, &snapshot(device), false, HashSet::new())
                .await
                .is_empty()
        );
        assert!(
            receive_topic(&mut received, &grouped_topic)
                .await
                .payload
                .is_empty()
        );
        assert!(
            receive_topic(&mut received, &old_topic)
                .await
                .payload
                .is_empty()
        );
        client
            .publish(
                "homeassistant/fence",
                b"ready".to_vec(),
                PublishOptions::at_least_once(),
            )
            .await
            .unwrap();
        receive_topic(&mut received, "homeassistant/fence").await;
        let (late, mut replayed) = observed_client("disabled-late-observer", broker.port);
        late.subscribe("homeassistant/#", QoS::AtLeastOnce)
            .await
            .unwrap();
        assert!(
            timeout(Duration::from_millis(200), replayed.next())
                .await
                .is_err()
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
        let bridge = start_for_test(config(broker.port, true), snapshot(device.clone()));
        let online = receive_topic(&mut received, &topics.process_availability()).await;
        assert_eq!(online.payload.as_ref(), b"online");
        let discovery_topic = topics.discovery(&device.id);
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
        let mut inactive = snapshot(changed);
        inactive.descriptors.clear();
        publish_discovery(&client, topics, &inactive, true, HashSet::new()).await;
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
        let mut first_bridge = start_for_test(config(broker.port, true), snapshot(first.clone()));
        receive_topic(&mut received, &topics.process_availability()).await;
        observer
            .subscribe(other.process_availability(), QoS::AtLeastOnce)
            .await
            .unwrap();
        let mut second_bridge = start_for_test(config(broker.port, true), snapshot(second.clone()));
        receive_topic(&mut received, &other.process_availability()).await;
        observer
            .publish(
                topics.device(&first.id, "control/set"),
                request("first-only"),
                PublishOptions::at_least_once(),
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
            .subscribe(other.discovery(&second.id), QoS::AtLeastOnce)
            .await
            .unwrap();
        let discovered = receive_topic(&mut received, &other.discovery(&second.id)).await;
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
        late.subscribe(other.discovery(&second.id), QoS::AtLeastOnce)
            .await
            .unwrap();
        assert!(
            !receive_topic(&mut retained, &other.discovery(&second.id))
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
        let bridge = start_for_test(config(broker.port, true), snapshot(device.clone()));
        receive_topic(&mut received, &topics.process_availability()).await;
        let discovery_topic = topics.discovery(&device.id);
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
            .subscribe(topics.discovery(&device.id), QoS::AtLeastOnce)
            .await
            .unwrap();
        assert!(
            !receive_topic(&mut received, &topics.discovery(&device.id))
                .await
                .payload
                .is_empty()
        );
        drop(bridge);
    }
}
