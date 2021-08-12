use async_trait::async_trait;
use std::sync::Arc;
use tokio::sync::{Mutex, Notify};
use std::collections::HashMap;
use crate::client::{ReceiptId, ServerStompSender, ServerStompReceiver, ClientError};
use std::error::Error;
use crate::protocol::{Frame, ServerCommand, StompMessage, ClientCommand};
use tokio::sync::mpsc::error::SendError;
use crate::client::interceptor::Interceptor;
use log::debug;
use std::hash::Hash;
use tokio::sync::mpsc::channel;
use tokio::time::{Instant, Duration};

pub(crate) struct ReceiptAwaiter {
    pending_senders: Arc<Mutex<HashMap<ReceiptId, ServerStompSender>>>,
    pending_receivers: Arc<Mutex<HashMap<ReceiptId, ServerStompReceiver>>>
}

#[async_trait]
impl Interceptor for ReceiptAwaiter {
    async fn before_emit(&self, frame: Frame<ClientCommand>) -> Result<Frame<ClientCommand>, Box<dyn Error>> {
        if let Some(receipt) = frame.headers.get("receipt") {
            debug!("Register receipt {}", receipt);
            let (sender, receiver) = channel(1);
            let mut guard = self.pending_senders.lock().await;
            guard.insert(receipt.clone(), sender);
            drop(guard);

            let mut guard = self.pending_receivers.lock().await;
            guard.insert(receipt.clone(), receiver);
            drop(guard);
        }

        Ok(frame)
    }

    async fn after_emit(&self, frame: &Frame<ClientCommand>) -> Result<(), Box<dyn Error>> {
        if let Some(receipt) = frame.headers.get("receipt") {
            let mut guard = self.pending_receivers.lock().await;
            let server_receiver = guard.remove(receipt);
            drop(guard);


            if let Some(mut receipt_receiver) = server_receiver {
                let start = Instant::now();

                loop {
                    match tokio::time::timeout(Duration::from_millis(10), receipt_receiver.recv()).await {
                        Ok(Some(StompMessage::Frame(val))) => {
                            match val.command {
                                ServerCommand::Receipt => {
                                    self.cleanup(receipt);
                                    return Ok(());
                                }
                                ServerCommand::Error => {
                                    // @TODO: Include frame in error
                                    self.cleanup(receipt);
                                    return Err(Box::new(ClientError::Nack(format!("Error receipt for receipt"))));
                                }
                                _ => { /* non-relevant frame */ }
                            }
                        }
                        Ok(_) => { /* ignore, message not relevant for this process */ }
                        Err(_) => { /* elapsed time check done later */ }
                    }

                    if start.elapsed().as_millis() > 2000 {
                        self.cleanup(receipt);
                        return Err(Box::new(ClientError::ReceiptTimeout("".to_owned())));
                    }
                }
            }
        }

        Ok(())
    }

    async fn before_dispatch(&self, frame: Frame<ServerCommand>) -> Result<Frame<ServerCommand>, Box<dyn Error>> {
        if let Some(receipt_id) = frame.headers.get("receipt-id") {
            let mut lock = self.pending_senders.lock().await;
            if let Some(pending_sender) = lock.remove(receipt_id) {
                pending_sender.send(StompMessage::Frame(frame.clone())).await;
            };
        }
        Ok(frame)
    }
}

impl ReceiptAwaiter {
    pub(crate) fn new() -> Self {
        Self {
            pending_senders: Arc::new(Default::default()),
            pending_receivers: Arc::new(Default::default())
        }
    }

    async fn cleanup(&self, receipt: &str) {
        let mut guard = self.pending_senders.lock().await;
        guard.remove(receipt);
        drop(guard);

        let mut guard = self.pending_receivers.lock().await;
        guard.remove(receipt);
        drop(guard);
    }
}