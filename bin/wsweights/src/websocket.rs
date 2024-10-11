use futures_util::{StreamExt, SinkExt};
use tokio::net::TcpListener;
use tokio::sync::RwLock;
use tokio_stream::wrappers::TcpListenerStream;
use tokio_tungstenite::{accept_async, tungstenite::protocol::Message};
use std::sync::Arc;
use std::error::Error;

pub type ClientList = Arc<RwLock<Vec<tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>>>>;

pub async fn start_websocket_server(clients: ClientList) -> Result<(), Box<dyn Error>> {
    let addr = "127.0.0.1:9001";
    let listener = TcpListener::bind(addr).await?;
    let stream = TcpListenerStream::new(listener);

    stream
        .for_each(|stream| {
            let clients = clients.clone();
            async move {
                match stream {
                    Ok(stream) => {
                        if let Ok(ws_stream) = accept_async(stream).await {
                            tokio::spawn(handle_ws(ws_stream, clients));
                        }
                    }
                    Err(e) => eprintln!("Error accepting connection: {}", e),
                }
            }
        })
        .await;

    Ok(())
}

async fn handle_ws(
    mut websocket: tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>, 
    clients: ClientList,
) -> Result<(), Box<dyn Error>> {
    {
        let mut clients = clients.write().await;
        clients.push(websocket.clone());
    }

    while let Some(msg) = websocket.next().await {
        match msg {
            Ok(msg) if msg.is_text() => {
                let response = Message::Text("Received".into());
                websocket.send(response).await?;
            }
            Ok(_) => continue,
            Err(e) => {
                eprintln!("WebSocket error: {}", e);
                break;
            }
        }
    }

    // Remove disconnected clients
    {
        let mut clients = clients.write().await;
        clients.retain(|client| !client.is_terminated());
    }

    Ok(())
}

