pub mod data;

use axum::{
    Json, Router,
    extract::State,
    response::{Html, Sse, sse::Event},
    routing::{get, post},
};
use clap::Parser;
use futures::stream::{self, Stream};
use serde::{Deserialize, Serialize};
use std::{collections::VecDeque, convert::Infallible, net::SocketAddr, sync::Arc, time::Duration};
use tokio::sync::{RwLock, broadcast};
use tokio_stream::StreamExt as _;

#[derive(Parser)]
struct Args {
    #[arg(short = 't', long, default_value_t = 20)]
    history: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Content {
    content: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct History {
    items: Vec<String>,
}

#[derive(Clone)]
struct AppState {
    history: Arc<RwLock<VecDeque<String>>>,
    history_limit: usize,
    update_tx: Arc<broadcast::Sender<History>>,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    let (update_tx, _) = broadcast::channel(100);

    let state = AppState {
        history: Arc::new(RwLock::new(VecDeque::new())),
        history_limit: args.history,
        update_tx: Arc::new(update_tx),
    };

    let app = Router::new()
        .route("/", get(index_handler))
        .route("/events", get(sse_handler))
        .route("/update", post(update_handler))
        .with_state(state);

    let addr = SocketAddr::from(([0, 0, 0, 0], 3000));
    println!("Server running on http://{}", addr);
    println!("History limit: {}", args.history);

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

    let current_history = state.history.read().await.iter().cloned().collect();
    let initial_event = stream::once(async move {
        Ok(Event::default()
            .event("update")
            .json_data(History {
                items: current_history,
            })
            .unwrap())
    });

    let update_stream = async_stream::stream! {
        while let Ok(history) = rx.recv().await {
            yield Ok(Event::default()
                .event("update")
                .json_data(history)
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

async fn update_handler(State(state): State<AppState>, Json(payload): Json<Content>) {
    let mut history = state.history.write().await;

    if !payload.content.is_empty() {
        history.push_front(payload.content);
        if history.len() > state.history_limit {
            history.pop_back();
        }
    }

    let history_vec: Vec<String> = history.iter().cloned().collect();
    let _ = state.update_tx.send(History { items: history_vec });
}
