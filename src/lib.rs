use anyhow::{anyhow, bail, Context, Result};
use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{Arc, Mutex},
    u32,
};
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite;
use tungstenite::Message as WsMessage;

use futures::channel::mpsc::{self, UnboundedReceiver};

const MCU: u32 = 999;

// I have not got around to implementing peer messages as enum variants
// which are deserialized and serialized as JSON messages

// use serde_derive::{Deserialize, Serialize};
// #[derive(Serialize, Deserialize)]
// #[serde(rename_all = "lowercase")]
// enum SocketMsg {
//     RoomPeer {
//         // Messages starting with ROOM_PEER. Do not warrant the any action
//         // from the Socket Server other than routing the message itself
//         //
//         // TYPES:
//         // 1) ROOM_PEER_MSG - is either an SDP or ICE
//         // 2) ROOM_PEER_JOINED - notification that a peer has joined the ROOM
//         // 3) ROOM_PEER_LEFT - notification that a peer has left the ROOM
//         #[serde(rename = "type")]
//         type_: String,
//         to: u32,
//         from: u32,
//     },
//     Server {
//         // Messages meant specifically for/from the "MCU". Typically conveying
//         // some communication prompting the "MCU" to take some action/measure
//         //
//         // TYPES:
//         // 1) SERVER_RESTART - indicating that all peers from a particular ROOM have left
//         // 2) SERVER_CREATE - notify MCU to ready a new socket to serve a new ROOM session
//         // 3) SERVER_INVITE - notify the MCU to join a particular ROOM session
//         // ... Can be added to
//         #[serde(rename = "type")]
//         type_: String,
//         to: u32,
//         from: u32,
//     },
//     RoomCommand {
//         // Messages meant for the "Socket Server" for ROOM creation
//         room_id: u32,
//         to: u32,
//         from: u32,
//     },
// }

#[derive(Debug, Clone)]
pub struct Peer(Arc<PeerInner>);

#[derive(Debug)]
pub struct PeerInner {
    id: u32,
    addr: SocketAddr,
    status: Mutex<Option<u32>>,
    tx: Arc<Mutex<mpsc::UnboundedSender<WsMessage>>>,
    task_handle: JoinHandle<()>,
}

#[derive(Debug, Clone)]
pub struct Server(Arc<ServerInner>);

#[derive(Debug)]
pub struct ServerInner {
    //  peers: {uid: [Peer tx,
    //                remote_address,
    //                <"room_id"|None>]}
    peers: Mutex<HashMap<u32, Peer>>,
    //
    //
    // rooms: {room_id: [peer1_id, peer2_id, peer3_id, ...]}
    rooms: Mutex<HashMap<u32, Vec<u32>>>,
    //
    //
    // room_servers: {server_id: [room_id1, room_id2 ...]}
    room_servers: Mutex<HashMap<u32, Vec<u32>>>,
}

impl std::ops::Deref for Peer {
    type Target = PeerInner;

    fn deref(&self) -> &PeerInner {
        &self.0
    }
}

impl std::ops::Drop for PeerInner {
    fn drop(&mut self) {
        println!("Destructor was called for Peer {}\n", self.id);
        self.task_handle.abort()
    }
}

impl std::ops::Deref for Server {
    type Target = ServerInner;

    fn deref(&self) -> &ServerInner {
        &self.0
    }
}

impl Peer {
    fn new(
        id: u32,
        addr: SocketAddr,
        status: Option<u32>,
        task_handle: JoinHandle<()>,
    ) -> Result<(Self, UnboundedReceiver<WsMessage>)> {
        let (tx, rx) = mpsc::unbounded::<WsMessage>();
        let status = Mutex::new(status);

        let peer = Peer(Arc::new(PeerInner {
            id,
            addr,
            status,
            tx: Arc::new(Mutex::new(tx)),
            task_handle,
        }));

        Ok((peer, rx))
    }
}

impl Server {
    pub fn new() -> Result<Self> {
        // let (ws_msg_tx, ws_msg_rx) = mpsc::unbounded::<WsMessage>();

        let server = Server(Arc::new(ServerInner {
            peers: Mutex::new(HashMap::new()),
            rooms: Mutex::new(HashMap::new()),
            room_servers: Mutex::new(HashMap::new()),
        }));

        Ok(server)
    }

    pub fn register_peer(
        &self,
        msg: WsMessage,
        addr: SocketAddr,
        task_handle: JoinHandle<()>,
    ) -> Result<(u32, UnboundedReceiver<WsMessage>)> {
        let peer_id = msg
            .into_text()
            .map(|text| {
                if text.starts_with("Hello") {
                    let mut split = text["Hello ".len()..].splitn(2, ' ');
                    let peer_id = split
                        .next()
                        .and_then(|s| str::parse::<u32>(s).ok())
                        .ok_or_else(|| anyhow!("Cannot parse peer id"))
                        .unwrap();
                    println!("Peer is registering with ID: {}", &peer_id);

                    // If the connecting peer is MCU, register them
                    if peer_id.to_string().starts_with("999") {
                        println!("MCU {} has connected..", peer_id);
                        let mut room_servers = self.room_servers.lock().unwrap();
                        room_servers.insert(peer_id, vec![]);
                        drop(room_servers);
                    }

                    Some(peer_id)
                } else {
                    None
                }
            })
            .expect("Server did not say Hello");

        let ws_rx = if let Some(peer_id) = peer_id {
            let mut peers = self.peers.lock().unwrap();
            let (peer, rx) = Peer::new(peer_id, addr, None, task_handle).unwrap();
            peers.insert(peer_id, peer);
            Some(rx)
        } else {
            None
        }
        .unwrap();

        let peer_id = peer_id.unwrap();

        Ok((peer_id, ws_rx))
    }

    pub async fn remove_peer(&self, peer_id: u32) -> Result<()> {
        let mut peers = self.peers.lock().unwrap();

        let peer = match peers.get(&peer_id).map(|peer| peer.to_owned()) {
            Some(peer) => Some(peer),
            _ => None,
        }
        .unwrap();

        match peer.status.lock().map(|id| *id).unwrap() {
            Some(room_id) => {
                println!("\nCleanup for ROOM {}", room_id);
                let mut rooms = self.rooms.lock().unwrap();
                let room = rooms.get_mut(&room_id).unwrap();

                room.retain(|id| id != &peer_id);
                println!("Room cleaned for peer {}", peer_id);

                if (room.len() == 1) & room.contains(&room_id) {
                    // If only the MCU alias remains in the room, remove it
                    println!(
                        "\nLast peer in the room {} left, destroying room...",
                        room_id
                    );

                    rooms.remove(&room_id);
                    println!("Terminating alias..");
                    peers.remove(&room_id);
                    // Debug
                    // println!("\nROOMS: {:?}", rooms);
                    drop(rooms);
                } else {
                    // FIX: We only need to inform the server present in the ROOM
                    room.iter()
                        .map(|other| peers.get(&other).unwrap().to_owned())
                        .for_each(move |peer| {
                            let tx = &peer.tx.lock().unwrap();
                            let msg = format!("ROOM_PEER_LEFT {}", peer_id);
                            tx.unbounded_send(WsMessage::Text(msg.clone().into()))
                                .with_context(|| {
                                    format!("Failed to send message on channel: {}", msg.clone())
                                })
                                .unwrap();
                        });
                    drop(rooms);
                }
            }
            None => (),
        };

        println!("\nPeer {} left", &peer_id);
        peers.remove(&peer_id);
        // Debug
        // println!("PEERS: {:?}", peers);
        drop(peers);

        Ok(())
    }

    pub fn handle_message(&self, message: &str, from_id: u32) -> Result<()> {
        if message.starts_with("ROOM_PEER_MSG") {
            // TODO: Generalize this branch for "ROOM_PEER_..." messages

            // This condition is met under the following assumption:
            // The peer sending the message is already in a ROOM - Assert the following
            // i.e. peer.status != None

            // Action:
            // Forward message addressed to the recipient - if not connected, inform the sending peer

            let mut split = message["ROOM_PEER_MSG ".len()..].splitn(2, ' ');
            let to_id = split
                .next()
                .and_then(|s| str::parse::<u32>(s).ok())
                .ok_or_else(|| anyhow!("Cannot parse PEER ID from message"))
                .unwrap();

            let msg = split
                .next()
                .ok_or_else(|| anyhow!("Cannot parse peer message"))?;

            println!("{} -> {}: {}", from_id, to_id, msg);

            let peers = self.peers.lock().unwrap();
            let rooms = self.rooms.lock().unwrap();

            // Ensure peer.status == None
            let peer = peers.get(&from_id).unwrap().to_owned();
            if let None = *peer.status.lock().unwrap() {
                bail!("Peer {} is not in a ROOM", from_id)
            }

            let (peer, resp) = if let None = peers.get(&to_id) {
                // If recipient peer is not connected, inform the sending peer
                (
                    peers
                        .get(&from_id)
                        .ok_or_else(|| anyhow!("Cannot find peer {}", from_id))?
                        .to_owned(),
                    format!("ERROR peer {} not found", to_id),
                )
            } else {
                // recipient peer is connected

                // Get "room_id" of the sender
                let room_id = peers
                    .get(&from_id)
                    .ok_or_else(|| anyhow!("Cannot find peer {}", from_id))?
                    .status
                    .lock()
                    .unwrap()
                    .unwrap();

                // Inform the sender if recipient is not present
                // else forward the message to the recipient as is
                rooms
                    .get(&room_id)
                    .unwrap()
                    .contains(&to_id)
                    .then(|| ())
                    .map_or_else(
                        || {
                            (
                                peers
                                    .get(&from_id)
                                    .ok_or_else(|| anyhow!("Cannot find peer {}", from_id))
                                    .unwrap()
                                    .to_owned(),
                                format!("ERROR peer {} not present in room {}", to_id, room_id),
                            )
                        },
                        |_| {
                            (
                                peers
                                    .get(&to_id)
                                    .ok_or_else(|| anyhow!("Cannot find peer {}", to_id))
                                    .unwrap()
                                    .to_owned(),
                                format!("ROOM_PEER_MSG {} {}", from_id, msg),
                            )
                        },
                    )
            };
            drop(peers);
            drop(rooms);

            // Access the channel for the returned peer to we can forward them the message
            let tx = &peer.tx.lock().unwrap();
            tx.unbounded_send(WsMessage::Text(resp.clone().into()))
                .with_context(|| format!("failed to send message on channel: {}", resp.clone()))?;
            drop(tx);
        } else if message.starts_with("ROOM_CMD") {
            // This condition is met under the following assumption:
            // The peer sending the message is not in a ROOM - Assert the following
            // i.e. peer.status == None

            // Action:
            // Create the ROOM by amending the server state, let the peer know the ROOM was created

            // TODO:
            // Implement MCU logic for ROOM creation i.e. MCU creates aliases for every room created
            // The alias acts as a peer where peer_id == room_id and is the facilitator of audio/video conference

            let mut msg = message["ROOM_CMD".len()..].split("_").skip(1);
            let mut split = msg
                .next()
                .ok_or_else(|| anyhow!("Cannot split command message"))
                .unwrap()
                .splitn(2, " ");

            let command = split
                .next()
                .ok_or_else(|| anyhow!("Cannot pase command message"))
                .unwrap();
            let room_id = split
                .next()
                .and_then(|s| str::parse::<u32>(s).ok())
                .ok_or_else(|| anyhow!("Cannot parse ROOM ID from message"))
                .unwrap();

            println!("\nGOT COMMAND: {}\nROOM: {}\n", command, room_id);

            let peers = self.peers.lock().unwrap();
            let mut rooms = self.rooms.lock().unwrap();

            // We are now handling the CMD_ROOM 'CREATE' and 'JOIN' cases within the same branch for brevity and conciseness
            // since we will be checking/modifying related state, why not group the code at one place

            // Ensure peer.status == None
            let peer = peers.get(&from_id).unwrap().to_owned();
            if let Some(status) = *peer.status.lock().unwrap() {
                bail!(
                    "Peer {} is giving CMD_ROOM_{} but is already in ROOM {}",
                    from_id,
                    command,
                    status
                );
            }

            // Conditionally create the the response for the peer
            let resp = if command == "CREATE" {
                println!("{}: CREATE ROOM {}", from_id, room_id);

                if let None = rooms.get(&room_id) {
                    // If the room_id is unique, create the ROOM and enter the peer into the ROOM
                    rooms.insert(room_id, vec![from_id]);

                    // Change the peer state
                    let peer = peers.get(&from_id).unwrap().to_owned();
                    *peer.status.lock().unwrap() = Some(room_id);
                    drop(peer);

                    // ROOM created, we're good
                    println!("ROOM {} created", room_id);

                    // TODO: Inform the MCU so it can create an alias with a new connection
                    // and join the created room

                    println!("Informing MCU for alias creation");
                    let mcu = peers.get(&MCU).unwrap().to_owned();
                    let mcu_tx = mcu.tx.lock().unwrap();
                    let mcu_msg = format!("SERVER_ALIAS {}", room_id);
                    mcu_tx
                        .unbounded_send(WsMessage::Text(mcu_msg.clone()))
                        .with_context(|| {
                            format!("failed to send message on channel: {}", mcu_msg.clone())
                        })
                        .unwrap();
                    drop(mcu_tx);

                    format!("ROOM_OK")
                } else {
                    // The given room_id is not unique
                    println!("ROOM {} already exists", room_id);

                    format!("ERROR ROOM {} already exists", room_id)
                }
                // Debug
                // println!("\nPEERS: {:?}\nROOMS:{:?}\n", peers, rooms);
            } else if command == "JOIN" {
                println!("{}: JOIN ROOM {}", from_id, room_id);

                if let Some(_) = rooms.get(&room_id) {
                    // If the room_id exists, enter the peer into the rooms hashmap
                    let room = rooms.get_mut(&room_id).unwrap();
                    room.push(from_id);

                    // Change the peer status
                    let peer = peers.get(&from_id).unwrap().to_owned();
                    *peer.status.lock().unwrap() = Some(room_id);
                    drop(peer);

                    // ROOM joined, we're good
                    println!("ROOM {} exists, joining..", room_id);

                    // Let other peers know who joined
                    // FIX: We only need to inform the server present in the room
                    room.iter()
                        .filter(|id| *id != &from_id)
                        .map(|other| peers.get(&other).unwrap().to_owned())
                        .for_each(move |peer| {
                            let tx = &peer.tx.lock().unwrap();
                            let msg = format!("ROOM_PEER_JOINED {}", from_id);
                            tx.unbounded_send(WsMessage::Text(msg.clone().into()))
                                .with_context(|| {
                                    format!("failed to send message on channel: {}", msg.clone())
                                })
                                .unwrap();
                            drop(tx);
                        });

                    let list = room
                        .iter()
                        .filter(|id| *id != &from_id)
                        .map(|id| id.to_string())
                        .collect::<Vec<String>>()
                        .join(" ");

                    format!("ROOM_OK {}", list)
                } else {
                    // ROOM not present for the given room_id
                    println!("ROOM {} does not exist", room_id);

                    format!("ERROR ROOM {} does not exist", room_id)
                }
                // Debug
                // println!("\nPEERS: {:?}\nROOMS:{:?}\n", peers, rooms);
            } else {
                // CMD received was neither "CREATE" nor "JOIN"
                bail!("Received invalid command message: {}", command)
            };
            drop(peers);
            drop(rooms);

            // Forward the response we created to the peer's channel
            let tx = &peer.tx.lock().unwrap();
            tx.unbounded_send(WsMessage::Text(resp.clone().into()))
                .with_context(|| format!("failed to send message on channel: {}", resp.clone()))?;
            drop(tx);
        } else if message.starts_with("ROOM_PEER_LIST") {
            // TODO: Refactor this bit into the "ROOM_PEER_..." branch
            let peers = self.peers.lock().unwrap();
            let rooms = self.rooms.lock().unwrap();
            let peer = peers.get(&from_id).unwrap().to_owned();
            let room_id = peer.status.lock().unwrap().unwrap();

            let room = rooms.get(&room_id).unwrap();
            let list = room
                .iter()
                .filter(|id| *id != &from_id)
                .map(|id| id.to_string())
                .collect::<Vec<String>>()
                .join(" ");
            drop(peers);
            drop(rooms);

            let resp = format!("ROOM_PEER_LIST {}", list);

            let tx = &peer.tx.lock().unwrap();
            tx.unbounded_send(WsMessage::Text(resp.clone().into()))
                .with_context(|| format!("failed to send message on channel: {}", resp.clone()))?;
            drop(tx);
        } else if message.starts_with("SERVER_INVITE") {
            // TODO: Generalize this branch for "SERVER_..." messages
            // For testing purpose I'm gonna hard-code the MCU identity as being '999'
            // FIX: Follow guidelines as stated in the design document
            // let mut split = message["SERVER_INVITE ".len()..].splitn(2, ' ');
            // let room_id = split
            //     .next()
            //     .and_then(|s| str::parse::<u32>(s).ok())
            //     .unwrap();
            let peers = self.peers.lock().unwrap();
            let mcu = peers.get(&999_u32).unwrap().to_owned();

            let tx = &mcu.tx.lock().unwrap();
            tx.unbounded_send(WsMessage::Text(message.clone().into()))
                .with_context(|| {
                    format!("failed to send message on channel: {}", message.clone())
                })?;
            drop(tx);
        }

        Ok(())
    }
}
