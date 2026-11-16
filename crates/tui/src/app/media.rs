//! Attachments in the App: opening and previewing what others sent (issues 16 to 19, 34) and
//! choosing and sending files (20, 21, 35, 36). Pure like the rest of `app`: the App never reads
//! a picture, opens a file, touches the clipboard or deletes anything. It queues those jobs
//! (`take_open_files`, `take_decodes`, `take_clipboard_request`, `take_trash`) and the run loop
//! does them and reports back (`open_done`, `thumb_ready`, `clipboard_done`).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use chatcli_core::limits;
use chatcli_core::log;
use chatcli_core::media::{is_blocked, kind_for_path};
use chatcli_core::model::{
    Attachment, ChatMessage, Command, FetchRef, MediaKind, MessageState, Network,
};

use super::{too_long, App, Focus, Out, Prompt};
use crate::attach::{self, Candidate};
use crate::input::TextInput;
use crate::keymap::Action;
use crate::media_view::{self as mv, Depth, Fetch, MediaKey, Then, Thumb, View};
use crate::text::{clean_line, truncate};

const MIB: u64 = 1024 * 1024;
/// Longest file name shown in a chip or a question.
const NAME_MAX: usize = 40;

/// A file chosen for the next message of one chat. Nothing is read or sent until Enter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Pending {
    pub path: PathBuf,
    /// Display name, one line; never used as a path.
    pub name: String,
    pub size: u64,
    pub kind: MediaKind,
    /// A clipboard picture saved in the data folder: deleted once the send is over or cancelled.
    pub temp: bool,
}

impl Pending {
    /// `[photo.png 240 KB]`.
    pub fn chip(&self) -> String {
        format!(
            "[{} {}]",
            truncate(&self.name, NAME_MAX),
            mv::human_size(self.size)
        )
    }
}

/// The path prompt: what is typed and the folder entries that match it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct AttachPrompt {
    pub input: TextInput,
    pub cands: Vec<Candidate>,
    pub sel: usize,
}

/// A job for the run loop: a file to open with the OS, or a picture to decode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileJob {
    pub network: Network,
    pub key: MediaKey,
    pub path: PathBuf,
}

/// Media state that is not per network.
pub(crate) struct Ctl {
    /// `[media] inline_images`.
    pub inline: bool,
    /// Largest file a backend downloads (`[media] max_file_mb`).
    pub max_file_bytes: u64,
    /// The media cache folder: a path an event gives that is not inside it is refused.
    pub root: Option<PathBuf>,
    pub depth: Depth,
    pub attach: Option<AttachPrompt>,
    /// Folder the path prompt completes from; the working folder when `None`.
    pub attach_dir: Option<PathBuf>,
    open_files: Vec<FileJob>,
    decodes: Vec<FileJob>,
    clip_request: Option<(Network, String)>,
    trash: Vec<PathBuf>,
    /// Client id of a send -> the clipboard picture it carries, deleted when the send is over.
    temp_for: HashMap<String, PathBuf>,
}

impl Default for Ctl {
    fn default() -> Ctl {
        Ctl {
            inline: true,
            max_file_bytes: 25 * MIB,
            root: None,
            depth: Depth::detect(),
            attach: None,
            attach_dir: None,
            open_files: Vec::new(),
            decodes: Vec::new(),
            clip_request: None,
            trash: Vec::new(),
            temp_for: HashMap::new(),
        }
    }
}

/// The notice for a file over the network's limit: what is wrong, then what to do. The Teams
/// image text keeps the contract's `IMAGE OVER 4 MB`.
fn over_limit(n: Network, kind: MediaKind, limit: u64) -> String {
    let mb = limit / MIB;
    if kind == MediaKind::Image {
        format!(
            "IMAGE OVER {mb} MB for {}: pick a smaller picture.",
            n.label()
        )
    } else {
        format!("FILE OVER {mb} MB for {}: pick a smaller file.", n.label())
    }
}

/// `Opened`: the word for the run loop's answer.
const OPENED: &str = "Opened.";

/// `path` is inside `root`: it starts with it and never climbs out with `..` (`starts_with`
/// compares components and does not normalise, so `root/../x` would pass it).
fn inside(root: &Path, path: &Path) -> bool {
    path.starts_with(root)
        && !path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
}

impl App {
    // ---- queues the run loop drains ----

    /// Files to open with the OS default app (`o`), each already past the executable check.
    pub fn take_open_files(&mut self) -> Vec<FileJob> {
        std::mem::take(&mut self.media.open_files)
    }

    /// Pictures to decode into thumbnails on a worker thread (`v`).
    pub fn take_decodes(&mut self) -> Vec<FileJob> {
        std::mem::take(&mut self.media.decodes)
    }

    /// The chat that asked to attach the clipboard picture, once.
    pub fn take_clipboard_request(&mut self) -> Option<(Network, String)> {
        self.media.clip_request.take()
    }

    /// Temporary files (clipboard pictures) that are no longer needed.
    pub fn take_trash(&mut self) -> Vec<PathBuf> {
        std::mem::take(&mut self.media.trash)
    }

    /// The colour depth thumbnails are drawn with (detected from the environment by default).
    pub fn set_color_depth(&mut self, depth: Depth) {
        self.media.depth = depth;
    }

    pub fn color_depth(&self) -> Depth {
        self.media.depth
    }

    /// Largest download in bytes, for the worker that decodes thumbnails.
    pub fn max_file_bytes(&self) -> u64 {
        self.media.max_file_bytes
    }

    /// The folder the path prompt completes from (the working folder when never set).
    pub fn set_attach_dir(&mut self, dir: PathBuf) {
        self.media.attach_dir = Some(dir);
    }

    /// Thumbnails are allowed (`[media] inline_images`, and `image_protocol` not `off`).
    pub(crate) fn inline_images(&self) -> bool {
        self.media.inline
    }

    pub(crate) fn attach_prompt(&self) -> Option<&AttachPrompt> {
        self.media.attach.as_ref()
    }

    // ---- the run loop's answers ----

    /// The OS was asked to open a file: say how it went.
    pub fn open_done(&mut self, job: &FileJob, result: Result<(), String>) {
        let n = job.network;
        match result {
            Ok(()) => {
                self.ns_mut(n).media.entry(job.key.clone()).opened = true;
                self.push_notice(n, OPENED, false);
            }
            Err(e) => self.push_notice(n, &e, true),
        }
    }

    /// A worker finished decoding a picture. The thumbnail shows under its message; a failure
    /// is a chip line and a notice.
    pub fn thumb_ready(&mut self, job: &FileJob, result: Result<Thumb, String>) {
        let n = job.network;
        let failed = result.as_ref().err().cloned();
        let item = self.ns_mut(n).media.entry(job.key.clone());
        item.view = match result {
            Ok(t) => View::Shown(t),
            Err(e) => View::Failed(clean_line(&e)),
        };
        if let Some(e) = failed {
            self.push_notice(n, &clean_line(&e), true);
        }
        if self.ns(n).focus == Focus::Select {
            self.ensure_sel_visible(n);
        }
    }

    /// The run loop read the clipboard: a picture saved to `path`, or why not.
    pub fn clipboard_done(&mut self, n: Network, chat_id: &str, result: Result<PathBuf, String>) {
        match result {
            Ok(path) => self.attach_or_notice(n, chat_id, path, true),
            Err(e) => self.push_notice(n, &e, true),
        }
    }

    // ---- events from the backends ----

    /// A backend put attachment `index` in the media cache.
    pub(super) fn media_ready(
        &mut self,
        n: Network,
        chat_id: String,
        message_id: String,
        index: u32,
        path: PathBuf,
    ) {
        if self.media.root.as_ref().is_some_and(|r| !inside(r, &path)) {
            // Defence in depth: a path outside the cache is never opened.
            log::error("tui", "a backend gave a media path outside the cache");
            self.media_failed(
                n,
                chat_id,
                message_id,
                index,
                "MEDIA PATH OUTSIDE THE CACHE: restart chatcli.".to_string(),
                None,
            );
            return;
        }
        let key = MediaKey {
            chat: chat_id,
            msg: message_id,
            idx: index,
        };
        let then = {
            let item = self.ns_mut(n).media.entry(key.clone());
            let then = match item.fetch {
                Fetch::Downloading(t) => t,
                _ => Then::Nothing,
            };
            item.fetch = Fetch::Ready(path.clone());
            if then == Then::View {
                item.view = View::Decoding;
            }
            then
        };
        let job = FileJob {
            network: n,
            key,
            path,
        };
        match then {
            Then::Open => self.media.open_files.push(job),
            Then::View => self.media.decodes.push(job),
            Then::Nothing => {}
        }
    }

    /// A backend could not fetch attachment `index`. A Teams file it cannot download itself comes
    /// with its web address: the browser is offered behind the usual "Open host? y/n".
    pub(super) fn media_failed(
        &mut self,
        n: Network,
        chat_id: String,
        message_id: String,
        index: u32,
        reason: String,
        fallback_url: Option<String>,
    ) {
        let reason = clean_line(&reason);
        let key = MediaKey {
            chat: chat_id,
            msg: message_id,
            idx: index,
        };
        let item = self.ns_mut(n).media.entry(key);
        let then = match item.fetch {
            Fetch::Downloading(t) => t,
            _ => Then::Nothing,
        };
        if then == Then::View {
            item.view = View::Hidden;
        }
        item.fetch = if fallback_url.is_some() {
            Fetch::Idle
        } else {
            Fetch::Failed(reason.clone())
        };
        match fallback_url {
            Some(url) => {
                self.push_notice(n, &reason, false);
                if then == Then::Open {
                    self.offer_links(vec![url]);
                }
            }
            None => self.push_notice(n, &reason, true),
        }
    }

    /// Upload progress: shown in the footer and on the pending message. A late event for a send
    /// that is already over (or never was ours) is dropped, or the footer would show it for ever.
    pub(super) fn send_progress(
        &mut self,
        n: Network,
        chat_id: &str,
        client_id: String,
        done: u64,
        total: u64,
    ) {
        let ns = self.ns_mut(n);
        let pending = ns.messages.get(chat_id).is_some_and(|l| {
            l.iter()
                .any(|m| m.id == client_id && matches!(m.state, MessageState::Pending))
        });
        if pending {
            ns.progress.insert(client_id, (done, total));
        }
    }

    /// A send is over, confirmed or failed: its progress goes, and so does the clipboard picture
    /// it carried.
    pub(super) fn send_over(&mut self, n: Network, client_id: &str) {
        self.ns_mut(n).progress.remove(client_id);
        if let Some(path) = self.media.temp_for.remove(client_id) {
            self.media.trash.push(path);
        }
    }

    // ---- open and view what someone sent ----

    /// The selected message's first attachment (or its first picture), with where it is.
    fn pick_attachment(
        &self,
        n: Network,
        image: bool,
    ) -> Result<(String, String, u32, Attachment), String> {
        let chat = self
            .ns(n)
            .open
            .clone()
            .ok_or_else(|| "Open a chat first.".to_string())?;
        let m = self
            .selected_message(n)
            .ok_or_else(|| "Select a message first.".to_string())?;
        match m
            .attachments
            .iter()
            .enumerate()
            .find(|(_, a)| !image || a.kind == MediaKind::Image)
        {
            Some((i, a)) => Ok((chat, m.id.clone(), i as u32, a.clone())),
            None if m.attachments.is_empty() => Err("No attachment in this message.".to_string()),
            None => Err("No picture in this message: press o to open the file.".to_string()),
        }
    }

    /// The file is bigger than the download cap: the same sentence the cache gives.
    fn download_cap(&self, size: Option<u64>) -> Result<(), String> {
        match size {
            Some(s) if s > self.media.max_file_bytes => Err(format!(
                "FILE OVER {} MB: raise Largest file in /settings (Media) to download it.",
                self.media.max_file_bytes / MIB
            )),
            _ => Ok(()),
        }
    }

    /// `o`: download the first attachment of the selected message if needed, then open it with
    /// the default app. Executable types are refused before anything is fetched.
    pub(super) fn open_media(&mut self, n: Network) -> Out {
        let (chat, msg, idx, att) = match self.pick_attachment(n, false) {
            Ok(t) => t,
            Err(e) => {
                self.push_notice(n, &e, false);
                return Vec::new();
            }
        };
        if is_blocked(att.name.as_deref(), att.mime.as_deref()) {
            self.push_notice(n, "BLOCKED: executable", true);
            return Vec::new();
        }
        let key = MediaKey {
            chat: chat.clone(),
            msg: msg.clone(),
            idx,
        };
        let state = self.ns(n).media.items.get(&key).map(|i| i.fetch.clone());
        match state {
            // Already in the cache (and still there: the cache evicts): open it.
            Some(Fetch::Ready(p)) if p.is_file() => {
                self.media.open_files.push(FileJob {
                    network: n,
                    key,
                    path: p,
                });
                return Vec::new();
            }
            Some(Fetch::Downloading(_)) => {
                self.push_notice(n, "Already downloading.", false);
                return Vec::new();
            }
            _ => {}
        }
        // A Teams file in someone's OneDrive: without the optional file scope the browser is
        // the only way, so skip the round trip.
        if let Some(url) = att.open_url.clone() {
            if att.fetch_ref.is_empty() || !self.ns(n).caps.fetch_files {
                self.push_notice(n, "FILE: open in browser", false);
                self.offer_links(vec![url]);
                return Vec::new();
            }
        }
        if att.fetch_ref.is_empty() {
            self.push_notice(n, "This attachment cannot be downloaded.", true);
            return Vec::new();
        }
        if let Err(e) = self.download_cap(att.size) {
            self.push_notice(n, &e, true);
            return Vec::new();
        }
        let item = self.ns_mut(n).media.entry(key);
        item.fetch = Fetch::Downloading(Then::Open);
        item.opened = false;
        // No footer notice: the chip under the message already says "downloading...".
        vec![(
            n,
            Command::FetchMedia {
                chat_id: chat,
                message_id: msg,
                index: idx,
            },
        )]
    }

    /// `v`: show a thumbnail of the selected image under its message, or hide it again.
    pub(super) fn view_image(&mut self, n: Network) -> Out {
        if !self.media.inline {
            self.push_notice(
                n,
                "INLINE IMAGES OFF: turn on in /settings (Media), or press o to open it.",
                false,
            );
            return Vec::new();
        }
        let (chat, msg, idx, att) = match self.pick_attachment(n, true) {
            Ok(t) => t,
            Err(e) => {
                self.push_notice(n, &e, false);
                return Vec::new();
            }
        };
        let key = MediaKey {
            chat: chat.clone(),
            msg: msg.clone(),
            idx,
        };
        let (fetch, view) = self
            .ns(n)
            .media
            .items
            .get(&key)
            .map_or((Fetch::Idle, View::Hidden), |i| {
                (i.fetch.clone(), i.view.clone())
            });
        match (&fetch, &view) {
            (_, View::Shown(_)) => {
                self.ns_mut(n).media.entry(key).view = View::Hidden;
                return Vec::new();
            }
            (_, View::Decoding) => return Vec::new(),
            (Fetch::Downloading(_), _) => {
                self.push_notice(n, "Already downloading.", false);
                return Vec::new();
            }
            _ => {}
        }
        if let Err(e) = self.download_cap(att.size) {
            self.ns_mut(n).media.entry(key).view = View::Failed(mv::TOO_LARGE.to_string());
            self.push_notice(n, mv::TOO_LARGE, true);
            log::error("tui", &format!("image not previewed: {e}"));
            return Vec::new();
        }
        if let Fetch::Ready(p) = &fetch {
            if p.is_file() {
                self.ns_mut(n).media.entry(key.clone()).view = View::Decoding;
                self.media.decodes.push(FileJob {
                    network: n,
                    key,
                    path: p.clone(),
                });
                return Vec::new();
            }
        }
        if att.fetch_ref.is_empty() {
            self.push_notice(n, "This picture cannot be downloaded: press o.", true);
            return Vec::new();
        }
        let item = self.ns_mut(n).media.entry(key);
        item.fetch = Fetch::Downloading(Then::View);
        // No footer notice: the chip under the message already says "downloading...".
        vec![(
            n,
            Command::FetchMedia {
                chat_id: chat,
                message_id: msg,
                index: idx,
            },
        )]
    }

    // ---- choosing a file ----

    fn attach_base(&self) -> PathBuf {
        self.media
            .attach_dir
            .clone()
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_default()
    }

    /// Ctrl+O: the path prompt, listing the working folder.
    pub(super) fn start_attach(&mut self, n: Network) {
        if self.ns(n).open.is_none() {
            return self.push_notice(n, "Open a chat first.", false);
        }
        if matches!(self.ns(n).target(), Some(super::msg::Target::Edit { .. })) {
            return self.push_notice(n, "Finish or cancel the edit first.", false);
        }
        let cands = attach::complete("", &self.attach_base());
        self.media.attach = Some(AttachPrompt {
            input: TextInput::default(),
            cands,
            sel: 0,
        });
    }

    /// New candidates for what is typed; the highlight goes back to the first.
    pub(super) fn attach_refresh(&mut self) {
        let base = self.attach_base();
        if let Some(a) = self.media.attach.as_mut() {
            a.cands = attach::complete(a.input.text(), &base);
            a.sel = 0;
        }
    }

    /// Text typed or pasted into the path prompt.
    pub(super) fn attach_type(&mut self, text: &str) {
        if let Some(a) = self.media.attach.as_mut() {
            // A pasted path is often quoted (a dropped file): the quotes are not part of it.
            a.input.insert_str(if a.input.is_empty() {
                attach::unquote(text)
            } else {
                text
            });
        }
        self.attach_refresh();
    }

    /// Tab: finish the highlighted entry (a folder gets a separator, so its content lists next).
    pub(super) fn attach_complete(&mut self) {
        let base = self.attach_base();
        let Some(a) = self.media.attach.as_mut() else {
            return;
        };
        // A network path is listed only now, on the user's Tab: typing or pasting one never
        // makes Windows contact that machine.
        if a.cands.is_empty() && attach::is_network_path(a.input.text()) {
            a.cands = attach::scan(a.input.text(), &base);
            a.sel = 0;
        }
        if let Some(c) = a.cands.get(a.sel).cloned() {
            a.input.set(&attach::apply(a.input.text(), &c));
            self.attach_refresh();
        }
    }

    /// The popup keys inside the path prompt: Up and Down choose, Enter attaches, Esc cancels.
    pub(super) fn attach_action(&mut self, n: Network, action: Action) {
        match action {
            Action::PopupUp | Action::PopupDown => {
                if let Some(a) = self.media.attach.as_mut() {
                    let last = a.cands.len().saturating_sub(1);
                    a.sel = if action == Action::PopupUp {
                        a.sel.saturating_sub(1)
                    } else {
                        (a.sel + 1).min(last)
                    };
                }
            }
            Action::PopupAccept => self.attach_accept(n),
            _ => self.media.attach = None,
        }
    }

    /// Enter in the path prompt. A file attaches. A folder is entered. Text that is neither is
    /// finished with the highlighted entry; with no entry it is a missing file.
    fn attach_accept(&mut self, n: Network) {
        let Some(a) = self.media.attach.as_ref() else {
            return;
        };
        let mut input = a.input.text().to_string();
        if input.trim().is_empty() {
            return;
        }
        let base = self.attach_base();
        // Text that names nothing yet, or stops at a separator inside a folder, is finished with
        // the highlighted entry. A folder name without a separator is entered instead.
        let inside = input.ends_with(['/', '\\']);
        if inside || !attach::resolve(&input, &base).exists() {
            match a.cands.get(a.sel) {
                Some(c) => input = attach::apply(&input, c),
                None if inside => {
                    self.push_notice(n, "That folder has nothing to attach.", false);
                    return;
                }
                None => {
                    self.push_notice(n, "FILE NOT FOUND", true);
                    return;
                }
            }
        }
        let path = attach::resolve(&input, &base);
        if path.is_dir() {
            let sep = attach::separator(&input);
            if !input.ends_with(['/', '\\']) {
                input.push(sep);
            }
            if let Some(a) = self.media.attach.as_mut() {
                a.input.set(&input);
            }
            self.attach_refresh();
            return;
        }
        self.media.attach = None;
        if let Some(chat) = self.ns(n).open.clone() {
            self.attach_or_notice(n, &chat, path, false);
        }
    }

    /// Attaches `path` to the next message of `chat_id`, or says why not. A clipboard picture
    /// that cannot be attached is deleted at once.
    pub(super) fn attach_or_notice(
        &mut self,
        n: Network,
        chat_id: &str,
        path: PathBuf,
        temp: bool,
    ) {
        if let Err(e) = self.attach_path(n, chat_id, &path, temp) {
            self.push_notice(n, &e, true);
            if temp {
                self.media.trash.push(path);
            }
        }
    }

    /// Checks what can be checked without reading the file (it is there, a file, not empty,
    /// within the network's size limit) and keeps it as the chat's attachment.
    fn attach_path(
        &mut self,
        n: Network,
        chat_id: &str,
        path: &Path,
        temp: bool,
    ) -> Result<(), String> {
        if self.ns(n).open.as_deref() != Some(chat_id)
            && !self.ns(n).chats.iter().any(|c| c.id == chat_id)
        {
            return Err("Open a chat first.".to_string());
        }
        let meta = std::fs::metadata(path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                "FILE NOT FOUND".to_string()
            } else {
                "Cannot read that file: check that you can open it.".to_string()
            }
        })?;
        if meta.is_dir() {
            return Err("That is a folder: pick a file.".to_string());
        }
        if !meta.is_file() {
            return Err("That is not a file: pick a file.".to_string());
        }
        if meta.len() == 0 {
            return Err("That file is empty: pick another.".to_string());
        }
        let kind = kind_for_path(path);
        let limit = limits::send_limit_bytes(n, kind);
        if meta.len() > limit {
            return Err(over_limit(n, kind, limit));
        }
        let name = if temp {
            "pasted image".to_string()
        } else {
            path.file_name()
                .map(|s| clean_line(&s.to_string_lossy()))
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| "file".to_string())
        };
        let pending = Pending {
            path: path.to_path_buf(),
            name,
            size: meta.len(),
            kind,
            temp,
        };
        if let Some(old) = self.ns_mut(n).pending.insert(chat_id.to_string(), pending) {
            if old.temp {
                self.media.trash.push(old.path);
            }
        }
        Ok(())
    }

    /// Ctrl+Shift+V: ask the run loop for the clipboard picture.
    pub(super) fn request_clipboard(&mut self, n: Network) {
        match self.ns(n).open.clone() {
            Some(chat) => self.media.clip_request = Some((n, chat)),
            None => self.push_notice(n, "Open a chat first.", false),
        }
    }

    /// Takes the chip off the open chat (Backspace at the start of the box, or Esc).
    pub(super) fn drop_attachment(&mut self, n: Network) {
        let Some(chat) = self.ns(n).open.clone() else {
            return;
        };
        if let Some(p) = self.ns_mut(n).pending.remove(&chat) {
            if p.temp {
                self.media.trash.push(p.path);
            }
        }
    }

    /// Backspace with the cursor at the start of the box removes the chip, not text.
    pub(super) fn backspace_hits_chip(&self, n: Network) -> bool {
        let ns = self.ns(n);
        ns.attachment().is_some() && ns.composer().is_none_or(|c| c.before_cursor().is_empty())
    }

    // ---- sending ----

    /// Enter with a file chosen: check the caption, ask first for what needs asking, then send.
    pub(super) fn send_media(&mut self, n: Network) -> Out {
        let Some(chat_id) = self.ns(n).open.clone() else {
            return Vec::new();
        };
        let Some(p) = self.ns(n).pending.get(&chat_id).cloned() else {
            return Vec::new();
        };
        let caption = self.ns(n).draft().trim().to_string();
        if !caption.is_empty() && !limits::text_fits(n, &caption) {
            self.push_notice(n, &too_long(n), true);
            return Vec::new();
        }
        // The file may have gone since it was chosen.
        if !p.path.is_file() {
            self.drop_attachment(n);
            self.push_notice(n, "FILE NOT FOUND", true);
            return Vec::new();
        }
        let executable = is_blocked(Some(&p.name), None);
        // Teams puts a file in the sender's OneDrive and links it: people in the chat may have
        // no access to it. Say so before it is uploaded.
        let onedrive = n == Network::Teams && p.kind != MediaKind::Image;
        if executable || onedrive {
            self.prompt = Some(Prompt::SendFile {
                chat_id,
                name: p.name,
                executable,
                onedrive,
            });
            return Vec::new();
        }
        self.deliver_media(n, &chat_id)
    }

    /// Sends the chosen file: a pending message with a chip, and `Command::SendMedia`. The
    /// backend reads the file; the TUI never does.
    pub(super) fn deliver_media(&mut self, n: Network, chat_id: &str) -> Out {
        let Some(p) = self.ns_mut(n).pending.remove(chat_id) else {
            return Vec::new();
        };
        let caption = self
            .ns(n)
            .drafts
            .get(chat_id)
            .map(|c| c.text().trim().to_string())
            .unwrap_or_default();
        self.next_client += 1;
        let client_id = format!("local-{}-{}", self.clock.now_ms, self.next_client);
        let marker = match p.kind {
            MediaKind::Image => "[image]".to_string(),
            _ => format!("[document: {}]", p.name),
        };
        let text = if caption.is_empty() {
            marker
        } else {
            format!("{marker} {caption}")
        };
        let msg = ChatMessage {
            network: n,
            chat_id: chat_id.to_string(),
            id: client_id.clone(),
            ts: self.clock.now_ms,
            sender: "You".to_string(),
            from_me: true,
            text,
            state: MessageState::Pending,
            attachments: vec![Attachment {
                kind: p.kind,
                name: Some(p.name.clone()),
                mime: None,
                size: Some(p.size),
                fetch_ref: FetchRef::default(),
                open_url: None,
            }],
            ..ChatMessage::blank(n)
        };
        let reply_kept = matches!(
            self.ns(n).targets.get(chat_id),
            Some(super::msg::Target::Reply(_))
        );
        let ns = self.ns_mut(n);
        ns.drafts.remove(chat_id);
        ns.scroll = 0;
        super::upsert_message(ns.messages.entry(chat_id.to_string()).or_default(), msg);
        if p.temp {
            self.media
                .temp_for
                .insert(client_id.clone(), p.path.clone());
        }
        if reply_kept {
            self.push_notice(
                n,
                "A file cannot quote a reply: the reply stays for your next text.",
                false,
            );
        }
        vec![(
            n,
            Command::SendMedia {
                chat_id: chat_id.to_string(),
                client_id,
                path: p.path,
                kind: p.kind,
                caption,
            },
        )]
    }

    /// The answer to the y/n before a file goes out.
    pub(super) fn confirm_send_file(&mut self, n: Network, chat_id: &str) -> Out {
        self.deliver_media(n, chat_id)
    }

    /// Pasted text that is the path of one existing file: the file dragged onto the terminal.
    /// True when it was taken as an attachment.
    pub(super) fn paste_as_attachment(&mut self, n: Network, text: &str) -> bool {
        let Some(path) = attach::dropped_path(text) else {
            return false;
        };
        let Some(chat) = self.ns(n).open.clone() else {
            return false;
        };
        self.attach_or_notice(n, &chat, path, false);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text::Clock;
    use chatcli_core::model::{Capabilities, ChatKind, ChatSummary, Event, Status};
    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};

    fn clock() -> Clock {
        Clock {
            now_ms: 1_800_000_000_000,
            tz: chrono::FixedOffset::east_opt(0),
        }
    }

    fn key(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: mods,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    fn press(app: &mut App, code: KeyCode) -> Out {
        app.handle_key(key(code, KeyModifiers::NONE))
    }

    fn ctrl(app: &mut App, c: char) -> Out {
        app.handle_key(key(KeyCode::Char(c), KeyModifiers::CONTROL))
    }

    fn typed(app: &mut App, s: &str) -> Out {
        let mut out = Vec::new();
        for c in s.chars() {
            out.extend(press(app, KeyCode::Char(c)));
        }
        out
    }

    fn temp(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("chatcli-appmedia-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn attachment(kind: MediaKind, name: &str) -> Attachment {
        Attachment {
            kind,
            name: Some(name.into()),
            mime: None,
            size: Some(2048),
            fetch_ref: FetchRef::new("ref"),
            open_url: None,
        }
    }

    fn with_attachments(id: &str, text: &str, att: Vec<Attachment>) -> ChatMessage {
        ChatMessage {
            network: Network::Teams,
            chat_id: "a".into(),
            id: id.into(),
            ts: 1000,
            sender: "Alex Example".into(),
            text: text.into(),
            state: MessageState::Sent,
            attachments: att,
            ..ChatMessage::blank(Network::Teams)
        }
    }

    /// Teams online, chat "a" open (message box focused) holding `messages`.
    fn app_with(messages: Vec<ChatMessage>) -> App {
        let mut app = App::new(clock());
        app.set_color_depth(Depth::True);
        app.handle_event(Event::Status(
            Network::Teams,
            Status::Online {
                live: true,
                detail: None,
            },
        ));
        app.handle_event(Event::Capabilities {
            network: Network::Teams,
            caps: Capabilities::all(),
        });
        app.handle_event(Event::Chats(
            Network::Teams,
            vec![ChatSummary {
                id: "a".into(),
                title: "Alex Example".into(),
                kind: ChatKind::Direct,
                last_ts: Some(1),
                ..ChatSummary::blank(Network::Teams)
            }],
        ));
        press(&mut app, KeyCode::Enter);
        app.handle_event(Event::Messages {
            network: Network::Teams,
            chat_id: "a".into(),
            messages,
        });
        app
    }

    /// Select the newest message (Ctrl+Up).
    fn select(app: &mut App) {
        app.handle_key(key(KeyCode::Up, KeyModifiers::CONTROL));
        assert_eq!(app.active().focus, Focus::Select);
    }

    fn image_app() -> App {
        let mut app = app_with(vec![with_attachments(
            "m1",
            "[image] sunset",
            vec![attachment(MediaKind::Image, "sunset.png")],
        )]);
        select(&mut app);
        app
    }

    fn fetch_of(out: &Out) -> Option<(String, u32)> {
        out.iter().find_map(|(_, c)| match c {
            Command::FetchMedia {
                message_id, index, ..
            } => Some((message_id.clone(), *index)),
            _ => None,
        })
    }

    fn notice(app: &App) -> String {
        app.notices()
            .back()
            .map(|n| n.text.clone())
            .unwrap_or_default()
    }

    fn ready(app: &mut App, path: &Path) {
        app.handle_event(Event::MediaReady {
            network: Network::Teams,
            chat_id: "a".into(),
            message_id: "m1".into(),
            index: 0,
            path: path.to_path_buf(),
        });
    }

    fn state(app: &App) -> mv::Item {
        app.ns(Network::Teams)
            .media
            .get("a", "m1", 0)
            .cloned()
            .unwrap_or_default()
    }

    // ---- open (issues 17, 18, 34) ----

    #[test]
    fn o_fetches_nothing_until_pressed_then_opens_what_arrives() {
        let mut app = image_app();
        assert!(
            app.take_open_files().is_empty(),
            "nothing downloads by itself"
        );
        let out = press(&mut app, KeyCode::Char('o'));
        assert_eq!(fetch_of(&out), Some(("m1".to_string(), 0)));
        assert_eq!(state(&app).fetch, Fetch::Downloading(Then::Open));
        assert_eq!(
            notice(&app),
            "",
            "the chip says downloading, the footer stays quiet"
        );
        assert!(
            app.take_open_files().is_empty(),
            "not before the file is there"
        );

        let dir = temp("open");
        let file = dir.join("x.png");
        std::fs::write(&file, b"png").unwrap();
        ready(&mut app, &file);
        let jobs = app.take_open_files();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].path, file);
        assert_eq!(state(&app).fetch, Fetch::Ready(file.clone()));
        app.open_done(&jobs[0], Ok(()));
        assert!(state(&app).opened);
        assert_eq!(notice(&app), "Opened.");

        // The second `o` finds it in the cache: no command, the file opens at once.
        let out = press(&mut app, KeyCode::Char('o'));
        assert!(out.is_empty());
        assert_eq!(app.take_open_files().len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_the_cache_evicted_is_fetched_again() {
        let mut app = image_app();
        press(&mut app, KeyCode::Char('o'));
        ready(&mut app, Path::new("gone.png"));
        app.take_open_files();
        let out = press(&mut app, KeyCode::Char('o'));
        assert!(
            fetch_of(&out).is_some(),
            "the cached file is missing, so fetch it"
        );
    }

    #[test]
    fn open_failing_says_so() {
        let mut app = image_app();
        press(&mut app, KeyCode::Char('o'));
        ready(&mut app, Path::new("x.png"));
        let jobs = app.take_open_files();
        app.open_done(
            &jobs[0],
            Err("Could not open the file: find it in the cache folder.".into()),
        );
        assert!(!state(&app).opened);
        let n = app.notices().back().unwrap();
        assert!(n.is_error && n.text.starts_with("Could not open"));
    }

    #[test]
    fn an_executable_is_refused_before_anything_is_fetched() {
        for name in [
            "setup.exe",
            "RUN.BAT",
            "x.ps1",
            "a.lnk",
            "evil.js",
            "t.vbs",
            "i.msi",
            "s.scr",
            "p.exe.",
        ] {
            let mut app = app_with(vec![with_attachments(
                "m1",
                "[document]",
                vec![attachment(MediaKind::Document, name)],
            )]);
            select(&mut app);
            let out = press(&mut app, KeyCode::Char('o'));
            assert!(out.is_empty(), "{name}: no command");
            assert!(app.take_open_files().is_empty());
            let n = app.notices().back().unwrap();
            assert_eq!(n.text, "BLOCKED: executable", "{name}");
            assert!(n.is_error);
        }
        // A blocked MIME type too, whatever the name says.
        let mut a = attachment(MediaKind::Document, "report.pdf");
        a.mime = Some("application/x-msdownload".into());
        let mut app = app_with(vec![with_attachments("m1", "[document]", vec![a])]);
        select(&mut app);
        assert!(press(&mut app, KeyCode::Char('o')).is_empty());
        assert_eq!(notice(&app), "BLOCKED: executable");
    }

    #[test]
    fn a_message_without_attachments_says_so() {
        let mut app = app_with(vec![with_attachments("m1", "just text", vec![])]);
        select(&mut app);
        assert!(press(&mut app, KeyCode::Char('o')).is_empty());
        assert_eq!(notice(&app), "No attachment in this message.");
        assert!(press(&mut app, KeyCode::Char('v')).is_empty());
        assert_eq!(notice(&app), "No attachment in this message.");
    }

    #[test]
    fn a_failed_fetch_shows_its_reason_and_o_tries_again() {
        let mut app = image_app();
        press(&mut app, KeyCode::Char('o'));
        app.handle_event(Event::MediaFailed {
            network: Network::Teams,
            chat_id: "a".into(),
            message_id: "m1".into(),
            index: 0,
            reason: "MEDIA EXPIRED".into(),
            fallback_url: None,
        });
        assert_eq!(state(&app).fetch, Fetch::Failed("MEDIA EXPIRED".into()));
        let n = app.notices().back().unwrap();
        assert_eq!((n.text.as_str(), n.is_error), ("MEDIA EXPIRED", true));
        assert!(app.take_open_files().is_empty());
        let out = press(&mut app, KeyCode::Char('o'));
        assert!(fetch_of(&out).is_some(), "the next `o` asks again");
    }

    #[test]
    fn a_second_o_while_downloading_asks_nothing() {
        let mut app = image_app();
        press(&mut app, KeyCode::Char('o'));
        let out = press(&mut app, KeyCode::Char('o'));
        assert!(out.is_empty());
        assert_eq!(notice(&app), "Already downloading.");
    }

    #[test]
    fn a_media_event_nobody_asked_for_opens_nothing() {
        let mut app = image_app();
        ready(&mut app, Path::new("x.png"));
        assert!(app.take_open_files().is_empty());
        assert!(app.take_decodes().is_empty());
        assert_eq!(state(&app).fetch, Fetch::Ready(PathBuf::from("x.png")));
    }

    #[test]
    fn a_path_outside_the_cache_is_never_opened() {
        let dir = temp("root");
        let mut app = image_app();
        app.media.root = Some(dir.join("cache"));
        press(&mut app, KeyCode::Char('o'));
        ready(&mut app, &dir.join("elsewhere").join("x.png"));
        assert!(app.take_open_files().is_empty());
        assert!(matches!(state(&app).fetch, Fetch::Failed(_)));
        assert!(notice(&app).starts_with("MEDIA PATH OUTSIDE THE CACHE"));
        // Climbing out with `..` is outside, however it starts.
        press(&mut app, KeyCode::Char('o'));
        ready(
            &mut app,
            &dir.join("cache").join("..").join("elsewhere").join("x.png"),
        );
        assert!(app.take_open_files().is_empty());
        assert!(matches!(state(&app).fetch, Fetch::Failed(_)));
        // Inside the cache it is fine.
        press(&mut app, KeyCode::Char('o'));
        ready(&mut app, &dir.join("cache").join("whatsapp").join("x.png"));
        assert_eq!(app.take_open_files().len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_teams_file_without_the_scope_opens_in_the_browser() {
        let mut a = attachment(MediaKind::Document, "report.docx");
        a.open_url = Some("https://files.example.invalid/personal/alex/report.docx".into());
        let mut app = app_with(vec![with_attachments("m1", "[file]", vec![a])]);
        app.handle_event(Event::Capabilities {
            network: Network::Teams,
            caps: Capabilities {
                fetch_files: false,
                ..Capabilities::all()
            },
        });
        select(&mut app);
        let out = press(&mut app, KeyCode::Char('o'));
        assert!(out.is_empty(), "no round trip without the scope");
        assert_eq!(notice(&app), "FILE: open in browser");
        // Behind the usual "Open host? y/n".
        assert!(
            matches!(app.prompt(), Some(Prompt::Confirm(u)) if u.contains("files.example.invalid"))
        );
        press(&mut app, KeyCode::Char('y'));
        assert_eq!(app.take_open_urls().len(), 1);
    }

    #[test]
    fn a_teams_file_with_the_scope_is_fetched_and_falls_back_on_failure() {
        let mut a = attachment(MediaKind::Document, "report.docx");
        a.open_url = Some("https://files.example.invalid/personal/alex/report.docx".into());
        let mut app = app_with(vec![with_attachments("m1", "[file]", vec![a])]);
        select(&mut app);
        let out = press(&mut app, KeyCode::Char('o'));
        assert!(
            fetch_of(&out).is_some(),
            "with Files.Read.All the backend fetches it"
        );
        app.handle_event(Event::MediaFailed {
            network: Network::Teams,
            chat_id: "a".into(),
            message_id: "m1".into(),
            index: 0,
            reason: "FILE: open in browser".into(),
            fallback_url: Some("https://files.example.invalid/personal/alex/report.docx".into()),
        });
        assert_eq!(
            state(&app).fetch,
            Fetch::Idle,
            "a fallback is not a failure"
        );
        assert!(matches!(app.prompt(), Some(Prompt::Confirm(_))));
        let n = app.notices().back().unwrap();
        assert_eq!(
            (n.text.as_str(), n.is_error),
            ("FILE: open in browser", false)
        );
    }

    #[test]
    fn a_download_over_the_cap_is_refused_in_the_tui_too() {
        let mut a = attachment(MediaKind::Document, "big.zip");
        a.size = Some(26 * MIB);
        let mut app = app_with(vec![with_attachments("m1", "[file]", vec![a])]);
        select(&mut app);
        assert!(press(&mut app, KeyCode::Char('o')).is_empty());
        assert!(notice(&app).starts_with("FILE OVER 25 MB: raise Largest file in /settings"));
    }

    #[test]
    fn an_attachment_with_no_recipe_cannot_be_fetched() {
        let mut a = attachment(MediaKind::Image, "x.png");
        a.fetch_ref = FetchRef::default();
        let mut app = app_with(vec![with_attachments("m1", "[image]", vec![a])]);
        select(&mut app);
        assert!(press(&mut app, KeyCode::Char('o')).is_empty());
        assert_eq!(notice(&app), "This attachment cannot be downloaded.");
    }

    // ---- view (issue 19) ----

    fn thumb() -> Thumb {
        Thumb::from_rgb(2, 2, &[[255, 0, 0]; 4], Depth::True)
    }

    #[test]
    fn v_fetches_then_decodes_then_shows_and_a_second_v_hides() {
        let mut app = image_app();
        let out = press(&mut app, KeyCode::Char('v'));
        assert!(fetch_of(&out).is_some());
        assert_eq!(state(&app).fetch, Fetch::Downloading(Then::View));
        ready(&mut app, Path::new("p.png"));
        assert!(
            app.take_open_files().is_empty(),
            "view never opens the viewer"
        );
        let jobs = app.take_decodes();
        assert_eq!(jobs.len(), 1);
        assert_eq!(state(&app).view, View::Decoding);
        app.thumb_ready(&jobs[0], Ok(thumb()));
        assert!(matches!(state(&app).view, View::Shown(_)));
        // Hide, then show again from the cache: no download.
        assert!(press(&mut app, KeyCode::Char('v')).is_empty());
        assert_eq!(state(&app).view, View::Hidden);
        // "p.png" is not on disk, so the cache entry is stale: showing it again fetches again.
        let out = press(&mut app, KeyCode::Char('v'));
        assert!(fetch_of(&out).is_some());
    }

    #[test]
    fn a_picture_already_in_the_cache_is_decoded_without_a_download() {
        let dir = temp("view");
        let file = dir.join("p.png");
        std::fs::write(&file, b"x").unwrap();
        let mut app = image_app();
        press(&mut app, KeyCode::Char('o'));
        ready(&mut app, &file);
        app.take_open_files();
        let out = press(&mut app, KeyCode::Char('v'));
        assert!(out.is_empty(), "no command: it is cached");
        let jobs = app.take_decodes();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].path, file);
        assert_eq!(state(&app).view, View::Decoding);
        // Pressing v while it decodes does nothing more.
        assert!(press(&mut app, KeyCode::Char('v')).is_empty());
        assert!(app.take_decodes().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn inline_images_off_means_no_thumbnail_and_a_pointer_to_o() {
        let mut app = image_app();
        app.media.inline = false;
        assert!(press(&mut app, KeyCode::Char('v')).is_empty());
        assert!(notice(&app).contains("press o"));
        assert!(app.take_decodes().is_empty());
        assert_eq!(state(&app).view, View::Hidden);
    }

    #[test]
    fn v_on_a_non_image_points_to_o() {
        let mut app = app_with(vec![with_attachments(
            "m1",
            "[document: plan.pdf]",
            vec![attachment(MediaKind::Document, "plan.pdf")],
        )]);
        select(&mut app);
        assert!(press(&mut app, KeyCode::Char('v')).is_empty());
        assert_eq!(
            notice(&app),
            "No picture in this message: press o to open the file."
        );
    }

    #[test]
    fn an_image_over_the_size_cap_is_not_previewed() {
        let mut a = attachment(MediaKind::Image, "huge.png");
        a.size = Some(30 * MIB);
        let mut app = app_with(vec![with_attachments("m1", "[image]", vec![a])]);
        select(&mut app);
        assert!(press(&mut app, KeyCode::Char('v')).is_empty());
        let n = app.notices().back().unwrap();
        assert_eq!(
            (n.text.as_str(), n.is_error),
            ("IMAGE TOO LARGE, press o to open", true)
        );
        assert_eq!(
            state(&app).view,
            View::Failed("IMAGE TOO LARGE, press o to open".into())
        );
    }

    #[test]
    fn a_decode_failure_is_a_chip_line_and_a_notice() {
        let mut app = image_app();
        press(&mut app, KeyCode::Char('v'));
        ready(&mut app, Path::new("p.png"));
        let jobs = app.take_decodes();
        app.thumb_ready(&jobs[0], Err("IMAGE TOO LARGE, press o to open".into()));
        assert_eq!(
            state(&app).view,
            View::Failed("IMAGE TOO LARGE, press o to open".into())
        );
        assert!(app.notices().back().unwrap().is_error);
    }

    // ---- choosing and sending a file (issues 20, 21, 35, 36) ----

    fn plain_app() -> App {
        app_with(vec![with_attachments("m1", "hello", vec![])])
    }

    fn pending(app: &App) -> Option<Pending> {
        app.active().attachment().cloned()
    }

    fn file(dir: &Path, name: &str, bytes: usize) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, vec![b'x'; bytes]).unwrap();
        p
    }

    fn png_file(dir: &Path, name: &str) -> PathBuf {
        let p = dir.join(name);
        image::RgbaImage::from_pixel(8, 8, image::Rgba([1, 2, 3, 255]))
            .save_with_format(&p, image::ImageFormat::Png)
            .unwrap();
        p
    }

    #[test]
    fn ctrl_o_lists_the_folder_and_enter_attaches_a_typed_path() {
        let dir = temp("prompt");
        std::fs::create_dir_all(dir.join("My Pictures")).unwrap();
        png_file(&dir, "photo.png");
        let mut app = plain_app();
        app.set_attach_dir(dir.clone());
        ctrl(&mut app, 'o');
        let a = app.attach_prompt().expect("the prompt is open");
        assert_eq!(
            a.cands
                .iter()
                .map(|c| (c.name.as_str(), c.dir))
                .collect::<Vec<_>>(),
            [("My Pictures", true), ("photo.png", false)]
        );
        typed(&mut app, "pho");
        assert_eq!(app.attach_prompt().unwrap().cands.len(), 1);
        press(&mut app, KeyCode::Tab);
        assert_eq!(app.attach_prompt().unwrap().input, "photo.png");
        press(&mut app, KeyCode::Enter);
        assert!(app.attach_prompt().is_none());
        let p = pending(&app).expect("attached");
        assert_eq!(
            (p.name.as_str(), p.kind, p.temp),
            ("photo.png", MediaKind::Image, false)
        );
        assert!(app.take_trash().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn completion_enters_folders_with_spaces() {
        let dir = temp("spaces");
        std::fs::create_dir_all(dir.join("My Pictures")).unwrap();
        png_file(&dir.join("My Pictures"), "holiday photo.png");
        let mut app = plain_app();
        app.set_attach_dir(dir.clone());
        ctrl(&mut app, 'o');
        typed(&mut app, "my p");
        press(&mut app, KeyCode::Tab);
        let sep = std::path::MAIN_SEPARATOR;
        assert_eq!(
            app.attach_prompt().unwrap().input,
            format!("My Pictures{sep}")
        );
        // The folder's content is listed now; Enter on the highlighted entry attaches it.
        assert_eq!(app.attach_prompt().unwrap().cands.len(), 1);
        press(&mut app, KeyCode::Enter);
        assert!(app.attach_prompt().is_none());
        assert_eq!(pending(&app).unwrap().name, "holiday photo.png");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn enter_on_a_folder_goes_inside_and_a_missing_file_is_refused() {
        let dir = temp("folder");
        std::fs::create_dir_all(dir.join("docs")).unwrap();
        let mut app = plain_app();
        app.set_attach_dir(dir.clone());
        ctrl(&mut app, 'o');
        typed(&mut app, "docs");
        press(&mut app, KeyCode::Enter);
        assert!(app
            .attach_prompt()
            .unwrap()
            .input
            .text()
            .starts_with("docs"));
        assert!(
            app.attach_prompt()
                .unwrap()
                .input
                .text()
                .ends_with(['/', '\\']),
            "inside the folder now"
        );
        assert!(pending(&app).is_none(), "a folder is never attached");
        // Missing: nothing matches, so FILE NOT FOUND and the prompt stays.
        typed(&mut app, "nothing.png");
        press(&mut app, KeyCode::Enter);
        assert_eq!(notice(&app), "FILE NOT FOUND");
        assert!(app.attach_prompt().is_some());
        // Enter with nothing typed does nothing.
        let mut app2 = plain_app();
        app2.set_attach_dir(dir.clone());
        ctrl(&mut app2, 'o');
        press(&mut app2, KeyCode::Enter);
        assert!(app2.attach_prompt().is_some() && pending(&app2).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn esc_closes_the_prompt_and_up_down_choose() {
        let dir = temp("esc");
        file(&dir, "a.txt", 3);
        file(&dir, "b.txt", 3);
        let mut app = plain_app();
        app.set_attach_dir(dir.clone());
        ctrl(&mut app, 'o');
        press(&mut app, KeyCode::Down);
        assert_eq!(app.attach_prompt().unwrap().sel, 1);
        press(&mut app, KeyCode::Down);
        assert_eq!(app.attach_prompt().unwrap().sel, 1, "stops at the last");
        press(&mut app, KeyCode::Up);
        assert_eq!(app.attach_prompt().unwrap().sel, 0);
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Tab);
        assert_eq!(app.attach_prompt().unwrap().input, "b.txt");
        press(&mut app, KeyCode::Backspace);
        assert_eq!(app.attach_prompt().unwrap().input, "b.tx");
        press(&mut app, KeyCode::Esc);
        assert!(app.attach_prompt().is_none());
        assert_eq!(
            app.active().focus,
            Focus::Composer,
            "back in the message box"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_pasted_quoted_path_goes_into_the_prompt_without_its_quotes() {
        let dir = temp("promptpaste");
        let f = file(&dir, "my notes.txt", 3);
        let mut app = plain_app();
        ctrl(&mut app, 'o');
        app.handle_paste(&format!("\"{}\"", f.display()));
        assert_eq!(app.attach_prompt().unwrap().input, f.display().to_string());
        press(&mut app, KeyCode::Enter);
        assert_eq!(pending(&app).unwrap().name, "my notes.txt");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_dropped_file_path_attaches_but_plain_text_does_not() {
        let dir = temp("drop");
        let f = file(&dir, "My Report.pdf", 10);
        let mut app = plain_app();
        app.handle_paste("see you at ten");
        assert!(pending(&app).is_none());
        assert_eq!(app.active().draft(), "see you at ten");
        app.handle_paste(&format!("\"{}\"", f.display()));
        let p = pending(&app).expect("attached, not typed");
        assert_eq!(
            (p.name.as_str(), p.kind),
            ("My Report.pdf", MediaKind::Document)
        );
        assert_eq!(
            app.active().draft(),
            "see you at ten",
            "the draft is untouched"
        );
        // A missing file or a folder is text.
        app.handle_paste(&dir.join("gone.png").display().to_string());
        assert!(app.active().draft().contains("gone.png"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn attaching_checks_the_file_before_anything_is_sent() {
        let dir = temp("checks");
        let mut app = plain_app();
        let chat = "a";
        let cases: Vec<(PathBuf, &str)> = vec![
            (dir.join("gone.png"), "FILE NOT FOUND"),
            (dir.clone(), "That is a folder: pick a file."),
            (
                file(&dir, "empty.txt", 0),
                "That file is empty: pick another.",
            ),
        ];
        for (path, want) in cases {
            app.attach_or_notice(Network::Teams, chat, path, false);
            assert_eq!(notice(&app), want);
            assert!(pending(&app).is_none());
        }
        // Teams image limit 4 MiB, Teams file 100 MiB (not written: only the image is).
        let big = dir.join("big.png");
        let mut data = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        data.resize(4 * MIB as usize + 1, 0);
        std::fs::write(&big, data).unwrap();
        app.attach_or_notice(Network::Teams, chat, big, false);
        assert!(
            notice(&app).starts_with("IMAGE OVER 4 MB for Teams"),
            "{}",
            notice(&app)
        );
        assert!(pending(&app).is_none());
        // A clipboard picture that is refused is trashed at once.
        let tmp = dir.join("paste-1.png");
        std::fs::write(&tmp, b"").unwrap();
        app.attach_or_notice(Network::Teams, chat, tmp.clone(), true);
        assert_eq!(app.take_trash(), vec![tmp]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_chip_is_removed_by_backspace_at_the_start_and_by_esc() {
        let dir = temp("chip");
        let f = png_file(&dir, "photo.png");
        let mut app = plain_app();
        app.attach_or_notice(Network::Teams, "a", f.clone(), false);
        assert!(pending(&app).is_some());
        // Backspace in the middle of text edits the text.
        typed(&mut app, "ab");
        press(&mut app, KeyCode::Backspace);
        assert_eq!(app.active().draft(), "a");
        assert!(pending(&app).is_some());
        // At the start of the box it removes the chip.
        press(&mut app, KeyCode::Home);
        press(&mut app, KeyCode::Backspace);
        assert!(pending(&app).is_none());
        assert_eq!(app.active().draft(), "a");
        // Esc removes it before it leaves the box.
        app.attach_or_notice(Network::Teams, "a", f, false);
        press(&mut app, KeyCode::Esc);
        assert!(pending(&app).is_none());
        assert_eq!(app.active().focus, Focus::Composer);
        press(&mut app, KeyCode::Esc);
        assert_eq!(app.active().focus, Focus::List);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn enter_sends_the_file_with_the_draft_as_its_caption() {
        let dir = temp("send");
        let f = png_file(&dir, "photo.png");
        let mut app = plain_app();
        app.attach_or_notice(Network::Teams, "a", f.clone(), false);
        typed(&mut app, "look at this");
        let out = press(&mut app, KeyCode::Enter);
        assert_eq!(out.len(), 1);
        let Command::SendMedia {
            chat_id,
            client_id,
            path,
            kind,
            caption,
        } = &out[0].1
        else {
            panic!("expected SendMedia, got {:?}", out[0].1);
        };
        assert_eq!(
            (chat_id.as_str(), path, *kind, caption.as_str()),
            ("a", &f, MediaKind::Image, "look at this")
        );
        // A pending message with a chip, and the box is empty again.
        let m = app.active().messages["a"]
            .iter()
            .find(|m| &m.id == client_id)
            .unwrap();
        assert_eq!(m.state, MessageState::Pending);
        assert_eq!(m.text, "[image] look at this");
        assert_eq!(m.attachments.len(), 1);
        assert_eq!(m.attachments[0].name.as_deref(), Some("photo.png"));
        assert!(m.attachments[0].fetch_ref.is_empty());
        assert!(pending(&app).is_none());
        assert_eq!(app.active().draft(), "");
        // Confirmed: the local message is replaced.
        let confirmed = ChatMessage {
            id: "real".into(),
            ..m.clone()
        };
        app.handle_event(Event::MessageConfirmed {
            client_id: client_id.clone(),
            message: confirmed,
        });
        assert!(app.active().messages["a"]
            .iter()
            .all(|m| &m.id != client_id));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_caption_over_the_limit_keeps_everything() {
        let dir = temp("caption");
        let f = png_file(&dir, "photo.png");
        let mut app = app_with(vec![]);
        app.attach_or_notice(Network::Teams, "a", f, false);
        app.handle_paste(&"x".repeat(chatcli_core::limits::TEAMS_TEXT_MAX_BYTES + 1));
        assert!(press(&mut app, KeyCode::Enter).is_empty());
        assert!(notice(&app).starts_with("MESSAGE TOO LONG"));
        assert!(pending(&app).is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_deleted_after_choosing_is_refused_at_send() {
        let dir = temp("vanish");
        let f = png_file(&dir, "photo.png");
        let mut app = plain_app();
        app.attach_or_notice(Network::Teams, "a", f.clone(), false);
        std::fs::remove_file(&f).unwrap();
        assert!(press(&mut app, KeyCode::Enter).is_empty());
        assert_eq!(notice(&app), "FILE NOT FOUND");
        assert!(pending(&app).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_teams_document_asks_about_onedrive_first() {
        let dir = temp("onedrive");
        let f = file(&dir, "plan.pdf", 100);
        let mut app = plain_app();
        app.attach_or_notice(Network::Teams, "a", f, false);
        assert!(
            press(&mut app, KeyCode::Enter).is_empty(),
            "nothing goes before the answer"
        );
        assert!(matches!(
            app.prompt(),
            Some(Prompt::SendFile {
                onedrive: true,
                executable: false,
                ..
            })
        ));
        // n keeps the chip and the draft.
        press(&mut app, KeyCode::Char('n'));
        assert!(app.prompt().is_none() && pending(&app).is_some());
        press(&mut app, KeyCode::Enter);
        let out = press(&mut app, KeyCode::Char('y'));
        assert!(matches!(
            out[0].1,
            Command::SendMedia {
                kind: MediaKind::Document,
                ..
            }
        ));
        assert!(pending(&app).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_image_to_teams_and_a_document_to_whatsapp_need_no_question() {
        let dir = temp("noask");
        let img = png_file(&dir, "photo.png");
        let mut app = plain_app();
        app.attach_or_notice(Network::Teams, "a", img, false);
        assert!(matches!(
            press(&mut app, KeyCode::Enter)[0].1,
            Command::SendMedia { .. }
        ));
        assert!(app.prompt().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_executable_is_asked_about_on_every_network() {
        let dir = temp("exe");
        let f = file(&dir, "setup.exe", 100);
        let mut app = plain_app();
        app.attach_or_notice(Network::Teams, "a", f, false);
        assert!(press(&mut app, KeyCode::Enter).is_empty());
        assert!(matches!(
            app.prompt(),
            Some(Prompt::SendFile {
                executable: true,
                ..
            })
        ));
        press(&mut app, KeyCode::Esc);
        assert!(app.prompt().is_none());
        assert!(pending(&app).is_some(), "Esc is no");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_clipboard_picture_is_a_temp_file_deleted_after_the_send() {
        let dir = temp("clip");
        let tmp = png_file(&dir, "paste-1.png");
        let mut app = plain_app();
        app.handle_key(key(
            KeyCode::Char('v'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        ));
        assert_eq!(
            app.take_clipboard_request(),
            Some((Network::Teams, "a".to_string()))
        );
        assert_eq!(app.take_clipboard_request(), None, "once");
        app.clipboard_done(Network::Teams, "a", Ok(tmp.clone()));
        let p = pending(&app).unwrap();
        assert!(p.temp && p.name == "pasted image" && p.path == tmp);
        let out = press(&mut app, KeyCode::Enter);
        let Command::SendMedia { client_id, .. } = &out[0].1 else {
            panic!("expected SendMedia");
        };
        assert!(
            app.take_trash().is_empty(),
            "the backend is still reading it"
        );
        app.handle_event(Event::SendProgress {
            network: Network::Teams,
            chat_id: "a".into(),
            client_id: client_id.clone(),
            done: 5,
            total: 10,
        });
        assert_eq!(app.active().progress.len(), 1);
        app.handle_event(Event::MessageFailed {
            network: Network::Teams,
            chat_id: "a".into(),
            client_id: client_id.clone(),
            error: "x".into(),
        });
        assert_eq!(app.take_trash(), vec![tmp]);
        assert!(app.active().progress.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cancelling_a_clipboard_picture_deletes_it_and_a_failed_read_is_a_notice() {
        let dir = temp("clipcancel");
        let tmp = png_file(&dir, "paste-2.png");
        let mut app = plain_app();
        app.clipboard_done(Network::Teams, "a", Ok(tmp.clone()));
        press(&mut app, KeyCode::Esc);
        assert_eq!(app.take_trash(), vec![tmp.clone()]);
        // Replacing one with another deletes the first.
        let tmp2 = png_file(&dir, "paste-3.png");
        app.clipboard_done(Network::Teams, "a", Ok(tmp2.clone()));
        let tmp3 = png_file(&dir, "paste-4.png");
        app.clipboard_done(Network::Teams, "a", Ok(tmp3));
        assert_eq!(app.take_trash(), vec![tmp2]);
        app.clipboard_done(
            Network::Teams,
            "a",
            Err("The clipboard holds no picture.".into()),
        );
        let n = app.notices().back().unwrap();
        assert!(n.is_error && n.text == "The clipboard holds no picture.");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn upload_progress_is_kept_until_the_send_is_over() {
        let dir = temp("progress");
        let f = png_file(&dir, "photo.png");
        let mut app = plain_app();
        let progress = |app: &mut App, id: &str, done: u64| {
            app.handle_event(Event::SendProgress {
                network: Network::Teams,
                chat_id: "a".into(),
                client_id: id.into(),
                done,
                total: 10,
            });
        };
        // Not a send of ours: dropped, or the footer would show it for ever.
        progress(&mut app, "stranger", 5);
        assert!(app.active().progress.is_empty());
        app.attach_or_notice(Network::Teams, "a", f, false);
        let out = press(&mut app, KeyCode::Enter);
        let Command::SendMedia { client_id, .. } = &out[0].1 else {
            panic!("expected SendMedia");
        };
        progress(&mut app, client_id, 5);
        assert_eq!(
            app.active().progress.get(client_id.as_str()),
            Some(&(5, 10))
        );
        progress(&mut app, client_id, 10);
        assert_eq!(
            app.active().progress.get(client_id.as_str()),
            Some(&(10, 10))
        );
        app.handle_event(Event::MessageConfirmed {
            client_id: client_id.clone(),
            message: with_attachments("real", "x", vec![]),
        });
        assert!(app.active().progress.is_empty());
        // A late event after the confirmation is dropped.
        progress(&mut app, client_id, 10);
        assert!(app.active().progress.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn attach_needs_an_open_chat_and_no_edit_in_progress() {
        let mut app = App::new(clock());
        ctrl(&mut app, 'o');
        assert!(app.attach_prompt().is_none());
        let mut app = plain_app();
        select(&mut app);
        press(&mut app, KeyCode::Char('E'));
        assert!(
            app.active().target().is_none(),
            "someone else's message: no edit"
        );
    }

    #[test]
    fn names_from_the_network_are_one_clean_line() {
        let mut m = with_attachments(
            "m1",
            "[image]",
            vec![attachment(MediaKind::Image, "a\x1b[31mb.png")],
        );
        super::super::clean_message(&mut m);
        assert_eq!(m.attachments[0].name.as_deref(), Some("a [31mb.png"));
    }

    #[test]
    fn ctl_defaults_are_the_documented_ones() {
        let c = Ctl::default();
        assert!(c.inline);
        assert_eq!(c.max_file_bytes, 25 * MIB);
        assert!(c.root.is_none());
    }
}
