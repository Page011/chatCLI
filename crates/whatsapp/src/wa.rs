//! The seam between the backend and whatsapp-rust. Every network call the 0.2 features make goes
//! through `Wa`, so the command handling (reply, react, edit, delete, media, older history,
//! typing, members) is tested against a fake with no connection. `RealWa` is the one
//! implementation that talks to the library.

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use chatcli_core::chain;
use chatcli_core::model::{ChatFlag, MediaKind};
use whatsapp_rust::async_trait;
use whatsapp_rust::download::{DownloadParams, MediaType};
use whatsapp_rust::prelude::*;
use whatsapp_rust::{ErrorChainExt, RevokeType};

use crate::rich::{Fetch, Uploaded};

/// `(bytes sent so far, total)` of one upload. Called from a blocking thread, so it must be
/// cheap; the caller throttles what it forwards to the TUI.
pub(crate) type Progress = Arc<dyn Fn(u64, u64) + Send + Sync>;

/// What an upload reads. An image is already re-encoded in memory; a document streams from disk.
pub(crate) enum UploadInput {
    Bytes(Vec<u8>),
    File(PathBuf),
}

pub(crate) enum FetchError {
    /// The CDN refused the reference (401, 403, 404, 410): the media is gone.
    Expired,
    /// More bytes arrived than the size limit allows, whatever the sender declared.
    TooBig,
    Other(String),
}

/// One group participant: the address the group uses, and the phone number when it is hidden.
pub(crate) struct Member {
    pub jid: Jid,
    pub pn: Option<Jid>,
}

#[async_trait]
pub(crate) trait Wa: Send + Sync {
    /// User parts of our own phone-number and LID addresses, phone number first.
    fn own_users(&self) -> Vec<String>;
    async fn send(&self, to: Jid, message: wa::Message) -> Result<String, String>;
    async fn react(&self, chat: Jid, key: wa::MessageKey, emoji: String) -> Result<(), String>;
    async fn edit(&self, chat: Jid, id: String, message: wa::Message) -> Result<(), String>;
    async fn revoke(&self, chat: Jid, id: String) -> Result<(), String>;
    async fn upload(
        &self,
        input: UploadInput,
        kind: MediaKind,
        progress: Progress,
    ) -> Result<Uploaded, String>;
    /// Downloads and decrypts one file. Fails with `TooBig` as soon as more than `limit` bytes
    /// have been produced: the sender's declared size is not trusted.
    async fn download(&self, fetch: Fetch, limit: u64) -> Result<Vec<u8>, FetchError>;
    /// Asks the phone for older messages; returns the request id the answer will carry.
    async fn request_history(
        &self,
        chat: Jid,
        oldest_id: String,
        oldest_from_me: bool,
        oldest_ts_ms: i64,
    ) -> Result<String, String>;
    async fn typing(&self, chat: Jid, composing: bool) -> Result<(), String>;
    async fn members(&self, chat: Jid) -> Result<Vec<Member>, String>;
    /// Tells the phone (and the other linked devices) the chat is read. No read receipts.
    async fn mark_chat_read(&self, chat: Jid) -> Result<(), String>;
    /// Read receipts: tells the sender "seen". `sender` names the author in a group.
    async fn mark_receipts(
        &self,
        chat: Jid,
        sender: Option<Jid>,
        ids: Vec<String>,
    ) -> Result<(), String>;
    /// Pin, mute (for good) or archive a chat, or undo it.
    async fn set_flag(&self, chat: Jid, flag: ChatFlag, on: bool) -> Result<(), String>;
}

/// URLs carry tokens in their query strings. Anything shaped like one never reaches a log or a
/// notice: it becomes `<url>`.
pub(crate) fn redact_urls(text: &str) -> String {
    text.split(' ')
        .map(|w| if w.contains("://") { "<url>" } else { w })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Largest buffer reserved up front for a download whose size the sender declared.
const PREALLOC_MAX: u64 = 64 * 1024 * 1024;

pub(crate) struct RealWa {
    pub client: Arc<Client>,
}

#[async_trait]
impl Wa for RealWa {
    fn own_users(&self) -> Vec<String> {
        [self.client.pn(), self.client.lid()]
            .into_iter()
            .flatten()
            .map(|j| j.user.to_string())
            .collect()
    }

    async fn send(&self, to: Jid, message: wa::Message) -> Result<String, String> {
        self.client
            .send_message(to, message)
            .await
            .map(|r| r.message_id)
            .map_err(|e| redact_urls(&chain(&e)))
    }

    async fn react(&self, chat: Jid, key: wa::MessageKey, emoji: String) -> Result<(), String> {
        self.client
            .send_reaction(chat, key, &emoji)
            .await
            .map(|_| ())
            .map_err(|e| redact_urls(&chain(&e)))
    }

    async fn edit(&self, chat: Jid, id: String, message: wa::Message) -> Result<(), String> {
        self.client
            .edit_message(chat, id, message)
            .await
            .map(|_| ())
            .map_err(|e| redact_urls(&chain(&e)))
    }

    async fn revoke(&self, chat: Jid, id: String) -> Result<(), String> {
        self.client
            .revoke_message(chat, id, RevokeType::Sender)
            .await
            .map_err(|e| redact_urls(&chain(&e)))
    }

    async fn upload(
        &self,
        input: UploadInput,
        kind: MediaKind,
        progress: Progress,
    ) -> Result<Uploaded, String> {
        let media_type = match kind {
            MediaKind::Image => MediaType::Image,
            MediaKind::Video => MediaType::Video,
            MediaKind::Audio => MediaType::Audio,
            MediaKind::Document | MediaKind::Other => MediaType::Document,
        };
        // Encrypting reads the whole file: off the async threads. The ciphertext is held in memory
        // while it uploads (ponytail: at most the 100 MiB send limit; a temp file would lift it).
        let (cipher, info) = tokio::task::spawn_blocking(move || {
            let mut out = Vec::new();
            let info = match input {
                UploadInput::Bytes(b) => whatsapp_rust::wacore::upload::encrypt_media_streaming(
                    &b[..],
                    &mut out,
                    media_type,
                ),
                UploadInput::File(path) => {
                    let file = std::fs::File::open(&path).map_err(|e| {
                        format!("Could not read the file ({e}): check you can open it.")
                    })?;
                    whatsapp_rust::wacore::upload::encrypt_media_streaming(
                        std::io::BufReader::new(file),
                        &mut out,
                        media_type,
                    )
                }
            }
            .map_err(|e| {
                format!(
                    "Could not prepare the file for upload ({}).",
                    chain(e.as_ref())
                )
            })?;
            Ok::<_, String>((out, info))
        })
        .await
        .map_err(|e| format!("Preparing the file failed ({e})."))??;

        let source = CountingSource {
            data: whatsapp_rust::bytes::Bytes::from(cipher),
            progress,
        };
        let r = self
            .client
            .upload_stream(source, info, media_type)
            .await
            .map_err(|e| redact_urls(&chain(e.as_ref())))?;
        Ok(Uploaded {
            url: r.url,
            direct_path: r.direct_path,
            media_key: r.media_key,
            file_enc_sha256: r.file_enc_sha256,
            file_sha256: r.file_sha256,
            file_length: r.file_length,
            media_key_timestamp: r.media_key_timestamp,
        })
    }

    async fn download(&self, f: Fetch, limit: u64) -> Result<Vec<u8>, FetchError> {
        let media_type = match f.kind {
            MediaKind::Image => MediaType::Image,
            MediaKind::Video => MediaType::Video,
            MediaKind::Audio => MediaType::Audio,
            MediaKind::Document | MediaKind::Other => MediaType::Document,
        };
        let params = DownloadParams {
            direct_path: f.direct_path,
            media_key: Some(f.media_key),
            file_sha256: f.sha256,
            file_enc_sha256: (!f.enc_sha256.is_empty()).then_some(f.enc_sha256),
            file_length: f.length,
            media_type,
        };
        // The library streams and decrypts in constant memory, but into a seekable sink. The sink
        // stops at `limit`, so memory is bounded even when the sender declared a small size.
        let hit = Arc::new(AtomicBool::new(false));
        let reserve = f.length.min(limit).min(PREALLOC_MAX) as usize;
        let sink = LimitedSink {
            inner: std::io::Cursor::new(Vec::with_capacity(reserve)),
            limit,
            hit: hit.clone(),
        };
        match self.client.download_to_writer(&params, sink).await {
            Ok(done) => Ok(done.inner.into_inner()),
            Err(_) if hit.load(Ordering::SeqCst) => Err(FetchError::TooBig),
            Err(e) => {
                let cause: &(dyn std::error::Error + 'static) = e.as_ref();
                match cause.http_status() {
                    Some(401 | 403 | 404 | 410) => Err(FetchError::Expired),
                    Some(n) => Err(FetchError::Other(format!(
                        "WhatsApp's media server answered {n}"
                    ))),
                    None => Err(FetchError::Other(redact_urls(&chain(cause)))),
                }
            }
        }
    }

    async fn request_history(
        &self,
        chat: Jid,
        oldest_id: String,
        oldest_from_me: bool,
        oldest_ts_ms: i64,
    ) -> Result<String, String> {
        self.client
            .fetch_message_history(&chat, &oldest_id, oldest_from_me, oldest_ts_ms, 50)
            .await
            .map_err(|e| redact_urls(&chain(e.as_ref())))
    }

    async fn typing(&self, chat: Jid, composing: bool) -> Result<(), String> {
        let state = self.client.chatstate();
        let sent = if composing {
            state.send_composing(&chat).await
        } else {
            state.send_paused(&chat).await
        };
        sent.map_err(|e| chain(&e))
    }

    async fn mark_chat_read(&self, chat: Jid) -> Result<(), String> {
        self.client
            .chat_actions()
            .mark_chat_as_read(&chat, true, None)
            .await
            .map_err(|e| chain(&e))
    }

    async fn mark_receipts(
        &self,
        chat: Jid,
        sender: Option<Jid>,
        ids: Vec<String>,
    ) -> Result<(), String> {
        let refs: Vec<&str> = ids.iter().map(String::as_str).collect();
        self.client
            .mark_as_read(&chat, sender.as_ref(), &refs)
            .await
            .map_err(|e| chain(e.as_ref()))
    }

    async fn set_flag(&self, chat: Jid, flag: ChatFlag, on: bool) -> Result<(), String> {
        let actions = self.client.chat_actions();
        match (flag, on) {
            (ChatFlag::Pinned, true) => actions.pin_chat(&chat).await,
            (ChatFlag::Pinned, false) => actions.unpin_chat(&chat).await,
            (ChatFlag::Muted, true) => actions.mute_chat(&chat).await,
            (ChatFlag::Muted, false) => actions.unmute_chat(&chat).await,
            (ChatFlag::Hidden, true) => actions.archive_chat(&chat, None).await,
            (ChatFlag::Hidden, false) => actions.unarchive_chat(&chat, None).await,
        }
        .map_err(|e| chain(&e))
    }

    async fn members(&self, chat: Jid) -> Result<Vec<Member>, String> {
        let meta = self
            .client
            .groups()
            .get_metadata(&chat)
            .await
            .map_err(|e| chain(&e))?;
        Ok(meta
            .participants
            .iter()
            .map(|p| Member {
                jid: p.jid.to_non_ad(),
                pn: p.phone_number.as_ref().map(|j| j.to_non_ad()),
            })
            .collect())
    }
}

/// An in-memory download sink that refuses to grow past `limit` bytes and records that it did.
pub(crate) struct LimitedSink {
    pub inner: std::io::Cursor<Vec<u8>>,
    pub limit: u64,
    pub hit: Arc<AtomicBool>,
}

impl Write for LimitedSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.inner.position().saturating_add(buf.len() as u64) > self.limit {
            self.hit.store(true, Ordering::SeqCst);
            return Err(std::io::Error::other("the file is over the size limit"));
        }
        self.inner.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

impl Seek for LimitedSink {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        self.inner.seek(pos)
    }
}

impl whatsapp_rust::wacore::download::DownloadWriter for LimitedSink {
    fn truncate(&mut self, len: u64) -> std::io::Result<()> {
        self.inner.get_mut().truncate(len as usize);
        Ok(())
    }
}

/// The encrypted bytes to upload, with a read counter for the progress line. Each attempt
/// starts a new reader at the offset the upload resumes from.
struct CountingSource {
    data: whatsapp_rust::bytes::Bytes,
    progress: Progress,
}

impl whatsapp_rust::wacore::upload::UploadSource for CountingSource {
    fn len(&self) -> u64 {
        self.data.len() as u64
    }

    fn reader_from(&self, offset: u64) -> std::io::Result<Box<dyn Read + Send>> {
        let mut cursor = std::io::Cursor::new(self.data.clone());
        let offset = offset.min(self.data.len() as u64);
        cursor.set_position(offset);
        Ok(Box::new(CountingReader {
            inner: cursor,
            total: self.data.len() as u64,
            progress: self.progress.clone(),
        }))
    }
}

struct CountingReader {
    inner: std::io::Cursor<whatsapp_rust::bytes::Bytes>,
    total: u64,
    progress: Progress,
}

impl Read for CountingReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        (self.progress)(self.inner.position(), self.total);
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64;

    #[test]
    fn urls_never_survive_into_a_message() {
        assert_eq!(
            redact_urls("download failed for https://mmg.example.invalid/x?token=abc now"),
            "download failed for <url> now"
        );
        assert_eq!(redact_urls("no links here"), "no links here");
    }

    #[test]
    fn the_download_sink_stops_at_the_limit_whatever_the_sender_declared() {
        let hit = Arc::new(AtomicBool::new(false));
        let mut sink = LimitedSink {
            inner: std::io::Cursor::new(Vec::new()),
            limit: 10,
            hit: hit.clone(),
        };
        sink.write_all(&[1u8; 10]).unwrap();
        assert!(!hit.load(Ordering::SeqCst), "exactly the limit is fine");
        assert!(sink.write_all(&[1u8]).is_err());
        assert!(hit.load(Ordering::SeqCst));
        // A rewind and rewrite inside the limit still works (the library retries on a new host).
        sink.seek(SeekFrom::Start(0)).unwrap();
        assert!(sink.write_all(&[2u8; 10]).is_ok());
        assert_eq!(sink.inner.get_ref().len(), 10);
    }

    #[test]
    fn the_counting_reader_reports_where_the_upload_is() {
        use whatsapp_rust::wacore::upload::UploadSource;
        let seen = Arc::new(AtomicU64::new(0));
        let s2 = seen.clone();
        let source = CountingSource {
            data: whatsapp_rust::bytes::Bytes::from(vec![1u8; 100]),
            progress: Arc::new(move |done, total| {
                assert_eq!(total, 100);
                s2.store(done, Ordering::SeqCst);
            }),
        };
        assert_eq!(source.len(), 100);
        let mut r = source.reader_from(40).unwrap();
        let mut buf = [0u8; 30];
        assert_eq!(r.read(&mut buf).unwrap(), 30);
        assert_eq!(seen.load(Ordering::SeqCst), 70, "resumes at the offset");
        let mut rest = Vec::new();
        r.read_to_end(&mut rest).unwrap();
        assert_eq!(seen.load(Ordering::SeqCst), 100);
        assert_eq!(rest.len(), 30);
        // An offset past the end is clamped, not a panic.
        assert!(source.reader_from(1_000).is_ok());
    }
}
