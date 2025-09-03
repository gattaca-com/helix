use axum::http::Uri;
use futures::SinkExt;
use tokio_tungstenite::{connect_async, tungstenite::Message};

#[tokio::main]
async fn main() {
    let uri: Uri = "ws://127.0.0.1:4040/relay/v1/p2p".parse().unwrap();
    let (mut ws, _response) = connect_async(uri).await.unwrap();
    loop {
        ws.send(Message::text("asfasfas")).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
}
