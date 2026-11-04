# Clipboard / pasteboard — Apple Screen Sharing

Status: **text and images VERIFIED in both directions, automatic and duplex**
(2026-11-05, `mm.local`). File copy is a separate protocol and is **not**
implemented (§7).

Evidence: `artifacts/verify-clip.log` (this round: text both ways, PNG both ways
with matching sha1), `artifacts/cb-impl-*.log`, `artifacts/cb-recon/` (native
session unified logs + daemon disassembly), `artifacts/clipboard-recon.md`
(working notes). Earlier text evidence: `artifacts/gui-clip3.log`,
`artifacts/gui-clip5.log`, `artifacts/server-cb.log`, `artifacts/agent-cb.log`.

## 1. Messages

| Type | Direction | Size | Purpose |
|------|-----------|------|---------|
| `0x06` | client→server | 8 + n | standard RFB `ClientCutText`. Only lands on the remote pasteboard while `0x15 AutoPasteboard(1)` is active. |
| `0x0a` | client→server | 5 | status/select, BE16 selector at bytes 2..4 (no log unless verbose). |
| `0x0b` | client→server | **8** | `ClipboardFetch`. Byte 1 bit 0 = "promises only"; 0 = send all data. **Must be 8 bytes** — see §3. |
| `0x14` | server→client | 8 | `MiscStatus`. `cmd = be16(6..8)`; `cmd = 2` = remote pasteboard changed → reply with a `0x0b` fetch. Observed on this build: `14 00 00 04 00 01 00 02`. |
| `0x15` | client→server | 8 | `AutoPasteboard`, selector 1 = start, 2 = stop. Makes the viewer the pasteboard *provider*. |
| `0x1f` | server→client | 16 + c | `ClipboardSend`: zlib-compressed pasteboard archive. |
| `0x1f` | client→server | 16 + c | `HandleViewerClipboardSend`: same layout, used to push the viewer's pasteboard. |
| `0x22` | both | — | `HandleFileCopyMessage`: the dedicated file-transfer channel (§7), not the pasteboard. |

## 2. The archive

`0x1f` in either direction is a 16-byte header followed by a zlib stream
(`Z_SYNC_FLUSH`, no final block — inflate with a sync flush and stop at
`uncompressed_size`):

```
+0x00 u8  0x1f
+0x01 u8  pad
+0x02 u8  promise flag (bit 0)      # do not trust this alone, see below
+0x03 u8  pad
+0x04 u32 (unlabelled, observed 0)
+0x08 u32 uncompressed_size
+0x0c u32 compressed_size
+0x10 ... zlib stream
```

Inner archive:

```
u32 item_count
per item:
  u32 uti_len, uti bytes
  u32 reserved (0)
  u32 alias_count
    per alias: u32 name_len, name, u32 value_len, value
  u32 data_len, data bytes
```

Text flavours: `public.utf8-plain-text`, `public.utf16-plain-text` (UTF-16BE),
`com.apple.traditional-mac-plain-text` (latin-1), each with aliases
(`com.apple.ostype`, `public.mime-type`, …).

Image flavours: `public.png`, `public.tiff`, `public.jpeg`. We prefer PNG, then
TIFF, then JPEG.

**The +2 byte is not a reliable promise indicator.** It is 1 both for an empty
reply and for a real archive carrying data. Treat `uncompressed_size == 0` or
empty per-item data as "no data".

## 3. `0x0b ClipboardFetch` is 8 bytes — and the old "fetch closes the session" bug

Historical note, because it cost a long time: our first implementation sent a
**single bytes** `0x0b`. The server's handler consumes a fixed 8-byte body, so it
read our next 7 bytes from the *following* message. Its reply was still correct,
which made the bug look like a server-side close: the next `0x03
FramebufferUpdateRequest` was then parsed as `SetPixelFormat`
(`bitsperpixel 1 … viewer result -2 … going to close 9`), and the session died
~13 ms later. Padding with zeros was worse — the server read `0x00` as
`SetPixelFormat` immediately.

Send exactly:

```
0b 00 00 00 00 00 00 00        # flags = 0 -> send all data
```

Flags bit 0 = 1 asks for the flavour list only ("only want promises"). With
`AutoPasteboard(1)` active, a **flags = 0** fetch still returns the real archive;
that is how the native client gets duplex.

## 4. Duplex (VERIFIED)

The native client: `AutoPasteboard(1)` ~400 ms after `HandleViewerInfoMessage`,
then an initial fetch with promise-only, and a full-data fetch whenever the
remote pasteboard changes. Its daemon log shows `only want promises` at connect
and `send all data` after a remote copy.

Our client now does the same, automatically:

- `AutoPasteboard(1)` at connect;
- `0x0b` fetch with flags 0 at connect and on every `0x14 cmd=2`;
- local clipboard changes pushed with `0x06 ClientCutText` (text) or `0x1f`
  (images), with echo suppression so a fetched item is not sent straight back;
- `--no-clipboard` disables all of it (no `0x15`, no fetch, no watcher).

Verified live on one connection: remote copy → local `pbpaste`; local copy →
remote clipboard read back via `osascript`; no stall, no close, 582 framebuffer
updates during the session.

Known cost: the local pasteboard is polled (`pbpaste` / `osascript` per poll,
~700 ms). That is a process spawn per poll, so it is the obvious thing to
replace with an NSPasteboard change-count watcher later.

## 5. Images (VERIFIED, byte-identical)

- Receive: a PNG placed on the `mm.local` console pasteboard arrives as
  `clipboard <- image public.png N bytes` and is installed on the local
  clipboard. A 21484-byte `screencapture` round-tripped with an identical sha1.
- Send: a local PNG is read with `osascript` (`«class PNGf»`) and pushed as a
  viewer `0x1f` archive. Verified by reading the remote clipboard back to a file:
  sha1 `510cfbee…` on both sides, 549 bytes.
- The server echoes the viewer's own push back as a `0x1f`; echo suppression by
  content fingerprint prevents a ping-pong loop.

## 6. What the server will not do

- With no `AutoPasteboard`, `0x06` is accepted and even logged
  (`viewer text size N`) but the remote pasteboard stays empty.
- Promise items do not survive disconnect: the remote pasteboard empties.
- `pbpaste` on the remote cannot read promises; use `osascript -e
  'the clipboard as text'`, or `sudo -n launchctl asuser 501 pbpaste` for real
  data.

## 7. File copy (not implemented)

Native file copy is **not** a pasteboard flavour. The daemon has a dedicated
`0x22 HandleFileCopyMessage` channel with `RFBFileCopy_StartFileSendMessage`
(path + length) and `RFBFileCopy_StartFileReceiveMessage`, and the app has UI
state for it (`FileTransferHistory` with direction, paths, byte counts). No
`public.file-url` flavour was observed. Implementing it is a separate milestone:
message numbering and the directional start handshake are known, the transfer
framing is not.

## 8. Verifying on `mm.local`

```sh
# remote -> local text
ssh tai@mm.local 'printf "R2L\n" | sudo -n launchctl asuser 501 pbcopy'
pbpaste            # expect R2L

# local -> remote text (auto, no flags)
printf 'L2R\n' | pbcopy
ssh tai@mm.local 'osascript -e "the clipboard as text"'

# local -> remote image, byte check
osascript -e 'set the clipboard to (read (POSIX file "/tmp/x.png") as «class PNGf»)'
ssh tai@mm.local 'osascript -e "set png to the clipboard as «class PNGf»" \
  -e "set f to open for access POSIX file \"/tmp/rt.png\" with write permission" \
  -e "set eof f to 0" -e "write png to f" -e "close access f"'
ssh tai@mm.local 'shasum /tmp/rt.png'
```
