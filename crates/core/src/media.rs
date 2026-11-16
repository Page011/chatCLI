//! Downloaded attachments: a capped, on-demand cache of files, plus the checks around file
//! names, executable types and outgoing images.
//!
//! Files are named by a hash of the network and the fetch recipe, never by the sender's file
//! name, so a name like `..\..\x.exe` cannot choose a path. Cached media is not encrypted on
//! disk. Nothing here touches the network: backends fetch, this only stores.

use std::fs::{File, OpenOptions};
use std::io::{Cursor, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};

use crate::model::{FetchRef, MediaKind, Network};
use crate::settings::MediaSettings;
use crate::Result;

/// Largest picture we decode and re-encode (width x height).
pub const MAX_PIXELS: u64 = 25_000_000;

/// Extensions chatcli will not open and never uses for a cache file name.
pub const BLOCKED: &[&str] = &[
    "exe",
    "bat",
    "cmd",
    "com",
    "ps1",
    "psm1",
    "lnk",
    "js",
    "jse",
    "vbs",
    "vbe",
    "wsf",
    "wsh",
    "msi",
    "msp",
    "scr",
    "hta",
    "cpl",
    "jar",
    "reg",
    "pif",
    "application",
    "gadget",
    "scf",
    "url",
    "inf",
];

/// The only extensions a cache file may end in; anything else is stored as `.bin`.
pub const ALLOWED: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "webp", "bmp", "heic", "mp4", "mov", "webm", "3gp", "mp3", "m4a",
    "ogg", "opus", "wav", "aac", "pdf", "txt", "csv", "doc", "docx", "xls", "xlsx", "ppt", "pptx",
    "odt", "ods", "odp", "rtf", "zip", "7z", "vcf", "json", "xml", "md", "log",
];

/// MIME types that name an executable or script, whatever the file is called.
const BLOCKED_MIME: &[&str] = &[
    "application/x-msdownload",
    "application/x-msdos-program",
    "application/x-dosexec",
    "application/x-executable",
    "application/x-mach-binary",
    "application/vnd.microsoft.portable-executable",
    "application/x-msi",
    "application/x-ms-installer",
    "application/x-sh",
    "application/x-bat",
    "application/x-powershell",
    "application/hta",
    "application/java-archive",
    "application/javascript",
    "application/x-javascript",
    "text/javascript",
    "text/x-powershell",
    "text/x-shellscript",
];

const MIME_EXTENSIONS: &[(&str, &str)] = &[
    ("image/png", "png"),
    ("image/jpeg", "jpg"),
    ("image/gif", "gif"),
    ("image/webp", "webp"),
    ("image/bmp", "bmp"),
    ("image/heic", "heic"),
    ("video/mp4", "mp4"),
    ("video/quicktime", "mov"),
    ("video/webm", "webm"),
    ("video/3gpp", "3gp"),
    ("audio/mpeg", "mp3"),
    ("audio/mp4", "m4a"),
    ("audio/ogg", "ogg"),
    ("audio/opus", "opus"),
    ("audio/wav", "wav"),
    ("audio/x-wav", "wav"),
    ("audio/aac", "aac"),
    ("application/pdf", "pdf"),
    ("text/plain", "txt"),
    ("text/csv", "csv"),
    ("application/msword", "doc"),
    (
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "docx",
    ),
    ("application/vnd.ms-excel", "xls"),
    (
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "xlsx",
    ),
    ("application/vnd.ms-powerpoint", "ppt"),
    (
        "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "pptx",
    ),
    ("application/vnd.oasis.opendocument.text", "odt"),
    ("application/vnd.oasis.opendocument.spreadsheet", "ods"),
    ("application/vnd.oasis.opendocument.presentation", "odp"),
    ("application/rtf", "rtf"),
    ("application/zip", "zip"),
    ("application/x-7z-compressed", "7z"),
    ("text/vcard", "vcf"),
    ("application/json", "json"),
    ("application/xml", "xml"),
    ("text/xml", "xml"),
    ("text/markdown", "md"),
];

const MIB: u64 = 1024 * 1024;
/// A `.part` file older than this is a crashed download: `evict` removes it.
const PART_MAX_AGE_MS: i64 = 60 * 60 * 1000;
/// `begin` replaces a `.part` file older than this (a crashed earlier try) instead of failing.
const PART_RETRY_AGE_MS: i64 = 5 * 60 * 1000;
const TEMP_MAX_AGE_MS: i64 = 24 * 60 * 60 * 1000;
/// Cache layout is `<root>/<network>/<file>`; a walk never goes deeper than this.
const WALK_DEPTH: usize = 3;
/// Largest profile photo downloaded, before decoding.
pub const AVATAR_MAX_BYTES: u64 = MIB;
/// Stored profile photos are this many pixels square.
pub const AVATAR_PX: u32 = 96;
/// A stored photo (or "no photo" marker) older than this is looked up again.
pub const AVATAR_FRESH_MS: i64 = 7 * 24 * 60 * 60 * 1000;

/// What the cache knows about one person's profile photo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AvatarLookup {
    Fresh(PathBuf),
    NoPhoto,
    Stale(PathBuf),
    Missing,
}

#[derive(Clone, Debug)]
pub struct MediaCache {
    root: PathBuf,
    max_bytes: u64,
    retention_ms: i64,
    max_file_bytes: u64,
}

impl MediaCache {
    /// Uses `media_dir` as the cache root (`paths::media_dir()`, or a temp dir in tests) and
    /// creates one folder per network under it.
    pub fn open(media_dir: &Path, settings: &MediaSettings) -> Result<MediaCache> {
        for net in Network::ALL {
            let dir = media_dir.join(net.key());
            std::fs::create_dir_all(&dir)
                .map_err(|e| format!("cannot create media folder {}: {e}", dir.display()))?;
        }
        Ok(MediaCache {
            root: media_dir.to_path_buf(),
            max_bytes: settings.cache_max_mb.saturating_mul(MIB),
            retention_ms: i64::from(settings.retention_days) * 24 * 60 * 60 * 1000,
            max_file_bytes: settings.max_file_mb.saturating_mul(MIB),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The same cache folder with new limits (the user changed `[media]` while running).
    pub fn with_limits(&self, settings: &MediaSettings) -> MediaCache {
        MediaCache {
            root: self.root.clone(),
            max_bytes: settings.cache_max_mb.saturating_mul(MIB),
            retention_ms: i64::from(settings.retention_days) * 24 * 60 * 60 * 1000,
            max_file_bytes: settings.max_file_mb.saturating_mul(MIB),
        }
    }

    pub fn max_file_bytes(&self) -> u64 {
        self.max_file_bytes
    }

    /// `<root>/<network>/<32 hex of sha256(network + "\n" + fetch_ref)>.<ext>`. The extension
    /// is re-checked here, so even a careless caller cannot put a path piece or an executable
    /// type into the name.
    pub fn path_for(&self, network: Network, fetch_ref: &FetchRef, ext: &str) -> PathBuf {
        let mut hasher = Sha256::new();
        hasher.update(network.key().as_bytes());
        hasher.update(b"\n");
        hasher.update(fetch_ref.as_str().as_bytes());
        let hex: String = hasher
            .finalize()
            .iter()
            .take(16)
            .map(|b| format!("{b:02x}"))
            .collect();
        let ext = if is_plain_ext(ext) && !BLOCKED.contains(&ext) {
            ext
        } else {
            "bin"
        };
        self.root.join(network.key()).join(format!("{hex}.{ext}"))
    }

    /// The cached file when it is there. Touching its modified time makes eviction drop the
    /// least recently opened file first.
    pub fn lookup(&self, network: Network, fetch_ref: &FetchRef, ext: &str) -> Option<PathBuf> {
        let path = self.path_for(network, fetch_ref, ext);
        if !path.is_file() {
            return None;
        }
        if let Ok(f) = OpenOptions::new().write(true).open(&path) {
            let _ = f.set_modified(SystemTime::now());
        }
        Some(path)
    }

    /// Refuses a file over `max_file_mb` before any bytes are fetched. An unknown size passes:
    /// the `PartFile` limit stops it while writing.
    pub fn check_size(&self, size: Option<u64>) -> Result<()> {
        match size {
            Some(n) if n > self.max_file_bytes => Err(self.over_limit_text()),
            _ => Ok(()),
        }
    }

    fn over_limit_text(&self) -> String {
        format!(
            "FILE OVER {} MB: raise Largest file in /settings (Media) to download it.",
            self.max_file_bytes / MIB
        )
    }

    /// Starts writing a cache file as `<final>.part`. The writer errors once more than
    /// `max_file_mb` bytes were written. A `.part` left by a download that is still running
    /// is an error; one left by a crash (older than 5 minutes) is replaced.
    pub fn begin(&self, network: Network, fetch_ref: &FetchRef, ext: &str) -> Result<PartFile> {
        let final_path = self.path_for(network, fetch_ref, ext);
        let part_path = part_name(&final_path);
        let file = match open_new(&part_path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                let age = file_times(&part_path).map(|(_, m)| crate::model::now_ms() - m);
                if age.is_some_and(|a| a > PART_RETRY_AGE_MS) {
                    let _ = std::fs::remove_file(&part_path);
                    open_new(&part_path).map_err(cache_io)?
                } else {
                    return Err("That file is already downloading: try again in a moment.".into());
                }
            }
            Err(e) => return Err(cache_io(e)),
        };
        Ok(PartFile {
            file: Some(file),
            part_path,
            final_path,
            written: 0,
            limit: self.max_file_bytes,
            limit_text: self.over_limit_text(),
            committed: false,
        })
    }

    /// Renames the `.part` file to its final name, then evicts. The file just committed is
    /// never the one evicted. Returns the final path.
    pub fn commit(&self, mut part: PartFile) -> Result<PathBuf> {
        let mut file = part
            .file
            .take()
            .ok_or("The download was already finished.")?;
        file.flush().map_err(cache_io)?;
        // Windows cannot rename an open file.
        drop(file);
        std::fs::rename(&part.part_path, &part.final_path).map_err(cache_io)?;
        part.committed = true;
        mark_from_internet(&part.final_path);
        let _ = self.evict_keeping(crate::model::now_ms(), Some(&part.final_path));
        Ok(part.final_path.clone())
    }

    /// Removes `.part` files older than 1 h, files older than the retention, then the oldest
    /// (by modified time) until the total is within the cap. A file that vanished or is in use
    /// is skipped. Returns (files, bytes) removed.
    pub fn evict(&self, now_ms: i64) -> Result<(u64, u64)> {
        self.evict_keeping(now_ms, None)
    }

    fn evict_keeping(&self, now_ms: i64, keep: Option<&Path>) -> Result<(u64, u64)> {
        let (mut files, mut bytes) = (0u64, 0u64);
        let mut remove = |path: &Path, len: u64| {
            if std::fs::remove_file(path).is_ok() {
                files += 1;
                bytes += len;
                true
            } else {
                false
            }
        };
        let mut live: Vec<(PathBuf, u64, i64)> = Vec::new();
        for (path, len, mtime) in walk_files(&self.root) {
            let age = now_ms - mtime;
            if is_part(&path) {
                if age > PART_MAX_AGE_MS {
                    remove(&path, len);
                }
            } else if age > self.retention_ms && keep != Some(path.as_path()) {
                remove(&path, len);
            } else {
                live.push((path, len, mtime));
            }
        }
        let mut total: u64 = live.iter().map(|(_, len, _)| *len).sum();
        live.sort_by_key(|(_, _, mtime)| *mtime);
        for (path, len, _) in live {
            if total <= self.max_bytes {
                break;
            }
            if keep == Some(path.as_path()) {
                continue;
            }
            if remove(&path, len) {
                total -= len;
            }
        }
        Ok((files, bytes))
    }

    /// Deletes every file under the cache root. Returns the bytes freed. A file that cannot be
    /// deleted (open in another program) is an error after the rest were removed.
    pub fn clear(&self) -> Result<u64> {
        let (mut freed, mut stuck) = (0u64, 0u64);
        for (path, len, _) in walk_files(&self.root) {
            match std::fs::remove_file(&path) {
                Ok(()) => freed += len,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => stuck += 1,
            }
        }
        if stuck > 0 {
            return Err(format!(
                "Could not delete {stuck} cached file(s): close the programs showing them and try again."
            ));
        }
        Ok(freed)
    }

    /// `<root>/avatars/<32 hex of sha256("avatar\n" + network + "\n" + person_id)>`, no extension.
    /// Never built from network text, so a person id cannot choose a path.
    fn avatar_stem(&self, network: Network, person_id: &str) -> PathBuf {
        let mut hasher = Sha256::new();
        hasher.update(b"avatar\n");
        hasher.update(network.key().as_bytes());
        hasher.update(b"\n");
        hasher.update(person_id.as_bytes());
        let hex: String = hasher
            .finalize()
            .iter()
            .take(16)
            .map(|b| format!("{b:02x}"))
            .collect();
        self.root.join("avatars").join(hex)
    }

    /// By file modified time, which is the fetch time. Does NOT touch it (unlike `lookup`).
    /// A `.png` younger than 7 days is Fresh, older is Stale; a `.none` marker younger than
    /// 7 days is NoPhoto, older is Missing.
    pub fn avatar_lookup(&self, network: Network, person_id: &str, now_ms: i64) -> AvatarLookup {
        let stem = self.avatar_stem(network, person_id);
        let fresh = |p: &Path| file_times(p).map(|(_, m)| now_ms - m <= AVATAR_FRESH_MS);
        let png = stem.with_extension("png");
        match fresh(&png) {
            Some(true) => return AvatarLookup::Fresh(png),
            Some(false) => return AvatarLookup::Stale(png),
            None => {}
        }
        match fresh(&stem.with_extension("none")) {
            Some(true) => AvatarLookup::NoPhoto,
            _ => AvatarLookup::Missing,
        }
    }

    /// Re-encodes the photo (`avatar_png`), writes `<hex>.png` and drops the `.none` marker.
    /// CPU work: call from `spawn_blocking`.
    pub fn store_avatar(&self, network: Network, person_id: &str, bytes: &[u8]) -> Result<PathBuf> {
        let png = avatar_png(bytes)?;
        let stem = self.avatar_stem(network, person_id);
        let (path, part, none) = (
            stem.with_extension("png"),
            stem.with_extension("png.part"),
            stem.with_extension("none"),
        );
        self.avatar_dir()?;
        std::fs::write(&part, png).map_err(cache_io)?;
        std::fs::rename(&part, &path).map_err(cache_io)?;
        let _ = std::fs::remove_file(none);
        Ok(path)
    }

    /// Writes the `.none` marker ("no photo") and removes any `.png`.
    pub fn store_no_avatar(&self, network: Network, person_id: &str) -> Result<()> {
        let stem = self.avatar_stem(network, person_id);
        self.avatar_dir()?;
        std::fs::write(stem.with_extension("none"), []).map_err(cache_io)?;
        let _ = std::fs::remove_file(stem.with_extension("png"));
        Ok(())
    }

    /// Deletes every cached profile photo and marker (both networks share the folder; each
    /// network asks again on its next sign-in). A missing folder is fine.
    pub fn clear_avatars(&self) {
        let _ = std::fs::remove_dir_all(self.root.join("avatars"));
    }

    fn avatar_dir(&self) -> Result<()> {
        std::fs::create_dir_all(self.root.join("avatars")).map_err(cache_io)
    }
}

/// Square 96x96 PNG from a profile photo. Pure. Refuses more than `AVATAR_MAX_BYTES`, a format
/// other than PNG/JPEG/GIF/WebP, more than 4096 px a side or 64 MiB of decode memory (a 1 MB
/// PNG can inflate). Applies EXIF rotation, centre-crops, resizes.
pub fn avatar_png(bytes: &[u8]) -> Result<Vec<u8>> {
    use image::{
        imageops::FilterType, DynamicImage, ImageDecoder, ImageFormat, ImageReader, Limits,
    };

    let bad = || "not a usable profile photo".to_string();
    if bytes.len() as u64 > AVATAR_MAX_BYTES {
        return Err(bad());
    }
    let mut reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|_| bad())?;
    let mut limits = Limits::default();
    limits.max_image_width = Some(4096);
    limits.max_image_height = Some(4096);
    limits.max_alloc = Some(64 * MIB);
    reader.limits(limits);
    if !matches!(
        reader.format(),
        Some(ImageFormat::Png | ImageFormat::Jpeg | ImageFormat::Gif | ImageFormat::WebP)
    ) {
        return Err(bad());
    }
    let mut decoder = reader.into_decoder().map_err(|_| bad())?;
    let orientation = decoder.orientation().ok();
    let mut img = DynamicImage::from_decoder(decoder).map_err(|_| bad())?;
    if let Some(o) = orientation {
        img.apply_orientation(o);
    }
    let side = img.width().min(img.height());
    let square = img.crop_imm(
        (img.width() - side) / 2,
        (img.height() - side) / 2,
        side,
        side,
    );
    let small = square.resize_exact(AVATAR_PX, AVATAR_PX, FilterType::Triangle);
    let mut out = Vec::new();
    small
        .write_to(&mut Cursor::new(&mut out), ImageFormat::Png)
        .map_err(|_| bad())?;
    Ok(out)
}

fn cache_io(e: std::io::Error) -> String {
    format!("Could not write the media cache ({e}): check the disk has room.")
}

/// The stream Windows reads as "this file came from the internet" (Mark of the Web). Office
/// then opens it in Protected View and blocks macros, and SmartScreen checks it: the same as a
/// browser download. Zone 3 is the Internet zone.
#[cfg(windows)]
const MOTW: &str = "[ZoneTransfer]\r\nZoneId=3\r\n";

/// Marks a cached download as from the internet. Not NTFS (a FAT or network volume has no
/// streams) or no permission: one log line, the file still opens.
fn mark_from_internet(path: &Path) {
    #[cfg(windows)]
    {
        let mut stream = path.as_os_str().to_os_string();
        stream.push(":Zone.Identifier");
        if std::fs::write(&stream, MOTW).is_err() {
            crate::log::info(
                "media",
                "the file could not be marked as downloaded from the internet",
            );
        }
    }
    #[cfg(not(windows))]
    let _ = path;
}

fn open_new(path: &Path) -> std::io::Result<File> {
    OpenOptions::new().write(true).create_new(true).open(path)
}

fn part_name(final_path: &Path) -> PathBuf {
    let mut name = final_path.file_name().unwrap_or_default().to_os_string();
    name.push(".part");
    final_path.with_file_name(name)
}

fn is_part(path: &Path) -> bool {
    path.extension().is_some_and(|e| e == "part")
}

fn is_plain_ext(ext: &str) -> bool {
    (1..=8).contains(&ext.len())
        && ext
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
}

/// (length, modified time as Unix ms) of a file, or None when it cannot be read.
fn file_times(path: &Path) -> Option<(u64, i64)> {
    let meta = std::fs::metadata(path).ok()?;
    let modified = meta.modified().ok()?.duration_since(UNIX_EPOCH).ok()?;
    Some((meta.len(), modified.as_millis() as i64))
}

/// Every regular file under `dir` (symlinks are skipped, never followed), with its length and
/// modified time in Unix ms.
fn walk_files(dir: &Path) -> Vec<(PathBuf, u64, i64)> {
    fn go(dir: &Path, depth: usize, out: &mut Vec<(PathBuf, u64, i64)>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            let path = entry.path();
            if kind.is_dir() && depth < WALK_DEPTH {
                go(&path, depth + 1, out);
            } else if kind.is_file() {
                if let Some((len, mtime)) = file_times(&path) {
                    out.push((path, len, mtime));
                }
            }
        }
    }
    let mut out = Vec::new();
    go(dir, 0, &mut out);
    out
}

/// A cache file being written. Errors once more than the per-file limit is written, and
/// deletes the `.part` file when dropped without `MediaCache::commit`.
pub struct PartFile {
    file: Option<File>,
    part_path: PathBuf,
    final_path: PathBuf,
    written: u64,
    limit: u64,
    limit_text: String,
    committed: bool,
}

impl PartFile {
    pub fn written(&self) -> u64 {
        self.written
    }
}

impl Write for PartFile {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.written.saturating_add(buf.len() as u64) > self.limit {
            return Err(std::io::Error::other(self.limit_text.clone()));
        }
        let file = self
            .file
            .as_mut()
            .ok_or_else(|| std::io::Error::other("the download was already finished"))?;
        let n = file.write(buf)?;
        self.written += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self.file.as_mut() {
            Some(f) => f.flush(),
            None => Ok(()),
        }
    }
}

impl Drop for PartFile {
    fn drop(&mut self) {
        if !self.committed {
            // Close the handle first: Windows will not delete an open file.
            self.file = None;
            let _ = std::fs::remove_file(&self.part_path);
        }
    }
}

/// The file name part of `name`, with Windows' trailing dots and spaces removed (Windows opens
/// `a.exe.` and `a.exe ` as `a.exe`).
fn clean_file_name(name: &str) -> &str {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    base.trim_end_matches(['.', ' '])
}

/// Lowercase text after the last `.` of `name`, if there is a dot and something after it.
fn last_extension(name: &str) -> Option<String> {
    let (stem, ext) = name.rsplit_once('.')?;
    (!stem.is_empty() || !ext.is_empty()).then(|| ext.to_ascii_lowercase())
}

fn mime_essence(mime: &str) -> String {
    mime.split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase()
}

/// The extension for a cache file: lowercase `[a-z0-9]{1,8}`. From the file name's last
/// extension when it is in `ALLOWED`, else from a fixed MIME map, else `bin`. Never anything in
/// `BLOCKED`, never a path separator.
pub fn safe_extension(name: Option<&str>, mime: Option<&str>) -> String {
    if let Some(ext) = name.map(clean_file_name).and_then(last_extension) {
        if ALLOWED.contains(&ext.as_str()) {
            return ext;
        }
    }
    if let Some(m) = mime.map(mime_essence) {
        if let Some((_, ext)) = MIME_EXTENSIONS.iter().find(|(k, _)| *k == m) {
            return (*ext).to_string();
        }
    }
    "bin".to_string()
}

/// True when the name's extension (any case, trailing dots and spaces ignored) or the MIME
/// type is an executable or script type. A name with an NTFS stream suffix
/// (`a.exe::$DATA`) is checked both with and without it.
pub fn is_blocked(name: Option<&str>, mime: Option<&str>) -> bool {
    let by_name = name.map(clean_file_name).is_some_and(|n| {
        let plain = n
            .split(':')
            .next()
            .unwrap_or(n)
            .trim_end_matches(['.', ' ']);
        [n, plain]
            .iter()
            .filter_map(|s| last_extension(s))
            .any(|e| BLOCKED.contains(&e.as_str()))
    });
    by_name || mime.is_some_and(|m| BLOCKED_MIME.contains(&mime_essence(m).as_str()))
}

/// By extension, confirmed by the file's first bytes for png, jpeg, gif and webp: Image.
/// Anything else, or a picture extension whose bytes are not that picture: Document.
pub fn kind_for_path(path: &Path) -> MediaKind {
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    let mut head = [0u8; 12];
    let read = File::open(path)
        .and_then(|mut f| f.read(&mut head))
        .unwrap_or(0);
    let head = &head[..read];
    let matches = match ext.as_str() {
        "png" => head.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]),
        "jpg" | "jpeg" => head.starts_with(&[0xFF, 0xD8, 0xFF]),
        "gif" => head.starts_with(b"GIF87a") || head.starts_with(b"GIF89a"),
        "webp" => head.len() >= 12 && &head[..4] == b"RIFF" && &head[8..12] == b"WEBP",
        _ => false,
    };
    if matches {
        MediaKind::Image
    } else {
        MediaKind::Document
    }
}

pub struct PreparedImage {
    pub bytes: Vec<u8>,
    pub mime: &'static str,
    pub width: u32,
    pub height: u32,
}

/// Decodes a picture and encodes it again, which drops EXIF and every other metadata block
/// (location, camera, time). The EXIF rotation is applied first, so the picture still looks
/// the same. PNG stays PNG, JPEG stays JPEG (quality 90), GIF and WebP become PNG (first
/// frame). Refuses more than `MAX_PIXELS` ("IMAGE TOO LARGE") and an encoded result over
/// `max_bytes` ("IMAGE OVER <n> MB"; Teams passes 4 MiB).
pub fn prepare_image(path: &Path, max_bytes: u64) -> Result<PreparedImage> {
    use image::{DynamicImage, ImageDecoder, ImageFormat, ImageReader, Limits};

    let too_large = || "IMAGE TOO LARGE: pick a picture under 25 megapixels.".to_string();
    let meta = std::fs::metadata(path).map_err(|e| read_error(&e))?;
    // A compressed file this big cannot be a normal photo; do not even open it.
    if meta.len() > 256 * MIB {
        return Err(too_large());
    }
    let mut reader = ImageReader::open(path)
        .map_err(|e| read_error(&e))?
        .with_guessed_format()
        .map_err(|e| read_error(&e))?;
    let mut limits = Limits::default();
    limits.max_alloc = Some(512 * MIB);
    reader.limits(limits);
    let format = reader.format();
    let not_a_picture = || "That file is not a PNG, JPEG, GIF or WebP picture.".to_string();
    if !matches!(
        format,
        Some(ImageFormat::Png | ImageFormat::Jpeg | ImageFormat::Gif | ImageFormat::WebP)
    ) {
        return Err(not_a_picture());
    }
    let mut decoder = reader.into_decoder().map_err(|_| not_a_picture())?;
    let (w, h) = decoder.dimensions();
    if u64::from(w) * u64::from(h) > MAX_PIXELS {
        return Err(too_large());
    }
    let orientation = decoder.orientation().ok();
    let mut img = DynamicImage::from_decoder(decoder).map_err(|_| not_a_picture())?;
    if let Some(o) = orientation {
        img.apply_orientation(o);
    }

    let mut bytes = Vec::new();
    let mime = if format == Some(ImageFormat::Jpeg) {
        let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut bytes, 90);
        // JPEG has no alpha channel.
        DynamicImage::ImageRgb8(img.to_rgb8())
            .write_with_encoder(encoder)
            .map_err(|e| format!("Could not re-encode the picture ({e})."))?;
        "image/jpeg"
    } else {
        img.write_to(&mut Cursor::new(&mut bytes), ImageFormat::Png)
            .map_err(|e| format!("Could not re-encode the picture ({e})."))?;
        "image/png"
    };
    if bytes.len() as u64 > max_bytes {
        return Err(format!(
            "IMAGE OVER {} MB: pick a smaller picture.",
            max_bytes / MIB
        ));
    }
    Ok(PreparedImage {
        bytes,
        mime,
        width: img.width(),
        height: img.height(),
    })
}

fn read_error(e: &std::io::Error) -> String {
    if e.kind() == std::io::ErrorKind::NotFound {
        "FILE NOT FOUND".to_string()
    } else {
        format!("Could not read the picture ({e}): check you can open it.")
    }
}

/// `<data folder>/tmp`: where pasted clipboard images are written before sending.
pub fn temp_dir() -> Result<PathBuf> {
    let dir = crate::paths::data_dir()?.join("tmp");
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("cannot create temp folder {}: {e}", dir.display()))?;
    Ok(dir)
}

/// Removes files in the temp folder older than 24 h. A file in use is left for next time.
pub fn clean_temp(now_ms: i64) -> Result<()> {
    clean_dir(&temp_dir()?, now_ms);
    Ok(())
}

fn clean_dir(dir: &Path, now_ms: i64) {
    for (path, _, mtime) in walk_files(dir) {
        if now_ms - mtime > TEMP_MAX_AGE_MS {
            let _ = std::fs::remove_file(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{codecs::jpeg::JpegEncoder, DynamicImage, ImageFormat, RgbImage, RgbaImage};
    use std::time::Duration;

    fn temp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("chatcli-media-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn settings(cache_mb: u64, file_mb: u64) -> MediaSettings {
        MediaSettings {
            cache_max_mb: cache_mb,
            max_file_mb: file_mb,
            ..MediaSettings::default()
        }
    }

    /// A cache whose cap is a few bytes: `MediaSettings` counts whole MB, so tests shrink the
    /// private fields directly.
    fn tiny_cache(dir: &Path, cap_bytes: u64, file_bytes: u64) -> MediaCache {
        let mut c = MediaCache::open(dir, &settings(1, 1)).unwrap();
        c.max_bytes = cap_bytes;
        c.max_file_bytes = file_bytes;
        c
    }

    fn r(s: &str) -> FetchRef {
        FetchRef::new(s)
    }

    fn put(cache: &MediaCache, key: &str, bytes: &[u8]) -> PathBuf {
        let mut part = cache.begin(Network::Teams, &r(key), "bin").unwrap();
        part.write_all(bytes).unwrap();
        cache.commit(part).unwrap()
    }

    #[test]
    fn with_limits_changes_the_limits_and_keeps_the_root() {
        let dir = temp("limits");
        let c = MediaCache::open(&dir, &settings(200, 25)).unwrap();
        let c2 = c.with_limits(&settings(500, 50));
        assert_eq!(c2.root(), c.root());
        assert_eq!(c2.max_file_bytes(), 50 * MIB);
        assert_eq!(c2.max_bytes, 500 * MIB);
        assert_eq!(c.max_file_bytes(), 25 * MIB, "the original is untouched");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A cached download carries the Mark of the Web, so Office opens it in Protected View.
    #[cfg(windows)]
    #[test]
    fn a_committed_file_is_marked_as_from_the_internet() {
        let dir = temp("motw");
        let c = MediaCache::open(&dir, &settings(200, 25)).unwrap();
        let path = put(&c, "motw-key", b"payload");
        let mut stream = path.as_os_str().to_os_string();
        stream.push(":Zone.Identifier");
        let text = std::fs::read_to_string(&stream).unwrap();
        assert!(
            text.starts_with("[ZoneTransfer]") && text.contains("ZoneId=3"),
            "{text:?}"
        );
        // The file itself is untouched.
        assert_eq!(std::fs::read(&path).unwrap(), b"payload");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn set_age(path: &Path, age_ms: i64) {
        let f = OpenOptions::new().write(true).open(path).unwrap();
        let t = SystemTime::now() - Duration::from_millis(age_ms as u64);
        f.set_modified(t).unwrap();
    }

    #[test]
    fn path_is_a_hash_inside_the_network_folder_never_the_name() {
        let dir = temp("path");
        let c = MediaCache::open(&dir, &MediaSettings::default()).unwrap();
        let a = c.path_for(Network::Teams, &r("{\"id\":\"1\"}"), "png");
        let b = c.path_for(Network::WhatsApp, &r("{\"id\":\"1\"}"), "png");
        assert_ne!(a, b, "the network is part of the hash");
        assert_eq!(a.parent().unwrap(), dir.join("teams"));
        let stem = a.file_stem().unwrap().to_string_lossy().into_owned();
        assert_eq!(stem.len(), 32);
        assert!(stem
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()));
        assert_eq!(a.extension().unwrap(), "png");
        // Same input, same path.
        assert_eq!(a, c.path_for(Network::Teams, &r("{\"id\":\"1\"}"), "png"));
        // A hostile extension never reaches the name.
        for bad in ["exe", "../x", "a/b", "PNG", "", "toolongext1", "p.g"] {
            let p = c.path_for(Network::Teams, &r("x"), bad);
            assert_eq!(p.extension().unwrap(), "bin", "ext {bad:?}");
            assert_eq!(p.parent().unwrap(), dir.join("teams"));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn open_creates_one_folder_per_network() {
        let dir = temp("open");
        MediaCache::open(&dir, &MediaSettings::default()).unwrap();
        assert!(dir.join("teams").is_dir() && dir.join("whatsapp").is_dir());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_commit_lookup_round_trip() {
        let dir = temp("roundtrip");
        let c = MediaCache::open(&dir, &MediaSettings::default()).unwrap();
        assert!(c.lookup(Network::Teams, &r("a"), "txt").is_none());
        let mut part = c.begin(Network::Teams, &r("a"), "txt").unwrap();
        part.write_all(b"hello").unwrap();
        assert_eq!(part.written(), 5);
        let part_path = c
            .path_for(Network::Teams, &r("a"), "txt")
            .with_extension("txt.part");
        assert!(part_path.is_file(), "written to .part first");
        let path = c.commit(part).unwrap();
        assert!(!part_path.exists());
        assert_eq!(std::fs::read(&path).unwrap(), b"hello");
        assert_eq!(c.lookup(Network::Teams, &r("a"), "txt"), Some(path));
        assert!(
            c.lookup(Network::WhatsApp, &r("a"), "txt").is_none(),
            "per network"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_part_file_stops_at_the_limit_and_dropping_deletes_it() {
        let dir = temp("limit");
        let c = tiny_cache(&dir, 1000, 10);
        let mut part = c.begin(Network::Teams, &r("big"), "bin").unwrap();
        part.write_all(&[1u8; 10]).unwrap();
        let err = part.write_all(&[1u8; 1]).unwrap_err();
        assert!(err.to_string().starts_with("FILE OVER"), "{err}");
        let part_path = c
            .path_for(Network::Teams, &r("big"), "bin")
            .with_extension("bin.part");
        assert!(part_path.is_file());
        drop(part);
        assert!(!part_path.exists(), "an abandoned download leaves nothing");
        assert!(c.lookup(Network::Teams, &r("big"), "bin").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn check_size_uses_the_fixed_sentence() {
        let dir = temp("size");
        let c = MediaCache::open(&dir, &settings(200, 25)).unwrap();
        assert_eq!(c.max_file_bytes(), 25 * MIB);
        assert!(c.check_size(None).is_ok());
        assert!(c.check_size(Some(25 * MIB)).is_ok());
        assert_eq!(
            c.check_size(Some(25 * MIB + 1)).unwrap_err(),
            "FILE OVER 25 MB: raise Largest file in /settings (Media) to download it."
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_second_download_of_the_same_file_is_refused_a_crashed_one_is_replaced() {
        let dir = temp("twice");
        let c = tiny_cache(&dir, 1000, 100);
        let first = c.begin(Network::Teams, &r("k"), "bin").unwrap();
        let err = c.begin(Network::Teams, &r("k"), "bin").err().unwrap();
        assert!(err.contains("already downloading"), "{err}");
        // The first one survives and finishes.
        let mut first = first;
        first.write_all(b"ok").unwrap();
        let path = c.commit(first).unwrap();
        assert_eq!(std::fs::read(path).unwrap(), b"ok");
        // A stale .part (crash) does not block the next try.
        let stale = c
            .path_for(Network::Teams, &r("k2"), "bin")
            .with_extension("bin.part");
        std::fs::write(&stale, b"junk").unwrap();
        set_age(&stale, 10 * 60 * 1000);
        let again = c.begin(Network::Teams, &r("k2"), "bin");
        assert!(again.is_ok());
        drop(again);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn eviction_keeps_the_cache_under_the_cap_oldest_first() {
        let dir = temp("cap");
        let c = tiny_cache(&dir, 25, 100);
        let a = put(&c, "a", &[0; 10]);
        set_age(&a, 3000);
        let b = put(&c, "b", &[0; 10]);
        set_age(&b, 2000);
        let d = put(&c, "d", &[0; 10]);
        // Committing "d" took the total to 30 bytes over a 25 cap: the oldest ("a") went.
        assert!(!a.exists(), "oldest evicted");
        assert!(b.exists() && d.exists());
        let total: u64 = walk_files(c.root()).iter().map(|f| f.1).sum();
        assert!(total <= 25, "cache holds {total} bytes");
        // A single file bigger than the cap is still kept right after its own commit.
        let big = put(&c, "big", &[0; 40]);
        assert!(
            big.exists(),
            "the file just written is never the one evicted"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn retention_removes_old_files_and_stale_part_files() {
        let dir = temp("retention");
        let mut c = tiny_cache(&dir, 1_000_000, 100);
        c.retention_ms = 7 * 24 * 60 * 60 * 1000;
        let fresh = put(&c, "fresh", &[0; 5]);
        // Aged after its own commit, or that commit's eviction would remove it first.
        let old = put(&c, "old", &[0; 5]);
        set_age(&old, 8 * 24 * 60 * 60 * 1000);
        let stale_part = dir.join("teams").join("deadbeef.bin.part");
        std::fs::write(&stale_part, b"x").unwrap();
        set_age(&stale_part, 2 * 60 * 60 * 1000);
        let live_part = dir.join("teams").join("cafe.bin.part");
        std::fs::write(&live_part, b"x").unwrap();
        let (files, bytes) = c.evict(crate::model::now_ms()).unwrap();
        assert!(!old.exists() && !stale_part.exists());
        assert!(fresh.exists() && live_part.exists(), "recent files stay");
        assert_eq!((files, bytes), (2, 6));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn lookup_touches_the_file_so_it_is_evicted_last() {
        let dir = temp("lru");
        let c = tiny_cache(&dir, 25, 100);
        let a = put(&c, "a", &[0; 10]);
        set_age(&a, 5000);
        let b = put(&c, "b", &[0; 10]);
        set_age(&b, 4000);
        // Opening "a" makes it the newest.
        assert!(c.lookup(Network::Teams, &r("a"), "bin").is_some());
        put(&c, "d", &[0; 10]);
        assert!(a.exists(), "recently opened file kept");
        assert!(!b.exists(), "least recently used evicted");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn avatar_png_crops_to_a_square() {
        let mut src = Vec::new();
        RgbaImage::from_pixel(300, 100, image::Rgba([1, 2, 3, 255]))
            .write_to(&mut Cursor::new(&mut src), ImageFormat::Png)
            .unwrap();
        let out = avatar_png(&src).unwrap();
        let img = image::load_from_memory_with_format(&out, ImageFormat::Png).unwrap();
        assert_eq!((img.width(), img.height()), (96, 96));
    }

    #[test]
    fn avatar_png_refuses_big_junk_and_huge_pictures() {
        assert!(avatar_png(&vec![0u8; 1024 * 1024 + 1]).is_err());
        assert!(avatar_png(b"not a picture at all").is_err());
        let mut huge = Vec::new();
        RgbaImage::new(5000, 5000)
            .write_to(&mut Cursor::new(&mut huge), ImageFormat::Png)
            .unwrap();
        assert!(
            huge.len() < 1024 * 1024,
            "test picture must pass the byte cap"
        );
        assert!(avatar_png(&huge).is_err());
    }

    fn png_bytes() -> Vec<u8> {
        let mut v = Vec::new();
        RgbaImage::from_pixel(10, 10, image::Rgba([9, 9, 9, 255]))
            .write_to(&mut Cursor::new(&mut v), ImageFormat::Png)
            .unwrap();
        v
    }

    #[test]
    fn avatar_path_is_a_hash_and_lookup_follows_age() {
        let dir = temp("avatar");
        let c = tiny_cache(&dir, 1_000_000, 1000);
        let id = "../../evil id";
        let now = || crate::model::now_ms();
        let old = 8 * 24 * 3600 * 1000;
        assert_eq!(
            c.avatar_lookup(Network::Teams, id, now()),
            AvatarLookup::Missing
        );
        let path = c.store_avatar(Network::Teams, id, &png_bytes()).unwrap();
        assert_eq!(path.parent().unwrap(), dir.join("avatars"));
        let name = path.file_name().unwrap().to_str().unwrap();
        assert!(name.len() == 36 && name.ends_with(".png") && !name.contains("evil"));
        assert_eq!(
            c.avatar_lookup(Network::Teams, id, now()),
            AvatarLookup::Fresh(path.clone())
        );
        set_age(&path, old);
        assert_eq!(
            c.avatar_lookup(Network::Teams, id, now()),
            AvatarLookup::Stale(path.clone())
        );
        c.store_no_avatar(Network::Teams, id).unwrap();
        assert!(!path.exists(), "marker replaces the photo");
        assert_eq!(
            c.avatar_lookup(Network::Teams, id, now()),
            AvatarLookup::NoPhoto
        );
        let none = path.with_extension("none");
        set_age(&none, old);
        assert_eq!(
            c.avatar_lookup(Network::Teams, id, now()),
            AvatarLookup::Missing
        );
        let path = c.store_avatar(Network::Teams, id, &png_bytes()).unwrap();
        assert!(!none.exists(), "photo removes the marker");
        c.store_no_avatar(Network::WhatsApp, "x").unwrap();
        c.clear().unwrap();
        assert!(!path.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn clear_deletes_everything_and_reports_bytes() {
        let dir = temp("clear");
        let c = tiny_cache(&dir, 1000, 100);
        let a = put(&c, "a", &[0; 10]);
        let w = c.begin(Network::WhatsApp, &r("w"), "bin").unwrap();
        let mut w = w;
        w.write_all(&[0; 7]).unwrap();
        let wp = c.commit(w).unwrap();
        assert_eq!(c.clear().unwrap(), 17);
        assert!(!a.exists() && !wp.exists());
        assert!(dir.join("teams").is_dir(), "folders stay");
        assert_eq!(c.clear().unwrap(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn extension_rules() {
        let e = safe_extension;
        assert_eq!(e(Some("Photo.PNG"), None), "png");
        assert_eq!(e(Some("report.final.pdf"), None), "pdf");
        assert_eq!(e(Some("setup.exe"), None), "bin", "blocked never used");
        assert_eq!(e(Some("setup.exe"), Some("application/pdf")), "pdf");
        assert_eq!(e(Some("noext"), Some("image/jpeg")), "jpg");
        assert_eq!(e(None, Some("IMAGE/PNG; charset=binary")), "png");
        assert_eq!(e(None, None), "bin");
        assert_eq!(e(Some("a.weird"), Some("application/x-unknown")), "bin");
        assert_eq!(
            e(Some("..\\..\\evil.png"), None),
            "png",
            "separators never leak"
        );
        assert_eq!(e(Some("x.png."), None), "png", "trailing dot");
        for blocked in BLOCKED {
            let got = e(Some(&format!("f.{blocked}")), None);
            assert!(!BLOCKED.contains(&got.as_str()), "{blocked} -> {got}");
        }
        for allowed in ALLOWED {
            assert_eq!(e(Some(&format!("f.{allowed}")), None), *allowed);
        }
        let all_ok = |s: &str| is_plain_ext(s);
        assert!(all_ok(&e(Some("a.7z"), None)) && all_ok(&e(Some("a.3gp"), None)));
    }

    #[test]
    fn blocked_detection() {
        assert!(is_blocked(Some("a.exe"), None));
        assert!(is_blocked(Some("A.EXE"), None));
        assert!(is_blocked(Some("a.exe."), None));
        assert!(is_blocked(Some("a.exe "), None));
        assert!(is_blocked(Some("a.exe::$DATA"), None));
        assert!(is_blocked(Some("C:\\x\\run.ps1"), None));
        assert!(is_blocked(Some("macro.JSE"), None));
        assert!(is_blocked(Some("x.url"), None));
        assert!(is_blocked(None, Some("application/x-msdownload")));
        assert!(is_blocked(
            Some("readme.txt"),
            Some("application/x-msdownload; x=1")
        ));
        assert!(!is_blocked(Some("a.pdf"), Some("application/pdf")));
        assert!(!is_blocked(Some("exe"), None), "no extension");
        assert!(!is_blocked(Some("a.exe.txt"), None), "opens as txt");
        assert!(!is_blocked(None, None));
        for ext in BLOCKED {
            assert!(is_blocked(Some(&format!("f.{ext}")), None), "{ext}");
        }
    }

    fn write_png(path: &Path, w: u32, h: u32) {
        RgbaImage::from_pixel(w, h, image::Rgba([10, 120, 200, 255]))
            .save_with_format(path, ImageFormat::Png)
            .unwrap();
    }

    #[test]
    fn kind_comes_from_extension_confirmed_by_bytes() {
        let dir = temp("kind");
        let png = dir.join("a.png");
        write_png(&png, 4, 4);
        assert_eq!(kind_for_path(&png), MediaKind::Image);
        // The same bytes named .pdf are a document; a text file named .png is not an image.
        let renamed = dir.join("a.pdf");
        std::fs::copy(&png, &renamed).unwrap();
        assert_eq!(kind_for_path(&renamed), MediaKind::Document);
        let fake = dir.join("fake.png");
        std::fs::write(&fake, "not a picture").unwrap();
        assert_eq!(kind_for_path(&fake), MediaKind::Document);
        let jpg = dir.join("a.JPG");
        std::fs::write(&jpg, [0xFF, 0xD8, 0xFF, 0xE0, 0, 0]).unwrap();
        assert_eq!(kind_for_path(&jpg), MediaKind::Image);
        let gif = dir.join("a.gif");
        std::fs::write(&gif, b"GIF89a....").unwrap();
        assert_eq!(kind_for_path(&gif), MediaKind::Image);
        let webp = dir.join("a.webp");
        std::fs::write(&webp, b"RIFF\0\0\0\0WEBPVP8 ").unwrap();
        assert_eq!(kind_for_path(&webp), MediaKind::Image);
        assert_eq!(kind_for_path(&dir.join("missing.png")), MediaKind::Document);
        // bmp is not one of the four confirmed types.
        let bmp = dir.join("a.bmp");
        std::fs::write(&bmp, b"BM......").unwrap();
        assert_eq!(kind_for_path(&bmp), MediaKind::Document);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A JPEG with an EXIF block (orientation 6 = rotate 90) spliced in after the SOI marker.
    fn jpeg_with_exif(path: &Path, w: u32, h: u32) {
        let mut plain = Vec::new();
        let img = DynamicImage::ImageRgb8(RgbImage::from_pixel(w, h, image::Rgb([200, 30, 30])));
        img.write_with_encoder(JpegEncoder::new_with_quality(&mut plain, 80))
            .unwrap();
        let mut tiff = Vec::new();
        tiff.extend_from_slice(b"II\x2A\x00\x08\x00\x00\x00");
        tiff.extend_from_slice(&[1, 0, 0x12, 0x01, 3, 0, 1, 0, 0, 0, 6, 0, 0, 0, 0, 0, 0, 0]);
        let mut app1 = vec![0xFF, 0xE1];
        app1.extend_from_slice(&((2 + 6 + tiff.len()) as u16).to_be_bytes());
        app1.extend_from_slice(b"Exif\0\0");
        app1.extend_from_slice(&tiff);
        let mut out = plain[..2].to_vec();
        out.extend_from_slice(&app1);
        out.extend_from_slice(&plain[2..]);
        std::fs::write(path, out).unwrap();
    }

    fn has(bytes: &[u8], needle: &[u8]) -> bool {
        bytes.windows(needle.len()).any(|w| w == needle)
    }

    #[test]
    fn jpeg_is_re_encoded_without_exif_and_keeps_its_orientation() {
        let dir = temp("jpeg");
        let path = dir.join("p.jpg");
        jpeg_with_exif(&path, 40, 20);
        assert!(
            has(&std::fs::read(&path).unwrap(), b"Exif"),
            "test file has EXIF"
        );
        let out = prepare_image(&path, 4 * MIB).unwrap();
        assert_eq!(out.mime, "image/jpeg");
        assert!(!has(&out.bytes, b"Exif"), "EXIF is gone");
        assert_eq!(out.bytes[..3], [0xFF, 0xD8, 0xFF]);
        // Orientation 6 turns a 40x20 picture into 20x40, and the new file says so itself.
        assert_eq!((out.width, out.height), (20, 40));
        let back = image::load_from_memory(&out.bytes).unwrap();
        assert_eq!((back.width(), back.height()), (20, 40));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn png_gif_and_webp_come_out_as_png() {
        let dir = temp("formats");
        let png = dir.join("a.png");
        write_png(&png, 8, 6);
        let out = prepare_image(&png, 4 * MIB).unwrap();
        assert_eq!((out.mime, out.width, out.height), ("image/png", 8, 6));
        assert!(out.bytes.starts_with(&[0x89, b'P', b'N', b'G']));

        let img = RgbaImage::from_pixel(5, 5, image::Rgba([1, 2, 3, 255]));
        let gif = dir.join("a.gif");
        img.save_with_format(&gif, ImageFormat::Gif).unwrap();
        let out = prepare_image(&gif, 4 * MIB).unwrap();
        assert_eq!((out.mime, out.width), ("image/png", 5));

        let webp = dir.join("a.webp");
        img.save_with_format(&webp, ImageFormat::WebP).unwrap();
        let out = prepare_image(&webp, 4 * MIB).unwrap();
        assert_eq!((out.mime, out.height), ("image/png", 5));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn oversize_missing_and_fake_pictures_are_refused_with_the_fixed_words() {
        let dir = temp("refuse");
        // Over the pixel cap: refused from the header, before any pixel is decoded.
        let big = dir.join("big.png");
        let gray = image::GrayImage::new(5001, 5000);
        let mut f = File::create(&big).unwrap();
        gray.write_with_encoder(image::codecs::png::PngEncoder::new_with_quality(
            &mut f,
            image::codecs::png::CompressionType::Fast,
            image::codecs::png::FilterType::NoFilter,
        ))
        .unwrap();
        drop(f);
        let err = prepare_image(&big, 100 * MIB).err().unwrap();
        assert!(err.starts_with("IMAGE TOO LARGE"), "{err}");

        // Encoded result over the caller's limit: noisy pixels do not compress.
        let mut noise = RgbImage::new(900, 900);
        let mut x: u32 = 12345;
        for p in noise.pixels_mut() {
            x = x.wrapping_mul(1664525).wrapping_add(1013904223);
            *p = image::Rgb([(x >> 8) as u8, (x >> 16) as u8, (x >> 24) as u8]);
        }
        let noisy = dir.join("noisy.png");
        noise.save_with_format(&noisy, ImageFormat::Png).unwrap();
        let err = prepare_image(&noisy, MIB).err().unwrap();
        assert!(err.starts_with("IMAGE OVER 1 MB"), "{err}");
        assert!(prepare_image(&noisy, 8 * MIB).is_ok());

        assert_eq!(
            prepare_image(&dir.join("none.png"), MIB).err().unwrap(),
            "FILE NOT FOUND"
        );
        let fake = dir.join("fake.png");
        std::fs::write(&fake, "just text").unwrap();
        assert!(prepare_image(&fake, MIB)
            .err()
            .unwrap()
            .contains("not a PNG"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn clean_dir_removes_only_old_files() {
        let dir = temp("tmpclean");
        let old = dir.join("old.png");
        let fresh = dir.join("fresh.png");
        std::fs::write(&old, b"x").unwrap();
        std::fs::write(&fresh, b"x").unwrap();
        set_age(&old, 25 * 60 * 60 * 1000);
        clean_dir(&dir, crate::model::now_ms());
        assert!(!old.exists() && fresh.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
