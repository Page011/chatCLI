//! Offline: stopping a session must release whatsapp.db so Logout can delete it. Uses a
//! throwaway local port as the "WhatsApp server" and a pinned client version, so nothing leaves
//! the machine.
//!
//! Known limit (seen in the live QR check): while a QR is being shown, the library's pairing
//! timers keep the store open for up to a minute after a stop. The backend retries the delete
//! and remembers a leftover file (see `remove_db_files` and the pending-delete flag).

use std::time::Duration;

use whatsapp_rust::prelude::*;
use whatsapp_rust::transport::TokioWebSocketTransportFactory;

async fn released(db: &std::path::Path) -> bool {
    for _ in 0..100 {
        if std::fs::remove_file(db).is_ok() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

async fn build(dir: &std::path::Path) -> (Bot, std::path::PathBuf) {
    std::fs::create_dir_all(dir).unwrap();
    let db = dir.join("whatsapp.db");
    let backend = SqliteStore::new(&db.to_string_lossy()).await.unwrap();
    let bot = Bot::builder()
        .with_backend(backend)
        .with_transport_factory(TokioWebSocketTransportFactory::new().with_url("ws://127.0.0.1:9"))
        .with_version((2, 3000, 1))
        .build()
        .await
        .unwrap();
    (bot, db)
}

#[tokio::test]
async fn dropping_an_unstarted_bot_releases_the_file() {
    let dir = std::env::temp_dir().join(format!("chatcli-release-a-{}", std::process::id()));
    let (bot, db) = build(&dir).await;
    drop(bot);
    assert!(released(&db).await);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn shutting_down_a_connecting_session_releases_the_file() {
    let dir = std::env::temp_dir().join(format!("chatcli-release-b-{}", std::process::id()));
    let (bot, db) = build(&dir).await;
    let client = bot.client();
    let handle = bot.spawn();
    tokio::time::sleep(Duration::from_millis(1500)).await;
    handle.shutdown().await;
    drop(client);
    assert!(released(&db).await);
    let _ = std::fs::remove_dir_all(&dir);
}
