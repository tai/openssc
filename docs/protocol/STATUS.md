# Protocol status / agent handoff

## 2026-11-03 — interactive performance pass

- `AutoFrameBufferUpdate` (0x09) now works: server-driven streaming with dirty
  rectangles. The key is re-arming `0x09` + a non-incremental `0x03` on every
  `AppleDisplayLayout` (0x451). Verified with `ss-client stream --auto`:
  a later update was a 64×48 tile instead of a 2560×1600 full frame.
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
  not scroll, so the wire format is line-granular.

### Verified 2026-11-03 — text clipboard

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
