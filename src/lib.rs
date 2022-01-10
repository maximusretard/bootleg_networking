#![deny(missing_docs)]
#![deny(clippy::all)]

#![doc = include_str!("../README.md")]

use std::any::type_name;
use std::sync::atomic;
use std::fmt::Debug;
use std::net::{SocketAddr, ToSocketAddrs};
#[cfg(feature = "native")]
use std::sync::Arc;

use turbulence::message_channels::ChannelAlreadyRegistered;
use turbulence::{
    message_channels::ChannelMessage,
    packet::{Packet as PoolPacket, PacketPool},
};

use bevy_app::{App, Events, Plugin};
use bevy_ecs::system::ResMut;
use bevy_tasks::TaskPool;
use bevy_networking_turbulence::*;
use bevy_networking_turbulence::{
    NetworkResource as NaiaNetworkResource,
    ConnectionHandle as NaiaConnectionHandle
};
pub use bevy_networking_turbulence::{
    MessageChannelMode,
    ConnectionChannelsBuilder,
    MessageChannelSettings,
    ReliableChannelSettings,
};

#[cfg(feature = "native")]
use tokio::runtime::Builder;
#[cfg(feature = "native")]
use tokio::net::ToSocketAddrs as TokioToSocketAddrs;

use net_native::*;
pub use net_native::{ChannelProcessingError, MessageChannelID, SendMessageError, ConnectionHandle};
#[cfg(feature = "native")]
pub use net_native::Runtime;

/// Stores all the networking stuff
pub struct NetworkResource {
    // Sadly, web clients can't use TCP
    #[cfg(feature = "native")]
    native: NativeNetResourceWrapper,
    // Naia stuff is used for native WebRTC servers and wasm clients
    naia: Option<NaiaNetworkResource>,
    is_server: bool,
    is_setup: bool,

}

/// Fake Tokio runtime struct used for web compatibility
/// For example, you can use Res<Runtime> in Bevy without having to use optionals all the time
#[cfg(feature = "web")]
pub struct Runtime;

impl NetworkResource {
    /// Constructs a new server, using both TCP and Naia
    #[cfg(feature = "native")]
    pub fn new_server(tokio_rt: Option<Runtime>, task_pool: TaskPool) -> Self {
        Self {
            native: NativeNetResourceWrapper::new_server(tokio_rt.unwrap()),
            naia: Some(NaiaNetworkResource::new(task_pool, None, MessageFlushingStrategy::OnEverySend, None, None)),
            is_server: true,
            is_setup: false,
        }
    }

    /// Constructs a new client. On web builds it uses Naia, while on native builds it uses the custom built native_client
    pub fn new_client(tokio_rt: Option<Runtime>, task_pool: TaskPool) -> Self {
        Self {
            #[cfg(feature = "native")]
            native: NativeNetResourceWrapper::new_client(tokio_rt.unwrap()),
            // The match statement should be optimized out by the compiler
            #[cfg(feature = "native")]
            naia: None,
            // Web clients should
            #[cfg(feature = "web")]
            naia: Some(NaiaNetworkResource::new(task_pool, None, MessageFlushingStrategy::OnEverySend, None, None)),
            is_server: false,
            is_setup: false,
        } 
    }

    /// The WebRTC listen info is only necessary for naia 
    /// The max_native_packet_size is only necessary for native builds
    /// Sets up listening for native servers
    #[cfg(feature = "native")]
    pub fn listen(&mut self, tcp_addr: impl TokioToSocketAddrs + Send + Clone + 'static, udp_addr: impl TokioToSocketAddrs + Send + Clone + 'static, webrtc_listen_info: Option<(impl ToSocketAddrs + Send + 'static, impl ToSocketAddrs + Send + 'static, impl ToSocketAddrs + Send + 'static)>, max_native_packet_size: Option<usize>) {
        if self.is_server() {
            #[cfg(feature = "native")]
            self.native.setup(tcp_addr, udp_addr, max_native_packet_size.unwrap());

            let naia = self.naia.as_mut().unwrap();

            let (naia_addr, webrtc_listen_addr, public_webrtc_listen_addr) = webrtc_listen_info.unwrap();

            let naia_addr = naia_addr.to_socket_addrs().unwrap().next().unwrap();
            let webrtc_listen_addr = webrtc_listen_addr.to_socket_addrs().unwrap().next().unwrap();
            let public_webrtc_listen_addr = public_webrtc_listen_addr.to_socket_addrs().unwrap().next().unwrap();

            naia.listen(naia_addr, Some(webrtc_listen_addr), Some(public_webrtc_listen_addr));


        } else {
            panic!("Tried to listen while client");

        }

    }

    // TODO: Make this an impl ToSocketAddr
    /// The first addr is either a TCP socket address, or a Naia address. The second address is always a UDP address, and is thus optional
    pub fn connect(&mut self, addr: SocketAddr, udp_addr: Option<SocketAddr>, max_native_packet_size: Option<usize>) {
        if self.is_client() {
            #[cfg(feature = "native")]
            self.native.setup(addr, udp_addr.unwrap(), max_native_packet_size.unwrap());

            #[cfg(feature = "web")]
            if let Some(naia) = self.naia.as_mut() {
                naia.connect(addr);

            }

        } else {
            panic!("Tried to connect while server");

        }

    }

    /// View all the messages that a certain channel as received
    pub fn view_messages<M>(&mut self, channel: &MessageChannelID) -> Result<Vec<(ConnectionHandle, M)>, ChannelProcessingError>
        where M: ChannelMessage + Debug + Clone {
        let mut messages: Vec<(ConnectionHandle, M)> = Vec::new();

        #[cfg(feature = "native")]
        {
            let mut tcp_messages = self.native.process_message_channel(channel)?;
            messages.append(&mut tcp_messages);
        }

        if let Some(naia) = self.naia.as_mut() {
            for (handle, connection) in naia.connections.iter_mut() {
                let channels = connection.channels().unwrap();

                while let Some(message) = channels.try_recv::<M>()? {
                    messages.push((ConnectionHandle::new_naia(*handle), message));

                }
            }

        }

        Ok(messages)

    }

    /// Sends a message to each connected client on servers, and on client it is equivalent to send_message
    pub fn broadcast_message<M>(&mut self, message: &M, channel: &MessageChannelID) -> Result<(), SendMessageError>
        where M: ChannelMessage + Debug + Clone {
        #[cfg(feature = "native")]
        self.native.broadcast_message(message, channel)?;

        if let Some(naia) = self.naia.as_mut() {
            // Inlined version of naia.broadcast_message(), with some modifications
            if !naia.connections.is_empty() {
                for (_handle, connection) in naia.connections.iter_mut() {
                    let channels = connection.channels().unwrap();
                    // If the result is Some(msg), that means that the message channel is full, which is no bueno. 
                    //  There's probably a better way to do this (TODO?) but since I haven't run into this issue yet, 
                    //  I don't care lol
                    if channels.try_send(message.clone())?.is_some() {
                        panic!("Message channel full for type: {:?}", type_name::<M>());

                    }

                    // Since we're using OnEverySend channel flushing, we don't need the if statement in the normal fn
                    channels.try_flush::<M>()?;

                }
            // If there are no connections and the resource is a client
            } else if self.is_client() {
                return Err(SendMessageError::NotConnected);

            }

        }

        Ok(())

    }

    /// Sends a message to a specific connection handle
    pub fn send_message<M>(&mut self, message: &M, channel: &MessageChannelID, handle: &ConnectionHandle) -> Result<(), SendMessageError>
        where M: ChannelMessage + Debug + Clone {
        if handle.is_native() {
            #[cfg(feature = "native")]
            self.native.send_message(message, channel, handle.native())?;

        } else {
            let naia = self.naia.as_mut().unwrap();

            // Inlined version of naia.send_message(), with some modifications
            match naia.connections.get_mut(handle.naia()) {
                Some(connection) => {
                    let channels = connection.channels().unwrap();
                    if channels.try_send(message.clone())?.is_some() {
                        panic!("Message channel full for type: {:?}", type_name::<M>());

                    }

                    channels.try_flush::<M>()?;
                }
                None => return Err(SendMessageError::NotConnected),
            }

        }

        Ok(())

    }

    /// On native builds, this sets up a channel to use TCP or UDP. On web builds, this does nothing
    pub fn register_message_channel_native(&mut self, settings: MessageChannelSettings, channel: &MessageChannelID) -> Result<(), ChannelAlreadyRegistered> {
        #[cfg(feature = "native")]
        self.native.register_message(channel, match &settings.channel_mode {
            MessageChannelMode::Unreliable => ChannelType::Unreliable,
            _ => ChannelType::Reliable,

        })?;

        Ok(())
        
    }

    // TODO: Combine register_message_channel_native with this fn
    /// Used for registering message channels with naia.
    pub fn set_channels_builder<F>(&mut self, builder: F) where F: Fn(&mut ConnectionChannelsBuilder) + Send + Sync + 'static {
        if let Some(naia) = self.naia.as_mut() {
            naia.set_channels_builder(builder);
        }

    }

    /// Checks for a connection
    pub fn is_connected(&self) -> bool {
        let naia_connected = match self.naia.as_ref() {
            Some(naia) => !naia.connections.is_empty(),
            None => false,
        };


        let tcp_connected = {
            #[cfg(feature = "native")]
            let connected = self.native.is_connected();

            #[cfg(feature = "web")]
            let connected = false;

            connected

        };

        tcp_connected || naia_connected

    }

    /// Disconnects from a specific client on servers, and just disconnects from the server on clients
    pub fn disconnect_from(&mut self, handle: &ConnectionHandle) -> Result<(), DisconnectError> {
        #[cfg(feature = "native")]
        if handle.is_native() {
            let handle = handle.native();
            self.native.disconnect_from(handle)?;

        }

        if handle.is_naia() {
            let handle = handle.naia();

            match self.naia.as_mut() {
                Some(naia) => naia.disconnect(*handle),
                None => return Err(DisconnectError::NotConnected),

            };

        }

        Ok(())

    }

    /// Runs the disconnect function on every connection
    pub fn disconnect_from_all(&mut self) {
        #[cfg(feature = "native")]
        self.native.disconnect_from_all();

        if let Some(naia) = self.naia.as_mut() {
            naia.connections.clear()

        }

    }

    /// Checks if it's a server
    pub fn is_server(&self) -> bool {
        self.is_server

    }

    /// Checks if it's a client
    pub fn is_client(&self) -> bool {
        !self.is_server
    }

    /// Returns true if either the listen function or the connect function has been run
    pub fn is_setup(&self) -> bool {
        self.is_setup
    }

    /// Returns a mutable refrence to the naia NetworkResource, if it exists
    pub(crate) fn as_naia_mut(&mut self) -> Option<&mut NaiaNetworkResource> {
        self.naia.as_mut()

    }
}

/// A plugin for setting up the NetworkResource
pub struct NetworkingPlugin;

impl Plugin for NetworkingPlugin {
    fn build(&self, app: &mut App) {
        #[cfg(feature = "native")]
        let tokio_rt = Arc::new(Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap());

        #[cfg(feature = "native")]
        app.insert_resource(tokio_rt);

        #[cfg(feature = "web")]
        app.insert_resource(Runtime);

        app
        .add_event::<NetworkEvent>()
        .add_system(rcv_naia_packets);

    }
}

fn rcv_naia_packets(super_net: Option<ResMut<NetworkResource>>, mut network_events: ResMut<Events<NetworkEvent>>) {
    let mut net = match super_net {
        Some(it) => it,
        _ => return,
    };

    let naia = net.as_naia_mut();

    if naia.is_none() {
        return;

    }

    let net = naia.unwrap();

    let pending_connections: Vec<Box<dyn Connection>> = net.pending_connections.lock().unwrap().drain(..).collect();
    
    for mut conn in pending_connections {
        let handle: NaiaConnectionHandle = net
            .connection_sequence
            .fetch_add(1, atomic::Ordering::Relaxed);

        if let Some(channels_builder_fn) = net.channels_builder_fn.as_ref() {
            conn.build_channels(
                channels_builder_fn,
                net.runtime.clone(),
                net.packet_pool.clone(),
            );
        }

        net.connections.insert(handle, conn);
        network_events.send(NetworkEvent::Connected(handle));

    }

    let packet_pool = net.packet_pool.clone();
    for (handle, connection) in net.connections.iter_mut() {
        while let Some(result) = connection.receive() {
            match result {
                Ok(packet) => {
                    // heartbeat packets are empty
                    if packet.is_empty() {
                        // discard without sending a NetworkEvent
                        continue;
                    }

                    if let Some(channels_rx) = connection.channels_rx() {
                        let mut pool_packet = packet_pool.acquire();
                        pool_packet.resize(packet.len(), 0);
                        pool_packet[..].copy_from_slice(&*packet);

                        if let Err(err) = channels_rx.try_send(pool_packet) {
                           network_events.send(NetworkEvent::Error(
                                *handle,
                                NetworkError::TurbulenceChannelError(err),
                            ));
                        }

                    } else {
                        network_events.send(NetworkEvent::Packet(*handle, packet));
                    }
                }
                Err(err) => {
                    network_events.send(NetworkEvent::Error(*handle, err));
                }
            }
        }
    }
}
