pub mod data;

use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    response::{Html, Sse, sse::Event},
    routing::{get, post},
};
use clap::Parser;
use futures::stream::{self, Stream};
use serde::{Deserialize, Serialize};
use std::{
    collections::VecDeque, convert::Infallible, net::SocketAddr, path::Path, sync::Arc,
    time::Duration,
};
use tokio::io::AsyncWriteExt;
use tokio::sync::{RwLock, broadcast};
use tokio_stream::StreamExt as _;

const SHARED_DIR: &str = "hb_shared_files";

#[derive(Parser)]
struct Args {
    #[arg(short, long, default_value_t = 20)]
    text_history_length: usize,
    #[arg(short, long, default_value_t = 20)]
    file_history_length: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Content {
    content: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct FileInfo {
    filename: String,
    timestamp: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct HistoryData {
    text_items: Vec<String>,
    file_items: Vec<FileInfo>,
}

#[derive(Clone)]
struct AppState {
    text_history: Arc<RwLock<VecDeque<String>>>,
    file_history: Arc<RwLock<VecDeque<FileInfo>>>,
    text_history_limit: usize,
    file_history_limit: usize,
    update_tx: Arc<broadcast::Sender<HistoryData>>,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();

    std::fs::create_dir_all(SHARED_DIR).expect("Failed to create shared files directory");

    let (update_tx, _) = broadcast::channel(100);

    let state = AppState {
        text_history: Arc::new(RwLock::new(VecDeque::new())),
        file_history: Arc::new(RwLock::new(VecDeque::new())),
        text_history_limit: args.text_history_length,
        file_history_limit: args.file_history_length,
        update_tx: Arc::new(update_tx),
    };

    let app = Router::new()
        .route("/", get(index_handler))
        .route("/events", get(sse_handler))
        .route("/update", post(update_handler))
        .route("/upload", post(upload_handler))
        .route("/files/{filename}", get(file_handler))
        .layer(axum::extract::DefaultBodyLimit::disable())
        .with_state(state);

    let addr = SocketAddr::from(([0, 0, 0, 0], 3000));
    println!("Server running on http://{}", addr);
    println!("Text history limit: {}", args.text_history_length);
    println!("File history limit: {}", args.file_history_length);
    println!("Shared files directory: {}", SHARED_DIR);

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

    let text_items = state.text_history.read().await.iter().cloned().collect();
    let file_items = state.file_history.read().await.iter().cloned().collect();
    let initial_event = stream::once(async move {
        Ok(Event::default()
            .event("update")
            .json_data(HistoryData {
                text_items,
                file_items,
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
    let mut text_history = state.text_history.write().await;

    if !payload.content.is_empty() {
        text_history.push_front(payload.content);
        if text_history.len() > state.text_history_limit {
            text_history.pop_back();
        }
    }

    let text_items: Vec<String> = text_history.iter().cloned().collect();

    let file_items = state.file_history.read().await.iter().cloned().collect();
    let _ = state.update_tx.send(HistoryData {
        text_items,
        file_items,
    });
}

async fn upload_handler(
    State(state): State<AppState>,
    mut multipart: axum::extract::Multipart,
) -> Result<(), StatusCode> {
    while let Ok(Some(mut field)) = multipart.next_field().await {
        if let Some(filename) = field.file_name() {
            let filename = filename.to_string();
            let filepath = Path::new(SHARED_DIR).join(&filename);

            let mut file = tokio::fs::File::create(&filepath).await.map_err(|e| {
                eprintln!("Failed to create file: {}", e);
                StatusCode::INTERNAL_SERVER_ERROR
            })?;

            while let Ok(Some(chunk)) = field.chunk().await {
                file.write_all(&chunk).await.map_err(|e| {
                    eprintln!("Failed to write chunk: {}", e);
                    StatusCode::INTERNAL_SERVER_ERROR
                })?;
            }

            let timestamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs();

            let file_info = FileInfo {
                filename,
                timestamp,
            };

            let mut file_history = state.file_history.write().await;
            file_history.push_front(file_info);
            if file_history.len() > state.file_history_limit {
                file_history.pop_back();
            }

            let file_items = file_history.iter().cloned().collect();

            let text_items = state.text_history.read().await.iter().cloned().collect();
            let _ = state.update_tx.send(HistoryData {
                text_items,
                file_items,
            });
        }
    }

    Ok(())
}

async fn file_handler(
    axum::extract::Path(filename): axum::extract::Path<String>,
) -> Result<impl axum::response::IntoResponse, StatusCode> {
    use axum::body::Body;
    use tokio_util::io::ReaderStream;

    let filepath = Path::new(SHARED_DIR).join(&filename);
    let file = tokio::fs::File::open(&filepath)
        .await
        .map_err(|_| StatusCode::NOT_FOUND)?;
    let stream = ReaderStream::new(file);
    let body = Body::from_stream(stream);

    Ok((
        [(
            axum::http::header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{}\"", filename),
        )],
        body,
    ))
}
