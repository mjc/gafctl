use std::{sync::Arc, time::Duration};

use futures_util::{Stream, StreamExt, future, stream};
#[cfg(test)]
use rumqttc::mqttbytes::v5::Publish;
use rumqttc::{
    AsyncClient, ConnectionError, Event, EventLoop, MqttOptions, MqttOptionsBuilder, PublishNotice,
    PublishNoticeError, PublishOptions,
    mqttbytes::{
        QoS,
        v5::{LastWill, Packet, SubscribeFilter as Filter},
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
    Acknowledged(oneshot::Sender<PublishNotice>),
}

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

    fn new(client: AsyncClient, topics: Topics) -> (Self, tokio::task::JoinHandle<()>) {
        let (publications, receiver) = mpsc::channel(32);
        let publisher = tokio::spawn(publish_requests(client.clone(), receiver));
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
                {
                    tracing::warn!("could not enqueue MQTT control result");
                    return;
                }
                if let Err(error) = publication_completed(acknowledged).await {
                    tracing::warn!(%error, "could not acknowledge MQTT control result");
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

async fn publication_completed(
    receipt: oneshot::Receiver<PublishNotice>,
) -> Result<(), PublishNoticeError> {
    receipt.await?.wait_completion_async().await
}

async fn publish_requests(client: AsyncClient, receiver: mpsc::Receiver<PublishRequest>) {
    let client = &client;
    stream::unfold(receiver, |mut receiver| async {
        receiver.recv().await.map(|request| (request, receiver))
    })
    .for_each(|request| async move {
        let (retain, receipt) = match request.delivery {
            Delivery::Retained => (true, None),
            Delivery::Untracked => (false, None),
            Delivery::Acknowledged(receipt) => (false, Some(receipt)),
        };
        let options = PublishOptions::at_least_once().retain(retain);
        let publication = match receipt {
            Some(receipt) => client
                .publish_tracked(request.topic, request.payload, options)
                .await
                .map(|notice| {
                    let _ = receipt.send(notice);
                }),
            None => {
                client
                    .publish(request.topic, request.payload, options)
                    .await
            }
        };
        if let Err(error) = publication {
            tracing::warn!(%error, "could not enqueue MQTT publication");
        }
    })
    .await;
}

fn mqtt_options(config: MqttConfig, topics: Topics) -> MqttOptions {
    MqttOptionsBuilder::new(topics.client_id(), (config.host, config.port))
        .keep_alive(30)
        .credentials(config.username, config.password)
        .last_will(LastWill::new(
            topics.process_availability(),
            "offline",
            QoS::AtLeastOnce,
            true,
            None,
        ))
        .build()
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

fn mqtt_events(eventloop: EventLoop) -> impl Stream<Item = Result<Event, ConnectionError>> {
    stream::unfold(eventloop, |mut eventloop| async {
        let event = eventloop.poll().await;
        Some((event, eventloop))
    })
}

async fn run_event_loop(
    eventloop: EventLoop,
    connection: MqttConnection,
    connected: watch::Sender<bool>,
    controls: MqttRequestIntake,
) {
    let pending_results = Arc::new(Semaphore::new(MAX_PENDING_CONTROL_RESULTS));
    mqtt_events(eventloop)
        .for_each(|event| {
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
    let (client, eventloop) = AsyncClient::builder(mqtt_options(config, topics))
        .capacity(32)
        .build();
    let (connection, publisher) = MqttConnection::new(client, topics);
    let setup = tokio::spawn(setup_connection(connection.clone(), connected_rx));
    let event_loop = tokio::spawn(run_event_loop(
        eventloop,
        connection.clone(),
        connected_tx,
        controls,
    ));
    (connection, [setup, event_loop, publisher])
}

#[cfg(test)]
pub(super) fn test_connection(
    client: AsyncClient,
    topics: Topics,
) -> (MqttConnection, tokio::task::JoinHandle<()>) {
    MqttConnection::new(client, topics)
}

#[cfg(test)]
pub(super) fn observed_client(
    client_id: &str,
    port: u16,
) -> (AsyncClient, mpsc::UnboundedReceiver<Publish>) {
    let (client, eventloop) =
        AsyncClient::builder(super::test_support::test_mqtt_options(client_id, port))
            .capacity(16)
            .build();
    let (messages, received) = mpsc::unbounded_channel();
    tokio::spawn(
        mqtt_events(eventloop)
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
    use tokio::time::timeout;

    #[tokio::test]
    async fn publication_modes_queue_receipts_and_wait_only_for_acknowledged_results() {
        use super::super::requests::{MqttRefreshRequest, RequestKind};
        use futures_util::FutureExt;
        use gafctl_api::{CommandId, DeviceRefreshStatus};
        let topics = Topics(ProxyId::default());
        let device = DeviceId::configured_ble();
        let (client, eventloop) = AsyncClient::builder(test_mqtt_options("queue-modes", 1))
            .capacity(4)
            .build();
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
        let notice = connection
            .client
            .publish_tracked(
                "synthetic/result",
                b"result",
                PublishOptions::at_least_once(),
            )
            .await
            .unwrap();
        receipt.send(notice).unwrap();
        assert!(
            publication.as_mut().now_or_never().is_none(),
            "notice admission must not complete broker acknowledgement"
        );
        drop(eventloop);
    }

    #[test]
    fn subscriptions_preserve_retained_request_metadata() {
        let filter = control_subscription(Topics(ProxyId::default()));
        let mut subscription = rumqttc::mqttbytes::v5::Subscribe::new(filter, None);
        subscription.pkid = 1;
        let mut encoded = bytes::BytesMut::new();
        subscription.write(&mut encoded).unwrap();
        assert_eq!(encoded.last(), Some(&0x09));
    }

    #[tokio::test]
    async fn publisher_admits_later_results_without_waiting_for_earlier_acknowledgements() {
        use futures_util::FutureExt;
        use tokio_util::task::AbortOnDropHandle;

        let (client, eventloop) = AsyncClient::builder(test_mqtt_options("concurrent-results", 1))
            .capacity(4)
            .build();
        let (connection, publisher) = MqttConnection::new(client, Topics(ProxyId::default()));
        let _publisher = AbortOnDropHandle::new(publisher);
        let (first, first_notice) = oneshot::channel();
        let (second, second_notice) = oneshot::channel();
        for (payload, receipt) in [(b"first".to_vec(), first), (b"second".to_vec(), second)] {
            connection
                .publications
                .send(PublishRequest {
                    topic: "synthetic/result".to_owned(),
                    payload,
                    delivery: Delivery::Acknowledged(receipt),
                })
                .await
                .unwrap();
        }
        let first = timeout(Duration::from_secs(1), first_notice)
            .await
            .unwrap()
            .unwrap();
        let second = timeout(Duration::from_secs(1), second_notice)
            .await
            .unwrap()
            .unwrap();
        let completions = async {
            tokio::join!(
                first.wait_completion_async(),
                second.wait_completion_async()
            )
        };
        tokio::pin!(completions);
        assert!(completions.as_mut().now_or_never().is_none());
        drop(eventloop);
    }

    #[tokio::test]
    async fn native_broker_fresh_session_fails_old_notices_and_completes_new_publications() {
        use rumqttc::{Outgoing, PublishNoticeError};
        use tokio_util::task::AbortOnDropHandle;

        let broker = start_native_broker().await;
        let (client, mut eventloop) =
            AsyncClient::builder(test_mqtt_options("fresh-notices", broker.port))
                .capacity(8)
                .build();
        let Event::Incoming(Packet::ConnAck(_)) = timeout(Duration::from_secs(5), eventloop.poll())
            .await
            .unwrap()
            .unwrap()
        else {
            unreachable!("native broker must acknowledge the initial connection");
        };
        let sent = client
            .publish_tracked(
                "synthetic/result",
                "sent-before-reset",
                PublishOptions::at_least_once(),
            )
            .await
            .unwrap();
        {
            let events = stream::unfold(&mut eventloop, |eventloop| async {
                Some((eventloop.poll().await, eventloop))
            })
            .filter_map(|event| {
                future::ready(match event {
                    Ok(Event::Outgoing(Outgoing::Publish(id))) => Some(id),
                    _ => None,
                })
            });
            tokio::pin!(events);
            timeout(Duration::from_secs(5), events.next())
                .await
                .unwrap()
                .unwrap();
        }
        let unsent = client
            .publish_tracked(
                "synthetic/result",
                "queued-before-reset",
                PublishOptions::at_least_once(),
            )
            .await
            .unwrap();
        eventloop.clean();
        let fresh = client
            .publish_tracked(
                "synthetic/result",
                "queued-after-reset",
                PublishOptions::at_least_once(),
            )
            .await
            .unwrap();
        let _events = AbortOnDropHandle::new(tokio::spawn(
            mqtt_events(eventloop).for_each(|_| future::ready(())),
        ));
        assert_eq!(
            timeout(Duration::from_secs(5), sent.wait_completion_async())
                .await
                .unwrap(),
            Err(PublishNoticeError::SessionReset)
        );
        assert_eq!(
            timeout(Duration::from_secs(5), unsent.wait_completion_async())
                .await
                .unwrap(),
            Err(PublishNoticeError::SessionReset)
        );
        timeout(Duration::from_secs(5), fresh.wait_completion_async())
            .await
            .unwrap()
            .unwrap();
        let next = client
            .publish_tracked(
                "synthetic/result",
                "after-reset-ack",
                PublishOptions::at_least_once(),
            )
            .await
            .unwrap();
        timeout(Duration::from_secs(5), next.wait_completion_async())
            .await
            .unwrap()
            .unwrap();
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
            AsyncClient::builder(mqtt_options(config(broker.port, false), topics))
                .capacity(16)
                .build();
        let (connected, mut connection_events) = mpsc::unbounded_channel();
        let task = tokio::spawn(mqtt_events(eventloop).for_each(move |event| {
            if let Ok(Event::Incoming(Packet::ConnAck(_))) = event {
                let _ = connected.send(());
            }
            future::ready(())
        }));
        timeout(Duration::from_secs(5), connection_events.recv())
            .await
            .unwrap()
            .unwrap();
        client
            .publish(
                topics.process_availability(),
                "online",
                PublishOptions::at_least_once().retained(),
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
