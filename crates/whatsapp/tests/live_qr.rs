//! LIVE check, ignored by default. Starts the backend on an empty temp session folder, asks to
//! link, waits for the first QR, prints only its length, and shuts down. It never scans, never
//! links, never loops, and never prints the QR (the code pairs a phone with the session).
//!
//! Run by hand. If SSL_CERT_FILE is set, unset it for this run:
//!   env -u SSL_CERT_FILE cargo test -p chatcli-whatsapp --test live_qr -- --ignored --nocapture

use std::time::Duration;

use std::sync::Arc;

use chatcli_core::media::MediaCache;
use chatcli_core::model::{Command, Event, Status};
use chatcli_core::runtime::BackendCtx;
use chatcli_core::settings::Settings;
use chatcli_core::store::Store;
use tokio::sync::mpsc;

#[tokio::test]
#[ignore = "talks to WhatsApp: run by hand"]
async fn first_qr_arrives_with_an_empty_session() {
    let dir = std::env::temp_dir().join(format!("chatcli-whatsapp-live-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::env::set_var("CHATCLI_DATA_DIR", &dir);

    let store = Store::open(&dir.join("store.db")).unwrap();
    let (etx, mut erx) = mpsc::unbounded_channel();
    let (cmd_tx, crx) = mpsc::unbounded_channel();
    let settings = Settings::default();
    let media = MediaCache::open(&dir.join("media"), &settings.media).unwrap();
    let ctx = BackendCtx {
        store,
        settings: Arc::new(settings),
        media,
    };
    let task = tokio::spawn(chatcli_whatsapp::run(ctx, crx, etx));
    cmd_tx.send(Command::Login).unwrap();

    let mut seen: Vec<String> = Vec::new();
    let waited = tokio::time::timeout(Duration::from_secs(60), async {
        while let Some(e) = erx.recv().await {
            match e {
                Event::Status(_, Status::Qr { code }) => return Some(code.len()),
                Event::Status(_, Status::Error { message }) => {
                    seen.push(format!("Error: {message}"))
                }
                Event::Status(_, s) => seen.push(format!("{s:?}")),
                Event::Notice { text, .. } => seen.push(format!("Notice: {text}")),
                _ => {}
            }
        }
        None
    })
    .await;

    cmd_tx.send(Command::Shutdown).unwrap();
    let stopped = tokio::time::timeout(Duration::from_secs(15), task)
        .await
        .is_ok();

    match waited {
        Ok(Some(len)) => println!("QR received (len {len})"),
        _ => {
            let log = std::fs::read_to_string(dir.join("chatcli.log")).unwrap_or_default();
            let tail: Vec<&str> = log.lines().rev().take(8).collect();
            panic!("no QR within 60 s. statuses: {seen:?}. log tail: {tail:?}");
        }
    }
    assert!(stopped, "backend did not shut down within 15 s");

    // Informational: Logout deletes the session file, which on Windows only works once the
    // library has let go. During a QR wait it does not (a library limit, see session_release.rs),
    // so this prints the fact instead of failing the check.
    let db = dir.join("whatsapp.db");
    let started = std::time::Instant::now();
    let mut released = false;
    while started.elapsed() < Duration::from_secs(5) {
        if std::fs::remove_file(&db).is_ok() {
            released = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    println!(
        "session file released after {} ms: {released}",
        started.elapsed().as_millis()
    );
    let _ = std::fs::remove_dir_all(&dir);
}
