use std::{
    cell::RefCell,
    future::Future,
    pin::Pin,
    task::Poll,
    time::Duration,
};

use anyhow::{
    anyhow,
    Context,
    Error,
};
use radar_shared::{
    protocol::{
        C2SMessage,
        ClientEvent,
        RadarUpdate,
        S2CMessage,
    },
    RadarSettings,
    RadarState,
};
use tokio::{
    self,
    sync::mpsc::{
        Receiver,
        Sender,
    },
    time::{
        self,
        Interval,
    },
};

mod generator;
pub use generator::*;

pub trait RadarGenerator {
    fn generate_state(&mut self, settings: &RadarSettings) -> anyhow::Result<RadarState>;
}

pub struct WebRadarPublisher {
    pub session_id: String,

    generator: RefCell<Box<dyn RadarGenerator>>,
    generate_interval: Pin<Box<Interval>>,

    settings: RadarSettings,

    transport_tx: Sender<C2SMessage>,
    transport_rx: Receiver<ClientEvent<S2CMessage>>,
}

impl WebRadarPublisher {
    pub async fn create_from_transport(
        generator: Box<dyn RadarGenerator>,
        tx: Sender<C2SMessage>,
        mut rx: Receiver<ClientEvent<S2CMessage>>,
    ) -> anyhow::Result<Self> {
        let _ = tx.send(C2SMessage::InitializePublish { version: 1 }).await;
        let event = tokio::select! {
            message = rx.recv() => message.context("unexpected client disconnect")?,
            _ = time::sleep(Duration::from_secs(5)) => {
                anyhow::bail!("session init timeout");
            }
        };

        let session_id = match event {
            ClientEvent::RecvMessage(message) => match message {
                S2CMessage::ResponseError { error } => {
                    anyhow::bail!("server error: {}", error)
                }
                S2CMessage::ResponseInitializePublish { session_id, .. } => session_id,
                _ => anyhow::bail!("invalid response"),
            },
            ClientEvent::RecvError(err) => anyhow::bail!("recv err: {:#}", err),
            ClientEvent::SendError(err) => anyhow::bail!("send err: {:#}", err),
        };

        log::debug!("Connected with session id {}", session_id);
        Ok(Self {
            session_id,
            generator: RefCell::new(generator),

            transport_rx: rx,
            transport_tx: tx,

            generate_interval: Box::pin(time::interval(Duration::from_millis(50))),

            settings: RadarSettings {
                show_team_players: true,
                show_enemy_players: true,
            },
        })
    }

    fn send_message(&self, message: C2SMessage) {
        let _ = self.transport_tx.try_send(message);
    }
}

impl Future for WebRadarPublisher {
    type Output = Option<Error>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<Self::Output> {
        while let Poll::Ready(message) = self.transport_rx.poll_recv(cx) {
            match message {
                Some(event) => {
                    match event {
                        ClientEvent::RecvError(err) => {
                            log::debug!("Recv error: {}", err);
                            return Poll::Ready(Some(err));
                        }
                        ClientEvent::SendError(err) => {
                            log::debug!("Send error: {}", err);
                            return Poll::Ready(Some(err));
                        }
                        ClientEvent::RecvMessage(_message) => { /* TODO? */ }
                    }
                }
                None => return Poll::Ready(Some(anyhow!("transport closed"))),
            }
        }

        while let Poll::Ready(_) = self.generate_interval.poll_tick(cx) {
            match self.generator.borrow_mut().generate_state(&self.settings) {
                Ok(state) => self.send_message(C2SMessage::RadarUpdate {
                    update: RadarUpdate::State { state },
                }),
                Err(err) => {
                    log::warn!("Failed to generate radar state: {:#}", err);
                }
            }
        }

        Poll::Pending
    }
}
