# File copy — Apple Screen Sharing (`0x22`)

Status: **VERIFIED both directions** (2026-11-05, `mm.local`). Send by dropping a
file on the window; received files land in `~/Downloads`. Drag-out of the window
is not implemented and is out of scope.

Evidence: `artifacts/filecopy-1/mm-live2.log` (native client, local→remote),
`artifacts/filecopy-2/*.bin` (bodies captured from the server during a
remote→local transfer), `artifacts/filecopy-recon.md` (working notes with
addresses), `tools/filecopy/arrival_harness.py` (sha1 verifier).

## 1. Transport

Every frame starts with the ordinary RFB message type `0x22`, a zero pad byte
and a 32-bit length — there is no rectangle header:

```
+0x00 u8  0x22
+0x01 u8  0                      # pad, always zero
+0x02 u32 totalSize              # body bytes, counted from +0x06; must be < 0x100001
+0x06 ... body
```

Inside the body, every command shares a 8-byte prefix:

```
+0x00 u16 version   (1)
+0x02 u16 cmd
+0x04 u32 sessionID
```

All integers are big-endian. The daemon dispatches `cmd` 1..5 itself and
forwards anything else (100..) opaquely to the receiving helper process, which
is why a client only has to speak the inner numbering.

## 2. Commands

| cmd | name | direction | purpose |
|---|---|---|---|
| 1 | StartFileSend | viewer→server | viewer asks for a file the server holds (used by drag-out; we only use it for testing) |
| 2 | StartFileReceive | viewer→server | viewer announces an upload and names the **destination folder on the remote** |
| 3 | Pause | either | pause a transfer |
| 4 | Continue | either | resume |
| 5 | Stop | either | abort |
| 100 | ItemInfo | sender→receiver | totals: logical/physical size, file/fork/folder counts |
| 101 | NewItem | sender→receiver | one item: type, size, dates, mode, name; `level` is folder depth |
| 102 | FileData | sender→receiver | 64 KiB of file bytes per message |
| 104 | EndSession | sender→receiver | transfer complete, with an error code |

`README`-level rule from the live capture: a 300000-byte file went out as
`2, 100, 101, 102×5, 104` with `error code = 0`.

## 3. cmd 2 StartFileReceive

```
+0x00 u16 version (1)
+0x02 u16 cmd (2)
+0x04 u32 sessionID
+0x08 u32 flags        # must be 0
+0x0c u32 unknown      # 0 in every capture
+0x10 u16 pathLen      # INCLUDES the trailing NUL
+0x12 ... destPath     # absolute POSIX path, UTF-8, NUL-terminated
```

The viewer chooses the destination. The native client sent
`/Users/tai/Downloads` (absolute, 19 characters, `pathLen = 20`) — the folder
under the remote cursor at drop time. **No accept prompt exists**: the remote
launches `SSFileCopyReceiver` as the console user, which makes the directory if
needed and writes the items. Name collisions are resolved by the receiver with a
`" 2"` suffix rather than by asking.

We use `StartFileReceive` with `/Users/<remote user>/Downloads`.

## 4. cmd 100 ItemInfo

Observed 66 bytes on the wire (`lFileSize 300000 pFileSize 303104 fileCount 1
forkCount 0 folderCount 0 allocSize 0`). Field-by-field offsets beyond those
logged values are unverified; we emit the values the receiver logs and it
accepts the frame.

## 5. cmd 101 NewItem — the block that decides whether the file is writable

This is where the implementation stalled for a while: with this block zeroed the
receiver creates the item in the macOS in-progress state with **mode 000**, and
every subsequent `FileData` write fails. The layout below was recovered by
capturing a genuine server-sent `NewItem` during a remote→local transfer
(`artifacts/filecopy-2/005-0x22-cmd101.bin`), which is possible because we
decrypt our own stream:

```
+0x00 u16 version (1)
+0x02 u16 cmd (101)
+0x04 u32 sessionID
+0x08 u8  item type            # 1 = file
+0x0a .. 0x35                  # Finder info / node flags block, zero in captures
+0x36 u32 data logical size
+0x3a u64 create  date        \
+0x42 u64 content date         |  UTCDateTime: { u16 highSeconds, u32 lowSeconds, u16 fraction }
+0x4a u64 attribute date       |
+0x52 u64 access  date        /
+0x5a u64 (fifth date)         # zero in captures
+0x64 u16 level                # folder depth; 0 for the dropped top-level item
+0x66 u16 st_mode              # 0o100644 for a regular file; file-type bits included
+0x68 u32 text encoding        # 0x7e in all captures
+0x6c u16 nameLen              # EXCLUDING the trailing NUL
+0x70 ... name                 # UTF-8, NUL-terminated
```

There is **no uid/gid** in the block. The three mistakes that cost the time:
mode belongs at `+0x66` (not `+0x0a`), nameLen excludes the NUL, and the dates
are `UTCDateTime` (a 16.32 fixed point) rather than a bare 64-bit value.

Folder items recurse: a directory is a `NewItem` with its own item type, and each
entry below it is another `NewItem` with `level` set to its depth. Empty
directories get their own `NewItem` and survive a round trip. Symlinks are
deliberately **not** recreated on receive (they are skipped and logged); they are
the classic escape vector for a file-sync feature.

## 6. cmd 102 FileData

```
+0x00 u16 version, +0x02 u16 cmd (102), +0x04 u32 sessionID
+0x08 .. data            # 65536 bytes for every full chunk
```

The last chunk is short (`37856` in the 300000-byte capture). Chunks are
implicitly sequential; no offset field and no per-chunk ack is visible. Messages
are 6 bytes larger on the wire than the helper sees because the daemon strips
and re-frames the transport header.

## 7. cmd 104 EndSession

18 bytes on the wire, carrying an error code (`0` on success).

## 8. Client behaviour

| direction | trigger | destination |
|---|---|---|
| local → remote | drop a file **or a folder** on the openssc window (`WindowEvent::DroppedFile`) | `/Users/<remote user>/Downloads` |
| remote → local | incoming transfer | `~/Downloads`, collisions suffixed `" 2"` |

Transfers run on a worker thread, so the UI keeps painting.

Pulling something from the remote has no drag-out (out of scope), but it has a
gesture: **`Cmd+Shift+V`** asks the remote clipboard for what to fetch (§8a).
The explicit forms are `--recv-file /absolute/remote/path` (fetched at connect),
`--pull <path>`, or dropping into the GUI. A folder arrives as a folder, tree
intact, with `" 2"` applied to the top-level name on collision.

Test hooks (not user-facing): `--send-file <path>` and `--recv-file <remote
path>` exercise the two directions without a drag.

## 8a. Pulling a file or folder FROM the remote (VERIFIED 2026-11-05)

There is no drag-out of the window by decision, so the reverse direction is
explicit: **Cmd+Shift+V** in `openssc` pulls whatever the remote clipboard names.

How it resolves the path:
1. request a fresh clipboard archive (`0x0b` flags 0) and retry briefly if the
   server's pasteboard agent has not caught up yet;
2. take the first flavour whose text, after percent-decoding, is an absolute
   path (`/...`) - this is what Finder's *Copy as Pathname* puts on the
   pasteboard;
3. otherwise refuse with the observed content named, and say what to do instead.

A plain Finder `Cmd+C` on a file cannot work and never could: the remote
pasteboard then holds `«class furl»` plus the item **name**, the server strips
the file URL when composing its archive, and the archive carries no path for any
client, Apple's included. Finder's *Copy as Pathname* (Option+Cmd+C) puts the
path on the pasteboard as text, which is the supported gesture.

Requires Option to actually reach the remote - see
`docs/protocol/input-and-gestures.md` §5; with the wrong Option keysym the remote
performs a plain copy and the clipboard holds a name, which looks exactly like
"no path on the remote clipboard".

## 9. Open

- `cmd 200` frames arrive after a successful send (28 bytes) and are currently
  logged as unknown. Harmless, but they are almost certainly a status/ack and
  should be decoded.
- The reverse trigger: the protocol's `StartFileSend` (cmd 1) exists and works,
  but a *user-facing* way to pull a file from the remote (drag-out) is not
  implemented.
- Extended attributes / resource forks: the trailing block after the name
  (`extra amount`) is not emitted by us and not required for plain files.
