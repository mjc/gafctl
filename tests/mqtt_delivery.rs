use std::{
    io,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicU16, Ordering},
    },
    task::{Context, Poll},
    time::Duration,
};

use bytes::BytesMut;
use futures_util::{SinkExt, StreamExt, future};
use rumqttc_next::{
    AsyncClient, Broker, ConnAck, ConnectReturnCode, EventLoop, MqttOptions, Packet, PubAck,
    PubAckReason, Publish, PublishNotice, PublishNoticeError, PublishOptions, SessionMode,
    mqttbytes::v5::Codec,
};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    net::{TcpListener, TcpStream},
    sync::oneshot,
    time::{sleep, timeout},
};
use tokio_util::{codec::Framed, task::AbortOnDropHandle};

const DEADLINE: Duration = Duration::from_secs(5);

async fn packet(socket: &mut Framed<TcpStream, Codec>) -> Packet {
    timeout(DEADLINE, socket.next())
        .await
        .expect("broker packet deadline")
        .expect("broker packet stream")
        .expect("valid broker packet")
}

async fn send(socket: &mut Framed<TcpStream, Codec>, packet: Packet) {
    timeout(DEADLINE, socket.send(packet))
        .await
        .expect("broker write deadline")
        .expect("broker packet write");
}

async fn accept(listener: &TcpListener, session_present: bool) -> Framed<TcpStream, Codec> {
    let (socket, _) = timeout(DEADLINE, listener.accept())
        .await
        .expect("client connection deadline")
        .expect("client connection");
    let mut socket = Framed::new(
        socket,
        Codec {
            max_incoming_size: Some(1024),
            max_outgoing_size: Some(1024),
        },
    );
    let Packet::Connect(_, _, _) = packet(&mut socket).await else {
        unreachable!("client must start with CONNECT")
    };
    send(
        &mut socket,
        Packet::ConnAck(ConnAck {
            session_present,
            code: ConnectReturnCode::Success,
            properties: None,
        }),
    )
    .await;
    socket
}

async fn publication(socket: &mut Framed<TcpStream, Codec>) -> Publish {
    let Packet::Publish(publish) = packet(socket).await else {
        unreachable!("expected PUBLISH")
    };
    assert_ne!(publish.pkid, 0);
    publish
}

async fn acknowledge(
    socket: &mut Framed<TcpStream, Codec>,
    publish: &Publish,
    reason: PubAckReason,
) {
    send(
        socket,
        Packet::PubAck(PubAck {
            pkid: publish.pkid,
            reason,
            properties: None,
        }),
    )
    .await;
}

fn poll_client(eventloop: EventLoop) -> AbortOnDropHandle<()> {
    AbortOnDropHandle::new(tokio::spawn(
        eventloop
            .into_stream()
            .then(|event| async move {
                if event.is_err() {
                    sleep(Duration::from_millis(10)).await;
                }
            })
            .for_each(|()| future::ready(())),
    ))
}

fn options(listener: &TcpListener) -> MqttOptions {
    let address = listener.local_addr().unwrap();
    let mut options = MqttOptions::new(
        "delivery-contract",
        Broker::tcp("127.0.0.1", address.port()),
    );
    options
        .set_keep_alive(0)
        .set_session_mode(SessionMode::Persistent)
        .set_session_expiry_interval(Some(60));
    options
}

fn client(listener: &TcpListener) -> (AsyncClient, AbortOnDropHandle<()>) {
    let (client, eventloop) = AsyncClient::builder(options(listener)).capacity(2).build();
    (client, poll_client(eventloop))
}

async fn publish(client: &AsyncClient, payload: &'static str) -> PublishNotice {
    timeout(
        DEADLINE,
        client.publish_tracked(
            "gafctl/test/control/result",
            payload,
            PublishOptions::at_least_once(),
        ),
    )
    .await
    .expect("publish admission deadline")
    .expect("publish admission")
}

async fn completion(notice: PublishNotice) -> Result<(), PublishNoticeError> {
    timeout(DEADLINE, notice.wait_completion_async())
        .await
        .expect("publish completion deadline")
}

async fn finish(task: AbortOnDropHandle<()>) {
    timeout(DEADLINE, task)
        .await
        .expect("broker task deadline")
        .expect("broker task succeeded");
}

#[tokio::test]
async fn negative_puback_fails_delivery_and_keeps_the_rejection_reason() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (client, _poller) = client(&listener);
    let broker = AbortOnDropHandle::new(tokio::spawn(async move {
        let mut socket = accept(&listener, false).await;
        let publish = publication(&mut socket).await;
        assert_eq!(publish.payload, "rejected-request-id");
        acknowledge(&mut socket, &publish, PubAckReason::NotAuthorized).await;
    }));
    assert_eq!(
        completion(publish(&client, "rejected-request-id").await).await,
        Err(PublishNoticeError::V5PubAck(PubAckReason::NotAuthorized))
    );
    finish(broker).await;
}

#[tokio::test]
async fn no_matching_subscribers_still_completes_delivery() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (client, _poller) = client(&listener);
    let broker = AbortOnDropHandle::new(tokio::spawn(async move {
        let mut socket = accept(&listener, false).await;
        let publish = publication(&mut socket).await;
        acknowledge(&mut socket, &publish, PubAckReason::NoMatchingSubscribers).await;
    }));
    completion(publish(&client, "no-subscriber").await)
        .await
        .unwrap();
    finish(broker).await;
}

#[tokio::test]
async fn later_publication_completes_while_the_first_acknowledgement_is_delayed() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (client, _poller) = client(&listener);
    let (release_first, first_released) = oneshot::channel();
    let broker = AbortOnDropHandle::new(tokio::spawn(async move {
        let mut socket = accept(&listener, false).await;
        let first = publication(&mut socket).await;
        let second = publication(&mut socket).await;
        assert_eq!(first.payload, "first-request-id");
        assert_eq!(second.payload, "second-request-id");
        assert_ne!(first.pkid, second.pkid);
        acknowledge(&mut socket, &second, PubAckReason::Success).await;
        timeout(DEADLINE, first_released).await.unwrap().unwrap();
        acknowledge(&mut socket, &first, PubAckReason::Success).await;
    }));
    let first = publish(&client, "first-request-id").await;
    let second = publish(&client, "second-request-id").await;
    completion(second).await.unwrap();
    let first = first.wait_completion_async();
    tokio::pin!(first);
    assert!(
        timeout(Duration::from_millis(30), &mut first)
            .await
            .is_err()
    );
    release_first.send(()).unwrap();
    timeout(DEADLINE, first).await.unwrap().unwrap();
    finish(broker).await;
}

#[tokio::test]
async fn fresh_session_fails_the_old_receipt_without_acknowledging_a_new_publication() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (client, _poller) = client(&listener);
    let broker = AbortOnDropHandle::new(tokio::spawn(async move {
        let mut socket = accept(&listener, false).await;
        let old = publication(&mut socket).await;
        assert_eq!(old.payload, "old-request-id");
        drop(socket);
        let mut socket = accept(&listener, false).await;
        let new = publication(&mut socket).await;
        assert_eq!(new.payload, "new-request-id");
        assert_eq!(new.pkid, old.pkid, "fresh session reuses the old packet id");
        acknowledge(&mut socket, &new, PubAckReason::Success).await;
    }));
    assert_eq!(
        completion(publish(&client, "old-request-id").await).await,
        Err(PublishNoticeError::SessionReset)
    );
    completion(publish(&client, "new-request-id").await)
        .await
        .unwrap();
    finish(broker).await;
}

#[tokio::test]
async fn resumed_session_keeps_the_receipt_for_the_replayed_publication() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (client, _poller) = client(&listener);
    let broker = AbortOnDropHandle::new(tokio::spawn(async move {
        let mut socket = accept(&listener, false).await;
        let old = publication(&mut socket).await;
        drop(socket);
        let mut socket = accept(&listener, true).await;
        let replayed = publication(&mut socket).await;
        assert_eq!(replayed.pkid, old.pkid);
        assert_eq!(replayed.payload, old.payload);
        assert!(replayed.dup);
        acknowledge(&mut socket, &replayed, PubAckReason::Success).await;
    }));
    completion(publish(&client, "resumed-request-id").await)
        .await
        .unwrap();
    finish(broker).await;
}

#[tokio::test]
async fn aborting_the_event_loop_fails_its_unacknowledged_delivery_notice() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (client, poller) = client(&listener);
    let (received, publication_received) = oneshot::channel();
    let (closed, connection_closed) = oneshot::channel();
    let broker = AbortOnDropHandle::new(tokio::spawn(async move {
        let mut socket = accept(&listener, false).await;
        publication(&mut socket).await;
        received.send(()).unwrap();
        assert!(timeout(DEADLINE, socket.next()).await.unwrap().is_none());
        closed.send(()).unwrap();
    }));
    let notice = publish(&client, "unfinished-request-id").await;
    timeout(DEADLINE, publication_received)
        .await
        .unwrap()
        .unwrap();
    poller.abort();
    assert!(
        timeout(DEADLINE, poller)
            .await
            .unwrap()
            .unwrap_err()
            .is_cancelled()
    );
    assert_eq!(completion(notice).await, Err(PublishNoticeError::Recv));
    timeout(DEADLINE, connection_closed).await.unwrap().unwrap();
    finish(broker).await;
}

struct FailFirstPublish {
    socket: TcpStream,
    failed_pkid: Arc<AtomicU16>,
}

impl AsyncRead for FailFirstPublish {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.socket).poll_read(context, buffer)
    }
}

impl AsyncWrite for FailFirstPublish {
    fn poll_write(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<io::Result<usize>> {
        if buffer.first().is_some_and(|header| header >> 4 == 3)
            && self.failed_pkid.load(Ordering::SeqCst) == 0
        {
            let Packet::Publish(publish) =
                Packet::read(&mut BytesMut::from(buffer), Some(1024)).unwrap()
            else {
                unreachable!("injected write must contain PUBLISH")
            };
            assert_ne!(
                publish.pkid, 0,
                "packet id must be assigned before network write"
            );
            self.failed_pkid.store(publish.pkid, Ordering::SeqCst);
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "injected PUBLISH write failure",
            )));
        }
        Pin::new(&mut self.socket).poll_write(context, buffer)
    }

    fn poll_flush(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.socket).poll_flush(context)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.socket).poll_shutdown(context)
    }
}

#[tokio::test]
async fn failed_first_publish_write_preserves_the_receipt_for_session_resume() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let failed_pkid = Arc::new(AtomicU16::new(0));
    let mut options = options(&listener);
    options.set_socket_connector({
        let failed_pkid = Arc::clone(&failed_pkid);
        move |host, network_options| {
            let failed_pkid = Arc::clone(&failed_pkid);
            async move {
                let socket = rumqttc_next::default_socket_connect(host, network_options).await?;
                Ok(FailFirstPublish {
                    socket,
                    failed_pkid,
                })
            }
        }
    });
    let (client, eventloop) = AsyncClient::builder(options).capacity(2).build();
    let _poller = poll_client(eventloop);
    let observed_pkid = Arc::clone(&failed_pkid);
    let broker = AbortOnDropHandle::new(tokio::spawn(async move {
        let mut socket = accept(&listener, false).await;
        assert!(
            timeout(DEADLINE, socket.next()).await.unwrap().is_none(),
            "failed first PUBLISH must not reach the broker"
        );
        drop(socket);
        let mut socket = accept(&listener, true).await;
        let replayed = publication(&mut socket).await;
        assert_eq!(replayed.payload, "unwritten-request-id");
        assert_eq!(
            replayed.pkid,
            observed_pkid.load(Ordering::SeqCst),
            "replay preserves the id assigned before the failed write"
        );
        assert!(replayed.dup);
        acknowledge(&mut socket, &replayed, PubAckReason::Success).await;
    }));
    completion(publish(&client, "unwritten-request-id").await)
        .await
        .unwrap();
    assert_ne!(failed_pkid.load(Ordering::SeqCst), 0);
    finish(broker).await;
}
