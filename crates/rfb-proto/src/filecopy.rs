//! Apple file copy (`0x22`) framing and message bodies.
//!
//! Layouts are from `artifacts/filecopy-recon.md` ("wK:pB — layouts"), verified
//! from the daemon/helper disassembly and a live local→remote transfer:
//!
//! ```text
//! RFB transport:  0x22, pad, u32 totalSize (body length from +6), body
//! body:           u16 version=1, u16 cmd, u32 sessionID, fields...
//! cmd 2   StartFileReceive: flags u32=0, unknown u32, pathLen u16 (incl NUL),
//!                           path, NUL, one extra zero byte
//! cmd 100 ItemInfo:         unknownA u32, unknownB u32, lFileSize u64,
//!                           pFileSize u64, fileCount u64, forkCount u64,
//!                           folderCount u64, allocSize u32
//! cmd 101 NewItem:          type u8 @+8, level u16 @+0x64, nameLen u16 @+0x6c,
//!                           name C string @+0x70
//! cmd 102 FileData:         byteCount u32, data (65536-byte chunks)
//! cmd 104 EndSession:       errorCode u32
//! ```
//!
//! cmd 1 StartFileSend is the mirror used when the viewer asks the server to
//! send a file: flags bit 0 must be 1, then a source path.

pub const VERSION: u16 = 1;
pub const CMD_START_FILE_SEND: u16 = 1;
pub const CMD_START_FILE_RECEIVE: u16 = 2;
pub const CMD_ITEM_INFO: u16 = 100;
pub const CMD_NEW_ITEM: u16 = 101;
pub const CMD_FILE_DATA: u16 = 102;
pub const CMD_COMPRESSED_FILE_DATA: u16 = 103;
pub const CMD_END_SESSION: u16 = 104;

/// Native data chunk size (data bytes, not counting the 12-byte payload or
/// the 6-byte transport).
pub const MAX_DATA: usize = 65536;

/// NewItem type byte for a regular file (the native sender's `(type & ~2) == 1`
/// branch copies the data-fork sizes).
pub const ITEM_TYPE_FILE: u8 = 1;
pub const ITEM_TYPE_DIR: u8 = 2;
pub const ITEM_TYPE_SYMLINK: u8 = 3;

/// A parsed server→viewer file-copy event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileCopyEvent {
    /// cmd 100: sizes/counts before any item data.
    ItemInfo {
        session: u32,
        total_size: u64,
        physical_size: u64,
        file_count: u64,
        fork_count: u64,
        folder_count: u64,
    },
    /// cmd 101: one item header. `item_type` 1/3 = file, 2 = folder (per the
    /// receiver's `(type & ~2) == 1` file test).
    NewItem {
        session: u32,
        level: u16,
        item_type: u8,
        name: String,
    },
    /// cmd 102: the next sequential chunk of the current item.
    FileData { session: u32, data: Vec<u8> },
    /// cmd 104: transfer finished (0 = success).
    EndSession { session: u32, error: u32 },
    /// Anything else (progress frames, cmd 103 compression, unknown) so the
    /// reader never desynchronises on a message it does not consume yet.
    Unknown {
        session: u32,
        cmd: u16,
        payload_len: usize,
    },
}

fn be16(b: &[u8]) -> u16 {
    u16::from_be_bytes([b[0], b[1]])
}

fn be32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

fn be64(b: &[u8]) -> u64 {
    u64::from_be_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
}

/// Wrap `fields` (starting at body+8) in the transport and common header.
fn message(cmd: u16, session: u32, fields: &[u8]) -> Vec<u8> {
    let body_len = 8 + fields.len();
    let mut out = Vec::with_capacity(6 + body_len);
    out.push(0x22);
    out.push(0);
    out.extend_from_slice(&(body_len as u32).to_be_bytes());
    out.extend_from_slice(&VERSION.to_be_bytes());
    out.extend_from_slice(&cmd.to_be_bytes());
    out.extend_from_slice(&session.to_be_bytes());
    out.extend_from_slice(fields);
    out
}

fn start_message(cmd: u16, path: &str, session: u32, flags: u32) -> Vec<u8> {
    let mut fields = Vec::with_capacity(8 + 2 + path.len() + 2);
    fields.extend_from_slice(&flags.to_be_bytes());
    fields.extend_from_slice(&0u32.to_be_bytes()); // unknown (unlogged)
    // Live arithmetic (native: messageSize 45, pathLen 20, path
    // "/Users/tai/Downloads" = 20 chars): pathLen is the character count, then
    // the path bytes and one NUL. The doc's "+1 extra byte" is that NUL.
    fields.extend_from_slice(&(path.len() as u16).to_be_bytes());
    fields.extend_from_slice(path.as_bytes());
    fields.push(0);
    message(cmd, session, &fields)
}

/// Viewer asks the server to send a file at `source` (absolute remote path).
pub fn start_file_send(source: &str, session: u32) -> Vec<u8> {
    start_message(CMD_START_FILE_SEND, source, session, 1)
}

/// Viewer announces an upload into `destination` (absolute remote folder).
pub fn start_file_receive(destination: &str, session: u32) -> Vec<u8> {
    start_message(CMD_START_FILE_RECEIVE, destination, session, 0)
}

/// Sizes/counts for the whole session.
#[allow(clippy::too_many_arguments)]
pub fn item_info(
    session: u32,
    logical_size: u64,
    physical_size: u64,
    file_count: u64,
    fork_count: u64,
    folder_count: u64,
    alloc_size: u32,
) -> Vec<u8> {
    let mut fields = Vec::with_capacity(0x38);
    fields.extend_from_slice(&0u32.to_be_bytes()); // unknownA
    fields.extend_from_slice(&0u32.to_be_bytes()); // unknownB
    fields.extend_from_slice(&logical_size.to_be_bytes());
    fields.extend_from_slice(&physical_size.to_be_bytes());
    fields.extend_from_slice(&file_count.to_be_bytes());
    fields.extend_from_slice(&fork_count.to_be_bytes());
    fields.extend_from_slice(&folder_count.to_be_bytes());
    fields.extend_from_slice(&alloc_size.to_be_bytes());
    message(CMD_ITEM_INFO, session, &fields)
}

/// One item header, matching a genuine server NewItem byte-for-byte:
/// payload+0x08 type, +0x36 data logical size (u32), +0x3a/+0x42/+0x4a/+0x52
/// four UTCDateTime values (u64; {u16 highSeconds, u32 lowSeconds, u16
/// fraction}), +0x66 POSIX mode (u16), +0x68 the native text-encoding value
/// 0x7e, +0x6c nameLen (excluding NUL), +0x70 name + NUL.
pub fn new_item(
    session: u32,
    level: u16,
    item_type: u8,
    name: &str,
    mode: u32,
    logical_size: u64,
) -> Vec<u8> {
    // fields start at payload+8.
    let mut fields = vec![0u8; 0x68 + name.len() + 1];
    fields[0] = item_type;
    fields[0x2e..0x32].copy_from_slice(&(logical_size as u32).to_be_bytes());
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
        + 2_082_844_800;
    let mut stamp = [0u8; 8];
    stamp[0..2].copy_from_slice(&((now >> 32) as u16).to_be_bytes());
    stamp[2..6].copy_from_slice(&((now & 0xffff_ffff) as u32).to_be_bytes());
    for offset in [0x32usize, 0x3a, 0x42, 0x4a] {
        fields[offset..offset + 8].copy_from_slice(&stamp);
    }
    fields[0x5c..0x5e].copy_from_slice(&level.to_be_bytes());
    // The native sends st_mode as a u16 (file-type bits included).
    fields[0x5e..0x60].copy_from_slice(&((mode & 0xffff) as u16).to_be_bytes());
    fields[0x60..0x64].copy_from_slice(&0x7eu32.to_be_bytes());
    fields[0x64..0x66].copy_from_slice(&(name.len() as u16).to_be_bytes());
    fields[0x68..0x68 + name.len()].copy_from_slice(name.as_bytes());
    fields[0x68 + name.len()] = 0;
    message(CMD_NEW_ITEM, session, &fields)
}

/// The next sequential chunk of the current item.
pub fn file_data(session: u32, data: &[u8]) -> Vec<u8> {
    let mut fields = Vec::with_capacity(4 + data.len());
    fields.extend_from_slice(&(data.len() as u32).to_be_bytes());
    fields.extend_from_slice(data);
    message(CMD_FILE_DATA, session, &fields)
}

/// End of the session; `error` 0 = success.
pub fn end_session(session: u32, error: u32) -> Vec<u8> {
    message(CMD_END_SESSION, session, &error.to_be_bytes())
}

/// Parse one `0x22` body (starting at `version`, after the 6-byte transport).
pub fn parse_event(body: &[u8]) -> Option<FileCopyEvent> {
    if body.len() < 8 {
        return None;
    }
    let version = be16(&body[0..2]);
    if version != VERSION {
        return None;
    }
    let cmd = be16(&body[2..4]);
    let session = be32(&body[4..8]);
    let fields = &body[8..];
    match cmd {
        CMD_ITEM_INFO if fields.len() >= 0x30 => Some(FileCopyEvent::ItemInfo {
            session,
            total_size: be64(&fields[0x08..0x10]),
            physical_size: be64(&fields[0x10..0x18]),
            file_count: be64(&fields[0x18..0x20]),
            fork_count: be64(&fields[0x20..0x28]),
            folder_count: be64(&fields[0x28..0x30]),
        }),
        CMD_NEW_ITEM if fields.len() >= 0x68 => {
            let level = be16(&fields[0x5c..0x5e]);
            let name_len = be16(&fields[0x64..0x66]) as usize;
            let name_start: usize = 0x68;
            let name_end = name_start.checked_add(name_len)?;
            if name_len == 0 || name_end > fields.len() {
                return None;
            }
            let name = &fields[name_start..name_end];
            let name = name.split(|&b| b == 0).next().unwrap_or(name);
            let name = String::from_utf8_lossy(name).to_string();
            Some(FileCopyEvent::NewItem {
                session,
                level,
                item_type: fields[0],
                name,
            })
        }
        CMD_FILE_DATA if fields.len() >= 4 => {
            let count = be32(&fields[0..4]) as usize;
            let end = 4usize.checked_add(count)?;
            if end > fields.len() {
                return None;
            }
            Some(FileCopyEvent::FileData {
                session,
                data: fields[4..end].to_vec(),
            })
        }
        CMD_END_SESSION if fields.len() >= 4 => Some(FileCopyEvent::EndSession {
            session,
            error: be32(&fields[0..4]),
        }),
        _ => Some(FileCopyEvent::Unknown {
            session,
            cmd,
            payload_len: body.len(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_file_receive_layout() {
        let msg = start_file_receive("/Users/tai/Downloads", 1);
        assert_eq!(msg[0], 0x22);
        assert_eq!(be32(&msg[2..6]) as usize, msg.len() - 6);
        let body = &msg[6..];
        assert_eq!(be16(&body[0..2]), 1);
        assert_eq!(be16(&body[2..4]), 2);
        assert_eq!(be32(&body[4..8]), 1);
        assert_eq!(be32(&body[8..12]), 0); // flags
        assert_eq!(be32(&body[12..16]), 0); // unknown
        let path_len = be16(&body[16..18]) as usize;
        assert_eq!(path_len, "/Users/tai/Downloads".len());
        assert_eq!(&body[18..18 + path_len], b"/Users/tai/Downloads");
        assert_eq!(body[18 + path_len], 0);
        assert_eq!(body.len(), 18 + path_len + 1);
        assert_eq!(msg.len(), 45, "native StartFileReceive is 45 bytes");
    }

    #[test]
    fn new_item_layout_and_roundtrip() {
        let name = "fc_a.bin";
        let msg = new_item(1, 0, ITEM_TYPE_FILE, name, 0o100644, 300_000);
        let body = &msg[6..];
        assert_eq!(be16(&body[2..4]), 101);
        assert_eq!(body[8], ITEM_TYPE_FILE);
        assert_eq!(be16(&body[0x64..0x66]), 0);
        assert_eq!(be16(&body[0x6c..0x6e]), name.len() as u16);
        assert_eq!(&body[0x70..0x78], name.as_bytes());
        assert_eq!(body[0x78], 0);
        assert_eq!(body.len(), 0x70 + name.len() + 1);
        match parse_event(body).unwrap() {
            FileCopyEvent::NewItem { level, item_type, name: parsed, .. } => {
                assert_eq!(level, 0);
                assert_eq!(item_type, ITEM_TYPE_FILE);
                assert_eq!(parsed, name);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn item_info_and_data_roundtrip() {
        let info = item_info(7, 300_000, 303_104, 1, 0, 0, 0);
        match parse_event(&info[6..]).unwrap() {
            FileCopyEvent::ItemInfo { session, total_size, physical_size, file_count, .. } => {
                assert_eq!(session, 7);
                assert_eq!(total_size, 300_000);
                assert_eq!(physical_size, 303_104);
                assert_eq!(file_count, 1);
            }
            other => panic!("unexpected {other:?}"),
        }
        let data = file_data(7, &[1, 2, 3, 4]);
        match parse_event(&data[6..]).unwrap() {
            FileCopyEvent::FileData { session, data } => {
                assert_eq!(session, 7);
                assert_eq!(data, vec![1, 2, 3, 4]);
            }
            other => panic!("unexpected {other:?}"),
        }
        let end = end_session(7, 0);
        assert_eq!(
            parse_event(&end[6..]).unwrap(),
            FileCopyEvent::EndSession { session: 7, error: 0 }
        );
    }
}
