use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, RwLock};

use raynet_core::{
    ChannelId, ConvId, CoreAction, CoreEvent, CoreEventResult, EndpointCore, RelayCore,
};
use raynet_shell_plugins::{ProxyMessage, ProxyPlugin, ProxySession};
use tokio::sync::{Mutex, mpsc};
use tokio::time::{Duration, sleep};

use crate::channels::{ChannelSenders, InboundTransportPacket};
use crate::transport::OutboundTransportPacket;
use crate::utils::CoreClock;

type Sessions = HashMap<ConvId, mpsc::Sender<ProxyMessage>>;

#[derive(Clone)]
pub(crate) struct EndpointRuntime {
    sessions: Arc<RwLock<Sessions>>,
    proxy: Arc<dyn ProxyPlugin>,
    core: Arc<Mutex<EndpointCore>>,
    ray_tx: mpsc::Sender<OutboundTransportPacket>,
    clock: Arc<CoreClock>,
}

impl EndpointRuntime {
    pub fn new(
        proxy: Arc<dyn ProxyPlugin>,
        core: Arc<Mutex<EndpointCore>>,
        ray_tx: mpsc::Sender<OutboundTransportPacket>,
        clock: Arc<CoreClock>,
    ) -> Self {
        Self {
            sessions: Arc::new(RwLock::new(HashMap::new())),
            proxy,
            core,
            ray_tx,
            clock,
        }
    }

    pub async fn receive_packets(self, mut receiver: mpsc::Receiver<InboundTransportPacket>) {
        while let Some(packet) = receiver.recv().await {
            self.handle(CoreEvent::TransportPacketReceived {
                channel_id: packet.channel_id,
                bytes: packet.bytes,
            })
            .await;
        }
    }

    pub async fn open(&self, session: ProxySession) {
        let mut actions = Vec::new();
        let result = self.core.lock().await.handle_event(
            self.clock.elapsed_ms(),
            CoreEvent::SessionOpen,
            &mut actions,
        );
        let CoreEventResult::SessionCreated { conv_id } = result else {
            unreachable!()
        };
        self.attach(conv_id, session);
        self.execute(actions).await;
    }

    pub async fn send_failed(&self, packet: OutboundTransportPacket) {
        self.handle(CoreEvent::TransportPacketSendFailed {
            channel_id: packet.channel_id,
            bytes: packet.bytes,
        })
        .await;
    }

    pub async fn poll(self) {
        loop {
            let now = self.clock.elapsed_ms();
            let deadline = self.core.lock().await.next_deadline(now);
            if deadline == u64::MAX {
                sleep(Duration::from_millis(20)).await;
            } else if deadline > now {
                sleep(Duration::from_millis((deadline - now).min(100))).await;
            } else {
                let mut actions = Vec::new();
                self.core
                    .lock()
                    .await
                    .poll(self.clock.elapsed_ms(), &mut actions);
                self.execute(actions).await;
            }
        }
    }

    async fn handle(&self, event: CoreEvent) {
        let mut actions = Vec::new();
        let _ = self
            .core
            .lock()
            .await
            .handle_event(self.clock.elapsed_ms(), event, &mut actions);
        self.execute(actions).await;
    }

    async fn execute(&self, actions: Vec<CoreAction>) {
        for action in actions {
            match action {
                CoreAction::OpenSession { conv_id } => {
                    self.attach(conv_id, self.proxy.open());
                }
                CoreAction::WriteSession { conv_id, bytes } => {
                    let input = self.sessions.read().unwrap().get(&conv_id).cloned();
                    if let Some(input) = input {
                        let _ = input.send(ProxyMessage::Write(bytes)).await;
                    }
                }
                CoreAction::CloseSession { conv_id, reason } => {
                    let input = self.sessions.write().unwrap().remove(&conv_id);
                    if let Some(input) = input {
                        let _ = input.send(ProxyMessage::Close(reason)).await;
                    }
                }
                CoreAction::SendTransportPacket { channel_id, bytes } => {
                    send_transport_action(&self.ray_tx, channel_id, bytes).await;
                }
            }
        }
    }

    fn attach(&self, conv_id: ConvId, session: ProxySession) {
        let (input, mut output) = session.split();
        self.sessions.write().unwrap().insert(conv_id, input);
        let runtime = self.clone();
        tokio::spawn(async move {
            while let Some(message) = output.recv().await {
                let closed = matches!(message, ProxyMessage::Close(_));
                let event = match message {
                    ProxyMessage::Write(bytes) => CoreEvent::SessionWrite { conv_id, bytes },
                    ProxyMessage::Close(reason) => CoreEvent::SessionClose { conv_id, reason },
                };
                runtime.emit(event).await;
                if closed {
                    break;
                }
            }
            runtime.sessions.write().unwrap().remove(&conv_id);
        });
    }

    async fn emit(&self, event: CoreEvent) {
        if let CoreEvent::SessionWrite { conv_id, bytes } = event {
            loop {
                let mut actions = Vec::new();
                let result = self.core.lock().await.handle_event(
                    self.clock.elapsed_ms(),
                    CoreEvent::SessionWrite {
                        conv_id,
                        bytes: bytes.clone(),
                    },
                    &mut actions,
                );
                if matches!(result, CoreEventResult::SessionWriteBlocked) {
                    sleep(Duration::from_millis(1)).await;
                    continue;
                }
                assert!(matches!(result, CoreEventResult::None));
                self.execute(actions).await;
                return;
            }
        }
        self.handle(event).await;
    }
}

pub async fn forward_in(
    mut receiver: mpsc::Receiver<InboundTransportPacket>,
    ray_tx: mpsc::Sender<OutboundTransportPacket>,
    relay_core: Arc<Mutex<RelayCore>>,
    clock: Arc<CoreClock>,
) {
    while let Some(packet) = receiver.recv().await {
        let mut actions = Vec::new();
        let _ = relay_core.lock().await.handle_event(
            clock.elapsed_ms(),
            CoreEvent::TransportPacketReceived {
                channel_id: packet.channel_id,
                bytes: packet.bytes,
            },
            &mut actions,
        );
        send_transport_actions(&ray_tx, actions).await;
    }
}

async fn send_transport_actions(
    ray_tx: &mpsc::Sender<OutboundTransportPacket>,
    actions: Vec<CoreAction>,
) {
    for action in actions {
        if let CoreAction::SendTransportPacket { channel_id, bytes } = action {
            send_transport_action(ray_tx, channel_id, bytes).await;
        }
    }
}

async fn send_transport_action(
    ray_tx: &mpsc::Sender<OutboundTransportPacket>,
    channel_id: ChannelId,
    bytes: Vec<u8>,
) {
    ray_tx
        .send(OutboundTransportPacket { channel_id, bytes })
        .await
        .unwrap();
}

pub async fn forward_out<F, Fut>(
    mut rx: mpsc::Receiver<OutboundTransportPacket>,
    channels: Arc<ChannelSenders>,
    mut on_failure: F,
) where
    F: FnMut(OutboundTransportPacket) -> Fut,
    Fut: Future<Output = ()>,
{
    while let Some(packet) = rx.recv().await {
        if let Err(packet) = channels.send(packet).await {
            on_failure(packet).await;
        }
    }
}
