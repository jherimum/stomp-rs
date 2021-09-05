use crate::client::actor::frame_emitter::frame_emitter_actor;
use crate::client::actor::receipt_awaiter::receipt_actor;
use crate::client::actor::subscribers::{SubscriberActor, SubscriberMessage};
use crate::client::interceptor::{Forwarder, InterceptorMessage};
use crate::client::{ClientBuilder, ClientError, ServerStompSender, SubscriberId};
use crate::connection::{Connection, ConnectionError};
use crate::protocol::frame::{Ack, Connect, Nack, Send, Subscribe, Unsubscribe};
use crate::protocol::{ClientCommand, Frame, ServerCommand, StompMessage};
use log::debug;
use std::collections::HashMap;
use std::error::Error;
use std::marker::Send as MarkerSend;
use std::sync::Arc;
use tokio::net::TcpStream;
use tokio::sync::mpsc::{channel, Receiver, Sender};
use tokio::sync::oneshot::error::RecvError;
use tokio::sync::Mutex;
use tokio::time::{Duration, Instant};
use uuid::Uuid;

pub(crate) struct InternalClient {
    connection: Arc<Connection>,
    subscriber: SubscriberActor,
    interceptors: Arc<Vec<Sender<(Forwarder, InterceptorMessage)>>>,
}

impl InternalClient {
    pub(crate) async fn connect(builder: ClientBuilder) -> Result<Self, Box<dyn Error>> {
        let (sender, receiver) = channel(5);

        let connection = Arc::new(
            Connection::new(TcpStream::connect(builder.host.clone()).await?, sender).await,
        );

        let subscriber = SubscriberActor::new().await;
        let interceptors = Arc::new(vec![
            frame_emitter_actor(Arc::clone(&connection)).await,
            receipt_actor().await,
            subscriber.interceptor_sender()
        ]);

        let client = Self {
            connection,
            subscriber,
            interceptors,
        };

        let server_timeout: u128 = builder.heartbeat.unwrap_or((0, 0)).1.into();

        let (connected_sender, connected_receiver) = tokio::sync::oneshot::channel();

        client
            .spawn_server_frame_listener(connected_sender, receiver, server_timeout)
            .await;

        let mut connect_frame = Connect::new("1.2".to_owned(), builder.host);

        if let Some(heartbeat) = builder.heartbeat {
            connect_frame = connect_frame.heartbeat(heartbeat.0, heartbeat.1);
        }

        client.emit(connect_frame.into()).await?;
        let first_frame = connected_receiver.await?;

        if let ServerCommand::Connected = first_frame.command {
            Ok(client)
        } else {
            // @TODO: Include close reason
            client.connection.close().await;

            Err(Box::new(ClientError::ConnectionError(None)))
        }
    }

    async fn spawn_server_frame_listener(
        &self,
        connected_sender: tokio::sync::oneshot::Sender<Frame<ServerCommand>>,
        mut receiver: Receiver<Result<StompMessage<ServerCommand>, ConnectionError>>,
        server_timeout: u128,
    ) {
        let connection = Arc::clone(&self.connection.clone());
        let interceptors = Arc::clone(&self.interceptors);

        tokio::spawn(async move {
            let mut last_heartbeat = Instant::now();

            let mut connected_sender = Some(connected_sender);

            loop {
                if connection.is_closed().await {
                    debug!("Connection closed, closing client");
                    receiver.close();
                }

                if server_timeout > 0 && last_heartbeat.elapsed().as_millis() > server_timeout {
                    connection.clone().close().await;
                }

                if let Ok(message) =
                    tokio::time::timeout(Duration::from_millis(100), receiver.recv()).await
                {
                    match message {
                        Some(Ok(message)) => match message {
                            StompMessage::Frame(mut frame) => {
                                debug!("Frame received: {:?}", frame.clone());
                                last_heartbeat = Instant::now();

                                let (forwarder, receiver) = Forwarder::new((*interceptors).clone());
                                forwarder
                                    .proceed(InterceptorMessage::BeforeServerReceive(frame.clone()))
                                    .await;

                                if !connected_sender
                                    .as_ref()
                                    .map(|val| val.is_closed())
                                    .unwrap_or_else(|| true)
                                {
                                    connected_sender
                                        .take()
                                        .unwrap()
                                        .send(frame.clone())
                                        .unwrap();
                                }

                                match receiver.await.unwrap() {
                                    InterceptorMessage::BeforeServerReceive(frame) => {
                                        let (forwarder, receiver) =
                                            Forwarder::new((*interceptors).clone());
                                        forwarder
                                            .proceed(InterceptorMessage::AfterServerReceive(frame))
                                            .await;
                                    }
                                    _ => {
                                        // @TODO
                                    }
                                }
                            }
                            StompMessage::Ping => last_heartbeat = Instant::now(),
                        },
                        Some(Err(_)) => {
                            break;
                        }
                        None => {
                            break;
                        }
                    }
                }
            }
        });
    }

    pub(crate) async fn subscribe(
        &self,
        subscribe: Subscribe,
        sender: Sender<Frame<ServerCommand>>,
    ) -> Result<(), Box<dyn Error>> {
        let destination = subscribe.headers["destination"].clone();
        let subscriber_id = subscribe.headers["id"].clone();
        let receipt_id = Uuid::new_v4();

        self.emit(subscribe.receipt(receipt_id.to_string()).into())
            .await?;

        self.subscriber
            .subscriber_sender()
            .send(SubscriberMessage::Register {
                subscriber_id: subscriber_id.to_string(),
                destination,
                sender,
            })
            .await;

        Ok(())
    }

    pub(crate) async fn send(&self, send: Send) -> Result<(), Box<dyn Error>> {
        let receipt_id = Uuid::new_v4();

        self.emit(send.receipt(receipt_id.to_string()).into())
            .await?;

        Ok(())
    }

    pub(crate) async fn emit(&self, mut frame: Frame<ClientCommand>) -> Result<(), Box<dyn Error>> {
        debug!("Emit frame");
        let (forwarder, reciever) = Forwarder::new((*self.interceptors).clone());
        forwarder
            .proceed(InterceptorMessage::BeforeClientSend(frame))
            .await;

        match reciever.await? {
            InterceptorMessage::BeforeClientSend(frame) => {
                let (forwarder, reciever) = Forwarder::new((*self.interceptors).clone());

                forwarder
                    .proceed(InterceptorMessage::AfterClientSend(frame))
                    .await;

                reciever.await?;
            }
            _ => {
                // @TODO: Error describing internal error
                return Err(Box::new(ClientError::ConnectionError(None)));
            }
        }
        Ok(())
    }

    pub(crate) async fn ack(&self, ack: Ack) -> Result<(), Box<dyn Error>> {
        self.emit(ack.receipt(Uuid::new_v4().to_string()).into())
            .await
    }

    pub(crate) async fn nack(&self, nack: Nack) -> Result<(), Box<dyn Error>> {
        self.emit(nack.receipt(Uuid::new_v4().to_string()).into())
            .await
    }
}
