use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use delay_map::HashMapDelay;
use futures::StreamExt;
use rand::{thread_rng, Rng};
use tokio::net::UdpSocket;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::{mpsc, oneshot};

use crate::cid::{ConnectionId, ConnectionPeer};
use crate::conn::ConnectionConfig;
use crate::event::{SocketEvent, StreamEvent};
use crate::packet::{Packet, PacketBuilder, PacketType};
use crate::stream::UtpStream;
use crate::udp::AsyncUdpSocket;

type ConnChannel = UnboundedSender<StreamEvent>;

struct Accept<P> {
    stream: oneshot::Sender<io::Result<UtpStream<P>>>,
    config: ConnectionConfig,
}

const MAX_UDP_PAYLOAD_SIZE: usize = u16::MAX as usize;
const CID_GENERATION_TRY_WARNING_COUNT: usize = 10;

/// accept_with_cid() has unique interactions compared to accept()
/// accept() pulls awaiting requests off a queue, but accept_with_cid() only
/// takes a connection off if CID matches. Because of this if we are awaiting a CID
/// eventually we need to timeout the await, or the queue would never stop growing with stale awaits
/// 20 seconds is arbitrary, after the uTP config refactor is done that can replace this constant.
/// but thee uTP config refactor is currently very low priority.
const AWAITING_CONNECTION_TIMEOUT: Duration = Duration::from_secs(20);

pub struct UtpSocket<P> {
    conns: Arc<RwLock<HashMap<ConnectionId<P>, ConnChannel>>>,
    accepts: UnboundedSender<Accept<P>>,
    accepts_with_cid: UnboundedSender<(Accept<P>, ConnectionId<P>)>,
    socket_events: UnboundedSender<SocketEvent<P>>,
}

impl UtpSocket<SocketAddr> {
    pub async fn bind(addr: SocketAddr) -> io::Result<Self> {
        let socket = UdpSocket::bind(addr).await?;
        let socket = Self::with_socket(socket);
        Ok(socket)
    }
}

impl<P> UtpSocket<P>
where
    P: ConnectionPeer + Unpin + 'static,
{
    pub fn with_socket<S>(mut socket: S) -> Self
    where
        S: AsyncUdpSocket<P> + 'static,
    {
        let conns = HashMap::new();
        let conns = Arc::new(RwLock::new(conns));

        let mut awaiting: HashMapDelay<ConnectionId<P>, Accept<P>> =
            HashMapDelay::new(AWAITING_CONNECTION_TIMEOUT);

        let mut incoming_conns: HashMapDelay<ConnectionId<P>, Packet> =
            HashMapDelay::new(AWAITING_CONNECTION_TIMEOUT);

        let (socket_event_tx, mut socket_event_rx) = mpsc::unbounded_channel();
        let (accepts_tx, mut accepts_rx) = mpsc::unbounded_channel();
        let (accepts_with_cid_tx, mut accepts_with_cid_rx) = mpsc::unbounded_channel();

        let utp = Self {
            conns: Arc::clone(&conns),
            accepts: accepts_tx,
            accepts_with_cid: accepts_with_cid_tx,
            socket_events: socket_event_tx.clone(),
        };

        tokio::spawn(async move {
            let mut buf = [0; MAX_UDP_PAYLOAD_SIZE];
            loop {
                tokio::select! {
                    biased;
                    Ok((n, src)) = socket.recv_from(&mut buf) => {
                        let packet = match Packet::decode(&buf[..n]) {
                            Ok(pkt) => pkt,
                            Err(..) => {
                                tracing::warn!(?src, "unable to decode uTP packet");
                                continue;
                            }
                        };

                        let peer_init_cid = cid_from_packet(&packet, &src, IdType::SendIdPeerInitiated);
                        let we_init_cid = cid_from_packet(&packet, &src, IdType::SendIdWeInitiated);
                        let acc_cid = cid_from_packet(&packet, &src, IdType::RecvId);
                        let mut conns = conns.write().unwrap();
                        let conn = conns
                            .get(&acc_cid)
                            .or_else(|| conns.get(&we_init_cid))
                            .or_else(|| conns.get(&peer_init_cid));
                        match conn {
                            Some(conn) => {
                                let _ = conn.send(StreamEvent::Incoming(packet));
                            }
                            None => {
                                if std::matches!(packet.packet_type(), PacketType::Syn) {
                                    let cid = cid_from_packet(&packet, &src, IdType::RecvId);

                                    // If there was an awaiting connection with the CID, then
                                    // create a new stream for that connection. Otherwise, add the
                                    // connection to the incoming connections.
                                    if let Some(accept) = awaiting.remove(&cid) {
                                        let (connected_tx, connected_rx) = oneshot::channel();
                                        let (events_tx, events_rx) = mpsc::unbounded_channel();

                                        conns.insert(cid.clone(), events_tx);

                                        let stream = UtpStream::new(
                                            cid,
                                            accept.config,
                                            Some(packet),
                                            socket_event_tx.clone(),
                                            events_rx,
                                            connected_tx
                                        );

                                        tokio::spawn(async move {
                                            Self::await_connected(stream, accept, connected_rx).await
                                        });
                                    } else {
                                        incoming_conns.insert(cid, packet);
                                    }
                                } else {
                                    tracing::debug!(
                                        cid = %packet.conn_id(),
                                        packet = ?packet.packet_type(),
                                        seq = %packet.seq_num(),
                                        ack = %packet.ack_num(),
                                        peer_init_cid = ?peer_init_cid,
                                        we_init_cid = ?we_init_cid,
                                        acc_cid = ?acc_cid,
                                        "received uTP packet for non-existing conn"
                                    );
                                    // don't send a reset if we are receiving a reset
                                    if packet.packet_type() != PacketType::Reset {
                                        // if we get a packet from an unknown source send a reset packet.
                                        let random_seq_num = thread_rng().gen_range(0..=65535);
                                        let reset_packet =
                                            PacketBuilder::new(PacketType::Reset, packet.conn_id(), crate::time::now_micros(), 100_000, random_seq_num)
                                                .build();
                                        let event = SocketEvent::Outgoing((reset_packet, src.clone()));
                                        if socket_event_tx.send(event).is_err() {
                                            tracing::warn!("Cannot transmit reset packet: socket closed channel");
                                            return;
                                        }
                                    }
                                }
                            },
                        }
                    }
                    Some((accept, cid)) = accepts_with_cid_rx.recv() => {
                        let Some(syn) = incoming_conns.remove(&cid) else {
                            awaiting.insert(cid, accept);
                            continue;
                        };
                        Self::select_accept_helper(cid, syn, conns.clone(), accept, socket_event_tx.clone());
                    }
                    Some(accept) = accepts_rx.recv(), if !incoming_conns.is_empty() => {
                        let (cid, _) = incoming_conns.iter().next().expect("at least one incoming connection");
                        let cid = cid.clone();
                        let packet = incoming_conns.remove(&cid).expect("to delete incoming connection");
                        Self::select_accept_helper(cid, packet, conns.clone(), accept, socket_event_tx.clone());
                    }
                    Some(event) = socket_event_rx.recv() => {
                        match event {
                            SocketEvent::Outgoing((packet, dst)) => {
                                let encoded = packet.encode();
                                if let Err(err) = socket.send_to(&encoded, &dst).await {
                                    tracing::debug!(
                                        %err,
                                        cid = %packet.conn_id(),
                                        packet = ?packet.packet_type(),
                                        seq = %packet.seq_num(),
                                        ack = %packet.ack_num(),
                                        "unable to send uTP packet over socket"
                                    );
                                }
                            }
                            SocketEvent::Shutdown(cid) => {
                                tracing::debug!(%cid.send, %cid.recv, "uTP conn shutdown");
                                conns.write().unwrap().remove(&cid);
                            }
                        }
                    }
                    Some(Ok((cid, accept))) = awaiting.next() => {
                        // accept_with_cid didn't receive an inbound connection within the timeout period
                        // log it and return a timeout error
                        tracing::debug!(%cid.send, %cid.recv, "accept_with_cid timed out");
                        let _ = accept
                            .stream
                            .send(Err(io::Error::from(io::ErrorKind::TimedOut)));
                    }
                    Some(Ok((cid, _packet))) = incoming_conns.next() => {
                        // didn't handle inbound connection within the timeout period
                        // log it and return a timeout error
                        tracing::debug!(%cid.send, %cid.recv, "inbound connection timed out");
                    }
                }
            }
        });

        utp
    }

    /// Internal cid generation
    fn generate_cid(
        &self,
        peer: P,
        is_initiator: bool,
        event_tx: Option<UnboundedSender<StreamEvent>>,
    ) -> ConnectionId<P> {
        let mut cid = ConnectionId {
            send: 0,
            recv: 0,
            peer,
        };
        let mut generation_attempt_count = 0;
        loop {
            if generation_attempt_count > CID_GENERATION_TRY_WARNING_COUNT {
                tracing::error!("cid() tried to generate a cid {generation_attempt_count} times")
            }
            let recv: u16 = rand::random();
            let send = if is_initiator {
                recv.wrapping_add(1)
            } else {
                recv.wrapping_sub(1)
            };
            cid.send = send;
            cid.recv = recv;

            if !self.conns.read().unwrap().contains_key(&cid) {
                if let Some(event_tx) = event_tx {
                    self.conns.write().unwrap().insert(cid.clone(), event_tx);
                }
                return cid;
            }
            generation_attempt_count += 1;
        }
    }

    pub fn cid(&self, peer: P, is_initiator: bool) -> ConnectionId<P> {
        self.generate_cid(peer, is_initiator, None)
    }

    /// Returns the number of connections currently open, both inbound and outbound.
    pub fn num_connections(&self) -> usize {
        self.conns.read().unwrap().len()
    }

    /// WARNING: only accept() or accept_with_cid() can be used in an application.
    /// they aren't compatible to use interchangeably in a program
    pub async fn accept(&self, config: ConnectionConfig) -> io::Result<UtpStream<P>> {
        let (stream_tx, stream_rx) = oneshot::channel();
        let accept = Accept {
            stream: stream_tx,
            config,
        };
        self.accepts
            .send(accept)
            .map_err(|_| io::Error::from(io::ErrorKind::NotConnected))?;
        match stream_rx.await {
            Ok(stream) => Ok(stream?),
            Err(..) => Err(io::Error::from(io::ErrorKind::TimedOut)),
        }
    }

    /// WARNING: only accept() or accept_with_cid() can be used in an application.
    /// they aren't compatible to use interchangeably in a program
    pub async fn accept_with_cid(
        &self,
        cid: ConnectionId<P>,
        config: ConnectionConfig,
    ) -> io::Result<UtpStream<P>> {
        let (stream_tx, stream_rx) = oneshot::channel();
        let accept = Accept {
            stream: stream_tx,
            config,
        };
        self.accepts_with_cid
            .send((accept, cid))
            .map_err(|_| io::Error::from(io::ErrorKind::NotConnected))?;
        match stream_rx.await {
            Ok(stream) => Ok(stream?),
            Err(..) => Err(io::Error::from(io::ErrorKind::TimedOut)),
        }
    }

    pub async fn connect(&self, peer: P, config: ConnectionConfig) -> io::Result<UtpStream<P>> {
        let (connected_tx, connected_rx) = oneshot::channel();
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        let cid = self.generate_cid(peer, true, Some(events_tx));

        let stream = UtpStream::new(
            cid,
            config,
            None,
            self.socket_events.clone(),
            events_rx,
            connected_tx,
        );

        match connected_rx.await {
            Ok(Ok(..)) => Ok(stream),
            Ok(Err(err)) => Err(err),
            Err(..) => Err(io::Error::from(io::ErrorKind::TimedOut)),
        }
    }

    pub async fn connect_with_cid(
        &self,
        cid: ConnectionId<P>,
        config: ConnectionConfig,
    ) -> io::Result<UtpStream<P>> {
        if self.conns.read().unwrap().contains_key(&cid) {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                "connection ID unavailable".to_string(),
            ));
        }

        let (connected_tx, connected_rx) = oneshot::channel();
        let (events_tx, events_rx) = mpsc::unbounded_channel();

        {
            self.conns.write().unwrap().insert(cid.clone(), events_tx);
        }

        let stream = UtpStream::new(
            cid.clone(),
            config,
            None,
            self.socket_events.clone(),
            events_rx,
            connected_tx,
        );

        match connected_rx.await {
            Ok(Ok(..)) => Ok(stream),
            Ok(Err(err)) => {
                tracing::error!(%err, "failed to open connection with {cid:?}");
                Err(err)
            }
            Err(_) => {
                tracing::error!("failed to open connection with {cid:?}");
                Err(io::Error::from(io::ErrorKind::TimedOut))
            }
        }
    }

    async fn await_connected(
        stream: UtpStream<P>,
        accept: Accept<P>,
        connected: oneshot::Receiver<io::Result<()>>,
    ) {
        match connected.await {
            Ok(Ok(..)) => {
                let _ = accept.stream.send(Ok(stream));
            }
            Ok(Err(err)) => {
                let _ = accept.stream.send(Err(err));
            }
            Err(..) => {
                let _ = accept
                    .stream
                    .send(Err(io::Error::from(io::ErrorKind::ConnectionAborted)));
            }
        }
    }

    fn select_accept_helper(
        cid: ConnectionId<P>,
        syn: Packet,
        conns: Arc<RwLock<HashMap<ConnectionId<P>, UnboundedSender<StreamEvent>>>>,
        accept: Accept<P>,
        socket_event_tx: UnboundedSender<SocketEvent<P>>,
    ) {
        if conns.read().unwrap().contains_key(&cid) {
            let _ = accept.stream.send(Err(io::Error::new(
                io::ErrorKind::Other,
                "connection ID unavailable".to_string(),
            )));
            return;
        }

        let (connected_tx, connected_rx) = oneshot::channel();
        let (events_tx, events_rx) = mpsc::unbounded_channel();

        {
            conns.write().unwrap().insert(cid.clone(), events_tx);
        }

        let stream = UtpStream::new(
            cid,
            accept.config,
            Some(syn),
            socket_event_tx,
            events_rx,
            connected_tx,
        );

        tokio::spawn(async move { Self::await_connected(stream, accept, connected_rx).await });
    }
}

#[derive(Copy, Clone, Debug)]
enum IdType {
    RecvId,
    SendIdWeInitiated,
    SendIdPeerInitiated,
}

fn cid_from_packet<P: ConnectionPeer>(
    packet: &Packet,
    src: &P,
    id_type: IdType,
) -> ConnectionId<P> {
    match id_type {
        IdType::RecvId => {
            let (send, recv) = match packet.packet_type() {
                PacketType::Syn => (packet.conn_id(), packet.conn_id().wrapping_add(1)),
                PacketType::State | PacketType::Data | PacketType::Fin | PacketType::Reset => {
                    (packet.conn_id().wrapping_sub(1), packet.conn_id())
                }
            };
            ConnectionId {
                send,
                recv,
                peer: src.clone(),
            }
        }
        IdType::SendIdWeInitiated => {
            let (send, recv) = (packet.conn_id().wrapping_add(1), packet.conn_id());
            ConnectionId {
                send,
                recv,
                peer: src.clone(),
            }
        }
        IdType::SendIdPeerInitiated => {
            let (send, recv) = (packet.conn_id(), packet.conn_id().wrapping_sub(1));
            ConnectionId {
                send,
                recv,
                peer: src.clone(),
            }
        }
    }
}

impl<P> Drop for UtpSocket<P> {
    fn drop(&mut self) {
        for conn in self.conns.read().unwrap().values() {
            let _ = conn.send(StreamEvent::Shutdown);
        }
    }
}
