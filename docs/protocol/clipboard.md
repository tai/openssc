# Clipboard / pasteboard — Apple Screen Sharing

Status: **text clipboard VERIFIED in both directions** (2026-11-03, `mm.local`).
Images and file copy are still TODO.

Evidence: `artifacts/gui-clip3.log` (remote→local), `artifacts/gui-clip5.log`
(local→remote), `artifacts/clipboard-archive.bin`, `artifacts/cb-live*.log`,
server/agent logs captured in `artifacts/server-cb.log`, `artifacts/agent-cb.log`.

## 1. Messages

| Type | Direction | Purpose |
|------|-----------|---------|
| `0x06` | client→server | standard RFB `ClientCutText`. **Only lands on the remote pasteboard while `0x15 AutoPasteboard(1)` is active** (VERIFIED: without it the server accepts the text — `viewer text size N`, `clipboard first read N bytes` — but the remote pasteboard stays empty). |
| `0x15` | client→server | `AutoPasteboard`, 8 bytes: `type, reserved[2], selector, reserved[4]`. `1` = start, `2` = stop. Server log: `autopasteboard 1`. |
| `0x0b` | client→server | `ClipboardFetch`, a single byte. Asks for the current remote pasteboard. |
| `0x14` | server→client | `MiscStatus`, 8 bytes. `cmd = 2` (bytes 6..8) = remote clipboard changed → reply with `0x0b`. Has **not** been observed on this build yet (see §4). |
| `0x1f` | server→client | `ClipboardSend`, see §2. |

## 2. `0x1f` ClipboardSend

```
+0x00 u8  0x1f
+0x01 u8  pad
+0x02 u8  promise        # 1 = flavour list only, no data
+0x03 u8  pad
+0x04 u32 (unlabelled; observed 0 or 10)
+0x08 u32 uncompressed_size
+0x0c u32 compressed_size
+0x10 ... zlib stream, `Z_SYNC_FLUSH` framed
```

The payload is **not** length-prefixed at the end; `compressed_size` closes the
message, so the frame is `16 + compressed_size` bytes (observed 197 for a
promise archive). A strict zlib reader fails with "incomplete deflate stream"
because the final block is never emitted — use `Decompress` with
`FlushDecompress::Sync` and stop at `uncompressed_size`.

### 2.1 Inner archive

```
u32 item_count
per item:
  u32 uti_len, uti bytes
  u32 reserved (0)
  u32 alias_count
    per alias: u32 name_len, name, u32 value_len, value
  u32 data_len, data bytes
```

Flavours seen for a plain-text copy:

| UTI | Aliases | Data |
|-----|---------|------|
| `public.utf16-plain-text` | `com.apple.ostype=utf8`… , `public.mime-type=text/plain;charset=utf-16` | UTF-16BE |
| `com.apple.traditional-mac-plain-text` | `com.apple.ostype=TEXT` | latin-1 |
| `public.utf8-plain-text` | `com.apple.ostype=utf8`, `com.apple.nspboard-type=NSStringPboardType`, `public.mime-type=text/plain;charset=utf-8` | UTF-8 |

`u32 item_count` alone covers an empty clipboard (`uncompressed_size = 0`).

## 3. Promise semantics (important)

Enabling `0x15 AutoPasteboard(1)` makes the **viewer the clipboard provider**.
From then on every `0x1f` the server sends has `promise = 1` and every flavour
carries `data_len = 0`, i.e. "this pasteboard belongs to you, I will not resend
it". Verified both ways:

- **No AutoPasteboard** → `0x0b` fetch returns the real remote text
  (`text: "GUI_COPY_NOAUTO\n"`).
- **With AutoPasteboard** → the same fetch returns a promise archive with no
  data, and the remote pasteboard is only readable through AppKit
  (`osascript -e "the clipboard as text"`), never through `pbpaste`.

Consequences for the client:

- `ss-gui --clipboard` = **read mode**: no AutoPasteboard, fetch on connect,
  apply the decoded text to the local clipboard. Off by default: see §3.1.
- `ss-gui --send-clipboard` = **write mode**: AutoPasteboard(1) at connect, push
  local clipboard changes with `0x06`. Remote→local reads are disabled in this
  mode on purpose, because they would only ever yield promises.
- Both directions at once on a single connection is an **open question** (§4).

### 3.1 The fetch closes the viewer (open bug)

`0x0b ClipboardFetch` gets a correct answer — the server logs
`HandleViewerClipboardRequest`, `uncompressed size 96 pasteboardDataCnt 87` —
and then closes the session ~13 ms later (`closing 0`, `going to close 5`).
The client sees `failed to fill whole buffer`.

What we ruled out:

- **Padding the fetch.** Sending `0x0b` padded to 1/4/8/12 bytes behaves
  identically. Zero padding is actively harmful: the server then reads `0x00`
  as SetPixelFormat and logs `bitsperpixel 1 … Only 16 or 32 bit pixels
  supported`, `viewer result -2`, `going to close 9`.
- **Pacing.** Issuing no framebuffer requests at all means the answer never
  arrives (the reply seems to be piggybacked on framebuffer traffic), so the
  close cannot be avoided by idling.

Until the request framing is recovered, `ss-gui` leaves clipboard sync off and
exposes it only through `--clipboard` / `--send-clipboard`.

## 4. Open questions

- How does the native client keep bidirectional sync on one connection? It
  enables AutoPasteboard and still receives remote copies, so either a viewer
  capability declares promise support, or `0x14 cmd=2` triggers a fetch that
  returns real data once the item was *not* created by the viewer. Our fetches
  after a GUI-session `pbcopy` returned no new `0x1f` at all.
- `0x14 cmd=2` was never observed; only `cmd=12` (heartbeat) and `cmd=4`.
- Whether promise items survive disconnect: they do not. Once the viewer
  disconnects the remote pasteboard empties, so a one-shot "send then exit"
  client leaves nothing behind.
- Image (`public.png`) and file-copy flavours are not implemented.

## 5. Verifying on `mm.local`

```sh
# Remote -> local (default mode)
ssh tai@mm.local 'echo REMOTE_TEXT | sudo -n launchctl asuser 501 pbcopy'
cargo run -p ss-gui -- --host mm.local --debug-input   # local pbpaste updates

# Local -> remote (write mode)
printf 'LOCAL_TEXT' | pbcopy
cargo run -p ss-gui -- --host mm.local --send-clipboard
ssh tai@mm.local 'osascript -e "the clipboard as text"'   # NOT pbpaste
```

`pbpaste` runs in a different bootstrap namespace unless started with
`sudo -n launchctl asuser 501 pbpaste`, and it cannot materialise promises —
prefer `osascript` when checking the remote pasteboard.
