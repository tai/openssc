//! Local↔remote file copy: enumeration, transfer loop and the receive worker.
//!
//! Verified wire sequence (`artifacts/filecopy-recon.md`): cmd 2
//! StartFileReceive, 100 ItemInfo, 101 NewItem per item, 102 FileData in
//! 65536-byte chunks, 104 EndSession. Directories are NewItem type 2 with
//! `level` = depth and bare names; symlinks are type 3 and are skipped on
//! both sides. The NewItem layout (size at +0x36, mode u16 at +0x66, nameLen
//! excluding NUL at +0x6c, name + NUL at +0x70) was recovered from a genuine
//! server-sent NewItem.
//!
//! Entry points: [`enumerate_drop`] + [`run_transfer`] for a drop, and
//! [`receive_worker`] for the server→viewer stream.

use anyhow::{bail, Context, Result};
use rfb_proto::filecopy as proto;
use rfb_proto::{pasteboard_items, pasteboard_text, ClipboardSend, PasteboardItem};
use sha1::{Digest, Sha1};
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{Receiver, Sender};

/// Data chunk size observed from the native client (65536 data bytes).
pub const CHUNK_SIZE: usize = 65536;

/// Destination folder the viewer asks the remote to write into. The native
/// client sends an absolute path (live sample `/Users/tai/Downloads`), so the
/// safe default is the same shape built from the authenticated remote user.
pub fn remote_destination(user: &str) -> String {
    format!("/Users/{user}/Downloads")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemKind {
    File,
    Dir,
}

impl ItemKind {
    fn type_byte(self) -> u8 {
        match self {
            ItemKind::File => proto::ITEM_TYPE_FILE,
            ItemKind::Dir => proto::ITEM_TYPE_DIR,
        }
    }
}

/// One item in a drop. Regular files carry a source path; directories don't.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DropItem {
    pub kind: ItemKind,
    pub source: Option<PathBuf>,
    /// Bare name on the remote (the parent comes from `level`).
    pub name: String,
    pub size: u64,
    /// POSIX mode including the file-type bits.
    pub mode: u32,
    /// 0 for the top-level item, +1 per directory level.
    pub level: u16,
}

/// A drop resolved into the items to send and the remote destination.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferPlan {
    pub destination: String,
    pub items: Vec<DropItem>,
    pub skipped_symlinks: Vec<PathBuf>,
}

impl TransferPlan {
    pub fn total_bytes(&self) -> u64 {
        self.items
            .iter()
            .filter(|item| item.kind == ItemKind::File)
            .map(|item| item.size)
            .sum()
    }

    pub fn file_count(&self) -> u32 {
        self.items
            .iter()
            .filter(|item| item.kind == ItemKind::File)
            .count() as u32
    }

    pub fn folder_count(&self) -> u32 {
        self.items
            .iter()
            .filter(|item| item.kind == ItemKind::Dir)
            .count() as u32
    }
}

#[cfg(unix)]
fn mode_of(metadata: &std::fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode()
}

#[cfg(not(unix))]
fn mode_of(_metadata: &std::fs::Metadata) -> u32 {
    0o100644
}

fn utf8_name(path: &Path) -> Result<String> {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(str::to_string)
        .ok_or_else(|| anyhow::anyhow!("file name is not valid UTF-8: {}", path.display()))
}

/// Resolve a winit `DroppedFile` path into a transfer plan.
///
/// Files become one level-0 item. Directories become a level-0 directory item
/// followed by a depth-first walk: every directory precedes its children, its
/// children are level+1, and empty directories are emitted. Symlinks (and
/// other special files) are skipped and reported; the remote receives only
/// regular files and real directories.
pub fn enumerate_drop(path: &Path) -> Result<TransferPlan> {
    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("stat dropped path {}", path.display()))?;
    if metadata.file_type().is_symlink() {
        bail!("symlink drops are not supported: {}", path.display());
    }
    let mut plan = TransferPlan {
        destination: String::new(),
        items: Vec::new(),
        skipped_symlinks: Vec::new(),
    };
    if metadata.is_file() {
        plan.items.push(DropItem {
            kind: ItemKind::File,
            source: Some(path.to_path_buf()),
            name: utf8_name(path)?,
            size: metadata.len(),
            mode: mode_of(&metadata),
            level: 0,
        });
        return Ok(plan);
    }
    if !metadata.is_dir() {
        bail!("dropped path is not a regular file or directory: {}", path.display());
    }
    plan.items.push(DropItem {
        kind: ItemKind::Dir,
        source: None,
        name: utf8_name(path)?,
        size: 0,
        mode: mode_of(&metadata),
        level: 0,
    });
    walk_dir(path, 1, &mut plan)?;
    Ok(plan)
}

/// Depth-first walk. The receiver keeps one FSRef slot per level
/// (`g+0x1ba + level*80`) and resolves a child's parent from the slot at
/// `level-1`, so the stream must be DFS-ordered: a directory item, then its
/// descendants, before the next sibling. Pre-order satisfies that; files and
/// subdirectories may be interleaved as long as no sibling is visited between
/// a directory and its children.
fn walk_dir(dir: &Path, level: u16, plan: &mut TransferPlan) -> Result<()> {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .with_context(|| format!("read directory {}", dir.display()))?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .collect();
    entries.sort();
    for entry in entries {
        let metadata = std::fs::symlink_metadata(&entry)
            .with_context(|| format!("stat {}", entry.display()))?;
        let file_type = metadata.file_type();
        if file_type.is_symlink() {
            // Security: never recreate a symlink on the remote.
            plan.skipped_symlinks.push(entry);
            continue;
        }
        let name = utf8_name(&entry)?;
        if file_type.is_dir() {
            plan.items.push(DropItem {
                kind: ItemKind::Dir,
                source: None,
                name,
                size: 0,
                mode: mode_of(&metadata),
                level,
            });
            walk_dir(&entry, level.saturating_add(1), plan)?;
        } else if file_type.is_file() {
            plan.items.push(DropItem {
                kind: ItemKind::File,
                source: Some(entry),
                name,
                size: metadata.len(),
                mode: mode_of(&metadata),
                level,
            });
        } else {
            // FIFOs, sockets, devices: not part of a file copy.
            plan.skipped_symlinks.push(entry);
        }
    }
    Ok(())
}

/// Semantic file-copy messages. Implementations own the byte layouts; this
/// module only knows the verified order and the data chunk size.
pub trait FileCopySender {
    fn start_file_receive(&mut self, destination: &str) -> Result<()>;
    fn item_info(
        &mut self,
        logical_bytes: u64,
        physical_bytes: u64,
        file_count: u32,
        folder_count: u32,
    ) -> Result<()>;
    fn new_item(
        &mut self,
        level: u32,
        item_type: u8,
        name: &str,
        mode: u32,
        size: u64,
    ) -> Result<()>;
    fn file_data(&mut self, offset: u64, data: &[u8]) -> Result<()>;
    fn end_session(&mut self) -> Result<()>;
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct TransferStats {
    pub files: u32,
    pub folders: u32,
    pub bytes: u64,
    pub chunks: u32,
}

/// Drive the verified sequence for one plan.
pub fn run_transfer<S: FileCopySender>(
    sender: &mut S,
    plan: &TransferPlan,
) -> Result<TransferStats> {
    let total = plan.total_bytes();
    let physical: u64 = plan
        .items
        .iter()
        .filter_map(|item| item.source.as_deref())
        .map(physical_size)
        .sum();
    sender.start_file_receive(&plan.destination)?;
    sender.item_info(
        total,
        physical,
        plan.file_count(),
        plan.folder_count(),
    )?;

    let mut stats = TransferStats::default();
    for item in &plan.items {
        sender.new_item(
            item.level as u32,
            item.kind.type_byte(),
            &item.name,
            item.mode,
            item.size,
        )?;
        if item.kind == ItemKind::Dir {
            stats.folders += 1;
            continue;
        }
        let source = item
            .source
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("file item {} has no source", item.name))?;
        let mut handle = File::open(source)
            .with_context(|| format!("open dropped file {}", source.display()))?;
        let mut buffer = vec![0u8; CHUNK_SIZE];
        let mut offset = 0u64;
        loop {
            let n = handle
                .read(&mut buffer)
                .with_context(|| format!("read {}", source.display()))?;
            if n == 0 {
                break;
            }
            sender.file_data(offset, &buffer[..n])?;
            offset += n as u64;
            stats.chunks += 1;
            stats.bytes += n as u64;
        }
        if offset != item.size {
            bail!(
                "{} changed size during transfer (planned {}, read {})",
                source.display(),
                item.size,
                offset
            );
        }
        stats.files += 1;
    }
    sender.end_session()?;
    Ok(stats)
}

/// Physical (allocated) size hint for ItemInfo.
fn physical_size(path: &Path) -> u64 {
    let Ok(meta) = std::fs::metadata(path) else {
        return 0;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let blocks = meta.blocks();
        if blocks > 0 {
            return blocks * 512;
        }
    }
    meta.len().div_ceil(4096) * 4096
}

/// Monotonic session id for transfers we start.
pub fn next_session() -> u32 {
    static NEXT: AtomicU32 = AtomicU32::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// A file-copy sender that encodes onto the RFB writer channel.
pub struct NetworkSender {
    tx: Sender<Vec<u8>>,
    session: u32,
    verbose: bool,
    hash: Sha1,
    bytes: u64,
}

impl NetworkSender {
    pub fn new(tx: Sender<Vec<u8>>, session: u32, verbose: bool) -> Self {
        Self {
            tx,
            session,
            verbose,
            hash: Sha1::new(),
            bytes: 0,
        }
    }

    fn send(&mut self, message: Vec<u8>) -> Result<()> {
        // One record per file-copy message: the writer channel is shared with
        // framebuffer traffic, so splitting the transport header and body into
        // two records would let another producer interleave between them.
        if self.verbose {
            eprintln!("filecopy: tx {} bytes (session {})", message.len(), self.session);
        }
        self.tx
            .send(message)
            .map_err(|_| anyhow::anyhow!("session writer closed"))
    }

    pub fn sha1_hex(&self) -> String {
        hex_digest(&self.hash.clone().finalize())
    }
}

impl FileCopySender for NetworkSender {
    fn start_file_receive(&mut self, destination: &str) -> Result<()> {
        let message = proto::start_file_receive(destination, self.session);
        self.send(message)?;
        // The native client leaves ~18 ms between StartFileReceive and
        // ItemInfo; the daemon spawns the receiver in that window.
        std::thread::sleep(std::time::Duration::from_millis(20));
        Ok(())
    }

    fn item_info(
        &mut self,
        logical_bytes: u64,
        physical_bytes: u64,
        file_count: u32,
        folder_count: u32,
    ) -> Result<()> {
        let message = proto::item_info(
            self.session,
            logical_bytes,
            physical_bytes,
            file_count as u64,
            0,
            folder_count as u64,
            0,
        );
        self.send(message)
    }

    fn new_item(
        &mut self,
        level: u32,
        item_type: u8,
        name: &str,
        mode: u32,
        size: u64,
    ) -> Result<()> {
        let message = proto::new_item(
            self.session,
            level as u16,
            item_type,
            name,
            mode,
            size,
        );
        self.send(message)
    }

    fn file_data(&mut self, _offset: u64, data: &[u8]) -> Result<()> {
        self.hash.update(data);
        self.bytes += data.len() as u64;
        let message = proto::file_data(self.session, data);
        self.send(message)
    }

    fn end_session(&mut self) -> Result<()> {
        let message = proto::end_session(self.session, 0);
        self.send(message)
    }
}

/// Enumerate + send one dropped path. Runs on a worker thread.
pub fn send_file(
    tx: Sender<Vec<u8>>,
    source: PathBuf,
    destination: String,
    verbose: bool,
) -> Result<()> {
    let mut plan = enumerate_drop(&source)?;
    plan.destination = destination;
    let session = next_session();
    eprintln!(
        "filecopy: sending {} -> {} (session {session}, {} file(s), {} folder(s), {} bytes, {} symlink(s) skipped)",
        source.display(),
        plan.destination,
        plan.file_count(),
        plan.folder_count(),
        plan.total_bytes(),
        plan.skipped_symlinks.len()
    );
    for skipped in &plan.skipped_symlinks {
        eprintln!("filecopy: skipped symlink/special {}", skipped.display());
    }
    let mut sender = NetworkSender::new(tx, session, verbose);
    match run_transfer(&mut sender, &plan) {
        Ok(stats) => {
            eprintln!(
                "filecopy: sent {} file(s), {} folder(s), {} bytes, {} chunk(s), sha1={} dest={}",
                stats.files,
                stats.folders,
                stats.bytes,
                stats.chunks,
                sender.sha1_hex(),
                plan.destination
            );
            Ok(())
        }
        Err(error) => {
            // Best effort: close the remote session so the receiver does not
            // wait forever.
            let _ = NetworkSender::send(&mut sender, proto::end_session(session, 1));
            Err(error)
        }
    }
}

fn hex_digest(digest: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// A file path extracted from the remote clipboard for an explicit pull.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullSource {
    /// Absolute POSIX path on the remote.
    pub path: String,
}

/// Percent-decode `%XX` escapes. `None` on a malformed escape or bad UTF-8.
pub fn percent_decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16)?;
            let lo = (bytes[i + 2] as char).to_digit(16)?;
            out.push(((hi << 4) | lo) as u8);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// A single-line absolute POSIX path.
pub fn is_absolute_path(text: &str) -> bool {
    let t = text.trim();
    t.starts_with('/') && !t.contains('\n') && !t.contains('\r') && !t.contains('\0')
}

/// Decode one flavour's bytes as text, when its UTI or aliases say it is text.
fn decode_text_flavour(item: &PasteboardItem) -> Option<String> {
    let uti = item.uti.as_str();
    let ostype = item.alias("com.apple.ostype").unwrap_or("");
    if uti.contains("utf16") || ostype == "utxt" || ostype == "ut16" {
        // `public.utf16-external-plain-text` carries a BOM; the plain utf16
        // flavour in these archives is big-endian without one.
        let units: Vec<u16> = match item.data.get(..2) {
            Some([0xfe, 0xff]) => item.data[2..]
                .chunks_exact(2)
                .map(|c| u16::from_be_bytes([c[0], c[1]]))
                .collect(),
            Some([0xff, 0xfe]) => item.data[2..]
                .chunks_exact(2)
                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                .collect(),
            _ => item
                .data
                .chunks_exact(2)
                .map(|c| u16::from_be_bytes([c[0], c[1]]))
                .collect(),
        };
        return Some(String::from_utf16_lossy(&units));
    }
    if uti.contains("traditional-mac") || ostype == "TEXT" {
        return Some(item.data.iter().map(|&b| b as char).collect());
    }
    if uti.contains("utf8")
        || uti.contains("utf-8")
        || uti.contains("plain-text")
        || uti == "public.text"
        || ostype == "utf8"
    {
        return Some(String::from_utf8_lossy(&item.data).to_string());
    }
    None
}

/// The text-side choice for a received archive, plus why it won.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalClipboard {
    /// Value to write to the local clipboard.
    pub text: String,
    /// Winning flavour, e.g. `public.utf8-plain-text item 1, path-like`.
    pub source: String,
}

/// Pick the local-clipboard value from every flavour in the archive.
///
/// The native client puts the whole flavour set on the pasteboard and lets
/// macOS resolve it at paste time; we can only write one string, so:
///
/// 1. any text-ish flavour whose content is an absolute path (after percent
///    decoding) wins, whatever its UTI and position. Finder's Option+Cmd+C
///    ("Copy as Pathname") sends three text items, all the absolute path;
/// 2. otherwise today's behaviour: the first plain-text flavour (a Finder
///    plain copy lands here, carrying only the item name).
///
/// There is no file-URL stage: the agent strips `furl` when it composes the
/// archive, so a URL can never reach a client.
fn choose_text(items: &[PasteboardItem], archive: &[u8]) -> Result<Option<LocalClipboard>> {
    for (index, item) in items.iter().enumerate() {
        if item.data.is_empty() {
            continue;
        }
        let Some(text) = decode_text_flavour(item) else {
            continue;
        };
        let trimmed = text.trim();
        if !is_absolute_path(trimmed) {
            continue;
        }
        let decoded = percent_decode(trimmed).unwrap_or_else(|| trimmed.to_string());
        let value = if is_absolute_path(&decoded) {
            decoded
        } else {
            trimmed.to_string()
        };
        return Ok(Some(LocalClipboard {
            text: value,
            source: format!("{} item {}, path-like", item.uti, index + 1),
        }));
    }
    Ok(pasteboard_text(archive)?
        .filter(|text| !text.trim().is_empty())
        .map(|text| LocalClipboard {
            text,
            source: "first plain-text flavour (unchanged)".to_string(),
        }))
}

/// Choose the local-clipboard content for a received archive.
///
/// Images are handled separately by the caller; this covers the text side and
/// never returns a basename when a flavour carries the absolute path.
pub fn local_clipboard(archive: &[u8]) -> Result<Option<LocalClipboard>> {
    let items = pasteboard_items(archive)?;
    choose_text(&items, archive)
}

/// Find the first pullable path in a pasteboard archive.
///
/// This server's agent strips `furl` from the archive, so the only path a
/// client can see is one that is already TEXT on the remote clipboard (for
/// example Finder's "Copy as Pathname"). Anything else is refused.
pub fn pull_source(archive: &[u8]) -> Result<Option<PullSource>> {
    let items = pasteboard_items(archive)?;
    let Some(choice) = choose_text(&items, archive)? else {
        return Ok(None);
    };
    Ok(is_absolute_path(&choice.text).then_some(PullSource { path: choice.text }))
}

/// One-line description of what an archive actually held, for refusal logs.
///
/// The point is to make a failure self-diagnosing: the remote clipboard may
/// carry the basename (`clipboard holds "rf_b.txt"`), something else entirely,
/// or nothing at all (`clipboard archive was empty (0 items)`).
pub fn describe_archives(archives: &[Vec<u8>]) -> String {
    let mut items = Vec::new();
    for archive in archives {
        if let Ok(parsed) = pasteboard_items(archive) {
            items.extend(parsed);
        }
    }
    if items.is_empty() {
        return "clipboard archive was empty (0 items)".to_string();
    }
    let mut texts: Vec<String> = Vec::new();
    let mut non_text = 0usize;
    for item in &items {
        if item.data.is_empty() {
            continue;
        }
        match decode_text_flavour(item) {
            Some(text) => {
                let text = text.trim();
                if !text.is_empty() && !texts.iter().any(|t| t == text) {
                    texts.push(text.to_string());
                }
            }
            None => non_text += 1,
        }
    }
    if texts.is_empty() {
        return format!(
            "clipboard holds {} item(s), none of them text",
            items.len()
        );
    }
    let shown: Vec<String> = texts.iter().take(3).map(|t| format!("{t:?}")).collect();
    let mut out = format!("clipboard holds {}", shown.join(", "));
    if texts.len() > shown.len() {
        out.push_str(&format!(" (+{} more)", texts.len() - shown.len()));
    }
    if non_text > 0 {
        out.push_str(&format!(" + {non_text} non-text item(s)"));
    }
    out.push_str(" (no path)");
    out
}

/// The same description for the raw messages of one update.
pub fn describe_sends(sends: &[ClipboardSend]) -> String {
    let mut archives = Vec::new();
    for send in sends {
        if send.uncompressed_size == 0 {
            continue;
        }
        if let Ok(archive) = send.archive() {
            if !archive.is_empty() {
                archives.push(archive);
            }
        }
    }
    describe_archives(&archives)
}

/// Resolve a pull from the ClipboardSend messages in one update.
///
/// `Empty` means every archive was a promise/empty (typically the viewer's own
/// pasteboard while AutoPasteboard is active); `NoFile` means data arrived but
/// it contained no path-shaped text.
pub enum PullOutcome {
    File(PullSource),
    Empty,
    NoFile,
}

pub fn pull_from_sends(sends: &[ClipboardSend]) -> Result<PullOutcome> {
    let mut archives = Vec::new();
    for send in sends {
        if send.uncompressed_size == 0 {
            continue;
        }
        let archive = send.archive()?;
        if archive.is_empty() {
            continue;
        }
        archives.push(archive);
    }
    pull_from_archives(&archives)
}

/// The same decision as [`pull_from_sends`], on already-inflated archives.
pub fn pull_from_archives(archives: &[Vec<u8>]) -> Result<PullOutcome> {
    // A zero-item archive is a promise (no data), not a failed match.
    let mut usable = Vec::new();
    for archive in archives {
        if !pasteboard_items(archive)?.is_empty() {
            usable.push(archive);
        }
    }
    for archive in &usable {
        if let Some(source) = pull_source(archive)? {
            return Ok(PullOutcome::File(source));
        }
    }
    if usable.is_empty() {
        return Ok(PullOutcome::Empty);
    }
    // Diagnose the common Finder case: plain Cmd+C puts only the item NAME in
    // the text flavours, and the path exists solely as a `furl` the agent
    // strips before it reaches any client.
    for archive in &usable {
        if let Ok(items) = pasteboard_items(archive) {
            for item in items {
                match decode_text_flavour(&item) {
                    Some(text) => eprintln!(
                        "pull-from-remote: no path in {:?} ({} bytes) {:?}",
                        item.uti,
                        item.data.len(),
                        text.chars().take(80).collect::<String>()
                    ),
                    None => eprintln!(
                        "pull-from-remote: no path in {:?} ({} bytes, not text)",
                        item.uti,
                        item.data.len()
                    ),
                }
            }
        }
    }
    Ok(PullOutcome::NoFile)
}

/// Build a pasteboard archive for tests.
#[cfg(test)]
fn test_archive(items: &[(&str, &str)]) -> Vec<u8> {
    let items: Vec<rfb_proto::PasteboardItem> = items
        .iter()
        .map(|(uti, data)| rfb_proto::PasteboardItem {
            uti: (*uti).to_string(),
            aliases: Vec::new(),
            data: data.as_bytes().to_vec(),
        })
        .collect();
    rfb_proto::pasteboard_archive(&items)
}

/// Finder-style unique name: `report.pdf`, `report 2.pdf`, `report 3.pdf`.
pub fn unique_path(directory: &Path, name: &str) -> PathBuf {
    let candidate = directory.join(name);
    if !candidate.exists() {
        return candidate;
    }
    let (stem, extension) = match name.rsplit_once('.') {
        Some((stem, extension)) if !stem.is_empty() && !extension.is_empty() => {
            (stem, Some(extension))
        }
        _ => (name, None),
    };
    for counter in 2..10_000 {
        let candidate_name = match extension {
            Some(extension) => format!("{stem} {counter}.{extension}"),
            None => format!("{name} {counter}"),
        };
        let candidate = directory.join(candidate_name);
        if !candidate.exists() {
            return candidate;
        }
    }
    directory.join(name)
}

/// One safe path component from a peer-supplied item name.
///
/// Rejects anything that is not exactly one normal component: absolute paths,
/// separators (`/`, `\`), `.`/`..`, empty names, and embedded NULs. This is
/// the receive-side security boundary, not a cosmetic check.
pub fn safe_component(name: &str) -> Option<&str> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.contains('/')
        || name.contains('\\')
        || name.contains('\0')
    {
        return None;
    }
    let mut components = Path::new(name).components();
    match (components.next(), components.next()) {
        (Some(Component::Normal(part)), None) => part.to_str(),
        _ => None,
    }
}

/// Join a peer name under `parent`, validating the component. The result is
/// guaranteed to be inside `parent`.
pub fn safe_join(parent: &Path, name: &str) -> Option<PathBuf> {
    let component = safe_component(name)?;
    Some(parent.join(component))
}

struct Incoming {
    file: File,
    path: PathBuf,
    bytes: u64,
    hash: Sha1,
}

fn finish_incoming(current: &mut Option<Incoming>) {
    if let Some(incoming) = current.take() {
        let _ = incoming.file.sync_all();
        drop(incoming.file);
        eprintln!(
            "filecopy: received {} ({} bytes, sha1={})",
            incoming.path.display(),
            incoming.bytes,
            hex_digest(&incoming.hash.finalize())
        );
    }
}

/// Write a server→viewer file-copy stream into `out_dir`.
///
/// Runs on a worker thread. Directories are created and tracked by `level`;
/// files are written into the most recent directory with a smaller level.
/// A peer-supplied name that is not one safe component is rejected (and a
/// rejected directory swallows its descendants). Symlinks (type 3) are
/// skipped. Top-level name collisions get the Finder-style " 2" suffix, so a
/// re-received folder keeps its whole tree together.
pub fn receive_worker(events: Receiver<proto::FileCopyEvent>, out_dir: PathBuf, verbose: bool) {
    if let Err(error) = std::fs::create_dir_all(&out_dir) {
        eprintln!(
            "filecopy: cannot create receive directory {}: {error}",
            out_dir.display()
        );
        return;
    }
    let mut current: Option<Incoming> = None;
    // (level, Some(path)) for a real directory, None for a rejected one.
    let mut folders: Vec<(u16, Option<PathBuf>)> = Vec::new();

    for event in events {
        match event {
            proto::FileCopyEvent::ItemInfo {
                total_size,
                file_count,
                folder_count,
                ..
            } => {
                eprintln!(
                    "filecopy: incoming {total_size} bytes, {file_count} file(s), {folder_count} folder(s)"
                );
            }
            proto::FileCopyEvent::NewItem {
                level,
                item_type,
                name,
                ..
            } => {
                finish_incoming(&mut current);
                let parent = folders
                    .iter()
                    .rev()
                    .find(|(folder_level, _)| *folder_level < level)
                    .map(|(_, path)| path.clone())
                    .unwrap_or(Some(out_dir.clone()));
                let Some(parent) = parent else {
                    eprintln!(
                        "filecopy: skipping {name:?} at level {level} inside a rejected directory"
                    );
                    if item_type == proto::ITEM_TYPE_DIR {
                        folders.retain(|(folder_level, _)| *folder_level < level);
                        folders.push((level, None));
                    }
                    continue;
                };
                let Some(component) = safe_component(&name) else {
                    eprintln!(
                        "filecopy: rejecting unsafe item name {name:?} at level {level}"
                    );
                    if item_type == proto::ITEM_TYPE_DIR {
                        folders.retain(|(folder_level, _)| *folder_level < level);
                        folders.push((level, None));
                    }
                    continue;
                };
                let Some(base) = safe_join(&parent, component) else {
                    continue;
                };
                if !base.starts_with(&out_dir) {
                    eprintln!(
                        "filecopy: rejecting path outside the receive directory: {}",
                        base.display()
                    );
                    continue;
                }
                let target = unique_path(&parent, component);
                match item_type {
                    proto::ITEM_TYPE_FILE => match File::create(&target) {
                        Ok(file) => {
                            if verbose {
                                eprintln!(
                                    "filecopy: new file level={level} {name:?} -> {}",
                                    target.display()
                                );
                            }
                            current = Some(Incoming {
                                file,
                                path: target,
                                bytes: 0,
                                hash: Sha1::new(),
                            });
                        }
                        Err(error) => {
                            eprintln!("filecopy: create {}: {error}", target.display());
                        }
                    },
                    proto::ITEM_TYPE_DIR => match std::fs::create_dir_all(&target) {
                        Ok(()) => {
                            folders.retain(|(folder_level, _)| *folder_level < level);
                            if verbose {
                                eprintln!(
                                    "filecopy: new folder level={level} {name:?} -> {}",
                                    target.display()
                                );
                            }
                            folders.push((level, Some(target)));
                        }
                        Err(error) => {
                            eprintln!("filecopy: mkdir {}: {error}", target.display());
                            folders.retain(|(folder_level, _)| *folder_level < level);
                            folders.push((level, None));
                        }
                    },
                    proto::ITEM_TYPE_SYMLINK => {
                        eprintln!(
                            "filecopy: skipping symlink {name:?} at level {level} (not recreated)"
                        );
                    }
                    other => {
                        eprintln!(
                            "filecopy: skipping unknown item type {other} ({name:?}, level {level})"
                        );
                    }
                }
            }
            proto::FileCopyEvent::FileData { data, .. } => {
                if let Some(incoming) = current.as_mut() {
                    if let Err(error) = incoming.file.write_all(&data) {
                        eprintln!("filecopy: write {}: {error}", incoming.path.display());
                    } else {
                        incoming.hash.update(&data);
                        incoming.bytes += data.len() as u64;
                        if verbose {
                            eprintln!("filecopy: {} bytes so far", incoming.bytes);
                        }
                    }
                } else if verbose {
                    eprintln!("filecopy: data with no open item ({} bytes)", data.len());
                }
            }
            proto::FileCopyEvent::EndSession { error, .. } => {
                finish_incoming(&mut current);
                if error != 0 {
                    eprintln!("filecopy: session ended with error {error}");
                }
            }
            proto::FileCopyEvent::Unknown { cmd, payload_len, .. } => {
                if cmd == proto::CMD_COMPRESSED_FILE_DATA {
                    eprintln!(
                        "filecopy: compressed chunk (cmd 103) is not supported yet; aborting item"
                    );
                    finish_incoming(&mut current);
                } else if verbose {
                    eprintln!("filecopy: unknown frame cmd={cmd} len={payload_len}");
                }
            }
        }
    }
    finish_incoming(&mut current);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use std::sync::atomic::{AtomicU32, Ordering};

    static NEXT: AtomicU32 = AtomicU32::new(0);

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ss-filecopy-{tag}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_file(path: &Path, data: &[u8]) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let mut file = File::create(path).unwrap();
        file.write_all(data).unwrap();
    }

    #[derive(Default)]
    struct RecordingSender {
        calls: Vec<String>,
    }

    impl FileCopySender for RecordingSender {
        fn start_file_receive(&mut self, destination: &str) -> Result<()> {
            self.calls.push(format!("start:{destination}"));
            Ok(())
        }

        fn item_info(
            &mut self,
            logical: u64,
            _physical: u64,
            files: u32,
            folders: u32,
        ) -> Result<()> {
            self.calls.push(format!("info:{logical}:{files}:{folders}"));
            Ok(())
        }

        fn new_item(
            &mut self,
            level: u32,
            item_type: u8,
            name: &str,
            _mode: u32,
            _size: u64,
        ) -> Result<()> {
            self.calls.push(format!("item:{level}:{item_type}:{name}"));
            Ok(())
        }

        fn file_data(&mut self, offset: u64, data: &[u8]) -> Result<()> {
            self.calls.push(format!("data:{offset}:{}", data.len()));
            Ok(())
        }

        fn end_session(&mut self) -> Result<()> {
            self.calls.push("end".to_string());
            Ok(())
        }
    }

    #[test]
    fn enumerate_single_file() {
        let dir = temp_dir("single");
        let path = dir.join("one.bin");
        write_file(&path, b"abc");
        let plan = enumerate_drop(&path).unwrap();
        assert_eq!(plan.items.len(), 1);
        assert_eq!(plan.items[0].kind, ItemKind::File);
        assert_eq!(plan.items[0].level, 0);
        assert_eq!(plan.items[0].name, "one.bin");
        assert_eq!(plan.items[0].size, 3);
        assert!(plan.destination.is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn enumerate_tree_levels_and_symlink_skip() {
        let dir = temp_dir("tree");
        let root = dir.join("tree");
        write_file(&root.join("a.bin"), b"aaaa");
        write_file(&root.join("sub/b.bin"), b"bb");
        write_file(&root.join("sub/deep/c.bin"), b"c");
        std::fs::create_dir_all(root.join("empty")).unwrap();
        symlink("a.bin", root.join("link")).unwrap();

        let plan = enumerate_drop(&root).unwrap();
        let names: Vec<_> = plan
            .items
            .iter()
            .map(|item| (item.level, item.kind, item.name.as_str()))
            .collect();
        assert_eq!(
            names,
            vec![
                (0, ItemKind::Dir, "tree"),
                (1, ItemKind::File, "a.bin"),
                (1, ItemKind::Dir, "empty"),
                (1, ItemKind::Dir, "sub"),
                (2, ItemKind::File, "b.bin"),
                (2, ItemKind::Dir, "deep"),
                (3, ItemKind::File, "c.bin"),
            ]
        );
        assert_eq!(plan.file_count(), 3);
        assert_eq!(plan.folder_count(), 4);
        assert_eq!(plan.total_bytes(), 7);
        assert_eq!(plan.skipped_symlinks.len(), 1);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn transfer_sequence_and_chunking() {
        let dir = temp_dir("chunk");
        let path = dir.join("big.bin");
        let data = vec![0xABu8; 150_000];
        write_file(&path, &data);
        let mut plan = enumerate_drop(&path).unwrap();
        plan.destination = "/Users/tai/Downloads".to_string();
        let mut sender = RecordingSender::default();
        let stats = run_transfer(&mut sender, &plan).unwrap();
        assert_eq!(
            stats,
            TransferStats {
                files: 1,
                folders: 0,
                bytes: 150_000,
                chunks: 3
            }
        );
        let calls = sender.calls;
        assert_eq!(calls[0], "start:/Users/tai/Downloads");
        assert_eq!(calls[1], "info:150000:1:0");
        assert!(calls[2].starts_with("item:0:1:big.bin"), "{}", calls[2]);
        assert!(calls[3].starts_with("data:0:65536"), "{}", calls[3]);
        assert!(calls[4].starts_with("data:65536:65536"), "{}", calls[4]);
        assert!(calls[5].starts_with("data:131072:18928"), "{}", calls[5]);
        assert_eq!(calls[6], "end");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn sender_order_is_dfs_even_when_a_file_sorts_after_a_directory() {
        let dir = temp_dir("dfs");
        let root = dir.join("tree");
        // Lexicographic order: "Alpha" (dir), "Beta" (dir), "zeta.bin" (file).
        write_file(&root.join("Alpha/inner.bin"), b"i");
        write_file(&root.join("Beta/deep.bin"), b"d");
        write_file(&root.join("zeta.bin"), b"z");
        let plan = enumerate_drop(&root).unwrap();
        let order: Vec<_> = plan
            .items
            .iter()
            .map(|item| (item.level, item.name.as_str()))
            .collect();
        assert_eq!(
            order,
            vec![
                (0, "tree"),
                (1, "Alpha"),
                (2, "inner.bin"),
                (1, "Beta"),
                (2, "deep.bin"),
                (1, "zeta.bin"),
            ]
        );
        // zeta.bin at level 1 still resolves its parent from slot[0].
        assert_eq!(plan.items.last().unwrap().level, 1);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn receive_worker_accepts_native_fts_order() {
        // The native sender emits directories pre-order but files after the
        // subdirectories (captured order for /Users/tai/fc-dir):
        // root, empty, sub, deep, c.bin, b.bin, a.bin, link.
        let dir = temp_dir("fts");
        let out = dir.join("Downloads");
        let worker_out = out.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || receive_worker(rx, worker_out, false));
        let item = |level: u16, item_type: u8, name: &str| proto::FileCopyEvent::NewItem {
            session: 1,
            level,
            item_type,
            name: name.to_string(),
        };
        tx.send(proto::FileCopyEvent::ItemInfo {
            session: 1,
            total_size: 7,
            physical_size: 4096,
            file_count: 3,
            fork_count: 0,
            folder_count: 3,
        })
        .unwrap();
        tx.send(item(0, proto::ITEM_TYPE_DIR, "tree")).unwrap();
        tx.send(item(1, proto::ITEM_TYPE_DIR, "empty")).unwrap();
        tx.send(item(1, proto::ITEM_TYPE_DIR, "sub")).unwrap();
        tx.send(item(2, proto::ITEM_TYPE_DIR, "deep")).unwrap();
        tx.send(item(3, proto::ITEM_TYPE_FILE, "c.bin")).unwrap();
        tx.send(proto::FileCopyEvent::FileData {
            session: 1,
            data: b"c".to_vec(),
        })
        .unwrap();
        tx.send(item(2, proto::ITEM_TYPE_FILE, "b.bin")).unwrap();
        tx.send(proto::FileCopyEvent::FileData {
            session: 1,
            data: b"bb".to_vec(),
        })
        .unwrap();
        tx.send(item(1, proto::ITEM_TYPE_FILE, "a.bin")).unwrap();
        tx.send(proto::FileCopyEvent::FileData {
            session: 1,
            data: b"aaaa".to_vec(),
        })
        .unwrap();
        tx.send(item(1, proto::ITEM_TYPE_SYMLINK, "link")).unwrap();
        tx.send(proto::FileCopyEvent::EndSession {
            session: 1,
            error: 0,
        })
        .unwrap();
        drop(tx);
        worker.join().unwrap();
        assert_eq!(std::fs::read(out.join("tree/a.bin")).unwrap(), b"aaaa");
        assert_eq!(std::fs::read(out.join("tree/sub/b.bin")).unwrap(), b"bb");
        assert_eq!(
            std::fs::read(out.join("tree/sub/deep/c.bin")).unwrap(),
            b"c"
        );
        assert!(out.join("tree/empty").is_dir());
        assert!(!out.join("tree/link").exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn pathname_archive_uses_the_absolute_path() {
        // pD's Option+Cmd+C archive: three text items, all the absolute path,
        // no file-url flavour.
        let archive = test_archive(&[
            ("public.utf8-plain-text", "/Users/me/Desktop/for-md/Makefile"),
            (
                "com.apple.traditional-mac-plain-text",
                "/Users/me/Desktop/for-md/Makefile",
            ),
            ("public.utf16-plain-text", "/Users/me/Desktop/for-md/Makefile"),
        ]);
        let choice = local_clipboard(&archive).unwrap().unwrap();
        assert_eq!(choice.text, "/Users/me/Desktop/for-md/Makefile");
        assert_eq!(choice.source, "public.utf8-plain-text item 1, path-like");
        match pull_from_archives(&[archive]).unwrap() {
            PullOutcome::File(source) => {
                assert_eq!(source.path, "/Users/me/Desktop/for-md/Makefile");
            }
            _ => panic!("expected the pathname to be pullable"),
        }
    }

    #[test]
    fn plain_copy_keeps_the_basename_and_is_not_pullable() {
        // pD's plain Cmd+C archive: basename text plus an icon blob; the path
        // is only in the remote pasteboard's `furl`, which the agent strips.
        let archive = test_archive(&[
            ("public.utf16-external-plain-text", "Makefile"),
            ("public.utf8-plain-text", "Makefile"),
            ("com.apple.icns", "icon"),
            ("public.utf16-plain-text", "Makefile"),
            ("com.apple.traditional-mac-plain-text", "Makefile"),
        ]);
        let choice = local_clipboard(&archive).unwrap().unwrap();
        assert_eq!(choice.text, "Makefile");
        assert_eq!(choice.source, "first plain-text flavour (unchanged)");
        assert!(matches!(
            pull_from_archives(&[archive]).unwrap(),
            PullOutcome::NoFile
        ));
    }

    #[test]
    fn folder_pathname_pulls_the_folder_path() {
        let archive = test_archive(&[
            ("public.utf8-plain-text", "/Users/me/Desktop/for-md/pullfolder"),
            (
                "com.apple.traditional-mac-plain-text",
                "/Users/me/Desktop/for-md/pullfolder",
            ),
            (
                "public.utf16-plain-text",
                "/Users/me/Desktop/for-md/pullfolder",
            ),
        ]);
        assert_eq!(
            local_clipboard(&archive).unwrap().unwrap().text,
            "/Users/me/Desktop/for-md/pullfolder"
        );
        match pull_from_archives(&[archive]).unwrap() {
            PullOutcome::File(source) => {
                assert_eq!(source.path, "/Users/me/Desktop/for-md/pullfolder")
            }
            _ => panic!("expected the folder pathname to be pullable"),
        }
    }

    #[test]
    fn refusal_describes_what_the_clipboard_held() {
        let plain = test_archive(&[
            ("public.utf8-plain-text", "rf_b.txt"),
            ("com.apple.icns", "icon"),
        ]);
        assert_eq!(
            describe_archives(&[plain]),
            "clipboard holds \"rf_b.txt\" + 1 non-text item(s) (no path)"
        );
        assert_eq!(
            describe_archives(&[]),
            "clipboard archive was empty (0 items)"
        );
        assert_eq!(
            describe_archives(&[test_archive(&[])]),
            "clipboard archive was empty (0 items)"
        );
        // Duplicate text flavours collapse to one value.
        let dupes = test_archive(&[
            ("public.utf8-plain-text", "Makefile"),
            ("com.apple.traditional-mac-plain-text", "Makefile"),
        ]);
        assert_eq!(
            describe_archives(&[dupes]),
            "clipboard holds \"Makefile\" (no path)"
        );
        // A binary-only archive says so instead of printing bytes.
        let icon = test_archive(&[("com.apple.icns", "icon")]);
        assert_eq!(
            describe_archives(&[icon]),
            "clipboard holds 1 item(s), none of them text"
        );
    }

    #[test]
    fn plain_text_is_unchanged() {
        let archive = test_archive(&[
            ("public.utf16-plain-text", "hello world"),
            ("public.utf8-plain-text", "hello world"),
        ]);
        let choice = local_clipboard(&archive).unwrap().unwrap();
        assert_eq!(choice.text, "hello world");
        assert!(matches!(
            pull_from_archives(&[archive]).unwrap(),
            PullOutcome::NoFile
        ));
        // A promise / empty archive writes nothing, so no stale path survives.
        assert_eq!(local_clipboard(&test_archive(&[])).unwrap(), None);
        assert!(matches!(
            pull_from_archives(&[]).unwrap(),
            PullOutcome::Empty
        ));
        assert!(matches!(
            pull_from_archives(&[test_archive(&[])]).unwrap(),
            PullOutcome::Empty
        ));
    }

    #[test]
    fn real_pull_ab_archives_decode_as_expected() {
        // Offline check against wK:pD's captures when they are present.
        let dir = std::path::Path::new("../../artifacts/pull-ab");
        let read = |name: &str| std::fs::read(dir.join(name)).ok();
        let Some(pathname) = read("file-pathname-flags0.archive") else {
            return;
        };
        let choice = local_clipboard(&pathname).unwrap().unwrap();
        assert_eq!(choice.text, "/Users/tai/Desktop/for-md/Makefile");
        assert!(matches!(
            pull_from_archives(&[pathname]).unwrap(),
            PullOutcome::File(source) if source.path == "/Users/tai/Desktop/for-md/Makefile"
        ));

        let folder = read("folder-pathname-flags0.archive").unwrap();
        assert!(matches!(
            pull_from_archives(&[folder]).unwrap(),
            PullOutcome::File(source)
                if source.path == "/Users/tai/Desktop/for-md/pullfolder"
        ));

        let plain = read("smoke-flags0.archive").unwrap();
        assert_eq!(
            local_clipboard(&plain).unwrap().unwrap().text,
            "Makefile"
        );
        assert!(matches!(
            pull_from_archives(&[plain]).unwrap(),
            PullOutcome::NoFile
        ));

        let plain_folder = read("folder-plain-flags0.archive").unwrap();
        assert_eq!(
            local_clipboard(&plain_folder).unwrap().unwrap().text,
            "pullfolder"
        );
        assert!(matches!(
            pull_from_archives(&[plain_folder]).unwrap(),
            PullOutcome::NoFile
        ));
    }

    #[test]
    fn pull_source_text_fallback_and_rejections() {
        let path = test_archive(&[("public.utf8-plain-text", "/Users/me/file.bin")]);
        assert_eq!(
            pull_source(&path).unwrap().unwrap().path,
            "/Users/me/file.bin"
        );
        let not_path = test_archive(&[("public.utf8-plain-text", "hello")]);
        assert!(pull_source(&not_path).unwrap().is_none());
        let multiline = test_archive(&[("public.utf8-plain-text", "/a\n/b")]);
        assert!(pull_source(&multiline).unwrap().is_none());
        let empty = test_archive(&[]);
        assert!(pull_source(&empty).unwrap().is_none());
    }

    #[test]
    fn a_file_url_alone_is_not_pulled() {
        // The agent strips `furl`, so an archive must never rely on one; this
        // pins the behaviour that only TEXT paths are pullable.
        let archive = test_archive(&[(
            "public.file-url",
            "file:///Users/me/My%20File.txt",
        )]);
        assert!(local_clipboard(&archive).unwrap().is_none());
        assert!(pull_source(&archive).unwrap().is_none());
    }

    #[test]
    fn safe_component_rejects_traversal_and_separators() {
        assert_eq!(safe_component("../../evil"), None);
        assert_eq!(safe_component("/abs"), None);
        assert_eq!(safe_component("a/b"), None);
        assert_eq!(safe_component("a\\b"), None);
        assert_eq!(safe_component(".."), None);
        assert_eq!(safe_component("."), None);
        assert_eq!(safe_component(""), None);
        assert_eq!(safe_component("a\0b"), None);
        assert_eq!(safe_component("normal.bin"), Some("normal.bin"));
        assert_eq!(safe_component("with space.txt"), Some("with space.txt"));
        let root = Path::new("/tmp/recv");
        assert_eq!(safe_join(root, "../../evil"), None);
        assert_eq!(
            safe_join(root, "ok.bin"),
            Some(PathBuf::from("/tmp/recv/ok.bin"))
        );
    }

    #[test]
    fn receive_worker_tree_collision_and_traversal() {
        let dir = temp_dir("recv");
        let out = dir.join("Downloads");
        let worker_out = out.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || receive_worker(rx, worker_out, false));

        let send_tree = |tx: &std::sync::mpsc::Sender<proto::FileCopyEvent>| {
            tx.send(proto::FileCopyEvent::ItemInfo {
                session: 1,
                total_size: 10,
                physical_size: 4096,
                file_count: 3,
                fork_count: 0,
                folder_count: 3,
            })
            .unwrap();
            tx.send(proto::FileCopyEvent::NewItem {
                session: 1,
                level: 0,
                item_type: proto::ITEM_TYPE_DIR,
                name: "tree".to_string(),
            })
            .unwrap();
            tx.send(proto::FileCopyEvent::NewItem {
                session: 1,
                level: 1,
                item_type: proto::ITEM_TYPE_FILE,
                name: "a.bin".to_string(),
            })
            .unwrap();
            tx.send(proto::FileCopyEvent::FileData {
                session: 1,
                data: b"aaaa".to_vec(),
            })
            .unwrap();
            tx.send(proto::FileCopyEvent::NewItem {
                session: 1,
                level: 1,
                item_type: proto::ITEM_TYPE_DIR,
                name: "empty".to_string(),
            })
            .unwrap();
            tx.send(proto::FileCopyEvent::NewItem {
                session: 1,
                level: 1,
                item_type: proto::ITEM_TYPE_DIR,
                name: "sub".to_string(),
            })
            .unwrap();
            tx.send(proto::FileCopyEvent::NewItem {
                session: 1,
                level: 2,
                item_type: proto::ITEM_TYPE_FILE,
                name: "b.bin".to_string(),
            })
            .unwrap();
            tx.send(proto::FileCopyEvent::FileData {
                session: 1,
                data: b"bb".to_vec(),
            })
            .unwrap();
            // A symlink must not be recreated.
            tx.send(proto::FileCopyEvent::NewItem {
                session: 1,
                level: 1,
                item_type: proto::ITEM_TYPE_SYMLINK,
                name: "link".to_string(),
            })
            .unwrap();
            // Traversal attempts must not escape the receive directory.
            for evil in ["../../evil", "/abs", "a/b"] {
                tx.send(proto::FileCopyEvent::NewItem {
                    session: 1,
                    level: 1,
                    item_type: proto::ITEM_TYPE_FILE,
                    name: evil.to_string(),
                })
                .unwrap();
                tx.send(proto::FileCopyEvent::FileData {
                    session: 1,
                    data: b"x".to_vec(),
                })
                .unwrap();
            }
            tx.send(proto::FileCopyEvent::EndSession {
                session: 1,
                error: 0,
            })
            .unwrap();
        };
        send_tree(&tx);
        // Same folder again: the whole tree lands in "tree 2".
        send_tree(&tx);
        drop(tx);
        worker.join().unwrap();

        assert_eq!(std::fs::read(out.join("tree/a.bin")).unwrap(), b"aaaa");
        assert_eq!(std::fs::read(out.join("tree/sub/b.bin")).unwrap(), b"bb");
        assert!(out.join("tree/empty").is_dir());
        assert!(!out.join("tree/link").exists());
        assert!(!dir.join("evil").exists());
        assert!(!out.join("tree/a").exists());
        assert_eq!(std::fs::read(out.join("tree 2/a.bin")).unwrap(), b"aaaa");
        assert_eq!(std::fs::read(out.join("tree 2/sub/b.bin")).unwrap(), b"bb");
        assert!(out.join("tree 2/empty").is_dir());
        let _ = std::fs::remove_dir_all(dir);
    }
}
