use std::{
    collections::{HashMap, VecDeque},
    sync::Arc,
    time::Duration,
};

use futures_util::{Stream, StreamExt, future, stream};
#[cfg(test)]
use rumqttc::v5::mqttbytes::v5::Publish;
use rumqttc::{
    Outgoing,
    v5::{
        AsyncClient, ConnectionError, Event, EventLoop, MqttOptions,
        mqttbytes::{
            QoS,
            v5::{Filter, LastWill, Packet},
        },
    },
};
use tokio::{
    sync::{Semaphore, mpsc, oneshot, watch},
    time::sleep,
};

use super::{
    MqttConfig,
    requests::{
        MAX_PENDING_CONTROL_RESULTS, MqttReply, MqttRequest, MqttRequestIntake, dispatch_request,
    },
    topics::Topics,
};
use gafctl_api::DeviceId;

struct PublishRequest {
    topic: String,
    payload: Vec<u8>,
    delivery: Delivery,
}

enum Delivery {
    Retained,
    Untracked,
    Acknowledged(oneshot::Sender<()>),
}

#[derive(Default)]
struct PublishReceipts {
    queued: VecDeque<Option<oneshot::Sender<()>>>,
    sent: HashMap<u16, Option<oneshot::Sender<()>>>,
    colliding: HashMap<u16, Option<oneshot::Sender<()>>>,
    pending_unsent: usize,
}

type ReceiptTracker = Arc<std::sync::Mutex<PublishReceipts>>;

#[derive(Clone)]
pub(super) struct MqttConnection {
    client: AsyncClient,
    publications: mpsc::Sender<PublishRequest>,
    topics: Topics,
}

impl MqttConnection {
    pub(super) fn topics(&self) -> Topics {
        self.topics
    }

    fn new(
        client: AsyncClient,
        topics: Topics,
        receipts: ReceiptTracker,
    ) -> (Self, tokio::task::JoinHandle<()>) {
        let (publications, receiver) = mpsc::channel(32);
        let publisher = tokio::spawn(publish_requests(client.clone(), receiver, receipts));
        (
            Self {
                client,
                publications,
                topics,
            },
            publisher,
        )
    }

    pub(super) async fn publish_retained(&self, topic: String, payload: Vec<u8>) {
        if self
            .publications
            .send(PublishRequest {
                topic,
                payload,
                delivery: Delivery::Retained,
            })
            .await
            .is_err()
        {
            tracing::warn!("could not queue retained MQTT message");
        }
    }

    pub(super) async fn publish_result(&self, device: &DeviceId, response: &MqttReply) {
        match serde_json::to_vec(response) {
            Ok(payload) => {
                let (receipt, acknowledged) = oneshot::channel();
                if self
                    .publications
                    .send(PublishRequest {
                        topic: self.topics.device(device, response.kind().result_suffix()),
                        payload,
                        delivery: Delivery::Acknowledged(receipt),
                    })
                    .await
                    .is_err()
                    || acknowledged.await.is_err()
                {
                    tracing::warn!("could not acknowledge MQTT control result");
                }
            }
            Err(error) => tracing::error!(%error, "could not serialize MQTT control result"),
        }
    }

    pub(super) fn reject(&self, device: &DeviceId, request: &MqttRequest, status: &'static str) {
        let response = MqttReply::Rejected {
            request_id: request.request_id().clone(),
            status,
            kind: request.kind(),
        };
        match serde_json::to_vec(&response) {
            Ok(payload) => {
                if let Err(error) = self.publications.try_send(PublishRequest {
                    topic: self.topics.device(device, response.kind().result_suffix()),
                    payload,
                    delivery: Delivery::Untracked,
                }) {
                    tracing::warn!(%error, "could not enqueue MQTT control rejection");
                }
            }
            Err(error) => tracing::error!(%error, "could not serialize MQTT control rejection"),
        }
    }
}

async fn publish_requests(
    client: AsyncClient,
    receiver: mpsc::Receiver<PublishRequest>,
    receipts: ReceiptTracker,
) {
    let client = &client;
    let receipts = &receipts;
    stream::unfold(receiver, |mut receiver| async {
        receiver.recv().await.map(|request| (request, receiver))
    })
    .for_each(|request| async move {
        let (retain, receipt) = match request.delivery {
            Delivery::Retained => (true, None),
            Delivery::Untracked => (false, None),
            Delivery::Acknowledged(receipt) => (false, Some(receipt)),
        };
        receipts
            .lock()
            .expect("MQTT receipts lock poisoned")
            .queued
            .push_back(receipt);
        if let Err(error) = client
            .publish(request.topic, QoS::AtLeastOnce, retain, request.payload)
            .await
        {
            receipts
                .lock()
                .expect("MQTT receipts lock poisoned")
                .queued
                .pop_back();
            tracing::warn!(%error, "could not enqueue MQTT publication");
        }
    })
    .await;
}

fn acknowledge_publication(event: &Result<Event, ConnectionError>, receipts: &ReceiptTracker) {
    let mut receipts = receipts.lock().expect("MQTT receipts lock poisoned");
    match event {
        Ok(Event::Incoming(Packet::ConnAck(ack))) if !ack.session_present => {
            receipts.sent.clear();
            receipts.colliding.clear();
            let discarded = receipts.pending_unsent.min(receipts.queued.len());
            receipts.queued.drain(..discarded);
            receipts.pending_unsent = 0;
        }
        Ok(Event::Outgoing(Outgoing::AwaitAck(id))) => {
            // rumqttc stores this publication separately from its pending queue.
            if let Some(receipt) = receipts.queued.pop_front() {
                receipts.colliding.insert(*id, receipt);
            }
        }
        Ok(Event::Outgoing(Outgoing::Publish(id))) if !receipts.sent.contains_key(id) => {
            let receipt = receipts
                .colliding
                .remove(id)
                .or_else(|| receipts.queued.pop_front());
            if let Some(receipt) = receipt {
                receipts.sent.insert(*id, receipt);
            }
        }
        Ok(Event::Incoming(Packet::PubAck(ack))) => {
            if let Some(Some(receipt)) = receipts.sent.remove(&ack.pkid) {
                match ack.reason {
                    rumqttc::v5::mqttbytes::v5::PubAckReason::Success
                    | rumqttc::v5::mqttbytes::v5::PubAckReason::NoMatchingSubscribers => {
                        let _ = receipt.send(());
                    }
                    _ => tracing::warn!(reason = ?ack.reason, "broker rejected MQTT publication"),
                }
            }
        }
        _ => {}
    }
}

fn mqtt_options(config: MqttConfig, topics: Topics) -> MqttOptions {
    let mut options = MqttOptions::new(topics.client_id(), config.host, config.port);
    options.set_keep_alive(Duration::from_secs(30));
    options.set_credentials(config.username, config.password);
    options.set_last_will(LastWill::new(
        topics.process_availability(),
        "offline",
        QoS::AtLeastOnce,
        true,
        None,
    ));
    options
}

fn control_subscription(topics: Topics) -> Filter {
    Filter {
        preserve_retain: true,
        ..Filter::new(topics.controls(), QoS::AtLeastOnce)
    }
}

async fn setup_connection(connection: MqttConnection, connected: watch::Receiver<bool>) {
    connection_changes(connected)
        .filter(|active| future::ready(*active))
        .for_each(|_| initialize_connection(&connection))
        .await;
}

fn connection_changes(connected: watch::Receiver<bool>) -> impl Stream<Item = bool> {
    stream::unfold(connected, |mut connected| async {
        connected.changed().await.ok()?;
        let active = *connected.borrow_and_update();
        Some((active, connected))
    })
}

async fn initialize_connection(connection: &MqttConnection) {
    if let Err(error) = connection
        .client
        .subscribe_many([
            control_subscription(connection.topics),
            Filter {
                preserve_retain: true,
                ..Filter::new(connection.topics.refreshes(), QoS::AtLeastOnce)
            },
        ])
        .await
    {
        tracing::warn!(%error, "could not subscribe to MQTT controls");
    }
    connection
        .publish_retained(connection.topics.process_availability(), b"online".to_vec())
        .await;
}

fn tracked_mqtt_events(
    eventloop: EventLoop,
    receipts: ReceiptTracker,
) -> impl Stream<Item = Result<Event, ConnectionError>> {
    stream::unfold((eventloop, receipts), |(mut eventloop, receipts)| async {
        let event = eventloop.poll().await;
        if let Ok(Event::Incoming(Packet::ConnAck(ack))) = &event
            && !ack.session_present
        {
            // rumqttc 0.25.1 clean() preserves collisions, but a fresh broker
            // session cannot acknowledge the old packet needed to release one.
            eventloop.state.collision = None;
        }
        if event.is_err() {
            let mut tracked = receipts.lock().expect("MQTT receipts lock poisoned");
            // A network write can fail after rumqttc assigned an ID but before it
            // returned Outgoing::Publish. Preserve that ticket for retransmission.
            eventloop.pending.iter().for_each(|request| {
                if let rumqttc::v5::Request::Publish(publish) = request
                    && publish.pkid != 0
                    && !tracked.sent.contains_key(&publish.pkid)
                    && let Some(receipt) = tracked.queued.pop_front()
                {
                    tracked.sent.insert(publish.pkid, receipt);
                }
            });
            // rumqttc drops pending packets when the broker starts a fresh session.
            tracked.pending_unsent = eventloop
                .pending
                .iter()
                .filter(|request| match request {
                    rumqttc::v5::Request::Publish(publish) => publish.pkid == 0,
                    _ => false,
                })
                .count();
        }
        Some((event, (eventloop, receipts)))
    })
}

async fn run_event_loop(
    eventloop: EventLoop,
    connection: MqttConnection,
    connected: watch::Sender<bool>,
    controls: MqttRequestIntake,
    receipts: ReceiptTracker,
) {
    let pending_results = Arc::new(Semaphore::new(MAX_PENDING_CONTROL_RESULTS));
    tracked_mqtt_events(eventloop, receipts.clone())
        .for_each(|event| {
            acknowledge_publication(&event, &receipts);
            handle_mqtt_event(event, &connection, &connected, &controls, &pending_results)
        })
        .await;
}

async fn handle_mqtt_event(
    event: Result<Event, ConnectionError>,
    connection: &MqttConnection,
    connected: &watch::Sender<bool>,
    controls: &MqttRequestIntake,
    pending_results: &Arc<Semaphore>,
) {
    match event {
        Ok(Event::Incoming(Packet::ConnAck(_))) => {
            connected.send_replace(true);
        }
        Ok(Event::Incoming(Packet::Publish(message))) => {
            if let Ok(topic) = std::str::from_utf8(&message.topic)
                && let Some((device, kind)) = connection.topics.request_device(topic)
            {
                dispatch_request(connection, controls, pending_results, device, kind, message);
            }
        }
        Ok(_) => {}
        Err(error) => {
            connected.send_replace(false);
            tracing::warn!(%error, "MQTT connection lost; reconnecting");
            sleep(Duration::from_secs(1)).await;
        }
    }
}

pub(super) fn start(
    config: MqttConfig,
    topics: Topics,
    controls: MqttRequestIntake,
    connected_tx: watch::Sender<bool>,
    connected_rx: watch::Receiver<bool>,
) -> (MqttConnection, [tokio::task::JoinHandle<()>; 3]) {
    let (client, eventloop) = AsyncClient::new(mqtt_options(config, topics), 32);
    let receipts = ReceiptTracker::default();
    let (connection, publisher) = MqttConnection::new(client, topics, receipts.clone());
    let setup = tokio::spawn(setup_connection(connection.clone(), connected_rx));
    let event_loop = tokio::spawn(run_event_loop(
        eventloop,
        connection.clone(),
        connected_tx,
        controls,
        receipts,
    ));
    (connection, [setup, event_loop, publisher])
}

#[cfg(test)]
pub(super) fn test_connection(
    client: AsyncClient,
    topics: Topics,
) -> (MqttConnection, tokio::task::JoinHandle<()>) {
    MqttConnection::new(client, topics, ReceiptTracker::default())
}

#[cfg(test)]
pub(super) fn observed_client(
    client_id: &str,
    port: u16,
) -> (AsyncClient, mpsc::UnboundedReceiver<Publish>) {
    let (client, eventloop) =
        AsyncClient::new(super::test_support::test_mqtt_options(client_id, port), 16);
    let (messages, received) = mpsc::unbounded_channel();
    tokio::spawn(
        tracked_mqtt_events(eventloop, ReceiptTracker::default())
            .filter_map(|event| async move { event.ok() })
            .filter_map(|event| async move {
                match event {
                    Event::Incoming(Packet::Publish(message)) => Some(message),
                    _ => None,
                }
            })
            .for_each(move |message| {
                let _ = messages.send(message);
                future::ready(())
            }),
    );
    (client, received)
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{
        config, receive_topic, start_native_broker, test_mqtt_options,
    };
    use super::*;
    use gafctl_api::ProxyId;
    use rumqttc::v5::mqttbytes::v5::Publish;
    use tokio::time::timeout;

    #[tokio::test]
    async fn publication_modes_queue_receipts_and_wait_only_for_acknowledged_results() {
        use super::super::requests::{MqttRefreshRequest, RequestKind};
        use futures_util::FutureExt;
        use gafctl_api::{CommandId, DeviceRefreshStatus};
        let topics = Topics(ProxyId::default());
        let device = DeviceId::configured_ble();
        let (client, _eventloop) = AsyncClient::new(test_mqtt_options("queue-modes", 1), 4);
        let (publications, mut queued) = mpsc::channel(4);
        let connection = MqttConnection {
            client,
            publications,
            topics,
        };

        connection
            .publish_retained(topics.device(&device, "state"), b"state".to_vec())
            .await;
        let retained = queued.recv().await.unwrap();
        assert_eq!(retained.topic, topics.device(&device, "state"));
        assert_eq!(retained.payload, b"state");
        let Delivery::Retained = retained.delivery else {
            unreachable!("retained publication must use retained delivery")
        };

        let request = MqttRequest::Refresh(MqttRefreshRequest {
            request_id: CommandId::parse("rejected-id").unwrap(),
            issued_at_unix_ms: 0,
        });
        connection.reject(&device, &request, "stale_request");
        let untracked = queued.recv().await.unwrap();
        assert_eq!(untracked.topic, topics.device(&device, "refresh/result"));
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&untracked.payload).unwrap(),
            serde_json::json!({"request_id": "rejected-id", "status": "stale_request"})
        );
        let Delivery::Untracked = untracked.delivery else {
            unreachable!("rejection publication must use untracked delivery")
        };

        let reply = MqttReply::Refresh {
            request_id: CommandId::parse("accepted-id").unwrap(),
            status: DeviceRefreshStatus::Fresh,
        };
        assert_eq!(reply.kind(), RequestKind::Refresh);
        let publication = connection.publish_result(&device, &reply);
        tokio::pin!(publication);
        assert!(publication.as_mut().now_or_never().is_none());
        let acknowledged = queued.recv().await.unwrap();
        assert_eq!(acknowledged.topic, topics.device(&device, "refresh/result"));
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&acknowledged.payload).unwrap(),
            serde_json::json!({"request_id": "accepted-id", "status": "fresh"})
        );
        let Delivery::Acknowledged(receipt) = acknowledged.delivery else {
            unreachable!("result publication must own its acknowledgement ticket")
        };
        assert!(
            publication.as_mut().now_or_never().is_none(),
            "queue admission must not complete receipt delivery"
        );
        receipt.send(()).unwrap();
        publication.await;
    }

    #[test]
    fn subscriptions_preserve_retained_request_metadata() {
        let filter = control_subscription(Topics(ProxyId::default()));
        let subscription = rumqttc::v5::mqttbytes::v5::Subscribe::new(filter, None);
        let mut encoded = bytes::BytesMut::new();
        subscription.write(&mut encoded).unwrap();
        assert_eq!(encoded.last(), Some(&0x09));
    }

    #[tokio::test]
    async fn native_broker_fresh_session_resumes_after_an_eventloop_collision() {
        use rumqttc::v5::{Request, mqttbytes::v5::PubAck};
        let broker = start_native_broker().await;
        let mut options = test_mqtt_options("collision-reconnect", broker.port);
        options.set_outgoing_inflight_upper_limit(2);
        let (client, mut eventloop) = AsyncClient::new(options, 8);
        let connected = timeout(Duration::from_secs(5), eventloop.poll())
            .await
            .unwrap()
            .unwrap();
        let Event::Incoming(Packet::ConnAck(initial)) = connected else {
            unreachable!("broker must acknowledge connection");
        };
        assert!(!initial.session_present);
        // Drive the pinned EventLoop's real state into AwaitAck deterministically:
        // packet 2 is acknowledged before packet 1, so the third publish collides.
        for _ in 0..2 {
            eventloop
                .state
                .handle_outgoing_packet(Request::Publish(Publish::new(
                    "collision/test",
                    QoS::AtLeastOnce,
                    "old",
                    None,
                )))
                .unwrap();
        }
        eventloop
            .state
            .handle_incoming_packet(Packet::PubAck(PubAck::new(2, None)))
            .unwrap();
        eventloop
            .state
            .handle_outgoing_packet(Request::Publish(Publish::new(
                "collision/test",
                QoS::AtLeastOnce,
                "colliding",
                None,
            )))
            .unwrap();
        assert_eq!(
            eventloop.state.events.pop_back(),
            Some(Event::Outgoing(Outgoing::AwaitAck(1)))
        );
        eventloop.state.events.clear();
        eventloop.clean();
        assert!(eventloop.state.collision.is_some());
        client
            .publish("collision/test", QoS::AtLeastOnce, false, "resumed")
            .await
            .unwrap();
        let events = tracked_mqtt_events(eventloop, ReceiptTracker::default());
        tokio::pin!(events);
        let resumed = timeout(
            Duration::from_secs(5),
            events
                .as_mut()
                .filter_map(|event| {
                    future::ready(match event {
                        Ok(Event::Incoming(Packet::PubAck(ack))) => Some(ack),
                        _ => None,
                    })
                })
                .next(),
        )
        .await
        .expect("fresh connection must send and acknowledge the queued publication")
        .unwrap();
        assert!(
            resumed.reason == rumqttc::v5::mqttbytes::v5::PubAckReason::Success
                || resumed.reason
                    == rumqttc::v5::mqttbytes::v5::PubAckReason::NoMatchingSubscribers
        );
    }

    #[test]
    fn publish_receipts_collision_then_fresh_session_does_not_ack_a_dropped_reply() {
        use rumqttc::v5::{
            MqttState, Request,
            mqttbytes::v5::{ConnAck, ConnectReturnCode, PubAck},
        };
        let receipts = ReceiptTracker::default();
        let mut state = MqttState::new(2, false);
        for id in [1, 2] {
            receipts.lock().unwrap().queued.push_back(None);
            let packet = state
                .handle_outgoing_packet(Request::Publish(Publish::new(
                    "result",
                    QoS::AtLeastOnce,
                    "old",
                    None,
                )))
                .unwrap();
            assert!(packet.is_some());
            acknowledge_publication(&Ok(Event::Outgoing(Outgoing::Publish(id))), &receipts);
        }
        state
            .handle_incoming_packet(Packet::PubAck(PubAck::new(2, None)))
            .unwrap();
        acknowledge_publication(
            &Ok(Event::Incoming(Packet::PubAck(PubAck::new(2, None)))),
            &receipts,
        );
        let (colliding, mut collision_result) = oneshot::channel();
        receipts.lock().unwrap().queued.push_back(Some(colliding));
        assert!(
            state
                .handle_outgoing_packet(Request::Publish(Publish::new(
                    "result",
                    QoS::AtLeastOnce,
                    "collision",
                    None
                )))
                .unwrap()
                .is_none()
        );
        acknowledge_publication(&Ok(Event::Outgoing(Outgoing::AwaitAck(1))), &receipts);
        // The pinned client retains the colliding publication outside clean()'s pending list.
        let pending = state.clean();
        assert_eq!(pending.len(), 1);
        let (dropped, mut dropped_result) = oneshot::channel();
        let (new, mut new_result) = oneshot::channel();
        {
            let mut tracked = receipts.lock().unwrap();
            tracked.queued.push_back(Some(dropped));
            tracked.pending_unsent = 1;
            // Enqueued after clean(), so this publication survives the new session.
            tracked.queued.push_back(Some(new));
        }
        acknowledge_publication(
            &Ok(Event::Incoming(Packet::ConnAck(ConnAck {
                session_present: false,
                code: ConnectReturnCode::Success,
                properties: None,
            }))),
            &receipts,
        );
        acknowledge_publication(&Ok(Event::Outgoing(Outgoing::Publish(1))), &receipts);
        acknowledge_publication(
            &Ok(Event::Incoming(Packet::PubAck(PubAck::new(1, None)))),
            &receipts,
        );
        assert_eq!(
            collision_result.try_recv(),
            Err(oneshot::error::TryRecvError::Closed)
        );
        assert_eq!(
            dropped_result.try_recv(),
            Err(oneshot::error::TryRecvError::Closed)
        );
        assert_eq!(new_result.try_recv(), Ok(()));
    }

    #[test]
    fn publish_receipts_discard_old_session_ids_before_packet_id_reuse() {
        let receipts = ReceiptTracker::default();
        let (old, mut old_result) = oneshot::channel();
        receipts.lock().unwrap().sent.insert(1, Some(old));
        let connack = rumqttc::v5::mqttbytes::v5::ConnAck {
            session_present: false,
            code: rumqttc::v5::mqttbytes::v5::ConnectReturnCode::Success,
            properties: None,
        };
        acknowledge_publication(&Ok(Event::Incoming(Packet::ConnAck(connack))), &receipts);
        assert!(receipts.lock().unwrap().sent.is_empty());
        assert_eq!(
            old_result.try_recv(),
            Err(oneshot::error::TryRecvError::Closed)
        );
    }

    #[tokio::test]
    async fn native_broker_publishes_process_will_on_unexpected_disconnect() {
        let broker = start_native_broker().await;
        let topics = Topics(ProxyId::default());
        let (observer, mut received) = observed_client("will-observer", broker.port);
        observer
            .subscribe_many([Filter {
                preserve_retain: true,
                ..Filter::new(topics.process_availability(), QoS::AtLeastOnce)
            }])
            .await
            .unwrap();
        let (client, eventloop) =
            AsyncClient::new(mqtt_options(config(broker.port, false), topics), 16);
        let (connected, mut connection_events) = mpsc::unbounded_channel();
        let task = tokio::spawn(
            tracked_mqtt_events(eventloop, ReceiptTracker::default()).for_each(move |event| {
                if let Ok(Event::Incoming(Packet::ConnAck(_))) = event {
                    let _ = connected.send(());
                }
                future::ready(())
            }),
        );
        timeout(Duration::from_secs(5), connection_events.recv())
            .await
            .unwrap()
            .unwrap();
        client
            .publish(
                topics.process_availability(),
                QoS::AtLeastOnce,
                true,
                "online",
            )
            .await
            .unwrap();
        receive_topic(&mut received, &topics.process_availability()).await;
        task.abort();
        let _ = task.await;
        let offline = receive_topic(&mut received, &topics.process_availability()).await;
        assert!(offline.retain);
        assert_eq!(offline.payload.as_ref(), b"offline");
    }
}
