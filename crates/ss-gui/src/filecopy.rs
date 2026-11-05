//! Local→remote file drop: enumeration and transfer-loop scaffolding.
//!
//! The verified native sequence is `StartFileReceive` (cmd 2), `ItemInfo`
//! (cmd 100), `NewItem` (101), `FileData` (102, 65536-byte chunks) and
//! `EndSession` (104). The byte layouts are still being specced, so this
//! module defines only the semantic calls behind [`FileCopySender`] and ships
//! a [`DryRunSender`] that logs the sequence. The real sender will wrap the
//! RFB writer and slot into the same trait.
//!
//! Entry point: [`enumerate_drop`] + [`run_transfer`], called from the winit
//! `WindowEvent::DroppedFile` handler on a worker thread.

use anyhow::{bail, Context, Result};
use rfb_proto::filecopy as proto;
use sha1::{Digest, Sha1};
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{Receiver, Sender};

/// Data chunk size observed from the native client (65536 data bytes; the
/// on-wire message adds the daemon wrapper and the FileData header).
pub const CHUNK_SIZE: usize = 65536;

/// Destination folder the viewer asks the remote to write into. The native
/// client sends an absolute path (live sample `/Users/tai/Downloads`), so the
/// safe default is the same shape built from the authenticated remote user.
pub fn remote_destination(user: &str) -> String {
    format!("/Users/{user}/Downloads")
}

/// One file selected by a drop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DropFile {
    /// Local path to read bytes from.
    pub source: PathBuf,
    /// Name to create on the remote side (basename for a single file).
    pub name: String,
    pub size: u64,
    /// POSIX mode including the file-type bits (0o100644 on macOS/Linux).
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
}

/// A drop resolved into the files to send and the remote destination.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferPlan {
    pub destination: String,
    pub files: Vec<DropFile>,
}

impl TransferPlan {
    pub fn total_bytes(&self) -> u64 {
        self.files.iter().map(|file| file.size).sum()
    }
}

/// Resolve a winit `DroppedFile` path into a transfer plan.
///
/// Single files are supported now. Directory drops need the folder-item
/// semantics (`NewItem` levels, `folderCount`, relative names) that the spec
/// has not pinned down yet, so they are rejected with a clear message instead
/// of guessed. The plan shape already carries a name per file, so directory
/// entries can be added without changing the sender interface.
pub fn enumerate_drop(path: &Path) -> Result<TransferPlan> {
    let metadata =
        std::fs::metadata(path).with_context(|| format!("stat dropped path {}", path.display()))?;
    if metadata.is_dir() {
        bail!(
            "directory drops are not wired yet (single-file drop first): {}",
            path.display()
        );
    }
    if !metadata.is_file() {
        bail!("dropped path is not a regular file: {}", path.display());
    }
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow::anyhow!("file name is not valid UTF-8: {}", path.display()))?
        .to_string();
    #[cfg(unix)]
    let (mode, uid, gid) = {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        (
            metadata.permissions().mode(),
            metadata.uid(),
            metadata.gid(),
        )
    };
    #[cfg(not(unix))]
    let (mode, uid, gid) = (0o100644, 501, 20);
    Ok(TransferPlan {
        destination: String::new(),
        files: vec![DropFile {
            source: path.to_path_buf(),
            name,
            size: metadata.len(),
            mode,
            uid,
            gid,
        }],
    })
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
    fn new_item(&mut self, level: u32, name: &str, mode: u32, size: u64) -> Result<()>;
    fn file_data(&mut self, offset: u64, data: &[u8]) -> Result<()>;
    fn end_session(&mut self) -> Result<()>;
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct TransferStats {
    pub files: u32,
    pub bytes: u64,
    pub chunks: u32,
}

/// Drive the verified sequence for one plan. Reads the files in 64 KiB chunks
/// and hands each chunk to the sender. No acks are visible in the native
/// capture, so this is a straight write loop; the real sender can apply
/// backpressure on the socket.
pub fn run_transfer<S: FileCopySender>(
    sender: &mut S,
    plan: &TransferPlan,
) -> Result<TransferStats> {
    let total = plan.total_bytes();
    let physical: u64 = plan.files.iter().map(|file| physical_size(&file.source)).sum();
    sender.start_file_receive(&plan.destination)?;
    sender.item_info(total, physical, plan.files.len() as u32, 0)?;

    let mut stats = TransferStats::default();
    for file in &plan.files {
        sender.new_item(0, &file.name, file.mode, file.size)?;
        let mut handle = File::open(&file.source)
            .with_context(|| format!("open dropped file {}", file.source.display()))?;
        let mut buffer = vec![0u8; CHUNK_SIZE];
        let mut offset = 0u64;
        loop {
            let n = handle
                .read(&mut buffer)
                .with_context(|| format!("read {}", file.source.display()))?;
            if n == 0 {
                break;
            }
            sender.file_data(offset, &buffer[..n])?;
            offset += n as u64;
            stats.chunks += 1;
            stats.bytes += n as u64;
        }
        if offset != file.size {
            bail!(
                "{} changed size during transfer (planned {}, read {})",
                file.source.display(),
                file.size,
                offset
            );
        }
        stats.files += 1;
    }
    sender.end_session()?;
    Ok(stats)
}

/// Physical (allocated) size hint for ItemInfo, matching the native client's
/// `pFileSize` (300000 → 303104 for a 300 KB file: 592 blocks × 512).
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

    fn new_item(&mut self, level: u32, name: &str, mode: u32, size: u64) -> Result<()> {
        let message = proto::new_item(
            self.session,
            level as u16,
            proto::ITEM_TYPE_FILE,
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

/// Enumerate + send one dropped file. Runs on a worker thread.
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
        "filecopy: sending {} -> {} (session {session}, {} bytes)",
        source.display(),
        plan.destination,
        plan.total_bytes()
    );
    let mut sender = NetworkSender::new(tx, session, verbose);
    match run_transfer(&mut sender, &plan) {
        Ok(stats) => {
            eprintln!(
                "filecopy: sent {} file(s), {} bytes, {} chunk(s), sha1={} dest={}",
                stats.files,
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
/// Runs on a worker thread; the reader thread only forwards events. Files are
/// created under `out_dir` (top-level items) or under the most recent folder
/// with a smaller `level`; collisions get a Finder-style " 2"/" 3" suffix.
pub fn receive_worker(
    events: Receiver<proto::FileCopyEvent>,
    out_dir: PathBuf,
    verbose: bool,
) {
    if let Err(error) = std::fs::create_dir_all(&out_dir) {
        eprintln!("filecopy: cannot create receive directory {}: {error}", out_dir.display());
        return;
    }
    let mut current: Option<Incoming> = None;
    let mut folders: Vec<(u16, PathBuf)> = Vec::new();

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
                    .unwrap_or_else(|| out_dir.clone());
                let is_file = (item_type & !2) == 1;
                let path = unique_path(&parent, &name);
                if is_file {
                    match File::create(&path) {
                        Ok(file) => {
                            if verbose {
                                eprintln!(
                                    "filecopy: new file level={level} {name:?} -> {}",
                                    path.display()
                                );
                            }
                            current = Some(Incoming {
                                file,
                                path,
                                bytes: 0,
                                hash: Sha1::new(),
                            });
                        }
                        Err(error) => {
                            eprintln!("filecopy: create {}: {error}", path.display());
                        }
                    }
                } else {
                    match std::fs::create_dir_all(&path) {
                        Ok(()) => {
                            folders.retain(|(folder_level, _)| *folder_level < level);
                            if verbose {
                                eprintln!(
                                    "filecopy: new folder level={level} {name:?} -> {}",
                                    path.display()
                                );
                            }
                            folders.push((level, path));
                        }
                        Err(error) => {
                            eprintln!("filecopy: mkdir {}: {error}", path.display());
                        }
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
    use std::io::Write;
    use std::sync::atomic::{AtomicU32, Ordering};

    static NEXT: AtomicU32 = AtomicU32::new(0);

    fn temp_file(len: usize) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "ss-filecopy-test-{}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed),
            len
        ));
        let mut file = File::create(&path).unwrap();
        file.write_all(&vec![0xABu8; len]).unwrap();
        path
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

        fn new_item(&mut self, level: u32, name: &str, _mode: u32, _size: u64) -> Result<()> {
            self.calls.push(format!("item:{level}:{name}"));
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
        let path = temp_file(10);
        let plan = enumerate_drop(&path).unwrap();
        assert_eq!(plan.files.len(), 1);
        assert_eq!(plan.files[0].size, 10);
        assert_eq!(plan.files[0].source, path);
        assert!(plan.destination.is_empty());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn enumerate_rejects_directories() {
        let path = std::env::temp_dir().join(format!(
            "ss-filecopy-dir-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).unwrap();
        let error = enumerate_drop(&path).unwrap_err().to_string();
        assert!(error.contains("directory drops"), "{error}");
        let _ = std::fs::remove_dir(path);
    }

    #[test]
    fn unique_path_appends_counter_before_extension() {
        let dir = std::env::temp_dir().join(format!(
            "ss-filecopy-unique-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        File::create(dir.join("report.txt")).unwrap();
        assert_eq!(unique_path(&dir, "report.txt"), dir.join("report 2.txt"));
        File::create(dir.join("report 2.txt")).unwrap();
        assert_eq!(unique_path(&dir, "report.txt"), dir.join("report 3.txt"));
        assert_eq!(unique_path(&dir, "notes"), dir.join("notes"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn receive_worker_writes_files_and_handles_collisions() {
        let dir = std::env::temp_dir().join(format!(
            "ss-filecopy-recv-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let worker_dir = dir.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || receive_worker(rx, worker_dir, false));
        tx.send(proto::FileCopyEvent::ItemInfo {
            session: 1,
            total_size: 11,
            physical_size: 4096,
            file_count: 1,
            fork_count: 0,
            folder_count: 0,
        })
        .unwrap();
        tx.send(proto::FileCopyEvent::NewItem {
            session: 1,
            level: 0,
            item_type: proto::ITEM_TYPE_FILE,
            name: "r.bin".to_string(),
        })
        .unwrap();
        tx.send(proto::FileCopyEvent::FileData {
            session: 1,
            data: b"hello ".to_vec(),
        })
        .unwrap();
        tx.send(proto::FileCopyEvent::FileData {
            session: 1,
            data: b"world".to_vec(),
        })
        .unwrap();
        tx.send(proto::FileCopyEvent::EndSession { session: 1, error: 0 })
            .unwrap();
        // Same name again: the worker must not overwrite.
        tx.send(proto::FileCopyEvent::NewItem {
            session: 2,
            level: 0,
            item_type: proto::ITEM_TYPE_FILE,
            name: "r.bin".to_string(),
        })
        .unwrap();
        tx.send(proto::FileCopyEvent::FileData {
            session: 2,
            data: b"x".to_vec(),
        })
        .unwrap();
        tx.send(proto::FileCopyEvent::EndSession { session: 2, error: 0 })
            .unwrap();
        drop(tx);
        worker.join().unwrap();
        assert_eq!(std::fs::read(dir.join("r.bin")).unwrap(), b"hello world");
        assert_eq!(std::fs::read(dir.join("r 2.bin")).unwrap(), b"x");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn transfer_sequence_and_chunking() {
        let path = temp_file(150_000);
        let mut plan = enumerate_drop(&path).unwrap();
        plan.destination = "/Users/tai/Downloads".to_string();
        let mut sender = RecordingSender::default();
        let stats = run_transfer(&mut sender, &plan).unwrap();
        assert_eq!(
            stats,
            TransferStats {
                files: 1,
                bytes: 150_000,
                chunks: 3
            }
        );
        let calls = sender.calls;
        assert_eq!(calls[0], "start:/Users/tai/Downloads");
        assert_eq!(calls[1], "info:150000:1:0");
        assert!(
            calls[2].starts_with("item:0:ss-filecopy-test-"),
            "{}",
            calls[2]
        );
        assert!(calls[3].starts_with("data:0:65536"), "{}", calls[3]);
        assert!(calls[4].starts_with("data:65536:65536"), "{}", calls[4]);
        assert!(calls[5].starts_with("data:131072:18928"), "{}", calls[5]);
        assert_eq!(calls[6], "end");
        let _ = std::fs::remove_file(path);
    }
}
