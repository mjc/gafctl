use std::{sync::Arc, time::Duration};

use futures_util::{StreamExt, future};
#[cfg(test)]
use rumqttc_next::Publish;
use rumqttc_next::{
    AsyncClient, ConnectionError, Event, EventLoop, LastWill, MqttOptions, MqttOptionsBuilder,
    Packet, PublishOptions, QoS, SubscribeFilter as Filter,
};
#[cfg(test)]
use tokio::sync::mpsc;
use tokio::{
    sync::{Semaphore, watch},
    time::sleep,
};
#[cfg(test)]
use tokio_stream::wrappers::UnboundedReceiverStream;
use tokio_stream::wrappers::WatchStream;

use super::{
    MqttConfig,
    requests::{MAX_PENDING_CONTROL_RESULTS, MqttRequestIntake, dispatch_request},
    topics::Topics,
};

pub(super) fn mqtt_options(config: MqttConfig, topics: Topics) -> MqttOptions {
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

pub(super) async fn setup_connection(
    client: AsyncClient,
    topics: Topics,
    connected: watch::Receiver<bool>,
) {
    WatchStream::from_changes(connected)
        .filter(|active| future::ready(*active))
        .for_each(|_| initialize_connection(&client, topics))
        .await;
}

async fn initialize_connection(client: &AsyncClient, topics: Topics) {
    if let Err(error) = client
        .subscribe_many([topics.controls(), topics.refreshes()].map(|topic| Filter {
            preserve_retain: true,
            ..Filter::new(topic, QoS::AtLeastOnce)
        }))
        .await
    {
        tracing::warn!(%error, "could not subscribe to MQTT controls");
    }
    if let Err(error) = client
        .publish(
            topics.process_availability(),
            "online",
            PublishOptions::at_least_once().retained(),
        )
        .await
    {
        tracing::warn!(%error, "could not publish MQTT availability");
    }
}

pub(super) async fn run_event_loop(
    eventloop: EventLoop,
    client: AsyncClient,
    topics: Topics,
    connected: watch::Sender<bool>,
    controls: MqttRequestIntake,
) {
    let pending_results = Arc::new(Semaphore::new(MAX_PENDING_CONTROL_RESULTS));
    eventloop
        .into_stream()
        .for_each(|event| {
            handle_mqtt_event(
                event,
                &client,
                topics,
                &connected,
                &controls,
                &pending_results,
            )
        })
        .await;
}

async fn handle_mqtt_event(
    event: Result<Event, ConnectionError>,
    client: &AsyncClient,
    topics: Topics,
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
                && let Some((device, kind)) = topics.request_device(topic)
            {
                dispatch_request(
                    client,
                    topics,
                    controls,
                    pending_results,
                    device,
                    kind,
                    message,
                );
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

#[cfg(test)]
pub(super) fn observed_client(
    client_id: &str,
    port: u16,
) -> (AsyncClient, UnboundedReceiverStream<Publish>) {
    let (client, eventloop) =
        AsyncClient::builder(super::test_support::test_mqtt_options(client_id, port))
            .capacity(16)
            .build();
    let (messages, received) = mpsc::unbounded_channel();
    tokio::spawn(
        eventloop
            .into_stream()
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
    (client, UnboundedReceiverStream::new(received))
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{config, receive_topic, start_native_broker};
    use super::*;
    use crate::model::ProxyId;
    use tokio::time::timeout;

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
        let task = tokio::spawn(eventloop.into_stream().for_each(move |event| {
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
