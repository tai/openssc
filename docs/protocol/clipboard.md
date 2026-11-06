# Clipboard / pasteboard — Apple Screen Sharing

Status: **text and images VERIFIED in both directions, automatic and duplex**
(2026-11-05, `mm.local`). File copy is a different channel on the same connection
and is implemented separately — see `docs/protocol/filecopy.md`.

Evidence: `artifacts/verify-clip.log` (this round: text both ways, PNG both ways
with matching sha1), `artifacts/cb-impl-*.log`, `artifacts/cb-recon/` (native
session unified logs + daemon disassembly), `artifacts/clipboard-recon.md`
(working notes). Earlier text evidence: `artifacts/gui-clip3.log`,
`artifacts/gui-clip5.log`, `artifacts/server-cb.log`, `artifacts/agent-cb.log`.

## 1. Messages

| Type | Direction | Size | Purpose |
|------|-----------|------|---------|
| `0x06` | client→server | 8 + n | standard RFB `ClientCutText`. Only lands on the remote pasteboard while `0x15 AutoPasteboard(1)` is active. **On this server it installs a *promise*:** `osascript` can read it, but `pbpaste` and app-level paste see nothing. We therefore also send the text as a viewer `0x1f` archive (§4.1). |
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
- local clipboard changes pushed with `0x06 ClientCutText` plus a viewer `0x1f`
  archive carrying the same text (the archive is what makes app-level paste
  work; see §4.1), and with a viewer `0x1f` archive for images, with echo
  suppression so a fetched item is not sent straight back;
- `--no-clipboard` disables all of it (no `0x15`, no fetch, no watcher).

Verified live on one connection: remote copy → local `pbpaste`; local copy →
remote clipboard read back via `osascript`; no stall, no close, 582 framebuffer
updates during the session.

Known cost: the local pasteboard is polled (`pbpaste` / `osascript` per poll,
~700 ms). That is a process spawn per poll, so it is the obvious thing to
replace with an NSPasteboard change-count watcher later.

## 4.1 Local → remote paste: Ctrl+V on Wayland (VERIFIED 2026-11-06)

Automatic duplex (§4) puts the text on the remote pasteboard, but the user still
has to paste it *into an app* on the remote. On macOS that is Cmd+V. On Wayland
neither obvious chord reached that point:

- **SUPER+V is owned by the compositor.** Hyprland binds `SUPER+V` to universal
  paste and `SUPER+SHIFT+V` to its clipboard manager (`hyprctl binds`:
  `modmask 64` and `68`, both `key: V`). The client has no keyboard grab on
  Linux, so the chord never arrives. (VERIFIED: `--debug-input` logged no key
  event for an injected Super+V, while Ctrl+V was logged in full.)
- **Ctrl+V arrived, but was forwarded literally.** The wire showed
  `Control_L down` (`0xffe3`), `v` (`0x76`) down/up, `Control_L up`: macOS
  receives Ctrl+V, which is not paste. (VERIFIED on `mm.local`: TextEdit stayed
  empty even though the remote pasteboard already held the text.)

So ss-gui translates Ctrl+V into the remote's own paste:

- The chord is **Ctrl+V alone** — no Shift, Alt or Super. `Ctrl+Shift+V` stays
  the pull-file shortcut and `Ctrl+Alt+…` stays the gesture-test shortcuts; on
  macOS with the keyboard grab active the path is inert, because Cmd+V already
  reaches the remote natively.
- On the press, the UI thread first lifts any forwarded Ctrl (so the remote sees
  a clean Cmd+V, not Ctrl+Cmd+V), then a worker thread reads the local text with
  `wl-paste --no-newline` **immediately** (not waiting for the ~700 ms watcher)
  and pushes it twice: `0x06 ClientCutText` for standard RFB, then a viewer
  `0x1f` archive carrying the real text data. The text is re-sent even if the
  watcher already pushed it, so a copy immediately followed by a paste cannot
  lose the race. If there is no text, a PNG image is pushed as a viewer `0x1f`
  archive; if there is neither, the log says `paste: local clipboard has no text
  or image - nothing sent` and no paste chord is sent.
- **Why both messages:** `0x06` alone installs a *promise* on this server.
  `osascript -e 'the clipboard as text'` reads it, but `pbpaste` is empty and
  TextEdit's Cmd+V pastes nothing (VERIFIED 2026-11-06: the wire was correct and
  the chord was correct — a remote-set clipboard pasted, a `0x06`-provided one
  did not). The viewer `0x1f` archive carries real data; with it, `pbpaste` and
  remote app paste both work. The automatic watcher uses the same pair, so a
  plain local copy is also pasteable remotely without pressing Ctrl+V.
- After a **200 ms** settle (the server's pasteboard agent needs a moment to
  install the data), the worker sends the remote chord as ordinary KeyEvents:
  `Super_L` (`0xffeb`) down, `v` (`0x76`) down/up, `Super_L` up. It never
  touches `draw()` or the UI thread, so painting does not stall.
- Repeats and the local key-up are swallowed: the remote never sees a bare
  Ctrl+V or a V-up without its V-down.

Verified with `wtype -M ctrl -P v -p v -m ctrl` into the focused ss-gui window
while `--debug-input` was on: the log showed `clipboard -> text …` and `paste:
remote Cmd+V sent ~205 ms after the clipboard`, the wire showed Ctrl-up,
`0x06`, `0x1f`, then `Super_L`/`v`/`v`/`Super_L`, and the remote TextEdit
received the exact string. A second paste of a different non-ASCII string
(`W3-FINAL-TWO-…-日本語`) appended correctly, and the remote `pbpaste` returned
the same bytes. The automatic watcher was on (the normal configuration), which
is required: `AutoPasteboard(1)` is what lets the pushed text land.

Limitation: without a keyboard grab the compositor's SUPER+V cannot be
reclaimed. Ctrl+V is the chord that is guaranteed to reach the window; this is
documented in `ss-gui --help`.

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
  data. Since 2026-11-06 our own text pushes include a real-data `0x1f`
  archive, so `pbpaste` does read *our* text; an item pushed by another client
  as `0x06` only is still a promise.

## 7. File copy

Native file copy is **not** a pasteboard flavour: it is the dedicated `0x22`
channel on the same connection, with its own command numbering and start
handshake. It is implemented and verified in both directions — see
`docs/protocol/filecopy.md`.

What matters here is the boundary: a pasteboard archive carries the file's
*URL* only as a name in the common case (`public.file-url` is stripped by the
server), so pulling a file is a separate `0x22` session, not a pasteboard read.
`docs/protocol/filecopy.md` §"pulling" covers which clipboard shapes may be
pulled at all.

## 8. Verifying on `mm.local`

```sh
# remote -> local text
ssh tai@mm.local 'printf "R2L\n" | sudo -n launchctl asuser 501 pbcopy'
pbpaste            # expect R2L

# local -> remote text (auto, no flags)
printf 'L2R\n' | pbcopy
ssh tai@mm.local 'osascript -e "the clipboard as text"'
ssh tai@mm.local 'pbpaste'      # expect L2R: real data, not a promise

# local -> remote paste: focus ss-gui, then Ctrl+V (Wayland; SUPER+V is the
# compositor's). Check both the clipboard and the app that received the paste.
wl-copy 'paste me'
wtype -M ctrl -P v -p v -m ctrl
ssh tai@mm.local 'pbpaste'
ssh tai@mm.local 'osascript -e "tell application \"TextEdit\" to get text of document 1"'

# local -> remote image, byte check
osascript -e 'set the clipboard to (read (POSIX file "/tmp/x.png") as «class PNGf»)'
ssh tai@mm.local 'osascript -e "set png to the clipboard as «class PNGf»" \
  -e "set f to open for access POSIX file \"/tmp/rt.png\" with write permission" \
  -e "set eof f to 0" -e "write png to f" -e "close access f"'
ssh tai@mm.local 'shasum /tmp/rt.png'
```
