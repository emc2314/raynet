use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, RwLock};

use raynet_core::{
    ChannelId, ConvId, CoreAction, CoreEvent, CoreEventResult, EndpointCore, RelayCore,
};
use raynet_shell_plugins::{ChannelSendFailure, ProxyMessage, ProxyPlugin, ProxySession};
use tokio::sync::{Mutex, Notify, mpsc};
use tokio::time::{Duration, sleep};

use crate::channels::{ChannelSenders, InboundTransportPacket};
use crate::transport::OutboundTransportPacket;
use crate::utils::CoreClock;

type Sessions = HashMap<ConvId, mpsc::Sender<ProxyMessage>>;
const PACKET_BATCH_SIZE: usize = 64;

#[derive(Clone)]
pub(crate) struct EndpointRuntime {
    sessions: Arc<RwLock<Sessions>>,
    proxy: Arc<dyn ProxyPlugin>,
    core: Arc<Mutex<EndpointCore>>,
    ray_tx: mpsc::Sender<OutboundTransportPacket>,
    clock: Arc<CoreClock>,
    /// Wakes session writers waiting on `SessionWriteBlocked`.
    writable: Arc<Notify>,
    /// Wakes the poll loop when a core event creates an earlier deadline.
    deadline_changed: Arc<Notify>,
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
            writable: Arc::new(Notify::new()),
            deadline_changed: Arc::new(Notify::new()),
        }
    }

    pub async fn receive_packets(self, mut receiver: mpsc::Receiver<InboundTransportPacket>) {
        let mut batch = Vec::with_capacity(PACKET_BATCH_SIZE);
        loop {
            batch.clear();
            let Some(first) = receiver.recv().await else {
                return;
            };
            batch.push(first);
            while batch.len() < PACKET_BATCH_SIZE {
                match receiver.try_recv() {
                    Ok(packet) => batch.push(packet),
                    Err(_) => break,
                }
            }
            let mut actions = Vec::new();
            {
                let mut core = self.core.lock().await;
                let elapsed = self.clock.elapsed_ms();
                for packet in batch.drain(..) {
                    let _ = core.handle_event(
                        elapsed,
                        CoreEvent::TransportPacketReceived {
                            channel_id: packet.channel_id,
                            bytes: packet.bytes,
                        },
                        &mut actions,
                    );
                }
            }
            self.writable.notify_waiters();
            self.deadline_changed.notify_one();
            self.execute(actions).await;
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
        self.deadline_changed.notify_one();
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
            if deadline <= now {
                let mut actions = Vec::new();
                self.core
                    .lock()
                    .await
                    .poll(self.clock.elapsed_ms(), &mut actions);
                self.writable.notify_waiters();
                self.execute(actions).await;
                continue;
            }
            let delay_ms = if deadline == u64::MAX {
                20
            } else {
                (deadline - now).min(100)
            };
            tokio::select! {
                _ = sleep(Duration::from_millis(delay_ms)) => {}
                _ = self.deadline_changed.notified() => {}
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
        self.writable.notify_waiters();
        self.deadline_changed.notify_one();
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
                let notified = self.writable.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
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
                    // Wait until inbound/poll frees send window.
                    notified.await;
                    continue;
                }
                assert!(matches!(result, CoreEventResult::None));
                self.deadline_changed.notify_one();
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
    let mut batch = Vec::with_capacity(PACKET_BATCH_SIZE);
    loop {
        batch.clear();
        let Some(first) = receiver.recv().await else {
            return;
        };
        batch.push(first);
        while batch.len() < PACKET_BATCH_SIZE {
            match receiver.try_recv() {
                Ok(packet) => batch.push(packet),
                Err(_) => break,
            }
        }
        let mut actions = Vec::new();
        {
            let mut core = relay_core.lock().await;
            let elapsed = clock.elapsed_ms();
            for packet in batch.drain(..) {
                let _ = core.handle_event(
                    elapsed,
                    CoreEvent::TransportPacketReceived {
                        channel_id: packet.channel_id,
                        bytes: packet.bytes,
                    },
                    &mut actions,
                );
            }
        }
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
    mut failures: mpsc::Receiver<ChannelSendFailure>,
    channels: Arc<ChannelSenders>,
    mut on_failure: F,
) where
    F: FnMut(OutboundTransportPacket) -> Fut,
    Fut: Future<Output = ()>,
{
    let mut batch = Vec::with_capacity(PACKET_BATCH_SIZE);
    loop {
        tokio::select! {
            biased;
            failure = failures.recv() => {
                let Some((channel_id, bytes)) = failure else { return };
                on_failure(OutboundTransportPacket { channel_id, bytes }).await;
            }
            packet = rx.recv() => {
                let Some(first) = packet else { return };
                batch.clear();
                batch.push(first);
                while batch.len() < PACKET_BATCH_SIZE {
                    match rx.try_recv() {
                        Ok(packet) => batch.push(packet),
                        Err(_) => break,
                    }
                }
                for packet in batch.drain(..) {
                    if let Err(packet) = channels.send(packet).await {
                        on_failure(packet).await;
                    }
                }
            }
        }
    }
}
