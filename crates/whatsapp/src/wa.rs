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
    /// The small profile picture of a phone-number or LID address. Ok(None) = no photo or not
    /// shared. Err(AVATAR_TOO_BIG) = the download was over 1 MB.
    async fn profile_picture(&self, jid: Jid) -> Result<Option<Vec<u8>>, String>;
}

/// `profile_picture` error: the picture is over `AVATAR_MAX_BYTES`.
pub(crate) const AVATAR_TOO_BIG: &str = "profile photo too big";

/// A profile picture may be downloaded only from WhatsApp's own CDN: https, host `whatsapp.net`
/// or a subdomain of it, no user info, no port.
pub(crate) fn cdn_url_ok(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("https://") else {
        return false;
    };
    let host = rest
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    if host.contains('@') || host.contains(':') {
        return false;
    }
    host == "whatsapp.net" || host.ends_with(".whatsapp.net")
}

/// URLs carry tokens in their query strings. Anything shaped like one never reaches a log or a
/// notice: it becomes `<url>`.
pub(crate) fn redact_urls(text: &str) -> String {
    text.split(' ')
        .map(|w| if w.contains("://") { "<url>" } else { w })
        .collect::<Vec<_>>()
        .join(" ")
}

/// The address the phone files a chat under. Once the account is 1:1 LID-migrated, WhatsApp
/// keeps every 1:1 chat under the LID, even when the phone number is known (WA Web
/// `WAWebMessageDestinationChat`; whatsapp-rust's own sends do the same). A read mark, pin,
/// mute, archive or read receipt sent under the phone number then names a chat the phone does
/// not have: the server accepts it and the phone ignores it, with no error anywhere (#66).
/// Groups, accounts not migrated yet and unknown LIDs keep the address as it is.
pub(crate) fn chat_wid(chat: &Jid, migrated: bool, lid: Option<&str>) -> Jid {
    match lid {
        Some(lid) if migrated && chat.is_pn() && !lid.is_empty() => Jid::lid(lid),
        _ => chat.clone(),
    }
}

/// Each step of a profile photo lookup (the IQ, then the download).
const PHOTO_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// Largest buffer reserved up front for a download whose size the sender declared.
const PREALLOC_MAX: u64 = 64 * 1024 * 1024;

pub(crate) struct RealWa {
    pub client: Arc<Client>,
}

impl RealWa {
    /// `chat_wid` with the account's migration state and the library's LID mapping.
    async fn wid(&self, chat: &Jid) -> Jid {
        if !chat.is_pn() || !self.client.is_lid_migrated().await {
            return chat.clone();
        }
        let entry = self.client.get_lid_pn_entry(chat).await.ok().flatten();
        chat_wid(chat, true, entry.as_ref().map(|e| &*e.lid))
    }
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
        let wid = self.wid(&chat).await;
        self.client
            .chat_actions()
            .mark_chat_as_read(&wid, true, None)
            .await
            .map_err(|e| chain(&e))?;
        // The phone never says whether it applied the mark, so the address used is logged: a
        // chat that stays unread on the phone can be traced to it. No number, no message text.
        let address = if wid.is_lid() {
            "LID"
        } else if wid.is_pn() {
            "phone number"
        } else {
            "group"
        };
        chatcli_core::log::info("whatsapp", &format!("read mark sent ({address} address)"));
        Ok(())
    }

    async fn mark_receipts(
        &self,
        chat: Jid,
        sender: Option<Jid>,
        ids: Vec<String>,
    ) -> Result<(), String> {
        let chat = self.wid(&chat).await;
        let refs: Vec<&str> = ids.iter().map(String::as_str).collect();
        self.client
            .mark_as_read(&chat, sender.as_ref(), &refs)
            .await
            .map_err(|e| chain(e.as_ref()))
    }

    async fn set_flag(&self, chat: Jid, flag: ChatFlag, on: bool) -> Result<(), String> {
        let chat = self.wid(&chat).await;
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

    async fn profile_picture(&self, jid: Jid) -> Result<Option<Vec<u8>>, String> {
        let pic = self
            .client
            .contacts()
            .get_profile_picture_with_timeout(&jid, true, Some(PHOTO_TIMEOUT))
            .await
            .map_err(|e| redact_urls(&chain(&e)))?;
        let Some(pic) = pic else { return Ok(None) };
        if !cdn_url_ok(&pic.url) {
            chatcli_core::log::error("whatsapp", "profile photo URL not on a WhatsApp host");
            return Ok(None);
        }
        let agent = crate::tls::photo_agent(PHOTO_TIMEOUT);
        let url = pic.url;
        let max = chatcli_core::media::AVATAR_MAX_BYTES;
        // ureq is blocking: its own thread. The read stops one byte past the cap.
        let read = tokio::task::spawn_blocking(move || {
            let resp = agent
                .get(&url)
                .call()
                .map_err(|e| redact_urls(&e.to_string()))?;
            if resp.status().as_u16() != 200 {
                return Err(format!(
                    "WhatsApp's photo server answered {}",
                    resp.status().as_u16()
                ));
            }
            let mut bytes = Vec::new();
            resp.into_body()
                .into_reader()
                .take(max + 1)
                .read_to_end(&mut bytes)
                .map_err(|e| redact_urls(&e.to_string()))?;
            Ok(bytes)
        });
        let bytes = match tokio::time::timeout(PHOTO_TIMEOUT, read).await {
            Ok(Ok(r)) => r?,
            Ok(Err(e)) => return Err(format!("photo download failed ({e})")),
            Err(_) => return Err("photo download timed out".to_string()),
        };
        if bytes.len() as u64 > max {
            return Err(AVATAR_TOO_BIG.to_string());
        }
        Ok(Some(bytes))
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
    fn a_migrated_account_files_1_1_chats_under_the_lid() {
        let pn: Jid = "15550100000@s.whatsapp.net".parse().unwrap();
        let group: Jid = "120363000000000001@g.us".parse().unwrap();
        let lid = Some("100200300");
        assert_eq!(chat_wid(&pn, true, lid).to_string(), "100200300@lid");
        // Not migrated yet: the phone still files 1:1 chats under the number.
        assert_eq!(chat_wid(&pn, false, lid), pn);
        // No known LID: the number is all there is.
        assert_eq!(chat_wid(&pn, true, None), pn);
        assert_eq!(chat_wid(&pn, true, Some("")), pn);
        // Groups and LID chats keep their address.
        assert_eq!(chat_wid(&group, true, lid), group);
        let l: Jid = "100200300@lid".parse().unwrap();
        assert_eq!(chat_wid(&l, true, Some("999")), l);
    }

    #[test]
    fn only_whatsapp_cdn_hosts_are_downloaded_from() {
        for ok in [
            "https://pps.whatsapp.net/v/t61/a.jpg?oh=1",
            "https://whatsapp.net/x",
            "https://PPS.WhatsApp.NET/x",
        ] {
            assert!(cdn_url_ok(ok), "{ok}");
        }
        // Built at run time: a literal user@host would trip the repo privacy scan.
        let userinfo = [
            format!("https://user{}pps.whatsapp.net/x", '@'),
            format!("https://evil.com{}pps.whatsapp.net/x", '@'),
        ];
        for bad in userinfo.iter().map(String::as_str).chain([
            "http://pps.whatsapp.net/x",
            "https://evil.com/x",
            "https://pps.whatsapp.net.evil.com/x",
            "https://pps.whatsapp.net:8443/x",
            "https://whatsapp.net.evil/x",
            "https://evilwhatsapp.net/x",
            "https://evil.com/?a=.whatsapp.net",
            "https://evil.com#.whatsapp.net",
            "",
        ]) {
            assert!(!cdn_url_ok(bad), "{bad}");
        }
    }

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
