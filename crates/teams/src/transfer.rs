//! Moving media between Graph and the local machine: fetching an attachment into the media
//! cache, and sending a picture or a file. Each function does one user request; the backend
//! runs them on spawned tasks so a slow transfer never blocks its command loop.
//!
//! Nothing here logs file names, links or ids: the callers log kinds, sizes and status codes.

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use chatcli_core::limits::send_limit_bytes;
use chatcli_core::media::{self, MediaCache};
use chatcli_core::model::{Attachment, MediaKind, Network};

use crate::body;
use crate::graph::{ApiError, ApiResult, DriveItem, Graph, Message};
use crate::map::FetchSpec;

/// Fixed texts the TUI shows as they are (contract section 2).
pub const FILE_OPEN_IN_BROWSER: &str = "FILE: open in browser";
pub const MEDIA_EXPIRED: &str = "MEDIA EXPIRED";
pub const NO_ACCESS_TO_FILE: &str = "NO ACCESS TO FILE";
pub const FILE_SEND_NEEDS_SCOPE: &str = "FILE SEND NEEDS Files.ReadWrite";
pub const FILE_NOT_FOUND: &str = "FILE NOT FOUND";

const MIB: u64 = 1024 * 1024;
/// Graph's simple upload takes small files in one request; larger ones use an upload session.
pub const SIMPLE_UPLOAD_MAX: u64 = 4 * MIB;
/// An upload session chunk must be a multiple of 320 KiB.
pub const UPLOAD_CHUNK: usize = 320 * 1024 * 32;
/// Inline pictures arrive in these formats; the cache name needs one of them to look a file up
/// before its type is known.
const HOSTED_EXTENSIONS: [&str; 7] = ["png", "jpg", "gif", "webp", "bmp", "heic", "bin"];

/// Why a fetch failed, and where the browser could still open the file.
#[derive(Debug, PartialEq, Eq)]
pub struct FetchFailure {
    pub reason: String,
    pub fallback_url: Option<String>,
}

fn failure(reason: impl Into<String>, fallback_url: Option<&str>) -> FetchFailure {
    FetchFailure {
        reason: reason.into(),
        fallback_url: fallback_url.map(str::to_string),
    }
}

/// Fetches one attachment into the cache. `files_allowed`: Files.Read.All is granted and
/// `[teams] files_read` is on; without it a linked file is never fetched, the browser opens it.
/// An inline picture needs nothing beyond Chat.ReadWrite. An already cached file is returned
/// without a request.
pub async fn fetch_media(
    graph: &Graph,
    cache: &MediaCache,
    att: &Attachment,
    spec: &FetchSpec,
    files_allowed: bool,
) -> Result<PathBuf, FetchFailure> {
    match spec {
        FetchSpec::Hosted {
            chat_id,
            message_id,
            hosted_id,
        } => {
            for ext in HOSTED_EXTENSIONS {
                if let Some(path) = cache.lookup(Network::Teams, &att.fetch_ref, ext) {
                    return Ok(path);
                }
            }
            let download = graph
                .hosted_content(chat_id, message_id, hosted_id)
                .await
                .map_err(|e| match e.status {
                    404 | 410 => failure(MEDIA_EXPIRED, None),
                    _ => failure(e.message, None),
                })?;
            cache
                .check_size(download.content_length)
                .map_err(|e| failure(e, None))?;
            let ext = media::safe_extension(None, download.content_type.as_deref());
            let mut part = cache
                .begin(Network::Teams, &att.fetch_ref, &ext)
                .map_err(|e| failure(e, None))?;
            download
                .copy_to(&mut part)
                .await
                .map_err(|e| failure(e, None))?;
            cache.commit(part).map_err(|e| failure(e, None))
        }
        FetchSpec::File { url } => {
            let open_url = att.open_url.as_deref().or(Some(url.as_str()));
            if !files_allowed {
                return Err(failure(FILE_OPEN_IN_BROWSER, open_url));
            }
            // The name alone picks the extension, so a second request finds the same file.
            let ext = media::safe_extension(att.name.as_deref(), None);
            if let Some(path) = cache.lookup(Network::Teams, &att.fetch_ref, &ext) {
                return Ok(path);
            }
            cache
                .check_size(att.size)
                .map_err(|e| failure(e, open_url))?;
            let download = graph.shared_file(url).await.map_err(|e| match e.status {
                // Never retried: the user cannot read it, or it is gone.
                403 => failure(NO_ACCESS_TO_FILE, open_url),
                404 | 410 => failure(
                    "FILE NOT AVAILABLE: the sender deleted or moved it.",
                    open_url,
                ),
                _ => failure(e.message, open_url),
            })?;
            cache
                .check_size(download.content_length)
                .map_err(|e| failure(e, open_url))?;
            let mut part = cache
                .begin(Network::Teams, &att.fetch_ref, &ext)
                .map_err(|e| failure(e, open_url))?;
            download
                .copy_to(&mut part)
                .await
                .map_err(|e| failure(e, open_url))?;
            cache.commit(part).map_err(|e| failure(e, open_url))
        }
    }
}

fn local(message: impl Into<String>) -> ApiError {
    ApiError {
        status: 0,
        message: message.into(),
    }
}

/// Re-encodes the picture (which drops EXIF), refuses one over 4 MiB before any request, then
/// posts it inline as hosted content.
pub async fn send_image(
    graph: &Graph,
    chat_id: &str,
    path: &Path,
    caption: &str,
) -> ApiResult<Message> {
    let (p, limit) = (
        path.to_path_buf(),
        send_limit_bytes(Network::Teams, MediaKind::Image),
    );
    let prepared = tokio::task::spawn_blocking(move || media::prepare_image(&p, limit))
        .await
        .map_err(|e| local(format!("Could not read the picture: {e}")))?
        .map_err(local)?;
    let message = body::image_message_json(caption, prepared.mime, &prepared.bytes);
    graph.post_message(chat_id, &message).await
}

fn read_chunk(path: &Path, start: u64, len: usize) -> std::io::Result<Vec<u8>> {
    let mut file = std::fs::File::open(path)?;
    file.seek(SeekFrom::Start(start))?;
    let mut buf = vec![0u8; len];
    file.read_exact(&mut buf)?;
    Ok(buf)
}

async fn read_chunk_async(path: &Path, start: u64, len: usize) -> ApiResult<Vec<u8>> {
    let p = path.to_path_buf();
    tokio::task::spawn_blocking(move || read_chunk(&p, start, len))
        .await
        .map_err(|e| local(format!("Could not read the file: {e}")))?
        .map_err(|e| {
            local(format!(
                "Could not read the file ({e}): check you can open it."
            ))
        })
}

/// Uploads the file to the user's OneDrive ("Microsoft Teams Chat Files"; an existing file of
/// that name is never replaced), then posts a message with a `reference` attachment to it.
/// The size is checked first: over 100 MB nothing is sent. `progress(done, total)` is called as
/// bytes go out. `chunk` is `UPLOAD_CHUNK` (a parameter so tests can use small chunks).
pub async fn send_file(
    graph: &Graph,
    chat_id: &str,
    path: &Path,
    caption: &str,
    chunk: usize,
    progress: &mut (dyn FnMut(u64, u64) + Send),
) -> ApiResult<Message> {
    let meta = std::fs::metadata(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            local(FILE_NOT_FOUND)
        } else {
            local(format!(
                "Could not read the file ({e}): check you can open it."
            ))
        }
    })?;
    if !meta.is_file() {
        return Err(local("That is not a file: pick a file, not a folder."));
    }
    let size = meta.len();
    let limit = send_limit_bytes(Network::Teams, MediaKind::Document);
    if size > limit {
        return Err(local(format!(
            "FILE OVER {} MB: Teams does not allow a larger chat file.",
            limit / MIB
        )));
    }
    if size == 0 {
        return Err(local("That file is empty: pick another."));
    }
    let name = body::drive_name(
        &path
            .file_name()
            .map_or_else(|| "file".to_string(), |n| n.to_string_lossy().into_owned()),
    );
    progress(0, size);
    let item = if size <= SIMPLE_UPLOAD_MAX {
        let bytes = read_chunk_async(path, 0, size as usize).await?;
        let item = graph.upload_small(&name, bytes).await?;
        progress(size, size);
        item
    } else {
        upload_session(graph, path, &name, size, chunk, progress).await?
    };
    let item = with_link_fields(graph, item).await?;
    let attachment_id = item
        .e_tag
        .as_deref()
        .and_then(body::etag_guid)
        .unwrap_or_else(body::random_guid);
    let link = item
        .web_dav_url
        .as_deref()
        .or(item.web_url.as_deref())
        .ok_or_else(|| local("OneDrive did not give a link for the uploaded file."))?;
    let shown = item.name.as_deref().unwrap_or(&name);
    let message = body::file_message_json(caption, &attachment_id, link, shown);
    graph.post_message(chat_id, &message).await
}

async fn upload_session(
    graph: &Graph,
    path: &Path,
    name: &str,
    size: u64,
    chunk: usize,
    progress: &mut (dyn FnMut(u64, u64) + Send),
) -> ApiResult<DriveItem> {
    let url = graph.create_upload_session(name).await?;
    let (mut start, mut finished) = (0u64, None);
    while start < size {
        let len = (size - start).min(chunk as u64) as usize;
        let bytes = read_chunk_async(path, start, len).await?;
        if let Some(item) = graph.upload_chunk(&url, start, size, bytes).await? {
            finished = Some(item);
        }
        start += len as u64;
        progress(start, size);
    }
    finished
        .ok_or_else(|| local("OneDrive did not confirm the upload: try sending the file again."))
}

/// The upload answer may lack the eTag or the WebDAV link (it is returned only when selected):
/// ask for them by id.
async fn with_link_fields(graph: &Graph, item: DriveItem) -> ApiResult<DriveItem> {
    if item.e_tag.is_some() && item.web_dav_url.is_some() {
        return Ok(item);
    }
    match item.id.as_deref() {
        Some(id) => graph.drive_item(id).await,
        None => Ok(item),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::map::FetchSpec;
    use crate::testkit::{Mock, Reply};
    use chatcli_core::model::FetchRef;
    use chatcli_core::settings::MediaSettings;
    use serde_json::json;
    use std::sync::atomic::{AtomicU32, Ordering};

    static N: AtomicU32 = AtomicU32::new(0);

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "chatcli-teams-transfer-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn cache(tag: &str, max_file_mb: u64) -> (MediaCache, PathBuf) {
        let dir = temp_dir(tag);
        let settings = MediaSettings {
            max_file_mb,
            ..MediaSettings::default()
        };
        (
            MediaCache::open(&dir.join("media"), &settings).unwrap(),
            dir,
        )
    }

    fn graph_for(mock: &Mock) -> Graph {
        Graph::new(reqwest::Client::new(), "tok".into()).with_base(&mock.graph_base())
    }

    fn hosted_attachment() -> (Attachment, FetchSpec) {
        let spec = FetchSpec::Hosted {
            chat_id: "19:a".into(),
            message_id: "m1".into(),
            hosted_id: "aWQ9MQ".into(),
        };
        (
            Attachment {
                kind: MediaKind::Image,
                name: None,
                mime: None,
                size: None,
                fetch_ref: spec.to_ref(),
                open_url: None,
            },
            spec,
        )
    }

    fn file_attachment(name: &str) -> (Attachment, FetchSpec) {
        let url = "https://contoso.example.invalid/personal/alex/Documents/a.pdf".to_string();
        let spec = FetchSpec::File { url: url.clone() };
        (
            Attachment {
                kind: MediaKind::Document,
                name: Some(name.into()),
                mime: None,
                size: None,
                fetch_ref: spec.to_ref(),
                open_url: Some(url),
            },
            spec,
        )
    }

    #[tokio::test]
    async fn a_hosted_picture_lands_in_the_cache_and_is_not_fetched_twice() {
        let mock = Mock::start(|_| Reply::bytes(200, "image/png", vec![7; 100]));
        let (cache, dir) = cache("hosted", 25);
        let (att, spec) = hosted_attachment();
        let path = fetch_media(&graph_for(&mock), &cache, &att, &spec, false)
            .await
            .unwrap();
        assert!(path.starts_with(cache.root()));
        assert_eq!(path.extension().and_then(|e| e.to_str()), Some("png"));
        assert_eq!(std::fs::read(&path).unwrap().len(), 100);
        // The cache file name is a hash: nothing of the ids or a sender's file name is in it.
        let file = path.file_name().unwrap().to_string_lossy().into_owned();
        assert!(!file.contains("aWQ9MQ") && !file.contains("m1"), "{file}");
        assert_eq!(mock.count(), 1);
        let again = fetch_media(&graph_for(&mock), &cache, &att, &spec, false)
            .await
            .unwrap();
        assert_eq!(again, path);
        assert_eq!(mock.count(), 1, "second fetch came from the cache");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn a_hosted_picture_gone_from_teams_says_media_expired() {
        let mock = Mock::start(|_| Reply::error(404, "NotFound", "gone"));
        let (cache, dir) = cache("expired", 25);
        let (att, spec) = hosted_attachment();
        let f = fetch_media(&graph_for(&mock), &cache, &att, &spec, false)
            .await
            .unwrap_err();
        assert_eq!(
            f,
            FetchFailure {
                reason: "MEDIA EXPIRED".into(),
                fallback_url: None
            }
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn a_picture_over_the_file_limit_is_refused_before_a_byte_is_written() {
        let mock = Mock::start(|_| Reply::bytes(200, "image/png", vec![0; 2 * 1024 * 1024]));
        let (cache, dir) = cache("toobig", 1);
        let (att, spec) = hosted_attachment();
        let f = fetch_media(&graph_for(&mock), &cache, &att, &spec, false)
            .await
            .unwrap_err();
        assert!(f.reason.starts_with("FILE OVER 1 MB"), "{}", f.reason);
        let leftovers: Vec<_> = std::fs::read_dir(cache.root().join("teams"))
            .unwrap()
            .collect();
        assert!(leftovers.is_empty(), "no .part file is left behind");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn a_linked_file_without_the_scope_opens_in_the_browser_and_makes_no_request() {
        let mock = Mock::start(|_| Reply::empty(500));
        let (cache, dir) = cache("nofiles", 25);
        let (att, spec) = file_attachment("a.pdf");
        let f = fetch_media(&graph_for(&mock), &cache, &att, &spec, false)
            .await
            .unwrap_err();
        assert_eq!(f.reason, "FILE: open in browser");
        assert_eq!(
            f.fallback_url.as_deref(),
            Some("https://contoso.example.invalid/personal/alex/Documents/a.pdf")
        );
        assert_eq!(mock.count(), 0);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn a_linked_file_downloads_through_shares_with_the_scope() {
        let mock = Mock::start(|_| Reply::bytes(200, "application/pdf", b"%PDF-1.7".to_vec()));
        let (cache, dir) = cache("files", 25);
        let (att, spec) = file_attachment("Quarterly Report.pdf");
        let path = fetch_media(&graph_for(&mock), &cache, &att, &spec, true)
            .await
            .unwrap();
        assert_eq!(path.extension().and_then(|e| e.to_str()), Some("pdf"));
        assert_eq!(std::fs::read(&path).unwrap(), b"%PDF-1.7");
        let req = &mock.requests()[0];
        assert!(
            req.path().starts_with("/v1.0/shares/u!") && req.path().ends_with("/driveItem/content")
        );
        // Second time: cache.
        fetch_media(&graph_for(&mock), &cache, &att, &spec, true)
            .await
            .unwrap();
        assert_eq!(mock.count(), 1);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn an_executable_name_is_cached_as_bin_never_as_an_executable() {
        let mock = Mock::start(|_| Reply::bytes(200, "application/octet-stream", b"MZ".to_vec()));
        let (cache, dir) = cache("exe", 25);
        let (att, spec) = file_attachment("setup.exe");
        let path = fetch_media(&graph_for(&mock), &cache, &att, &spec, true)
            .await
            .unwrap();
        assert_eq!(path.extension().and_then(|e| e.to_str()), Some("bin"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn linked_file_errors_say_what_happened_and_are_never_retried() {
        for (status, want) in [
            (403u16, "NO ACCESS TO FILE"),
            (404, "FILE NOT AVAILABLE: the sender deleted or moved it."),
        ] {
            let mock = Mock::start(move |_| Reply::error(status, "x", "y"));
            let (cache, dir) = cache("errs", 25);
            let (att, spec) = file_attachment("a.pdf");
            let f = fetch_media(&graph_for(&mock), &cache, &att, &spec, true)
                .await
                .unwrap_err();
            assert_eq!(f.reason, want);
            assert!(f.fallback_url.is_some(), "the browser stays an option");
            assert_eq!(mock.count(), 1, "one request, no retry loop");
            let _ = std::fs::remove_dir_all(dir);
        }
        let mock = Mock::start(|_| Reply::error(500, "x", "boom"));
        let (cache, dir) = cache("errs500", 25);
        let (att, spec) = file_attachment("a.pdf");
        let f = fetch_media(&graph_for(&mock), &cache, &att, &spec, true)
            .await
            .unwrap_err();
        assert!(f.reason.contains("boom"), "{}", f.reason);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Cached files are keyed by the ref: an unrelated ref never finds them.
    #[tokio::test]
    async fn the_cache_is_keyed_by_the_fetch_ref() {
        let mock = Mock::start(|_| Reply::bytes(200, "image/png", vec![1]));
        let (cache, dir) = cache("keyed", 25);
        let (mut att, spec) = hosted_attachment();
        fetch_media(&graph_for(&mock), &cache, &att, &spec, false)
            .await
            .unwrap();
        att.fetch_ref = FetchRef::new("other");
        assert!(cache
            .lookup(Network::Teams, &att.fetch_ref, "png")
            .is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    fn write_png(path: &Path, w: u32, h: u32, noisy: bool) {
        let mut seed = 12345u32;
        let img = image::RgbImage::from_fn(w, h, |x, y| {
            if noisy {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                image::Rgb([(seed >> 24) as u8, (seed >> 16) as u8, (seed >> 8) as u8])
            } else {
                image::Rgb([(x % 256) as u8, (y % 256) as u8, 7])
            }
        });
        img.save(path).unwrap();
    }

    #[tokio::test]
    async fn a_png_goes_out_as_hosted_content_without_its_metadata() {
        let mock = Mock::start(|_| {
            Reply::json(
                201,
                json!({ "id": "9", "createdDateTime": "2026-11-16T01:00:00Z" }),
            )
        });
        let dir = temp_dir("png");
        let file = dir.join("photo.png");
        write_png(&file, 16, 16, false);
        let sent = send_image(&graph_for(&mock), "19:a", &file, "look")
            .await
            .unwrap();
        assert_eq!(sent.id, "9");
        let r = &mock.requests()[0];
        assert_eq!(r.path(), "/v1.0/chats/19:a/messages");
        let j = r.json();
        assert_eq!(
            j["body"]["content"],
            "look<br><img src=\"../hostedContents/1/$value\">"
        );
        assert_eq!(j["hostedContents"][0]["contentType"], "image/png");
        assert_eq!(j["hostedContents"][0]["@microsoft.graph.temporaryId"], "1");
        assert!(
            j["hostedContents"][0]["contentBytes"]
                .as_str()
                .unwrap()
                .len()
                > 10
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// IMAGE OVER 4 MB, and the refusal happens before any network call.
    #[tokio::test]
    async fn a_picture_over_4_mb_is_refused_before_any_request() {
        let mock = Mock::start(|_| Reply::empty(500));
        let dir = temp_dir("bigpng");
        let file = dir.join("noise.png");
        write_png(&file, 1500, 1000, true);
        assert!(std::fs::metadata(&file).unwrap().len() > 4 * MIB);
        let err = send_image(&graph_for(&mock), "19:a", &file, "")
            .await
            .unwrap_err();
        assert!(
            err.message.starts_with("IMAGE OVER 4 MB"),
            "{}",
            err.message
        );
        assert_eq!(mock.count(), 0);
        let missing = send_image(&graph_for(&mock), "19:a", &dir.join("none.png"), "")
            .await
            .unwrap_err();
        assert_eq!(missing.message, "FILE NOT FOUND");
        let text = dir.join("notes.txt");
        std::fs::write(&text, "hello").unwrap();
        let not_pic = send_image(&graph_for(&mock), "19:a", &text, "")
            .await
            .unwrap_err();
        assert!(not_pic.message.contains("not a PNG"), "{}", not_pic.message);
        assert_eq!(mock.count(), 0);
        let _ = std::fs::remove_dir_all(dir);
    }

    fn drive_mock() -> Mock {
        Mock::start(|r| {
            let p = r.path();
            if r.method == "PUT" && p.ends_with(":/content") {
                Reply::json(
                    201,
                    json!({
                        "id": "item1", "name": "a b.txt",
                        "eTag": "\"{11111111-1111-1111-1111-111111111111},1\"",
                        "webDavUrl": "https://contoso.example.invalid/personal/alex/Documents/Microsoft%20Teams%20Chat%20Files/a%20b.txt"
                    }),
                )
            } else if p.ends_with("createUploadSession") {
                Reply::json(
                    200,
                    json!({ "uploadUrl": "https://upload.example.invalid/s" }),
                )
            } else if p.contains("/me/drive/items/") {
                Reply::json(
                    200,
                    json!({
                        "id": "item1", "name": "big.bin",
                        "eTag": "\"{00000000-0000-0000-0000-000000000002},2\"",
                        "webDavUrl": "https://contoso.example.invalid/personal/alex/Documents/Microsoft%20Teams%20Chat%20Files/big.bin"
                    }),
                )
            } else if p.ends_with("/messages") {
                Reply::json(
                    201,
                    json!({ "id": "msg1", "createdDateTime": "2026-11-16T01:00:00Z" }),
                )
            } else {
                Reply::empty(500)
            }
        })
    }

    #[tokio::test]
    async fn a_small_file_uploads_then_posts_a_reference_attachment() {
        let mock = drive_mock();
        let dir = temp_dir("small");
        let file = dir.join("a b.txt");
        std::fs::write(&file, b"hello").unwrap();
        let mut steps = Vec::new();
        let sent = send_file(
            &graph_for(&mock),
            "19:a",
            &file,
            "here",
            UPLOAD_CHUNK,
            &mut |d, t| steps.push((d, t)),
        )
        .await
        .unwrap();
        assert_eq!(sent.id, "msg1");
        assert_eq!(steps, [(0, 5), (5, 5)]);
        let reqs = mock.requests();
        assert_eq!(reqs[0].method, "PUT");
        assert_eq!(reqs[0].body, b"hello");
        let post = reqs.last().unwrap().json();
        assert_eq!(
            post["attachments"][0]["id"],
            "11111111-1111-1111-1111-111111111111"
        );
        assert_eq!(post["attachments"][0]["contentType"], "reference");
        assert_eq!(post["attachments"][0]["name"], "a b.txt");
        assert!(post["attachments"][0]["contentUrl"]
            .as_str()
            .unwrap()
            .ends_with("a%20b.txt"));
        assert_eq!(
            post["body"]["content"],
            "here<br><attachment id=\"11111111-1111-1111-1111-111111111111\"></attachment>"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Over 4 MiB: an upload session, every byte sent once in range-stamped chunks, progress
    /// after each chunk, and the pre-authorized address never sees the bearer token.
    #[tokio::test]
    async fn a_file_over_4_mib_goes_through_an_upload_session_in_chunks() {
        let total = SIMPLE_UPLOAD_MAX + 1;
        let last_range = format!("-{}/{total}", total - 1);
        let session = Mock::start(move |r| {
            if r.header("content-range")
                .unwrap_or("")
                .ends_with(&last_range)
            {
                Reply::json(200, json!({ "id": "item1" }))
            } else {
                Reply::json(202, json!({ "nextExpectedRanges": [] }))
            }
        });
        let upload_url = format!("{}/s", session.origin());
        let graph_mock = Mock::start(move |r| {
            let p = r.path();
            if p.ends_with("createUploadSession") {
                Reply::json(200, json!({ "uploadUrl": upload_url }))
            } else if p.contains("/me/drive/items/") {
                Reply::json(
                    200,
                    json!({
                        "id": "item1", "name": "big.bin",
                        "eTag": "\"{00000000-0000-0000-0000-000000000002},2\"",
                        "webDavUrl": "https://contoso.example.invalid/big.bin"
                    }),
                )
            } else if p.ends_with("/messages") {
                Reply::json(
                    201,
                    json!({ "id": "msg1", "createdDateTime": "2026-11-16T01:00:00Z" }),
                )
            } else {
                Reply::empty(500)
            }
        });
        let dir = temp_dir("big");
        let file = dir.join("big.bin");
        std::fs::write(&file, vec![9u8; total as usize]).unwrap();
        let chunk = 320 * 1024 * 3;
        let mut progress = Vec::new();
        let sent = send_file(
            &graph_for(&graph_mock),
            "19:a",
            &file,
            "",
            chunk,
            &mut |d, t| progress.push((d, t)),
        )
        .await
        .unwrap();
        assert_eq!(sent.id, "msg1");
        let chunks = session.requests();
        let bytes: u64 = chunks.iter().map(|r| r.body.len() as u64).sum();
        assert_eq!(bytes, total, "every byte exactly once");
        assert_eq!(chunks.len() as u64, total.div_ceil(chunk as u64));
        assert_eq!(
            chunks[0].header("content-range"),
            Some(format!("bytes 0-{}/{total}", chunk - 1).as_str())
        );
        assert!(
            chunks.iter().all(|r| r.header("authorization").is_none()),
            "no token to the session address"
        );
        assert_eq!(progress.first(), Some(&(0, total)));
        assert_eq!(progress.last(), Some(&(total, total)));
        assert!(
            progress.windows(2).all(|w| w[0].0 < w[1].0),
            "progress only moves forward"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn a_session_without_a_confirmation_is_an_error_not_a_silent_success() {
        let session = Mock::start(|_| Reply::json(202, json!({})));
        let upload_url = format!("{}/s", session.origin());
        let graph_mock = Mock::start(move |_| Reply::json(200, json!({ "uploadUrl": upload_url })));
        let dir = temp_dir("noconfirm");
        let file = dir.join("big.bin");
        std::fs::write(&file, vec![1u8; (SIMPLE_UPLOAD_MAX + 1) as usize]).unwrap();
        let err = send_file(
            &graph_for(&graph_mock),
            "c",
            &file,
            "",
            UPLOAD_CHUNK,
            &mut |_, _| {},
        )
        .await
        .unwrap_err();
        assert!(err.message.contains("did not confirm"), "{}", err.message);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn the_upload_answer_without_a_link_is_completed_by_id() {
        let dir = temp_dir("complete");
        let file = dir.join("c.txt");
        std::fs::write(&file, b"x").unwrap();
        // A PUT answer with no eTag or WebDAV link: send_file asks for them by id.
        let bare = Mock::start(|r| {
            if r.method == "PUT" {
                Reply::json(201, json!({ "id": "item1", "name": "c.txt" }))
            } else if r.path().contains("/me/drive/items/") {
                Reply::json(
                    200,
                    json!({
                        "id": "item1", "name": "c.txt", "eTag": "\"{00000000-0000-0000-0000-000000000003},1\"",
                        "webDavUrl": "https://contoso.example.invalid/c.txt"
                    }),
                )
            } else {
                Reply::json(
                    201,
                    json!({ "id": "msg", "createdDateTime": "2026-11-16T01:00:00Z" }),
                )
            }
        });
        send_file(
            &graph_for(&bare),
            "19:a",
            &file,
            "",
            UPLOAD_CHUNK,
            &mut |_, _| {},
        )
        .await
        .unwrap();
        let item_get = bare
            .requests()
            .into_iter()
            .find(|r| r.path().contains("/me/drive/items/"))
            .unwrap();
        assert_eq!(
            item_get.param("$select").as_deref(),
            Some("id,name,eTag,webUrl,webDavUrl")
        );
        let post = bare.requests().pop().unwrap().json();
        assert_eq!(
            post["attachments"][0]["id"],
            "00000000-0000-0000-0000-000000000003"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn file_checks_run_before_any_request() {
        let mock = Mock::start(|_| Reply::empty(500));
        let dir = temp_dir("checks");
        let g = graph_for(&mock);
        let nofile = send_file(
            &g,
            "c",
            &dir.join("none.txt"),
            "",
            UPLOAD_CHUNK,
            &mut |_, _| {},
        )
        .await
        .unwrap_err();
        assert_eq!(nofile.message, "FILE NOT FOUND");
        let folder = send_file(&g, "c", &dir, "", UPLOAD_CHUNK, &mut |_, _| {})
            .await
            .unwrap_err();
        assert!(folder.message.contains("not a file"), "{}", folder.message);
        let empty = dir.join("empty.txt");
        std::fs::write(&empty, b"").unwrap();
        let e = send_file(&g, "c", &empty, "", UPLOAD_CHUNK, &mut |_, _| {})
            .await
            .unwrap_err();
        assert!(e.message.contains("empty"), "{}", e.message);
        assert_eq!(mock.count(), 0);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// 100 MB is the Teams limit; a sparse file stands in so the test writes nothing big.
    #[tokio::test]
    async fn a_file_over_100_mb_is_refused_before_upload() {
        let mock = Mock::start(|_| Reply::empty(500));
        let dir = temp_dir("huge");
        let file = dir.join("huge.bin");
        let f = std::fs::File::create(&file).unwrap();
        f.set_len(100 * MIB + 1).unwrap();
        drop(f);
        let err = send_file(
            &graph_for(&mock),
            "c",
            &file,
            "",
            UPLOAD_CHUNK,
            &mut |_, _| {},
        )
        .await
        .unwrap_err();
        assert!(
            err.message.starts_with("FILE OVER 100 MB"),
            "{}",
            err.message
        );
        assert_eq!(mock.count(), 0);
        let _ = std::fs::remove_dir_all(dir);
    }
}
