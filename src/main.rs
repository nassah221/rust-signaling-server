use std::net::SocketAddr;

use tokio::net::{TcpListener, TcpStream};

use futures::channel::mpsc::UnboundedReceiver;
use futures::sink::SinkExt;
use futures::stream::StreamExt;

use tokio::sync::oneshot::{self, Receiver};
use tokio::task::JoinHandle;
use tokio_tungstenite::{tungstenite, WebSocketStream};
use tungstenite::Message as WsMessage;

use anyhow::{anyhow, Result};

use server::Server;

type Socket = WebSocketStream<TcpStream>;

async fn message_loop(
    server: &Server,
    socket: Socket,
    peer_id: u32,
    ws_rx: UnboundedReceiver<WsMessage>,
) -> Result<()> {
    let (mut ws_sink, ws_stream) = socket.split();

    let mut ws_rx = ws_rx.fuse();

    let mut ws_stream = ws_stream.fuse();
    loop {
        let ws_msg: Option<WsMessage> = futures::select! {
            ws_msg = ws_stream.select_next_some() => {
                match ws_msg? {
                    WsMessage::Text(text) => {
                        // println!("\nMessage Received: {}\n", &text);
                        server.handle_message(&text, peer_id)?;
                        None
                    },
                    _ => None
                }
            },
            ws_msg = ws_rx.select_next_some() => {
                Some(ws_msg)
            },
            complete => break
        };
        if let Some(ws_msg) = ws_msg {
            ws_sink.send(ws_msg).await?;
        }
    }

    Ok(())
}

async fn start_client(
    server: &Server,
    mut socket: Socket,
    handle: Receiver<JoinHandle<()>>,
    addr: SocketAddr,
) {
    // Register the incoming connection with a Peer_ID
    println!("\nWebsocket connection established: {}", &addr);

    let task_handle = match handle.await {
        Ok(task_handle) => task_handle,
        Err(_) => return,
    };

    let msg = socket
        .next()
        .await
        .ok_or_else(|| anyhow!("Did not receive Hello"))
        .unwrap()
        .unwrap();

    let (peer_id, ws_rx) = server.register_peer(msg, addr, task_handle).unwrap();

    // Let the peer know they're registered
    socket
        .send(WsMessage::Text("Hello".to_string()))
        .await
        .unwrap();
    println!("Peer {} registered\n", &peer_id);

    if let Err(_) = message_loop(&server, socket, peer_id, ws_rx).await {
        server.remove_peer(peer_id).await.unwrap();
    }
}

#[tokio::main]
async fn main() {
    let addr = "127.0.0.1:8765".to_string();

    let listener = TcpListener::bind(&addr).await.unwrap();
    println!("\nListening on: {}", addr);

    let server = Server::new().unwrap();

    while let Ok((stream, addr)) = listener.accept().await {
        let server = server.clone();
        let socket = tokio_tungstenite::accept_async(stream).await.unwrap();

        let (my_send, my_recv) = oneshot::channel();

        let kill = tokio::spawn(async move {
            start_client(&server, socket, my_recv, addr).await;
        });

        let _ = my_send.send(kill);
    }
}
