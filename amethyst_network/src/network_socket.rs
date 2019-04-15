//! The network send and receive System

use std::{clone::Clone, net::SocketAddr, thread};

use amethyst_core::ecs::{Join, Resources, System, SystemData, WriteStorage, Entities};

use crossbeam_channel::{Receiver, Sender};
use laminar::{Packet, SocketEvent};
use log::{error, warn};
use serde::{de::DeserializeOwned, Serialize};

use super::{
    deserialize_event,
    error::Result,
    send_event,
    server::{Host, ServerConfig},
    ConnectionState, NetConnection, NetEvent, NetFilter,
};

use log::info;

enum InternalSocketEvent<E> {
    SendEvents {
        target: SocketAddr,
        events: Vec<NetEvent<E>>,
    },
    Stop,
}

// If a client sends both a connect event and other events,
// only the connect event will be considered valid and all others will be lost.
/// The System managing the network state and connections.
/// The T generic parameter corresponds to the network event type.
/// Receives events and filters them.
/// Received events will be inserted into the NetReceiveBuffer resource.
/// To send an event, add it to the NetSendBuffer resource.
///
/// If both a connection (Connect or Connected) event is received at the same time as another event from the same connection,
/// only the connection event will be considered and rest will be filtered out.
// TODO: add Unchecked Event type list. Those events will be let pass the client connected filter (Example: NetEvent::Connect).
// Current behaviour: hardcoded passthrough of Connect and Connected events.
pub struct NetSocketSystem<E: 'static>
where
    E: PartialEq,
{
    /// The list of filters applied on the events received.
    pub filters: Vec<Box<dyn NetFilter<E>>>,
    // sender on which you can queue packets to send to some endpoint.
    transport_sender: Sender<InternalSocketEvent<E>>,
    // receiver from which you can read received packets.
    transport_receiver: Receiver<Packet>,
    config: ServerConfig,
}

impl<E> NetSocketSystem<E>
where
    E: Serialize + PartialEq + Send + 'static,
{
    /// Creates a `NetSocketSystem` and binds the Socket on the ip and port added in parameters.
    pub fn new(config: ServerConfig, filters: Vec<Box<dyn NetFilter<E>>>) -> Result<Self> {
        if config.udp_socket_addr.port() < 1024 {
            // Just warning the user here, just in case they want to use the root port.
            warn!("Using a port below 1024, this will require root permission and should not be done.");
        }

        let server = Host::run(&config)?;

        let udp_send_handle = server.udp_send_handle();
        let udp_receive_handle = server.udp_receive_handle();

        let server_sender = NetSocketSystem::<E>::start_sending(udp_send_handle);
        let server_receiver = NetSocketSystem::<E>::start_receiving(udp_receive_handle);

        Ok(NetSocketSystem {
            filters,
            transport_sender: server_sender,
            transport_receiver: server_receiver,
            config,
        })
    }

    /// Start a thread to send all queued packets.
    fn start_sending(sender: Sender<Packet>) -> Sender<InternalSocketEvent<E>> {
        let (tx, send_queue) = crossbeam_channel::unbounded();

        thread::spawn(move || loop {
            for control_event in send_queue.try_iter() {
                match control_event {
                    InternalSocketEvent::SendEvents { target, events } => {
                        for ev in events {
                            send_event(ev, target, &sender);
                        }
                    }
                    InternalSocketEvent::Stop => {
                        break;
                    }
                }
            }
        });

        tx
    }

    /// Starts a thread which receives incoming packets and sends them onto the 'Receiver' channel.
    fn start_receiving(receiver: Receiver<SocketEvent>) -> Receiver<Packet> {
        let (receive_queue, rx) = crossbeam_channel::unbounded();

        thread::spawn(move || loop {
            for event in receiver.iter() {
                match event {
                    SocketEvent::Packet(packet) => {
                        if let Err(error) = receive_queue.send(packet.clone()) {
                            error!("`NetworkSocketSystem` was dropped. Reason: {:?}", error);
                            break;
                        }
                    }
                    _ => error!("Event not supported"),
                }
            }
        });

        rx
    }
}

impl<'a, E> System<'a> for NetSocketSystem<E>
where
    E: Send + Sync + Serialize + Clone + DeserializeOwned + PartialEq + 'static,
{
    type SystemData = (
        Entities<'a>,
        WriteStorage<'a, NetConnection<E>>
    );

    fn run(&mut self, (entities, mut net_connections): Self::SystemData) {
        for net_connection in (&mut net_connections).join() {
            let target = net_connection.target_addr;

            if net_connection.state == ConnectionState::Connected
                || net_connection.state == ConnectionState::Connecting
            {
                self.transport_sender
                    .send(InternalSocketEvent::SendEvents {
                        target,
                        events: net_connection.send_buffer_early_read().cloned().collect(),
                    })
                    .expect("Unreachable: Channel will be alive until a stop event is sent");
            } else if net_connection.state == ConnectionState::Disconnected {
                self.transport_sender
                    .send(InternalSocketEvent::Stop)
                    .expect("Already sent a stop event to the channel");
            }
        }

        for (counter, raw_event) in self.transport_receiver.try_iter().enumerate() {
	        // Do it twice to collect from activated connections
            for _ in 0..2 {
                let mut matched = false;
                // Get the NetConnection from the source
                for net_connection in (&mut net_connections).join() {
                    // We found the origin
                    if net_connection.target_addr == raw_event.addr() {
                        matched = true;
                        // Get the event
                        match deserialize_event::<E>(raw_event.payload()) {
                            Ok(ev) => {
                                net_connection.receive_buffer.single_write(ev);
                            }
                            Err(e) => error!(
                                "Failed to deserialize an incoming network event: {} From source: {:?}",
                                e,
                                raw_event.addr()
                            ),
                        }
                        // No two NetConnections can share a target
                        break;
	                }
                }
                if !matched {
                    // Instead of just complaining about missing this source we are going to make a
                    // new NetConnection to receive from this source
                    // TODO: This is of course susceptible to DoS so uhhhh we need to deal with that
                    // Bring in the entities so that we can add a NetConnection
                    info!("MAKING A NETCONNECTION!!!! LOL GREP FOR XDXD");
                    entities.build_entity()
	                    // We need to assume the target will receive from the same address as they sent from, perhaps a (TODO) proper connection builder would send the recieve address as the next packet
                        .with(NetConnection::<E>::new(raw_event.addr()), &mut net_connections)
                        .build();
                }
                else {
                    break
                }
            }

            // this will prevent our system to be stuck in the iterator.
            // After 10000 packets we will continue and leave the other packets for the next run.
            // eventually some congestion prevention should be done.
            if counter >= self.config.max_throughput as usize {
                break;
            }
        }
        info!("end of NS::run");
    }

    fn setup(&mut self, res: &mut Resources) {
        Self::SystemData::setup(res);
    }
}
