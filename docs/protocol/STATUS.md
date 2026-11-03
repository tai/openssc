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

1. Capture a real trackpad swipe's CGEvent fields on `md.local` (event tap) and
   replay them; settle `motion`/`flags`/`flavor`/series.
2. Settle subtype-8/11 scroll (subtype 11 desynced this server build; mark
   experimental until the field mapping is recovered).
3. Add clipboard (standard `ClientCutText` + Apple `0x1f`/`0x15`), keepalive,
   and mid-session rekey handling.
4. Test `ss-gui --evdev` on a real Wayland host; add udev/permission docs.
5. Investigate MultiVariant (0x44c/0x44d) and adaptive media.
6. Add a `--scale`/quality control to the GUI UI (currently CLI-only).

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
