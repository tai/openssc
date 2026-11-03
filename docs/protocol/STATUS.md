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
60 ms**. MVS is GPU-encoded by the server and is 12x faster; the deflate tiers
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
