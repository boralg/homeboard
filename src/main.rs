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
    display_name: String,
    stored_name: String,
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
    hidden_text: Arc<RwLock<Option<String>>>,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();

    if std::path::Path::new(SHARED_DIR).exists() {
        let has_subdirs = std::fs::read_dir(SHARED_DIR).ok().map_or(false, |entries| {
            entries
                .flatten()
                .any(|e| e.file_type().map_or(false, |t| t.is_dir()))
        });

        if !has_subdirs {
            std::fs::remove_dir_all(SHARED_DIR).expect("Failed to clean shared files directory");
        } else {
            eprintln!(
                "Warning: {} contains subdirectories, skipping cleanup",
                SHARED_DIR
            );
        }
    }

    std::fs::create_dir_all(SHARED_DIR).expect("Failed to create shared files directory");

    let (update_tx, _) = broadcast::channel(100);

    let state = AppState {
        text_history: Arc::new(RwLock::new(VecDeque::new())),
        file_history: Arc::new(RwLock::new(VecDeque::new())),
        text_history_limit: args.text_history_length,
        file_history_limit: args.file_history_length,
        update_tx: Arc::new(update_tx),
        hidden_text: Arc::new(RwLock::new(None)),
    };

    let app = Router::new()
        .route("/", get(index_handler))
        .route("/events", get(sse_handler))
        .route("/update", post(update_handler))
        .route("/upload", post(upload_handler))
        .route("/files/{filename}", get(file_handler))
        .route("/hidden", post(set_hidden_handler))
        .route("/hidden", get(get_hidden_handler))
        .layer(axum::extract::DefaultBodyLimit::disable())
        .with_state(state);

    let addr = SocketAddr::from(([0, 0, 0, 0], 3000));

    let local_ip = local_ip_address::local_ip()
        .unwrap_or_else(|_| std::net::IpAddr::V4(std::net::Ipv4Addr::new(127, 0, 0, 1)));

    println!("Text history limit: {}", args.text_history_length);
    println!("File history limit: {}", args.file_history_length);
    println!("Shared files directory: {}", SHARED_DIR);
    println!("Server running on http://{}:3000", local_ip);

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
        if let Some(display_name) = field.file_name() {
            let display_name = display_name.to_string();
            let stored_name = uuid::Uuid::new_v4().to_string();
            let filepath = Path::new(SHARED_DIR).join(&stored_name);

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
                display_name,
                stored_name,
                timestamp,
            };

            let mut file_history = state.file_history.write().await;
            file_history.push_front(file_info);
            if file_history.len() > state.file_history_limit {
                if let Some(removed) = file_history.pop_back() {
                    let removed_path = Path::new(SHARED_DIR).join(&removed.stored_name);
                    let _ = tokio::fs::remove_file(removed_path).await;
                }
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
    axum::extract::Path(stored_name): axum::extract::Path<String>,
    State(state): State<AppState>,
) -> Result<impl axum::response::IntoResponse, StatusCode> {
    use axum::body::Body;
    use tokio_util::io::ReaderStream;

    let file_history = state.file_history.read().await;
    let file_info = file_history
        .iter()
        .find(|f| f.stored_name == stored_name)
        .ok_or(StatusCode::NOT_FOUND)?;

    let display_name = file_info.display_name.clone();
    let filepath = Path::new(SHARED_DIR).join(&stored_name);

    let file = tokio::fs::File::open(&filepath)
        .await
        .map_err(|_| StatusCode::NOT_FOUND)?;
    let stream = ReaderStream::new(file);
    let body = Body::from_stream(stream);

    Ok((
        [(
            axum::http::header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{}\"", display_name),
        )],
        body,
    ))
}

async fn set_hidden_handler(State(state): State<AppState>, Json(payload): Json<Content>) {
    let mut hidden_text = state.hidden_text.write().await;
    *hidden_text = Some(payload.content);
}

async fn get_hidden_handler(State(state): State<AppState>) -> Json<Content> {
    let hidden_text = state.hidden_text.read().await;
    Json(Content {
        content: hidden_text.clone().unwrap_or_default(),
    })
}
