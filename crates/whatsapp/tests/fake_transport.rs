//! Offline: how the whatsapp-rust 0.7 client behaves on a transport that misbehaves. All the
//! fakes live in this one file because the `Transport` traits belong to a 0.x crate and may
//! change: when they do, only this file breaks.
//!
//! Scope (honest limit, issue #51): these tests drive the library `Bot` directly, the same way
//! `session_release.rs` does. They prove the library backs off and stops cleanly, which is what
//! chatcli's `run` relies on. They do NOT assert the chips the TUI shows (`CONNECTING`,
//! `NOT LIVE`), because `chatcli_whatsapp::run` builds its own transport and has no seam to take
//! a factory. That needs a small `#[cfg(test)]` argument in `src/backend.rs` (see
//! `docs/testing.md`, "What this does not prove").
//!
//! The fakes cannot finish the Noise handshake, so linking, QR and message flow stay with the
//! hand-built events in `src/backend_tests.rs`. Nothing here touches the network or the real
//! data folder: the session file is a throwaway under the temp dir and the client version is
//! pinned so no version request is made.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use whatsapp_rust::prelude::*;
use whatsapp_rust::transport::{DisconnectReason, Transport, TransportEvent, TransportFactory};

type Made = Result<(Arc<dyn Transport>, async_channel::Receiver<TransportEvent>), anyhow::Error>;

/// What the fake server does when the client connects.
#[derive(Clone, Copy)]
enum Mode {
    /// The connect itself fails (no network, DNS, refused).
    Refuse,
    /// Connects, then the socket closes at once.
    DropAtOnce,
    /// Connects and accepts bytes but never answers (the handshake stalls).
    Stall,
    /// Connects, then closes after this long.
    CloseAfter(Duration),
}

struct FakeTransport;

#[async_trait]
impl Transport for FakeTransport {
    async fn send(&self, _data: Bytes) -> Result<(), anyhow::Error> {
        Ok(())
    }
    async fn disconnect(&self) {}
}

/// Counts every connect attempt, so a test can tell backoff from a fast reconnect loop.
#[derive(Clone)]
struct FakeFactory {
    mode: Mode,
    attempts: Arc<AtomicU32>,
}

#[async_trait]
impl TransportFactory for FakeFactory {
    async fn create_transport(&self) -> Made {
        self.attempts.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = async_channel::unbounded();
        match self.mode {
            Mode::Refuse => return Err(anyhow::anyhow!("connection refused (fake)")),
            Mode::DropAtOnce => {
                let _ = tx.send(TransportEvent::Connected).await;
                let _ = tx
                    .send(TransportEvent::Disconnected(DisconnectReason::StreamEnded))
                    .await;
            }
            Mode::Stall => {
                // Keep the sender alive for the life of the connection: the stream stays open
                // and silent, which is what a stuck server looks like.
                let _ = tx.send(TransportEvent::Connected).await;
                std::mem::forget(tx);
            }
            Mode::CloseAfter(after) => {
                let _ = tx.send(TransportEvent::Connected).await;
                tokio::spawn(async move {
                    tokio::time::sleep(after).await;
                    let _ = tx
                        .send(TransportEvent::Disconnected(
                            DisconnectReason::ServerClose {
                                code: Some(1011),
                                reason: "fake".into(),
                            },
                        ))
                        .await;
                });
            }
        }
        Ok((Arc::new(FakeTransport), rx))
    }
}

struct Session {
    attempts: Arc<AtomicU32>,
    handle: whatsapp_rust::bot::BotHandle,
    dir: std::path::PathBuf,
}

async fn start(tag: &str, mode: Mode) -> Session {
    let dir = std::env::temp_dir().join(format!(
        "chatcli-faketransport-{tag}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("whatsapp.db");
    let backend = SqliteStore::new(&db.to_string_lossy()).await.unwrap();
    let attempts = Arc::new(AtomicU32::new(0));
    let bot = Bot::builder()
        .with_backend(backend)
        .with_transport_factory(FakeFactory {
            mode,
            attempts: attempts.clone(),
        })
        .with_version((2, 3000, 1))
        .build()
        .await
        .unwrap();
    Session {
        attempts,
        handle: bot.spawn(),
        dir,
    }
}

impl Session {
    fn attempts(&self) -> u32 {
        self.attempts.load(Ordering::SeqCst)
    }

    /// Stops the session and proves it stopped: no connect attempt after `shutdown` returns.
    /// `limit` is the longest the library may take (virtual time). A stalled handshake keeps
    /// shutdown waiting for the library's own 20 s handshake deadline, which is why chatcli's
    /// `stop_session` wraps `shutdown` in its own `STOP_TIMEOUT` instead of trusting it.
    async fn stop_and_check_quiet(self, limit: Duration) {
        tokio::time::timeout(limit, self.handle.shutdown())
            .await
            .expect("shutdown did not finish in time");
        let after_stop = self.attempts.load(Ordering::SeqCst);
        tokio::time::sleep(Duration::from_secs(120)).await;
        assert_eq!(
            self.attempts.load(Ordering::SeqCst),
            after_stop,
            "a stopped session connected again"
        );
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Upper bound for 60 s of retrying: the library's Fibonacci backoff (1, 1, 2, 3, 5, 8, 13 ...)
/// gives about 8; 15 leaves room for jitter. A fast loop would show hundreds.
const MAX_ATTEMPTS_IN_60S: u32 = 15;
/// Shutdown of a session that is not mid-handshake.
const QUICK: Duration = Duration::from_secs(5);

#[tokio::test(start_paused = true)]
async fn a_refused_connect_retries_with_backoff_not_in_a_fast_loop() {
    let s = start("refuse", Mode::Refuse).await;
    tokio::time::sleep(Duration::from_secs(60)).await;
    let n = s.attempts();
    assert!(n >= 2, "no retry after a refused connect ({n} attempts)");
    assert!(
        n <= MAX_ATTEMPTS_IN_60S,
        "reconnect loop: {n} attempts in 60 s"
    );
    s.stop_and_check_quiet(QUICK).await;
}

#[tokio::test(start_paused = true)]
async fn a_socket_that_drops_at_once_does_not_reconnect_in_a_fast_loop() {
    let s = start("drop", Mode::DropAtOnce).await;
    tokio::time::sleep(Duration::from_secs(60)).await;
    let n = s.attempts();
    assert!(n >= 2, "no retry after a dropped socket ({n} attempts)");
    assert!(
        n <= MAX_ATTEMPTS_IN_60S,
        "reconnect loop: {n} attempts in 60 s"
    );
    s.stop_and_check_quiet(QUICK).await;
}

#[tokio::test(start_paused = true)]
async fn a_server_that_never_answers_is_one_attempt_until_the_library_gives_up() {
    let s = start("stall", Mode::Stall).await;
    // Well inside the library's own handshake deadline: still the first attempt, no pile-up.
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert_eq!(
        s.attempts(),
        1,
        "a stuck connect must not start a second one"
    );
    // Shutdown works even though the connect never finishes, once the library's 20 s deadline passes.
    s.stop_and_check_quiet(Duration::from_secs(25)).await;
}

#[tokio::test(start_paused = true)]
async fn a_socket_closed_after_a_few_seconds_also_backs_off_and_shutdown_stays_clean() {
    let s = start("after", Mode::CloseAfter(Duration::from_secs(3))).await;
    tokio::time::sleep(Duration::from_secs(60)).await;
    assert!(
        s.attempts() <= MAX_ATTEMPTS_IN_60S,
        "{} attempts",
        s.attempts()
    );
    s.stop_and_check_quiet(QUICK).await;
}
