pub mod data;

use axum::{
    Json, Router,
    extract::State,
    response::{Html, Sse, sse::Event},
    routing::{get, post},
};
use futures::stream::{self, Stream};
use serde::{Deserialize, Serialize};
use std::{convert::Infallible, net::SocketAddr, sync::Arc, time::Duration};
use tokio::sync::{RwLock, broadcast};
use tokio_stream::StreamExt as _;

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Content {
    content: String,
}

#[derive(Clone)]
struct AppState {
    content: Arc<RwLock<String>>,
    update_tx: Arc<broadcast::Sender<String>>,
}

#[tokio::main]
async fn main() {
    let (update_tx, _) = broadcast::channel(100);

    let state = AppState {
        content: Arc::new(RwLock::new(String::new())),
        update_tx: Arc::new(update_tx),
    };

    let app = Router::new()
        .route("/", get(index_handler))
        .route("/events", get(sse_handler))
        .route("/update", post(update_handler))
        .with_state(state);

    let addr = SocketAddr::from(([0, 0, 0, 0], 3000));
    println!("Server running on http://{}", addr);

    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

async fn index_handler() -> Html<&'static str> {
    Html(include_str!("../static/index.html"))
}

async fn sse_handler(
    State(state): State<AppState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let mut rx = state.update_tx.subscribe();

    let current_content = state.content.read().await.clone();
    let initial_event = stream::once(async move {
        Ok(Event::default()
            .event("update")
            .json_data(Content {
                content: current_content,
            })
            .unwrap())
    });

    let update_stream = async_stream::stream! {
        while let Ok(content) = rx.recv().await {
            yield Ok(Event::default()
                .event("update")
                .json_data(Content { content })
                .unwrap());
        }
    };

    let stream = initial_event.chain(update_stream);

    Sse::new(stream).keep_alive(
        axum::response::sse::KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("keep-alive"),
    )
}

async fn update_handler(State(state): State<AppState>, Json(payload): Json<Content>) -> Json<()> {
    let mut content = state.content.write().await;
    *content = payload.content.clone();

    let _ = state.update_tx.send(payload.content);

    Json(())
}
