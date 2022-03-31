use crate::multiaddress_ext::MultiaddrExt as _;
use crate::upgrade;
use crate::Connection;
use crate::Substream;
use anyhow::bail;
use anyhow::Context as _;
use anyhow::Result;
use async_trait::async_trait;
use futures::future::BoxFuture;
use futures::stream::BoxStream;
use futures::AsyncRead;
use futures::AsyncWrite;
use futures::TryStreamExt;
use libp2p_core::identity::Keypair;
use libp2p_core::transport::Boxed;
use libp2p_core::transport::ListenerEvent;
use libp2p_core::Multiaddr;
use libp2p_core::PeerId;
use libp2p_core::Transport;
use multistream_select::NegotiationError;
use multistream_select::Version;
use std::collections::HashMap;
use std::collections::HashSet;
use std::marker::PhantomData;
use std::time::Duration;
use thiserror::Error;
use tokio_tasks::Tasks;
use xtra::message_channel::StrongMessageChannel;
use xtra_productivity::xtra_productivity;
use xtras::SendAsyncSafe;

/// An actor for managing multiplexed connections over a given transport thus representing an
/// _endpoint_.
///
/// The actor does not impose any policy on connection and/or protocol management.
/// New connections can be established by sending a [`Connect`] messages. Existing connections can
/// be disconnected by sending [`Disconnect`]. Listening for incoming connections is done by sending
/// a [`ListenOn`] message. To list the current state, send the [`GetConnectionStats`] message.
///
/// The combination of the above should make it possible to implement a fairly large number of
/// policies. For example, to maintain a connection to an another endpoint, you can regularly check
/// if the connection is still established by sending [`GetConnectionStats`] and react accordingly
/// (f.e. sending [`Connect`] in case the connection has disappeared).
///
/// Once a connection with a peer is established, both sides can open substreams on top of the
/// connection. Any incoming substream will - assuming the protocol is supported by the endpoint -
/// trigger a [`NewInboundSubstream`] message to the actor provided in the constructor.
/// Opening a new substream can be achieved by sending the [`OpenSubstream`] message.
pub struct Endpoint {
    transport: Boxed<Connection>,
    tasks: Tasks,
    controls: HashMap<PeerId, (yamux::Control, Tasks)>,
    inbound_substream_channels:
        HashMap<&'static str, Box<dyn StrongMessageChannel<NewInboundSubstream>>>,
    listen_addresses: HashSet<Multiaddr>,
    inflight_connections: HashSet<PeerId>,
    connection_timeout: Duration,
}

/// Open a substream to the provided peer.
///
/// Fails if we are not connected to the peer or the peer does not support any of the requested
/// protocols.
#[derive(Debug)]
pub struct OpenSubstream<P> {
    peer: PeerId,
    protocols: Vec<&'static str>,
    marker_num_protocols: PhantomData<P>,
}

#[derive(Clone, Copy, Debug)]
pub enum Single {}

#[derive(Clone, Copy, Debug)]
pub enum Multiple {}

impl OpenSubstream<Single> {
    /// Constructs [`OpenSubstream`] with a single protocol.
    ///
    /// We will only attempt to negotiate the given protocol. If the endpoint does not speak this
    /// protocol, negotiation will fail.
    pub fn single_protocol(peer: PeerId, protocol: &'static str) -> Self {
        Self {
            peer,
            protocols: vec![protocol],
            marker_num_protocols: PhantomData,
        }
    }
}

impl OpenSubstream<Multiple> {
    /// Constructs [`OpenSubstream`] with multiple protocols.
    ///
    /// The given protocols will be tried **in order**, with the first successful one being used.
    /// Specifying multiple protocols can useful to maintain backwards-compatibility. An endpoint
    /// can attempt to first establish a substream with a new protocol and falling back to older
    /// versions in case the new version is not supported.
    pub fn multiple_protocols(peer: PeerId, protocols: Vec<&'static str>) -> Self {
        Self {
            peer,
            protocols,
            marker_num_protocols: PhantomData,
        }
    }
}

/// Connect to the given [`Multiaddr`].
///
/// The address must contain a `/p2p` suffix.
/// Will fail if we are already connected to the peer.
#[derive(Debug)]
pub struct Connect(pub Multiaddr);

/// Disconnect from the given peer.
#[derive(Clone, Copy, Debug)]
pub struct Disconnect(pub PeerId);

/// Listen on the provided [`Multiaddr`].
///
/// For this to work, the [`Endpoint`] needs to be constructed with a compatible transport.
/// In other words, you cannot listen on a `/memory` address if you haven't configured a `/memory`
/// transport.
pub struct ListenOn(pub Multiaddr);

/// Retrieve [`ConnectionStats`] from the [`Endpoint`].
#[derive(Clone, Copy, Debug)]
pub struct GetConnectionStats;

#[derive(Debug)]
pub struct ConnectionStats {
    pub connected_peers: HashSet<PeerId>,
    pub listen_addresses: HashSet<Multiaddr>,
}

/// Notifies an actor of a new, inbound substream from the given peer.
#[derive(Debug)]
pub struct NewInboundSubstream {
    pub peer: PeerId,
    pub stream: Substream,
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("No connection to {0}")]
    NoConnection(PeerId),
    #[error("Timeout in protocol negotiation")]
    NegotiationTimeoutReached,
    #[error("Failed to negotiate protocol")]
    NegotiationFailed(#[from] NegotiationError), // TODO(public-api): Consider breaking this up.
    #[error("Bad connection")]
    BadConnection(#[from] yamux::ConnectionError), // TODO(public-api): Consider removing this.
    #[error("Address {0} does not end with a peer ID")]
    NoPeerIdInAddress(Multiaddr),
    #[error("Either currently connecting or already connected to peer {0}")]
    AlreadyConnected(PeerId),
}

impl Endpoint {
    /// Construct a new [`Endpoint`] from the provided transport.
    ///
    /// An [`Endpoint`]s identity ([`PeerId`]) will be computed from the given [`Keypair`].
    ///
    /// The `connection_timeout` is applied to:
    /// 1. Connection upgrades (i.e. noise handshake, yamux upgrade, etc)
    /// 2. Protocol negotiations
    ///
    /// The provided substream handlers are actors that will be given the fully-negotiated
    /// substreams whenever a peer opens a new substream for the provided protocol.
    pub fn new<T, const N: usize>(
        transport: T,
        identity: Keypair,
        connection_timeout: Duration,
        inbound_substream_handlers: [(
            &'static str,
            Box<dyn StrongMessageChannel<NewInboundSubstream>>,
        ); N],
    ) -> Self
    where
        T: Transport + Clone + Send + Sync + 'static,
        T::Output: AsyncRead + AsyncWrite + Unpin + Send + 'static,
        T::Error: Send + Sync,
        T::Listener: Send + 'static,
        T::Dial: Send + 'static,
        T::ListenerUpgrade: Send + 'static,
    {
        let transport = upgrade::transport(
            transport,
            &identity,
            inbound_substream_handlers
                .iter()
                .map(|(proto, _)| *proto)
                .collect(),
            connection_timeout,
        );

        Self {
            transport,
            tasks: Tasks::default(),
            inbound_substream_channels: verify_unique_handlers(inbound_substream_handlers),
            controls: HashMap::default(),
            listen_addresses: HashSet::default(),
            inflight_connections: HashSet::default(),
            connection_timeout,
        }
    }

    fn drop_connection(&mut self, peer: &PeerId) {
        let (mut control, tasks) = match self.controls.remove(peer) {
            None => return,
            Some(control) => control,
        };

        // TODO: Evaluate whether dropping and closing has to be in a particular order.
        self.tasks.add(async move {
            let _ = control.close().await;
            drop(tasks);
        });
    }

    async fn open_substream(
        &mut self,
        peer: PeerId,
        protocols: Vec<&'static str>,
    ) -> Result<(&'static str, Substream), Error> {
        let (control, _) = self
            .controls
            .get_mut(&peer)
            .ok_or(Error::NoConnection(peer))?;

        let stream = control.open_stream().await?;

        let (protocol, stream) = tokio::time::timeout(
            self.connection_timeout,
            multistream_select::dialer_select_proto(stream, protocols, Version::V1),
        )
        .await
        .map_err(|_timeout| Error::NegotiationTimeoutReached)?
        .map_err(Error::NegotiationFailed)?;

        Ok((protocol, stream))
    }
}

#[xtra_productivity]
impl Endpoint {
    async fn handle(&mut self, msg: NewConnection, ctx: &mut xtra::Context<Self>) {
        self.inflight_connections.remove(&msg.peer);
        let this = ctx.address().expect("we are alive");

        let NewConnection {
            peer,
            control,
            mut incoming_substreams,
            worker,
        } = msg;

        let mut tasks = Tasks::default();
        tasks.add(worker);
        tasks.add_fallible(
            {
                let inbound_substream_channels = self
                    .inbound_substream_channels
                    .iter()
                    .map(|(proto, channel)| {
                        (
                            proto.to_owned(),
                            StrongMessageChannel::clone_channel(channel.as_ref()),
                        )
                    })
                    .collect::<HashMap<_, _>>();

                async move {
                    loop {
                        let (stream, protocol) = match incoming_substreams.try_next().await {
                            Ok(Some(Ok((stream, protocol)))) => (stream, protocol),
                            Ok(Some(Err(upgrade::Error::NegotiationTimeoutReached))) => {
                                tracing::debug!("Hit timeout while negotiating substream");
                                continue;
                            }
                            Ok(Some(Err(upgrade::Error::NegotiationFailed(e)))) => {
                                tracing::debug!("Failed to negotiate substream: {}", e);
                                continue;
                            }
                            Ok(None) => bail!("Substream listener closed"),
                            Err(e) => bail!(e),
                        };

                        let channel = inbound_substream_channels
                            .get(&protocol)
                            .expect("Cannot negotiate a protocol that we don't support");

                        let _ = channel
                            .send_async_safe(NewInboundSubstream { peer, stream })
                            .await;
                    }
                }
            },
            move |error| async move {
                let _ = this.send(ExistingConnectionFailed { peer, error }).await;
            },
        );
        self.controls.insert(peer, (control, tasks));
    }

    async fn handle(&mut self, msg: ListenerFailed) {
        tracing::debug!("Listener failed: {:#}", msg.error);

        self.listen_addresses.remove(&msg.address);
    }

    async fn handle(&mut self, msg: FailedToConnect) {
        tracing::debug!("Failed to connect: {:#}", msg.error);
        let peer = msg.peer;

        self.inflight_connections.remove(&peer);
        self.drop_connection(&peer);
    }

    async fn handle(&mut self, msg: ExistingConnectionFailed) {
        tracing::debug!("Connection failed: {:#}", msg.error);
        let peer = msg.peer;

        self.drop_connection(&peer);
    }

    async fn handle(&mut self, _: GetConnectionStats) -> ConnectionStats {
        ConnectionStats {
            connected_peers: self.controls.keys().copied().collect(),
            listen_addresses: self.listen_addresses.clone(),
        }
    }

    async fn handle(&mut self, msg: Connect, ctx: &mut xtra::Context<Self>) -> Result<(), Error> {
        let this = ctx.address().expect("we are alive");

        let peer = msg
            .0
            .clone()
            .extract_peer_id()
            .ok_or_else(|| Error::NoPeerIdInAddress(msg.0.clone()))?;

        if self.inflight_connections.contains(&peer) || self.controls.contains_key(&peer) {
            return Err(Error::AlreadyConnected(peer));
        }

        self.inflight_connections.insert(peer);
        self.tasks.add_fallible(
            {
                let transport = self.transport.clone();
                let this = this.clone();

                async move {
                    let (peer, control, incoming_substreams, worker) =
                        transport.clone().dial(msg.0)?.await?;

                    let _ = this
                        .send_async_safe(NewConnection {
                            peer,
                            control,
                            incoming_substreams,
                            worker,
                        })
                        .await;

                    anyhow::Ok(())
                }
            },
            move |error| async move {
                let _ = this.send(FailedToConnect { peer, error }).await;
            },
        );

        Ok(())
    }

    async fn handle(&mut self, msg: Disconnect) {
        self.drop_connection(&msg.0);
    }

    async fn handle(&mut self, msg: ListenOn, ctx: &mut xtra::Context<Self>) {
        let this = ctx.address().expect("we are alive");
        let listen_address = msg.0.clone();

        self.listen_addresses.insert(listen_address.clone()); // FIXME: This address could be a "catch-all" like "0.0.0.0" which actually results in
                                                              // listening on multiple interfaces.
        self.tasks.add_fallible(
            {
                let transport = self.transport.clone();
                let this = this.clone();

                async move {
                    let mut stream = transport.listen_on(msg.0)?;

                    loop {
                        let event = stream.try_next().await?.context("Listener closed")?;
                        let (peer, control, incoming_substreams, worker) = match event {
                            ListenerEvent::Upgrade { upgrade, .. } => upgrade.await?,
                            _ => continue,
                        };

                        this.send_async_safe(NewConnection {
                            peer,
                            control,
                            incoming_substreams,
                            worker,
                        })
                        .await?;
                    }
                }
            },
            |error| async move {
                let _ = this
                    .send(ListenerFailed {
                        address: listen_address,
                        error,
                    })
                    .await;
            },
        );
    }

    async fn handle(&mut self, msg: OpenSubstream<Single>) -> Result<Substream, Error> {
        let peer = msg.peer;
        let protocols = msg.protocols;

        debug_assert!(
            protocols.len() == 1,
            "Type-system enforces that we only try to negotiate one protocol"
        );

        let (protocol, stream) = self.open_substream(peer, protocols.clone()).await?;

        debug_assert!(
            protocol == protocols[0],
            "If negotiation is successful, must have selected the only protocol we sent."
        );

        Ok(stream)
    }

    async fn handle(
        &mut self,
        msg: OpenSubstream<Multiple>,
    ) -> Result<(&'static str, Substream), Error> {
        let peer = msg.peer;
        let protocols = msg.protocols;

        let (protocol, stream) = self.open_substream(peer, protocols).await?;

        Ok((protocol, stream))
    }
}

fn verify_unique_handlers<const N: usize>(
    inbound_substream_handlers: [(&str, Box<dyn StrongMessageChannel<NewInboundSubstream>>); N],
) -> HashMap<&str, Box<dyn StrongMessageChannel<NewInboundSubstream>>> {
    let mut map = HashMap::with_capacity(inbound_substream_handlers.len());

    for (protocol, handler) in inbound_substream_handlers {
        let previous_handler = map.insert(protocol, handler);

        debug_assert!(
            previous_handler.is_none(),
            "Duplicate handler declared for protocol {protocol}"
        );
    }

    map
}

#[async_trait]
impl xtra::Actor for Endpoint {
    type Stop = ();

    async fn stopped(self) -> Self::Stop {}
}

#[derive(Debug)]
struct ListenerFailed {
    address: Multiaddr,
    error: anyhow::Error,
}

#[derive(Debug)]
struct FailedToConnect {
    peer: PeerId,
    error: anyhow::Error,
}

#[derive(Debug)]
struct ExistingConnectionFailed {
    peer: PeerId,
    error: anyhow::Error,
}

struct NewConnection {
    peer: PeerId,
    control: yamux::Control,
    #[allow(clippy::type_complexity)]
    incoming_substreams: BoxStream<
        'static,
        Result<Result<(Substream, &'static str), upgrade::Error>, yamux::ConnectionError>,
    >,
    worker: BoxFuture<'static, ()>,
}

impl xtra::Message for NewInboundSubstream {
    type Result = ();
}