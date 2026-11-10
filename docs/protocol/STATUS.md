# Protocol status / agent handoff

Newest entry first. Each entry is a record of what was true **on that date**;
older entries are history, not current behaviour. Current behaviour is the
code, then `docs/protocol/*.md`; how to measure it is `docs/testing.md`. The
top entry is the one to read first, and the one to update at the end of a
session (template in `AGENTS.md` §10).

## 2026-11-10 — project rename `ss-clone` → `OpenSSC` (binary `openssc`)

- Marketing/identity rename only; no protocol or behaviour change intended.
- The GUI package `crates/ss-gui` is now `crates/openssc` and its binary is
  `openssc` (`cargo build --release -p openssc` → `target/release/openssc`).
  Window titles, usage text (`Usage: openssc [options]`), temp-file names, the
  udev rule name (`69-openssc-input.rules`), `ss-client` test strings and
  packaging (Linux `openssc.desktop` / `openssc-launch` /
  `share/icons/hicolor/scalable/apps/openssc.svg`, macOS
  `packaging/mac/build_openssc.sh` → `OpenSSC.app`) all follow the new name.
- Environment/config paths move accordingly: `~/.config/openssc/config`,
  `$XDG_STATE_HOME/openssc/openssc.log`, lock `/tmp/openssc-gui.lock`. The
  `SS_HOST` env var is unchanged; `SS_GUI` (launcher binary override) is kept
  for compatibility.
- Dated experiment records below (`## YYYY-MM-DD` entries) and
  `docs/findings/*` keep the spellings that were true on the day — they cite
  measurements, not current state. Current-state docs (`README.md`,
  `AGENTS.md`, `docs/testing.md`, `docs/protocol/*`) use the new name.
- Verified: `cargo test --workspace` green after the rename; macOS bundle
  built and launched from `packaging/mac/OpenSSC.app`.

## 2026-11-06 — **v1.5.0**: full-resolution MVS decode throughput and visual-latency parity (VERIFIED on the replay fixture and on camera)

- Phase / milestone: M1/M6 — the client must keep up with the server during a
  scroll at 2560x1600 without shrinking the image.
- **Camera parity check (VERIFIED):** two screens filmed at 30 fps, identical
  synthetic scroll into each client, window geometry pinned for both. Envelope
  lag between the remote screen and the client window fell from **483 ms** to
  **157 ms** for ss-gui (SS.app: 237 ms); time after the remote settles before
  the window settles fell from 533 ms to 133 ms (SS.app: 267 ms). The client now
  presents the motion in about the time the remote takes (stretch x0.94, was
  x1.38). Before/after runs were on the same page with the old client rebuilt
  from `2d0ea47`. Record: `docs/findings/2026-11-06-visual-latency-parity.md`.
- Verified: a captured scroll burst (132 bodies, 1,471,236 tiles, 233,522 DCT
  blocks, 94.2 Mpx) now decodes in **271.6 ms / 346.7 Mpx/s**, from
  **1000.2 ms / 94.1 Mpx/s** before. Live backlog over the same 300 px scroll:
  p50 **681 -> 16-17 ms**, p90 1081 -> 56-58 ms, max **1373 -> 88-106 ms**. Rendering
  stays within 1 channel of the f64 reference, asserted over the captured
  fixture, two push manifests and randomised blocks (full i16 range).
- Changed: `MvsDecoder` — fixed-point `islow` IDCT (18-bit constants, i64,
  single final descale, DC shortcut), direct-to-framebuffer tile writes, run
  spans for fills, chroma constants hoisted for DC-only chroma, incremental
  raster position, `peek`/`skip` on the bit reader. `ss-gui` — decoder thread
  publishes once per drained batch; `SS_DUMP_MVS` analysis hook; the reader
  heartbeat now advances on socket reads and treats a blocking read as alive;
  the 2 s push watchdog is an idle note, not "giving up".
- Measured with: `cargo test -p rfb-proto --test mvs_replay -- --nocapture`
  (skips when `artifacts/mvs-scroll-burst.bin` is absent) and `SS_LATDBG=1`
  (`tools`-free: pair `DEC ... first_arr=` with the batch end).
- Open: the 150 ms target on that fixture is **not reached** (271.6 ms). The
  remaining profile is render 20%, IDCT 17%, block decoder + bit reader 30%,
  command loop 12%, tile copies 10%; another 2x across all of them is needed.
  Also: the live client decodes a heavy body ~2.7x slower than the replay
  benchmark for the same bytes (UI blit and memory traffic on the same
  machine) — that gap is unmeasured and is the next thing to look at.
- **Cursor geometry fix (2026-11-06):** `0x451` was only advertised in the Zlib
  branch, so `--mvs` sized the cursor by framebuffer scaling (half size on a 2x
  backing) and the push re-arm keyed on `0x451` could never fire. Now advertised
  with MVS and `--encoding`; verified live (arrow 17x23 points -> 34x45 px).
- **Docs audit (2026-11-06):** every document was compared against the sources
  and corrected where they disagreed, taking the code as truth. New
  `docs/testing.md` records the measurement layers, the harness in
  `tools/measure/`, the recorded numbers and the pruning rule for `artifacts/`.
  Corrected in this pass: the scroll contract (§8.1 — `fixedDelta` carries the
  trackpad delta, it is not line-granular), `codecs.md`'s "pixel decoding
  unverified" status and its deflate-tier latency conclusion (measured while
  the reader decoded inline), the file-copy frame (pad byte at +1, body at +6,
  `level` at +0x64), Shift forwarding, and the `--scale` / clipboard / push
  claims that later work overturned.
- Lab state: `mm.local` healthy; `Screen Sharing` re-shown on `md.local`;
  no ss-gui running. `artifacts/` pruned to the fixtures tests read (9 MB from
  618 MB).

## 2026-11-05 — **v1.4.1**: keyboard grab, `--help` (VERIFIED)

- Phase / milestone: local shortcuts now reach the remote.
- Verified: with the window focused a session-level `CGEventTap` consumes
  keyDown/keyUp/flagsChanged and routes them through the same key mapping as
  winit; `--debug-input` shows 12 `tap key` events for a 6-character test. Focus
  loss disables the tap and focus re-enables it; shutdown releases it.
- Release chord: hold **Left Cmd+Shift 350 ms** (modifier-only chords need the
  hold so `Cmd+Shift+V`, `Cmd+Shift+3/4/5` etc. do not drop the grab on the way
  through). `--release-grab` overrides.
- Needs Accessibility/Input Monitoring; without it the app logs the pane and
  runs ungrabbed rather than failing. Pointer is deliberately NOT grabbed.
- Also: `--help`/`-h` in ss-gui (exit 0, grouped usage), with a test that fails
  if a parser flag is missing from the text; unknown flags still exit 2.
  `ss-client`'s usage brought up to date.
- Changed: new `crates/ss-gui/src/grab.rs` (tap, chords, keycode mapping,
  permission); main.rs routes tap events and prints usage.
- Open: mouse-grab not implemented (by choice); JIS-specific keys still have no
  keysym; `cmd 200` frames; Wayland packaging.
- Lab state: `mm.local` healthy, no client running.

## 2026-11-05 — **v1.4.0**: JIS keyboard (VERIFIED)

- Phase / milestone: input fidelity on a Japanese keyboard.
- Verified (owner and me, live): with a JIS/JIS pair, `a A @ ^ :` and the rest of
  the matrix type correctly; before this, `@` produced `2` and `^` produced `6`,
  and Shift was unreliable.
- Root cause: the client translated a character into a **US physical key plus
  Shift** (hardcoded table). On JIS that is the wrong key, and Shift is ignored
  for printable keys by this server anyway, so the shift was dropped too.
- Fix: send the **character's keysym** and let the remote layout resolve the
  position; `A` goes as `0x41` with no Shift, `@` as `0x40`, `:` as `0x3A`.
  Named keys keep the same shape (their named keysym). Layout is data
  (`position -> (plain, shifted)`), selected with `--keymap us|jis` for the
  REMOTE layout, default `us`.
- Limitation, documented: the daemon's keysym tables have no JIS-specific
  entries (yen, 変換/無変換, かな/英数), so those keys are logged as unsupported.
- Changed: ss-gui key handling rewritten around a `Keymap` (tables + inverse),
  `character_keysym`, and `wire_keysym`; Shift no longer synthesised per
  character; JIS tables added.
- Open: JIS-specific keys need raw keycodes or another channel; `cmd 200` status
  frames; xattr trailer; Wayland packaging.
- Lab state: `mm.local` healthy, no client running.

## 2026-11-05 — **v1.3.0**: pull a file/folder from the remote (VERIFIED)

- Phase / milestone: file copy, reverse direction (no drag-out; explicit pull).
- Verified (owner, live): remote Finder `Option+Cmd+C` then `Cmd+Shift+V` in
  ss-gui pulls the file into `~/Downloads` with a matching sha1.
- The bug behind the long "no path" hunt was **input, not clipboard**: our
  client forwarded Option as `0xFFE9` (Alt_L), which this server accepts as a
  key but never raises the Option flag, so the remote received plain `Cmd+C`,
  Finder copied the file, and the clipboard carried the item name instead of a
  path. Sending `0xFFE7` (Meta_L) fixes it. Modifiers are now re-synced per key
  so a missed `ModifiersChanged` cannot drop one. Note this contradicts a static
  reading of the daemon's KeyMap table - empirical result wins, re-test if ever
  revisited. `docs/protocol/input-and-gestures.md` §5.
- Also verified: a plain Finder Cmd+C of a file genuinely cannot carry a path -
  the server strips the file URL from the archive and no client, Apple's
  included, can recover it. Finder's *Copy as Pathname* is the supported
  gesture, documented in `filecopy.md` §8a.
- Changed: ss-gui gains the Cmd+Shift+V pull (fresh clipfetch with retry, path
  selection, actionable refusal), modifier re-sync, and the file-copy receive
  worker; `tools/probe/clipboard_pull_ab.py` is the A/B archive dumper used to
  settle the question.
- Open: drag-out remains out of scope; `cmd 200` status frames still logged as
  unknown; folder drops already work, xattr/resource-fork trailer does not.
- Lab state: `mm.local` healthy, test files removed on both sides, no client
  running.

## 2026-11-05 — **v1.2.0**: file copy, send by drop / receive into ~/Downloads (VERIFIED)

- Phase / milestone: post-v1.1.0, file copy.
- Verified (owner, live):
  - local -> remote: a 300000-byte file dropped/`--send-file` arrived at
    `/Users/tai/Downloads/verify-send.bin`, sha1 identical, mode 0644, owner tai.
  - remote -> local: a 500000-byte file arrived in `~/Downloads/fc-verify.bin`,
    sha1 identical; a second transfer of the same name produced
    `fc-verify 2.bin` and left the first untouched.
  - a directory drop is refused with a clear message rather than hanging.
- The blocker that cost the most time: `NewItem` (cmd 101) carries the item
  metadata and **the POSIX mode lives at +0x66**. With that block zeroed the
  receiver creates the file mode 000 in the macOS in-progress state and every
  data write fails. Solved by capturing a genuine server-sent `NewItem` during a
  reverse transfer (we decrypt our own stream), not by guessing.
- Wire: `0x22` + u32 length, body = version/cmd/sessionID, then cmds 2/100/101/
  102/104 (and 1/3/4/5 for the other flows). 64 KiB data chunks. The viewer
  names the destination folder; the remote launches `SSFileCopyReceiver` and
  never prompts.
- Changed: new `rfb-proto::filecopy` and `ss-gui::filecopy`; ss-gui gains a
  `DroppedFile` handler, a receive worker writing `~/Downloads`, and the
  `--send-file` / `--recv-file` test hooks.
- Captures / experiments (untracked): `artifacts/filecopy-1/` (native session
  log), `artifacts/filecopy-2/` (captured bodies incl. the reference `NewItem`),
  `artifacts/filecopy-recon.md`.
- Open: `cmd 200` status frames are logged as unknown; folder drops; xattr /
  resource-fork block; a user-facing way to pull a file from the remote
  (`StartFileSend` works but there is no drag-out).
- Lab state: `mm.local` healthy, all test files removed from both machines, no
  client running.

## 2026-11-05 — **v1.1.0**: remote cursor + duplex clipboard (VERIFIED)

- Phase / milestone: v1.0.x clipboard.
- Verified:
  - `0x0b ClipboardFetch` is **8 bytes**, byte 1 bit 0 = "promises only". Our
    old 1-byte fetch left the server 7 bytes short, so the *next* message was
    parsed as SetPixelFormat and the session closed ~13 ms later. That was the
    long-standing "the fetch closes the session" bug — it was our framing.
  - Duplex works with `AutoPasteboard(1)` active: a fetch with flags 0 returns
    the real archive even while the viewer is the pasteboard provider. Native
    client does exactly this (`only want promises` at connect, `send all data`
    after a remote copy).
  - `0x14 cmd=2` is really sent by this build and triggers the fetch.
  - Viewer→server `0x1f` (`HandleViewerClipboardSend`) is the same 16-byte
    header + zlib archive, so images push over the same format as text.
  - Images round-trip byte-identically both ways (sha1 matched on 549 B and
    21484 B PNGs), with echo suppression to stop ping-pong.
- Changed: `rfb-proto` gains `pasteboard_items`/`pasteboard_image`/
  `viewer_clipboard_send` and the 8-byte fetch; `ss-gui` now enables clipboard
  by default (AutoPasteboard, fetch on connect and on `0x14 cmd=2`, local
  watcher, echo suppression) with `--no-clipboard` as the escape hatch.
- Captures / experiments (untracked): `artifacts/verify-clip.log`,
  `artifacts/cb-recon/`, `artifacts/clipboard-recon.md`.
- Open: the local pasteboard is polled every ~700 ms (a process spawn per poll);
  an NSPasteboard change-count watcher would remove that. File copy (`0x22`
  `RFBFileCopy_*`) is a separate milestone and not started.
- Next command: `cargo run -p ss-gui -- --host mm.local --mvs` and copy
  something in either direction.
- Lab state: `mm.local` answers `RFB 003.889`; console user `tai` logged in; no
  stray clients or captures.

## 2026-11-04 — **v1.0.0**

- Milestone: the client is now on par with the real `Screen Sharing.app` in
  normal use, confirmed by the owner on the lab pair. Full summary and the
  explicit not-included list are in `AGENTS.md` §13.
- What got here: MVS fast path (44–49 ms full frame vs 632–880 ms for the
  deflate tiers), the complete MVS push vocabulary (type-0 dual streams,
  spatial records, 99-byte coefficient ring, type-1 refinement), and server push
  as the default transport (`--mvs` arms `0x09` screen 0 + one `0x03`; `--poll`
  keeps the request loop).
- Latency: keystroke → painted p50 92 ms with push, 85–98 ms polling, against
  65–72 ms for the native client measured at the packet boundary. A paired
  packet capture found no native advantage (native was 13–14 ms later), so the
  remaining difference is server capture/encode cadence, not our request path.
  Frame pacing, not round trip, is where any further gain would come from.
- Next: Linux/Wayland packaging (the actual target platform), swipe semantic
  calibration, force click and drag-gesture variants, clipboard images,
  keepalive/rekey hardening for long sessions.

## 2026-11-04 — push-mode MVS: cache + type-1 decoded (VERIFIED)

- Phase / milestone: M6 fast path, push mode.
- Verified:
  - The push stream's type-0 deltas reference earlier tiles through a 99-byte
    coefficient ring (`viewer+0xda0`); codes 6/7 read it and **only type-1
    code 1 writes it**. Our decoder ignored 6/7, which is exactly why push
    sheared while polling was clean (18,768 + 7,727 cache tiles in 45 of 51
    pushed delta bodies; zero in polling bodies).
  - Type-1 is refinement of 84-byte per-tile spatial records: 2-bit code per
    tile, `mvs` terminator, `Y64 / Cr15 / Cb20` ring order, luma AC Huffman
    table for both chroma planes. Full contract in `codecs.md` §2.20, with two
    corrections to earlier notes (code 1 = left/previous, code 2 = above).
  - Push arming needs `0x09` with **`selected_screen = 0`** plus one
    non-incremental `0x03`; `0xffffffff` stalls after the quant body.
  - Rust decoder over an 89-body push capture: 20,082 ring insertions,
    `last_read` 20,073, cursor 20,082 — matching the independent Python parser.
  - Independent native-assisted renderer: Rust agrees to **max channel diff 1,
    0 pixels above 1**, at bodies 12 / 45 / 78 and the final frame. The ±1 is
    the known native-vs-portable IDCT rounding.
  - Latency (twelve keystrokes, polling MVS): p50 85–98 ms, min 43 ms. The
    request loop is not the bottleneck — applied rects arrive 6–9 ms apart
    during a repaint; the floor is one server capture interval.
- Changed: `rfb-proto::mvs` gains the spatial-record table, the coefficient
  ring, type-1 decoding and `wrapping_mul` IDCT scaling; `ss-client` gains
  `mvs-sequence` (whole-manifest replay with per-body SHA-256). `ss-gui` gained
  the bootstrap-wedge fix, MVS banding removal and `--debug-input` latency
  instrumentation in the preceding commit.
- Captures / experiments (untracked): `artifacts/push-pC/` (89 bodies),
  `artifacts/push-codex-manifest.txt` (222 bodies),
  `artifacts/push-codex-render.py` (oracle), `artifacts/verify-push-rust.png`.
  Record: `docs/findings/2026-11-04-push-mode-mvs.md`.
- Open questions: does push beat the ~85 ms polling floor (needs keystrokes
  through a correctly-rendering push GUI); where the capture-interval floor
  actually comes from; a synthetic content-free fixture so CI exercises the
  cache/type-1 grammar (the replay tests skip without `artifacts/`).
- Next command: `cargo run -p ss-client -- mvs-sequence --manifest
  artifacts/push-pC/manifest.txt --out artifacts/push-rust.png`.
- Lab state: `mm.local` answers `RFB 003.889`; console user `tai` logged in; no
  clients left running. Note: `screensharingd` was restarted once early in the
  session (launchd respawn, verified healthy) before the no-restart rule was
  issued — it must not happen again without owner approval.

## 2026-11-03 — interactive performance pass

- `AutoFrameBufferUpdate` (0x09) now works: server-driven streaming with dirty
  rectangles. The key is re-arming `0x09` + a non-incremental `0x03` on every
  `AppleDisplayLayout` (0x451). Verified with `ss-client stream --auto`:
  a later update was a 64×48 tile instead of a 2560×1600 full frame.
**SUPERSEDED 2026-11-06**: the scaled path below does deliver incremental
updates after all (a 1280×800 session measured a p50 15 ms decode backlog); the
"stops incremental updates" conclusion was a 2026-11-03 observation.

- `ScaleFactor` (0x08) at 0.5 produces 1280×800 full frames (4,096,000 bytes)
  instead of 2560×1600 (16,384,000), **but this server build stops sending
  incremental updates after the initial scaled frame**. The GUI therefore
  defaults to full resolution (1.0) and treats `--scale <1.0` as experimental.
- Auto-streaming alone also stalled after the initial frame. The GUI now uses
  full-resolution **incremental polling** (`0x03` with the framebuffer
  geometry), which reliably delivers post-input dirty rectangles.
- Dev profile now optimizes `rfb-proto`, `flate2`/`miniz_oxide`, `aes`, and
  `sha1` at opt-level 3; release uses thin LTO.
- GUI pointer motion is coalesced to ≤125 Hz; button events are immediate.
- Render mapping is cached; metadata-only updates no longer redraw.
- Measured cold start + first scaled frame + dump: ~1.3–1.6 s on the lab LAN.
- Fixed GUI input under scaling: the server's `ScaleFactor` scales only the
  image; pointer events must stay in the original backing coordinate space.
  Sending scaled coordinates made clicks land at half position. Input now uses
  `input_w`/`input_h` (server geometry) while rendering uses the scaled
  framebuffer.
- Re-verified GUI input under scaling: local click/keys produced `line 014gui`
  in remote TextEdit after the coordinate fix.

## 2026-11-03 — live GUI, input, and first verified gestures

### Verified

- `RFB 003.889`; security types `30, 33, 36, 31, 32, 35`; native client chooses
  **33**.
- Type 33 = RSA1 identity envelope + SRP-6a; exact recipe confirmed by matching
  the real client's `M1` against a fake server with a known verifier.
- Record layer = `0x44f` rekey, AES-128-CBC, SHA-1 integrity, persistent CBC
  state, `u16 ciphertext_len` framing.
- Full 2560×1600 Zlib framebuffer decoded to PNG. The server uses **one
  continuous zlib stream** across updates: the first rectangle carries
  `78 01`; later rectangles are continuations.
- Multi-rect/incremental updates work (`ss-client stream --frames 6`).
- Keyboard + pointer input works end-to-end through both the CLI and the GUI.
- Gesture message `0x17` and subtypes 1–8/11 recovered; field layouts and
  server-side CGEvent mapping documented.
- **Magnify is verified live**: a phase series (1/2/2/4, mask 31) zoomed
  Preview on `mm.local`.
- `mm.local` trackpad settings: vertical three/four-finger swipes disabled;
  horizontal swipes enabled.
- The GUI window is a regular 1280×802 window; macOS Screen Recording
  permission blocks `screencapture`, so use `--dump` to verify compositing.

### Implemented

- Python reference: `tools/probe/apple_client.py`, `apple_framebuffer.py`,
  `apple_session.py`, `gesture_probe.py`, `fake_srp_server.py`, `mitm_proxy.py`.
- Rust protocol: `rfb-proto` (auth, SRP, RSA, record layer, Zlib stream,
  input, gesture builders, reader/writer split, unit tests).
- CLI: `ss-client` (`probe`, `snapshot`, `stream`, `demo-input`, `gesture`).
- GUI: `ss-gui` (winit + softbuffer; live BGRX rendering, input forwarding,
  pinch/rotate/pan/smart-zoom, `--dump` headless compositing).
- Gesture input: `gesture-input` (pure-Rust MT-B recognizer with tests; Linux
  evdev reader; `--evdev`).
- Static analysis: `tools/instrument/framework_probe.py`,
  `annotate_method.py`, `dump_addr.py`.
- Packaging: `packaging/ss-clone.desktop`, `scripts/run-gui.sh`.

### Next

1. Settle subtype-6 swipe values (`motion`/`flags`/`flavor`/series). Capturing
   the real client on `md.local` is blocked: Mission Control consumes the local
   three-finger swipe before `Screen Sharing.app` sees it, and the client then
   sends nothing.
2. Keepalive and mid-session rekey handling; clipboard duplex on one
   connection (see `docs/protocol/clipboard.md` §4).
3. Test `ss-gui --evdev` on a real Wayland host; add udev/permission docs and
   calibrate the evdev scroll/zoom feel constants.
4. Investigate MultiVariant (0x44c/0x44d) and adaptive media.
5. Verify horizontal scroll (`dx`) and settle the subtype-11 tail bytes.

### 2026-11-04 — MVS streams decoded (Codex + OpenCode)

**Two independent streams, not one.** Commands come from stream A
(`body[6:data_off]`, reader at `state+0x98`); every payload — solids, bitmaps,
DCT blocks, cache indices — comes from stream B (`body[data_off:]`, reader at
`state+0xa8`). Reading payloads from A is what broke every earlier parser.

A full frame now parses exactly: **64,000 tiles, 1,534 commands, 2,352 DCT
blocks**, stream A 12,361/12,376 bits and stream B 145,378/145,392 bits, each
ending with a **bit-aligned `0x6d` marker** plus zero padding. The block decoder
`0x7ff922480b28` is callable in-process (ABI and state layout recovered) and
accounts for 127,230 payload bits.

Independent confirmation: `tools/probe/mvs_commands.py` (Codex) and
`tools/probe/mvs_block.py` (full payload walk, with assertions). See
`docs/findings/2026-11-04-codex-mvs-streams.md`.

Still open: the block decoder is driven through the framework rather than
reimplemented (bit consumption is validated; pixel output is not), codes 3/6/7
were not exercised by the capture, and payload widths need a second capture
with different content.

- Next command: `python3 tools/probe/mvs_block.py artifacts/mvs-4.bin`
- Lab state: no preferences changed or test clients started; banner rechecked as
  `RFB 003.889`, console user `tai` still logged in.

### 2026-11-04 — banded refresh: big reported latency win without MVS

Full-screen requests are answered in one 700-880 ms block, which is what made a
window raise feel stalled. Sub-region requests are answered promptly **as long
as they are non-incremental**; incremental sub-region requests stall as soon as
a band has no changes (the server does not reply at all).

Measured with 8 horizontal bands on `mm.local`:

| Scheme | First content | Full screen |
|---|---|---|
| one full request | 700–880 ms | 700–880 ms |
| 8 bands, non-incremental | ~80 ms | ~400 ms |
| 8 bands, incremental | stalls after 2–3 bands | — |

`ss-gui` now sweeps non-incremental bands while input was seen in the last
250 ms (observed: 204-row bands arriving **~35 ms apart**, whole screen in
~180 ms), and falls back to cheap incremental full-screen polling when idle
(41 small updates over 13 s idle, no bandwidth spike). `--bands N` overrides,
`--bands 1` restores the old behaviour.

### 2026-11-03 — codec survey: MVS is the only fast path

Full-screen update production time on `mm.local`: Raw 1590 ms, Zlib 880 ms,
`0x3ea`/`0x3e9`/`0x3e8` all ~700 ms (33 KB and 632 KB payloads take the same
time, so the cost is per-pixel conversion, not compression), **MVS `0x3f3`
60 ms**. MVS is GPU-encoded by the server and is 12x faster *(measured with the
pre-fix client; see the caveat at the top of `codecs.md` — the deflate tiers'
floor was at least partly our own receive path)*; the deflate tiers
only reduce bandwidth.

Implemented meanwhile: `0x3ea` (RGB555 **big-endian**, verified pixel-exact
against `screencapture`), `0x3e8`/`0x3e9` passthrough, and a 0x3f3 container
parser that captures MVS bodies for analysis. MVS container layout and the
quantisation-table message are verified; the tile bitstream (command codes,
field order, LSB/MSB) is still open. See `docs/protocol/codecs.md`.

### 2026-11-03 — why a window raise takes ~500 ms

The user's sharpest observation: raising a window is instant on the real
monitor but takes 300–1000 ms in the mirror. Measured by timing
`read_update()` (which includes waiting for the server plus our decode) with
pipeline 1:

| Update | `read_update` time |
|---|---|
| 296x104 (small) | 60–88 ms |
| 1896x584 (~4.4 MB) | 500–620 ms |
| 1662x236 (~1.5 MB) | 360–420 ms |

Our decode of a 4 MB Zlib rect is ~30 ms and the whole `draw()` is 6–7 ms, so
~90% of that time is the server producing the update. `screensharingd` sits at
**0.4% CPU** during the wait, so it is not CPU-bound: it is the server's
encode/scan path, and the cost scales with the changed area.

Mitigations tried and their outcome:

- **Sub-region ("band") requests**: a band arrives in ~40 ms instead of 500 ms,
  but the server stops answering after the first pass, whatever the pipeline
  depth. Recorded as a server-side limitation.
- **Raw encoding**: some large rects arrived quickly, but frames reach 10 MB and
  the cadence is inconsistent; not a win on this link.
- **`0x44c`/`0x44d` (MultiVariant)**: advertising only 1100/1101 makes the
  server fall back to Raw, so MVS is gated behind the AVConference `0x1c`
  media-stream handshake — it is not reachable by encoding advertisement alone.
- **AutoFrameBufferUpdate**: the CLI free-runs indefinitely (60 frames over
  18 s), the GUI still stalls after the initial burst even when the arming
  bytes are sent pre-split (byte-identical messages). Unresolved.

Also fixed: `read_update()` used to swallow zero-rect updates, which deadlocks
any client that waits for a response before issuing the next request (this is
what stalled sub-region requests).

### 2026-11-03 — mirror latency measurements (the real bottleneck)

User observation: the remote monitor reacts instantly while the mirrored image
lags, so input delivery is fine and the framebuffer path is at fault. Measured
on a 2560x1600 host with a remote terminal printing continuously:

| Configuration | Updates in 4–5 s | Notes |
|---|---|---|
| polling, pipeline 1 | 82 / 5 s (~16 fps) | old default |
| polling, pipeline 3 | 104 / 5 s (~21 fps) | **new default** |
| polling, pipeline 8 | 77 / 4 s (~19 fps) | throughput plateau |
| Zlib only (6) | 90 / 4 s | best of the encodings |
| Raw only (0) | 18 / 4 s | far worse on the wire |
| ZRLE only (16) | 0 | server ignores/produces nothing |
| `--scale 0.5` | 0 pixel updates | scaled mode still stalls after the first frame |
| `--bands 8` | 0 | server stops answering sub-region requests |

Client render cost was a second, independent problem: `draw()` took **15–28 ms**
per frame because the framebuffer was byte-addressed and the window
(2560x1548) never matched the framebuffer 1:1, so every frame went through the
per-pixel scalar scaling loop. Rewritten to a `Vec<u32>` framebuffer with a
row-`memcpy` fast path and `opt-level = 3` for `ss-gui`: **~6–7 ms**, i.e. the
copy is now memory-bandwidth bound rather than interpreter bound.

Conclusion: with Zlib at full resolution the server's per-request screen scan
and encode cap the mirror at roughly 20 fps; pipeline depth no longer helps.
The remaining route to SS.app-like smoothness is **MultiVariant** (tiles, far
less data per update) or making `AutoFrameBufferUpdate` work from the GUI.

### 2026-11-03 — stability and smoothness pass

- **5-minute continuous session, zero read errors** (683 framebuffer updates,
  polling at the idle cadence). No keepalive message is required by the client
  for at least this long; `0x14 cmd=12` heartbeats are simply skipped.
- The GUI polls every **4 ms while input was seen in the last 250 ms** and
  every 16 ms when idle, cutting the input→frame round trip during scrolling
  without raising idle traffic.
- `--auto` (server push, `0x09`) still free-runs from `ss-client stream --auto`
  (30-frame run, small dirty tiles, no polling) but **stalls after the initial
  burst in `ss-gui`** even though the GUI sends the byte-identical sequence.
  Root cause unknown; polling remains the shipping path.
- Subtype-11 offsets corrected: two 4-byte words sit at `+0x1a` and `+0x2a`.
  With the corrected layout, `pointDelta*` and `fixedDelta*` (16.16) still do
  not scroll, so the wire format is line-granular. **SUPERSEDED 2026-11-05**:
  `fixedDelta` does scroll once the whole delta is sent in it with the line
  field zero — that is the native client's shape and what the client sends for
  trackpad input (`input-and-gestures.md` §8.1). `pointDelta` is the only field
  this server ignores.

### Verified 2026-11-03 — text clipboard

**SUPERSEDED 2026-11-05 by v1.1.0** — the "cannot share one connection" and
"closes the viewer after a fetch" findings below were caused by our own fetch
being 1 byte instead of 8; the clipboard is now automatic and duplex. Keep this
entry only as a record of the wrong turn.

Remote→local and local→remote text sync both work, via `0x0b ClipboardFetch`
(no AutoPasteboard) for reads and `0x06 ClientCutText` + `0x15
AutoPasteboard(1)` for writes. Enabling AutoPasteboard puts the session into
*promise* mode, so the two directions cannot share one connection; and this
server build closes the viewer right after answering a fetch
(`going to close 5`). Clipboard is therefore opt-in (`--clipboard` /
`--send-clipboard`) and off by default. Full framing in
`docs/protocol/clipboard.md`.

### Verified 2026-11-03 — two-finger scroll

Subtype 11 (MouseScroll), exactly 58 bytes, **`dy` = line delta is what macOS
honours**: `dy > 0` scrolls up, `dy < 0` scrolls down, ≈1 line per unit, and a
lone `phase = 2` frame is enough. `pointDelta*` had no visible effect. The
previous 50-byte frame desynchronised the server (hang), now fixed. `ss-gui`
uses this path for both winit wheel events and the evdev recogniser.

*(Since 2026-11-05 the trackpad path sends the whole delta in `fixedDelta` as
16.16 lines with the line field zero, because that is what the native client
does and it makes the motion sub-line smooth. The line field above still applies
to wheel input. See `docs/protocol/input-and-gestures.md` §8.1.)*

Evidence: `artifacts/scroll-after-wheel.png`, `artifacts/dy-only.png`,
`artifacts/dy-single-up.png`, `artifacts/gui-after.png`.

### Lab state

- `mm.local:5900` healthy; console user `tai` logged in.
- Test apps (`TextEdit`, `Preview`) closed and temp files removed after the
  gesture/input experiments.
- Client encryption/logging prefs restored to defaults.
- All test clients and GUI processes exited.

### Captures (untracked)

- `captures/20261103-baseline-handshake.pcap`
- `artifacts/mitm-auth-full.log`, `artifacts/fake-srp-*.log`
- `artifacts/remote.png`, `artifacts/rust.png`, `artifacts/input.png`
- `artifacts/gui-dump.png`, `artifacts/gui-in-after.png`,
  `artifacts/mag-before.png`, `artifacts/mag-after.png`

### Open questions

- Required swipe event values/space configuration on macOS.
- Exact subtype-11 wheel field order (`scrollCount`/`flags` vs
  phase/momentum).
- MultiVariant framing and whether Zlib is sufficient for all quality modes.
- Clipboard archive format edge cases and rekey timing.
