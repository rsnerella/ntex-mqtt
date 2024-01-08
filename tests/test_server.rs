use std::sync::{atomic::AtomicBool, atomic::Ordering::Relaxed, Arc};
use std::{cell::RefCell, future::Future, num::NonZeroU16, pin::Pin, rc::Rc, time::Duration};

use ntex::service::{fn_service, Pipeline, ServiceFactory};
use ntex::time::{sleep, Millis, Seconds};
use ntex::util::{join_all, lazy, ByteString, Bytes, BytesMut, Ready};
use ntex::{codec::Encoder, server, service::chain_factory};

use ntex_mqtt::v3::{
    client, codec, ControlMessage, Handshake, HandshakeAck, MqttServer, Publish, Session,
};
use ntex_mqtt::{error::ProtocolError, QoS};

struct St;

async fn handshake(mut packet: Handshake) -> Result<HandshakeAck<St>, ()> {
    packet.packet();
    packet.packet_mut();
    packet.io();
    packet.sink();
    Ok(packet.ack(St, false).idle_timeout(Seconds(16)))
}

#[ntex::test]
async fn test_simple() -> std::io::Result<()> {
    let srv =
        server::test_server(|| MqttServer::new(handshake).publish(|_t| Ready::Ok(())).finish());

    // connect to server
    let client =
        client::MqttConnector::new(srv.addr()).client_id("user").connect().await.unwrap();

    let sink = client.sink();

    ntex::rt::spawn(client.start_default());

    let res =
        sink.publish(ByteString::from_static("test"), Bytes::new()).send_at_least_once().await;
    assert!(res.is_ok());

    let res =
        sink.publish(ByteString::from_static("#"), Bytes::new()).send_at_least_once().await;
    assert!(res.is_err());

    sink.close();
    Ok(())
}

#[ntex::test]
async fn test_connect_fail() -> std::io::Result<()> {
    // bad user name or password
    let srv = server::test_server(|| {
        MqttServer::new(|conn: Handshake| Ready::Ok::<_, ()>(conn.bad_username_or_pwd::<St>()))
            .publish(|_t| Ready::Ok(()))
            .finish()
    });
    let err =
        client::MqttConnector::new(srv.addr()).client_id("user").connect().await.err().unwrap();
    if let client::ClientError::Ack(codec::ConnectAck { session_present, return_code }) = err {
        assert!(!session_present);
        assert_eq!(return_code, codec::ConnectAckReason::BadUserNameOrPassword);
    }

    // identifier rejected
    let srv = server::test_server(|| {
        MqttServer::new(|conn: Handshake| Ready::Ok::<_, ()>(conn.identifier_rejected::<St>()))
            .publish(|_t| Ready::Ok(()))
            .finish()
    });
    let err =
        client::MqttConnector::new(srv.addr()).client_id("user").connect().await.err().unwrap();
    if let client::ClientError::Ack(codec::ConnectAck { session_present, return_code }) = err {
        assert!(!session_present);
        assert_eq!(return_code, codec::ConnectAckReason::IdentifierRejected);
    }

    // not authorized
    let srv = server::test_server(|| {
        MqttServer::new(|conn: Handshake| Ready::Ok::<_, ()>(conn.not_authorized::<St>()))
            .publish(|_t| Ready::Ok(()))
            .finish()
    });
    let err =
        client::MqttConnector::new(srv.addr()).client_id("user").connect().await.err().unwrap();
    if let client::ClientError::Ack(codec::ConnectAck { session_present, return_code }) = err {
        assert!(!session_present);
        assert_eq!(return_code, codec::ConnectAckReason::NotAuthorized);
    }

    // service unavailable
    let srv = server::test_server(|| {
        MqttServer::new(|conn: Handshake| Ready::Ok::<_, ()>(conn.service_unavailable::<St>()))
            .publish(|_t| Ready::Ok(()))
            .finish()
    });
    let err =
        client::MqttConnector::new(srv.addr()).client_id("user").connect().await.err().unwrap();
    if let client::ClientError::Ack(codec::ConnectAck { session_present, return_code }) = err {
        assert!(!session_present);
        assert_eq!(return_code, codec::ConnectAckReason::ServiceUnavailable);
    }

    Ok(())
}

#[ntex::test]
async fn test_ping() -> std::io::Result<()> {
    let ping = Arc::new(AtomicBool::new(false));
    let ping2 = ping.clone();

    let srv = server::test_server(move || {
        let ping = ping2.clone();
        MqttServer::new(handshake)
            .publish(|_| Ready::Ok(()))
            .control(move |msg| {
                let ping = ping.clone();
                match msg {
                    ControlMessage::Ping(msg) => {
                        ping.store(true, Relaxed);
                        Ready::Ok(msg.ack())
                    }
                    _ => Ready::Ok(msg.disconnect()),
                }
            })
            .finish()
    });

    let io = srv.connect().await.unwrap();
    let codec = codec::Codec::default();
    io.send(codec::Packet::Connect(codec::Connect::default().client_id("user").into()), &codec)
        .await
        .unwrap();
    io.recv(&codec).await.unwrap().unwrap();

    io.send(codec::Packet::PingRequest, &codec).await.unwrap();
    let pkt = io.recv(&codec).await.unwrap().unwrap();
    assert_eq!(pkt.0, codec::Packet::PingResponse);
    assert!(ping.load(Relaxed));

    Ok(())
}

#[ntex::test]
async fn test_ack_order() -> std::io::Result<()> {
    let srv = server::test_server(move || {
        MqttServer::new(handshake)
            .publish(|_| async {
                sleep(Duration::from_millis(100)).await;
                Ok::<_, ()>(())
            })
            .control(move |msg| match msg {
                ControlMessage::Subscribe(mut msg) => {
                    for mut sub in &mut msg {
                        assert_eq!(sub.qos(), codec::QoS::AtLeastOnce);
                        sub.topic();
                        sub.subscribe(codec::QoS::AtLeastOnce);
                    }
                    Ready::Ok(msg.ack())
                }
                _ => Ready::Ok(msg.disconnect()),
            })
            .finish()
    });

    let io = srv.connect().await.unwrap();
    let codec = codec::Codec::default();
    io.send(codec::Connect::default().client_id("user").into(), &codec).await.unwrap();
    let _ = io.recv(&codec).await.unwrap().unwrap();

    io.send(
        codec::Publish {
            dup: false,
            retain: false,
            qos: codec::QoS::AtLeastOnce,
            topic: ByteString::from("test"),
            packet_id: Some(NonZeroU16::new(1).unwrap()),
            payload: Bytes::new(),
        }
        .into(),
        &codec,
    )
    .await
    .unwrap();
    io.send(
        codec::Packet::Subscribe {
            packet_id: NonZeroU16::new(2).unwrap(),
            topic_filters: vec![(ByteString::from("topic1"), codec::QoS::AtLeastOnce)],
        },
        &codec,
    )
    .await
    .unwrap();
    io.send(
        codec::Publish {
            dup: false,
            retain: false,
            qos: codec::QoS::AtLeastOnce,
            topic: ByteString::from("test"),
            packet_id: Some(NonZeroU16::new(3).unwrap()),
            payload: Bytes::new(),
        }
        .into(),
        &codec,
    )
    .await
    .unwrap();

    let pkt = io.recv(&codec).await.unwrap().unwrap();
    assert_eq!(pkt.0, codec::Packet::PublishAck { packet_id: NonZeroU16::new(1).unwrap() });

    let pkt = io.recv(&codec).await.unwrap().unwrap();
    assert_eq!(
        pkt.0,
        codec::Packet::SubscribeAck {
            packet_id: NonZeroU16::new(2).unwrap(),
            status: vec![codec::SubscribeReturnCode::Success(codec::QoS::AtLeastOnce)],
        }
    );

    let pkt = io.recv(&codec).await.unwrap().unwrap();
    assert_eq!(pkt.0, codec::Packet::PublishAck { packet_id: NonZeroU16::new(3).unwrap() });

    Ok(())
}

#[ntex::test]
async fn test_ack_order_sink() -> std::io::Result<()> {
    let srv = server::test_server(move || {
        MqttServer::new(handshake)
            .publish(|_| async {
                sleep(Duration::from_millis(100)).await;
                Ok::<_, ()>(())
            })
            .finish()
    });

    // connect to server
    let client =
        client::MqttConnector::new(srv.addr()).client_id("user").connect().await.unwrap();
    let sink = client.sink();

    ntex::rt::spawn(client.start_default());

    let topic = ByteString::from_static("test");
    let fut1 = sink.publish(topic.clone(), Bytes::from_static(b"pkt1")).send_at_least_once();
    let fut2 = sink.publish(topic.clone(), Bytes::from_static(b"pkt2")).send_at_least_once();
    let fut3 = sink.publish(topic.clone(), Bytes::from_static(b"pkt3")).send_at_least_once();

    let res = join_all(vec![fut1, fut2, fut3]).await;
    assert!(res[0].is_ok());
    assert!(res[1].is_ok());
    assert!(res[2].is_ok());

    Ok(())
}

#[ntex::test]
async fn test_disconnect() -> std::io::Result<()> {
    let srv = server::test_server(|| {
        MqttServer::new(handshake)
            .publish(ntex::service::fn_factory_with_config(|session: Session<St>| {
                Ready::Ok(ntex::service::fn_service(move |_: Publish| {
                    session.sink().force_close();
                    async {
                        sleep(Duration::from_millis(100)).await;
                        Ok(())
                    }
                }))
            }))
            .finish()
    });

    // connect to server
    let client =
        client::MqttConnector::new(srv.addr()).client_id("user").connect().await.unwrap();

    let sink = client.sink();

    ntex::rt::spawn(client.start_default());

    let res =
        sink.publish(ByteString::from_static("#"), Bytes::new()).send_at_least_once().await;
    assert!(res.is_err());

    Ok(())
}

#[ntex::test]
async fn test_client_disconnect() -> std::io::Result<()> {
    let disconnect = Arc::new(AtomicBool::new(false));
    let disconnect2 = disconnect.clone();

    let srv = server::test_server(move || {
        let disconnect = disconnect2.clone();

        MqttServer::new(handshake)
            .publish(ntex::service::fn_factory_with_config(|_: Session<St>| {
                Ready::Ok(ntex::service::fn_service(move |_: Publish| async { Ok(()) }))
            }))
            .control(move |msg| match msg {
                ControlMessage::Disconnect(msg) => {
                    disconnect.store(true, Relaxed);
                    Ready::Ok(msg.ack())
                }
                _ => Ready::Ok(msg.disconnect()),
            })
            .finish()
    });

    // connect to server
    let client =
        client::MqttConnector::new(srv.addr()).client_id("user").connect().await.unwrap();

    let sink = client.sink();

    ntex::rt::spawn(client.start_default());

    let res =
        sink.publish(ByteString::from_static("test"), Bytes::new()).send_at_least_once().await;
    assert!(res.is_ok());
    sink.close();
    sleep(Millis(50)).await;
    assert!(disconnect.load(Relaxed));

    Ok(())
}

#[ntex::test]
async fn test_handle_incoming() -> std::io::Result<()> {
    let publish = Arc::new(AtomicBool::new(false));
    let publish2 = publish.clone();
    let disconnect = Arc::new(AtomicBool::new(false));
    let disconnect2 = disconnect.clone();

    let srv = server::test_server(move || {
        let publish = publish2.clone();
        let disconnect = disconnect2.clone();
        MqttServer::new(handshake)
            .publish(move |_| {
                publish.store(true, Relaxed);
                async {
                    sleep(Duration::from_millis(100)).await;
                    Ok(())
                }
            })
            .control(move |msg| match msg {
                ControlMessage::Disconnect(msg) => {
                    disconnect.store(true, Relaxed);
                    Ready::Ok(msg.ack())
                }
                _ => Ready::Ok(msg.disconnect()),
            })
            .finish()
    });

    let io = srv.connect().await.unwrap();
    let codec = codec::Codec::default();
    io.encode(codec::Connect::default().client_id("user").into(), &codec).unwrap();
    io.encode(
        codec::Publish {
            dup: false,
            retain: false,
            qos: codec::QoS::AtLeastOnce,
            topic: ByteString::from("test"),
            packet_id: Some(NonZeroU16::new(3).unwrap()),
            payload: Bytes::new(),
        }
        .into(),
        &codec,
    )
    .unwrap();
    io.encode(codec::Packet::Disconnect, &codec).unwrap();
    io.flush(true).await.unwrap();
    sleep(Millis(50)).await;
    drop(io);
    sleep(Millis(50)).await;

    assert!(publish.load(Relaxed));
    assert!(disconnect.load(Relaxed));

    Ok(())
}

fn make_handle_or_drop_test(
    max_qos: QoS,
    handle_qos_after_disconnect: Option<QoS>,
) -> impl Fn(QoS) -> Pin<Box<dyn Future<Output = bool>>> {
    move |publish_qos| {
        Box::pin(handle_or_drop_publish_after_disconnect(
            publish_qos,
            max_qos,
            handle_qos_after_disconnect,
        ))
    }
}

async fn handle_or_drop_publish_after_disconnect(
    publish_qos: QoS,
    max_qos: QoS,
    handle_qos_after_disconnect: Option<QoS>,
) -> bool {
    let publish = Arc::new(AtomicBool::new(false));
    let publish2 = publish.clone();
    let disconnect = Arc::new(AtomicBool::new(false));
    let disconnect2 = disconnect.clone();

    let srv = server::test_server(move || {
        let publish = publish2.clone();
        let disconnect = disconnect2.clone();
        MqttServer::new(handshake)
            .max_qos(max_qos)
            .handle_qos_after_disconnect(handle_qos_after_disconnect)
            .publish(move |_| {
                publish.store(true, Relaxed);
                async {
                    sleep(Duration::from_millis(100)).await;
                    Ok(())
                }
            })
            .control(move |msg| match msg {
                ControlMessage::Disconnect(msg) => {
                    disconnect.store(true, Relaxed);
                    Ready::Ok(msg.ack())
                }
                _ => Ready::Ok(msg.disconnect()),
            })
            .finish()
    });

    let packet_id = match publish_qos {
        QoS::AtMostOnce => None,
        _ => Some(NonZeroU16::new(1).unwrap()),
    };
    let io = srv.connect().await.unwrap();
    let codec = codec::Codec::default();
    io.encode(codec::Connect::default().client_id("user").into(), &codec).unwrap();
    io.encode(
        codec::Publish {
            dup: false,
            retain: false,
            qos: publish_qos,
            topic: ByteString::from("test"),
            packet_id,
            payload: Bytes::new(),
        }
        .into(),
        &codec,
    )
    .unwrap();
    io.encode(codec::Packet::Disconnect, &codec).unwrap();
    io.flush(true).await.unwrap();
    drop(io);

    sleep(Millis(50)).await;

    assert!(disconnect.load(Relaxed));

    publish.load(Relaxed)
}

#[ntex::test]
async fn test_handle_incoming_after_disconnect() -> std::io::Result<()> {
    let handle_publish = make_handle_or_drop_test(QoS::AtMostOnce, Some(QoS::AtMostOnce));
    assert!(handle_publish(QoS::AtMostOnce).await);

    let handle_publish = make_handle_or_drop_test(QoS::AtLeastOnce, Some(QoS::AtMostOnce));
    assert!(handle_publish(QoS::AtMostOnce).await);

    let handle_publish = make_handle_or_drop_test(QoS::AtLeastOnce, Some(QoS::AtLeastOnce));
    assert!(handle_publish(QoS::AtMostOnce).await);
    assert!(handle_publish(QoS::AtLeastOnce).await);

    let handle_publish = make_handle_or_drop_test(QoS::ExactlyOnce, Some(QoS::ExactlyOnce));
    assert!(handle_publish(QoS::AtMostOnce).await);
    assert!(handle_publish(QoS::AtLeastOnce).await);
    assert!(handle_publish(QoS::ExactlyOnce).await);

    Ok(())
}

#[ntex::test]
async fn test_nested_errors() -> std::io::Result<()> {
    let srv = server::test_server(move || {
        MqttServer::new(handshake)
            .publish(|_| Ready::Ok(()))
            .control(move |msg| match msg {
                ControlMessage::Disconnect(_) => Ready::Err(()),
                ControlMessage::Error(_) => Ready::Err(()),
                _ => Ready::Ok(msg.disconnect()),
            })
            .finish()
    });

    let io = srv.connect().await.unwrap();
    let codec = codec::Codec::default();
    io.send(codec::Connect::default().client_id("user").into(), &codec).await.unwrap();
    let _ = io.recv(&codec).await.unwrap().unwrap();

    // disconnect
    io.send(codec::Packet::Disconnect, &codec).await.unwrap();
    assert!(io.recv(&codec).await.unwrap().is_none());

    Ok(())
}

#[ntex::test]
async fn test_large_publish() -> std::io::Result<()> {
    let srv = server::test_server(move || {
        MqttServer::new(handshake).publish(|_| Ready::Ok(())).finish()
    });

    let io = srv.connect().await.unwrap();
    let codec = codec::Codec::default();
    io.encode(codec::Connect::default().client_id("user").into(), &codec).unwrap();
    let _ = io.recv(&codec).await;

    let p = codec::Publish {
        dup: false,
        retain: false,
        qos: codec::QoS::AtLeastOnce,
        topic: ByteString::from("test"),
        packet_id: Some(NonZeroU16::new(3).unwrap()),
        payload: Bytes::from(vec![b'*'; 270 * 1024]),
    }
    .into();
    let res = io.send(p, &codec).await;
    assert!(res.is_ok());
    let result = io.recv(&codec).await;
    assert!(result.is_ok());

    Ok(())
}

fn ssl_acceptor() -> openssl::ssl::SslAcceptor {
    use openssl::ssl::{SslAcceptor, SslFiletype, SslMethod};

    // load ssl keys
    let mut builder = SslAcceptor::mozilla_intermediate(SslMethod::tls()).unwrap();
    builder.set_private_key_file("./tests/key.pem", SslFiletype::PEM).unwrap();
    builder.set_certificate_chain_file("./tests/cert.pem").unwrap();
    builder.build()
}

#[ntex::test]
async fn test_large_publish_openssl() -> std::io::Result<()> {
    use openssl::ssl::{SslConnector, SslMethod, SslVerifyMode};

    let srv = server::test_server(move || {
        chain_factory(server::openssl::SslAcceptor::new(ssl_acceptor()).map_err(|_| ()))
            .and_then(
                MqttServer::new(handshake)
                    .publish(|_| Ready::Ok(()))
                    .finish()
                    .map_err(|_| ())
                    .map_init_err(|_| ()),
            )
    });

    let mut builder = SslConnector::builder(SslMethod::tls()).unwrap();
    builder.set_verify(SslVerifyMode::NONE);
    let con = Pipeline::new(ntex::connect::openssl::Connector::new(builder.build()));
    let addr = format!("127.0.0.1:{}", srv.addr().port());
    let io = con.call(addr.into()).await.unwrap();

    let codec = codec::Codec::default();
    io.encode(codec::Connect::default().client_id("user").into(), &codec).unwrap();
    let _ = io.recv(&codec).await;

    let p = codec::Publish {
        dup: false,
        retain: false,
        qos: codec::QoS::AtLeastOnce,
        topic: ByteString::from("test"),
        packet_id: Some(NonZeroU16::new(3).unwrap()),
        payload: Bytes::from(vec![b'*'; 270 * 1024]),
    }
    .into();
    let res = io.send(p, &codec).await;
    assert!(res.is_ok());
    let result = io.recv(&codec).await;
    assert!(result.is_ok());

    Ok(())
}

#[ntex::test]
async fn test_max_qos() -> std::io::Result<()> {
    let violated = Arc::new(AtomicBool::new(false));
    let violated2 = violated.clone();

    let srv = server::test_server(move || {
        let violated = violated2.clone();
        MqttServer::new(handshake)
            .max_qos(QoS::AtMostOnce)
            .publish(|_| Ready::Ok(()))
            .control(move |msg| {
                let violated = violated.clone();
                match msg {
                    ControlMessage::ProtocolError(err) => {
                        if let ProtocolError::ProtocolViolation(_) = err.get_ref() {
                            violated.store(true, Relaxed);
                        }
                        Ready::Ok(err.ack())
                    }
                    _ => Ready::Ok(msg.disconnect()),
                }
            })
            .finish()
    });

    let io = srv.connect().await.unwrap();
    let codec = codec::Codec::default();
    io.send(codec::Packet::Connect(codec::Connect::default().client_id("user").into()), &codec)
        .await
        .unwrap();
    io.recv(&codec).await.unwrap().unwrap();

    let p = codec::Publish {
        dup: false,
        retain: false,
        qos: codec::QoS::AtLeastOnce,
        topic: ByteString::from("test"),
        packet_id: Some(NonZeroU16::new(3).unwrap()),
        payload: Bytes::from(vec![b'*'; 270 * 1024]),
    }
    .into();

    io.send(p, &codec).await.unwrap();
    assert!(io.recv(&codec).await.unwrap().is_none());
    assert!(violated.load(Relaxed));

    Ok(())
}

#[ntex::test]
async fn test_sink_ready() -> std::io::Result<()> {
    let srv = server::test_server(|| {
        MqttServer::new(fn_service(|packet: Handshake| async move {
            let sink = packet.sink();
            let mut ready = Box::pin(sink.ready());
            let res = lazy(|cx| Pin::new(&mut ready).poll(cx)).await;
            assert!(res.is_pending());
            assert!(!sink.is_ready());

            ntex::rt::spawn(async move {
                sink.ready().await;
                assert!(sink.is_ready());
                sink.publish("/test", Bytes::from_static(b"body")).send_at_most_once().unwrap();
            });

            Ok::<_, ()>(packet.ack(St, false).idle_timeout(Seconds(16)))
        }))
        .publish(|_| Ready::Ok(()))
        .finish()
    });

    // connect to server
    let io = srv.connect().await.unwrap();
    let codec = codec::Codec::default();
    io.send(codec::Packet::Connect(codec::Connect::default().client_id("user").into()), &codec)
        .await
        .unwrap();
    io.recv(&codec).await.unwrap().unwrap();

    let result = io.recv(&codec).await;
    assert!(result.is_ok());

    Ok(())
}

#[ntex::test]
async fn test_sink_publish_noblock() -> std::io::Result<()> {
    let srv = server::test_server(move || {
        MqttServer::new(handshake).publish(|_| Ready::Ok(())).finish()
    });

    // connect to server
    let client =
        client::MqttConnector::new(srv.addr()).client_id("user").connect().await.unwrap();

    let sink = client.sink();

    ntex::rt::spawn(client.start_default());

    let results = Rc::new(RefCell::new(Vec::new()));
    let results2 = results.clone();

    sink.publish_ack_cb(move |idx, disconnected| {
        assert!(!disconnected);
        results2.borrow_mut().push(idx);
    });

    let res = sink
        .publish(ByteString::from_static("test1"), Bytes::new())
        .send_at_least_once_no_block();
    assert!(res.is_ok());

    let res = sink
        .publish(ByteString::from_static("test2"), Bytes::new())
        .send_at_least_once_no_block();
    assert!(res.is_ok());

    let res =
        sink.publish(ByteString::from_static("test3"), Bytes::new()).send_at_least_once().await;
    assert!(res.is_ok());

    assert_eq!(*results.borrow(), &[NonZeroU16::new(1).unwrap(), NonZeroU16::new(2).unwrap()]);

    sink.close();
    Ok(())
}

// Slow frame rate
#[ntex::test]
async fn test_frame_read_rate() -> std::io::Result<()> {
    let check = Arc::new(AtomicBool::new(false));
    let check2 = check.clone();

    let srv = server::test_server(move || {
        let check = check2.clone();

        MqttServer::new(handshake)
            .frame_read_rate(Seconds(1), Seconds(2), 10)
            .publish(|_| Ready::Ok(()))
            .control(move |msg| {
                let check = check.clone();
                match msg {
                    ControlMessage::ProtocolError(msg) => {
                        if msg.get_ref() == &ProtocolError::ReadTimeout {
                            check.store(true, Relaxed);
                        }
                        Ready::Ok(msg.ack())
                    }
                    _ => Ready::Ok(msg.disconnect()),
                }
            })
            .finish()
            .map_err(|_| ())
            .map_init_err(|_| ())
    });

    let io = srv.connect().await.unwrap();
    let codec = codec::Codec::default();
    io.encode(codec::Connect::default().client_id("user").into(), &codec).unwrap();
    io.recv(&codec).await.unwrap();

    let p = codec::Publish {
        dup: false,
        retain: false,
        qos: codec::QoS::AtLeastOnce,
        topic: ByteString::from("test"),
        packet_id: Some(NonZeroU16::new(3).unwrap()),
        payload: Bytes::from(vec![b'*'; 270 * 1024]),
    }
    .into();

    let mut buf = BytesMut::new();
    codec.encode(p, &mut buf).unwrap();

    io.write(&buf[..5]).unwrap();
    buf.split_to(5);
    sleep(Millis(100)).await;
    io.write(&buf[..10]).unwrap();
    buf.split_to(10);
    sleep(Millis(1000)).await;
    assert!(!check.load(Relaxed));

    io.write(&buf[..12]).unwrap();
    buf.split_to(12);
    sleep(Millis(1000)).await;
    assert!(!check.load(Relaxed));

    sleep(Millis(2100)).await;
    assert!(check.load(Relaxed));

    Ok(())
}
