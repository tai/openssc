# ss-clone

A protocol-compatible client for Apple Screen Sharing (`RFB 003.889` + Apple
extensions), targeting **Linux/Wayland** while using the real macOS server as
the compatibility oracle.

This repository contains:

- a verified protocol write-up in [`docs/protocol/`](docs/protocol/),
- Python probes that recovered the protocol,
- a portable Rust protocol crate (`rfb-proto`),
- a working Rust CLI (`ss-client`) that authenticates, captures the framebuffer,
  sends input, and emits Apple gesture messages.

## Status

| Area | Status |
|------|--------|
| RFB 003.889 handshake | ✅ verified |
| Auth type 33 (RSA1 + SRP-6a) | ✅ verified, Rust + Python |
| Auth type 30 (legacy DH) | 📋 observed, not implemented |
| Rekey + AES-128-CBC/SHA-1 record layer | ✅ verified, Rust + Python |
| ServerInit / Zlib framebuffer | ✅ verified, Rust + Python |
| Keyboard / pointer input | ✅ verified via CLI and GUI (click + type on `mm.local`) |
| Gesture message 0x17 | ✅ wire format verified; **magnify verified live**; swipe/scroll calibration ongoing |
| Live window (winit + softbuffer) | ✅ built, macOS-tested; Wayland build pending |
| Linux evdev gesture backend | ✅ implemented + type-checked; hardware test pending |
| Clipboard / audio / MultiVariant | 🚧 later |

## Quick start (macOS lab)

Requirements: Rust 1.97+, `mm.local` reachable, account `tai` / `$SS_PASSWORD`.

```sh
# M0: authenticate and print ServerInit
cargo run -p ss-client -- probe --host mm.local

# M1: capture the remote desktop to PNG
cargo run -p ss-client -- snapshot --host mm.local --out artifacts/rust.png

# M2: click at backing-pixel (1000,800) and type "hello"
cargo run -p ss-client -- demo-input --host mm.local \
    --x 1000 --y 800 --text hello --out artifacts/input.png

# Gesture message: swipe (52-byte type 0x17 subtype 6)
cargo run -p ss-client -- gesture --host mm.local --kind swipe \
    --motion 1 --swipe-mask 1 --progress 1.0 --series --out artifacts/swipe.png

# Verified gesture: magnify (zooms Preview on the remote host)
cargo run -p ss-client -- gesture --host mm.local --kind magnify \
    --value 0.2 --mask 31 --series --out artifacts/magnify.png

# Live GUI window (full resolution; dirty-rect polling)
cargo run -p ss-gui -- --host mm.local

# Experimental server-side downscale (may stop incremental updates)
cargo run -p ss-gui -- --host mm.local --scale 0.5

# Live GUI + headless frame dump (verifies compositing)
cargo run -p ss-gui -- --host mm.local --dump artifacts/gui.png --dump-after 1
```

## Performance

The first version polled full 2560×1600 Zlib frames, which is unnecessarily
slow. The current client uses:

- **Server-driven updates** (`AutoFrameBufferUpdate`, 0x09) with the required
  re-arm on every `AppleDisplayLayout` (0x451), so the server sends dirty
  rectangles instead of a full frame per request.
- **Full-resolution polling** with dirty rectangles. `ScaleFactor` (0x08) is
  implemented (`--scale <1.0`) but **experimental** on this server build: after
  the initial frame it stops delivering incremental updates, so the GUI
  defaults to `1.0`.
- **Optimized dev profile**: `rfb-proto`, `flate2`/`miniz_oxide`, `aes`, and
  `sha1` are compiled at opt-level 3 even in `cargo run`; release adds thin LTO.
- **Pointer coalescing** to at most ~125 Hz; button events are immediate.
- **Cached render mapping** and a 1:1 fast path; metadata-only updates no
  longer trigger redraws.
- A bounded-latency reader: no per-frame polling loop when the server is
  streaming.

Measured on `md.local` → `mm.local`: cold GUI connect + first scaled frame +
headless dump completes in ~1.3–1.6 s in an optimized debug build.

## If input seems dead

- The GUI now forces itself to the front and makes the window key at startup.
  The title says `click to focus` when it is not focused, and
  `Apple Screen Sharing` when it is.
- `--scale` only scales the **image**; pointer/gesture coordinates remain in
  the server's original backing space (`input_w`/`input_h`), so clicks land at
  the position you actually clicked.
- Run with diagnostics:

  ```sh
  cargo run -p ss-gui -- --host mm.local --debug-input
  ```

  You should see `window focused=true`, `mouse Left Pressed`, and
  `key Pressed logical=Character("a") …` for real events.
- For Japanese/CJK IME input, add `--ime`. Non-ASCII characters are sent as
  X11 Unicode keysyms (`0x01000000 | codepoint`).
- If the title changes to `ss-clone — disconnected`, the reader thread hit a
  protocol/socket error; the terminal shows the reason. Re-run with
  `--scale 1.0` to rule out a server-side scaling issue.
- Clicking a **terminal** or another app takes focus away; click the
  `ss-clone` window (or its title bar) to restore it.

The Python probes are the reference implementations used to validate the Rust
port:

```sh
python3 tools/probe/apple_client.py --host mm.local
python3 tools/probe/apple_framebuffer.py --host mm.local --out artifacts/remote.png
python3 tools/probe/gesture_probe.py --host mm.local --kind swipe --motion 1 --swipe-mask 1
```

## Protocol summary

- Banner: `RFB 003.889\n`.
- Security types observed: `30, 33, 36, 31, 32, 35`; the native client chooses
  **33** (`RSA1` + SRP-6a).
- Auth type 33:
  - RSA-2048 PKCS#1 v1.5 encrypts the identity envelope (username).
  - SRP-6a with SHA-512, PBKDF2-HMAC-SHA512 password preprocessing, RFC 5054
    4096-bit group, `g=5`.
  - `x = SHA512(salt || SHA512(":" || PBKDF2(password)))`, empty SRP identity,
    `H(g)` over the N-padded generator.
  - Record wrap key: `SHA-256(K)[0:16]`.
- Record layer:
  - Server rekey is a FramebufferUpdate rectangle with encoding `0x44f`.
  - AES-128-CBC, persistent per-direction CBC state, SHA-1 integrity trailer.
  - `u16 ciphertext_len || ciphertext`.
- Framebuffer:
  - 32bpp BGRX little-endian; full frames use encoding 6 (Zlib) with a 4-byte
    compressed-length prefix, `Z_SYNC_FLUSH` (no final block).
  - Metadata encodings `0x451` (display layout), `0x453` (vendor keysyms),
    `0x450` (cursor).
- Gestures:
  - Client message type `0x17`, subtypes 1=begin, 2=end, 3=magnify,
    4=smart-magnify, 5=rotate, 6=swipe, 7=quicklook, 8=scroll, 11=wheel.
  - Swipe carries motion, swipeMask, progress, position, velocity, flags,
    flavor, and backing-pixel x/y.

Full details and capture references: [`docs/protocol/authentication.md`](docs/protocol/authentication.md),
[`record-layer.md`](docs/protocol/record-layer.md),
[`framebuffer.md`](docs/protocol/framebuffer.md),
[`input-and-gestures.md`](docs/protocol/input-and-gestures.md).

## Linux/Wayland

`rfb-proto`, `ss-client`, and `ss-gui` are platform-neutral Rust. `ss-gui` uses
`winit` + `softbuffer`, which select the native Wayland backend on Linux and
CoreGraphics on macOS.

```sh
# On Linux with a Wayland session
cargo build --release -p ss-gui
./target/release/ss-gui --host 192.168.3.114 --user tai --password <password>

# Advanced gestures: raw evdev multitouch (requires input group access)
sudo usermod -aG input "$USER"   # then log out/in
./target/release/ss-gui --host 192.168.3.114 --evdev
```

Typical Debian/Ubuntu build dependencies: `libwayland-dev`, `libxkbcommon-dev`,
`libx11-xcb-dev`, `libxcb1-dev`, `libxkbcommon-x11-dev`. Runtime packages
include `libwayland-client0` and `libxkbcommon0`.

`gesture-input` implements:
- MT protocol B parsing (`ABS_MT_SLOT`/`TRACKING_ID`/`POSITION_X/Y`),
- 3/4-finger swipe recognition with direction/progress,
- two-finger scroll, pinch (magnify), and rotate,
- a pure-Rust recognizer with unit tests, plus a Linux evdev reader.

The GUI also accepts `Ctrl+Alt+Arrow` to emit a synthetic swipe and
`Ctrl+Alt+=`/`-` to emit magnify, which is useful for calibrating the macOS
server without a physical trackpad.

The project deliberately does **not** require X11 or a compositor-specific
gesture protocol; raw evdev is the reliable path for advanced gestures.

## Gesture calibration

The wire format is known and the server-side agent logs every gesture struct
verbatim.

**Verified live:** a magnify phase series (subtype 3, phase 1/2/2/4, mask 31)
zoomed Preview on `mm.local` into the image under the pointer. This proves the
0x17 message, record-layer encryption, agent RPC, and CGEvent injection path.

On `mm.local`, vertical three/four-finger Mission Control swipes are disabled
(`TrackpadThreeFingerVertSwipeGesture = 0`), while horizontal swipes are
enabled. Synthetic subtype-6 swipes reach the agent and are logged, but have
not produced a visible action; the host may have a single desktop space.
Subtype 11 wheel scroll desynced this server build and is marked experimental;
standard button-4/5 scroll has not yet been observed to scroll.

Remaining calibration work:

- capture a real trackpad swipe's CGEvent fields on `md.local` (event tap) and
  replay those values;
- determine the required `motion` enum, `flags`, `flavor`, and whether a valid
  begin/end series or HID source is required;
- settle the subtype-11 wheel field order (`scrollCount`/`flags` vs
  phase/momentum) or implement subtype 8 scroll with a correct phase series;
- test subtype 11 (wheel) field order for normal scrolling.

## Layout

```
AGENTS.md                 operating guide for AI agents
docs/protocol/            verified protocol documentation
docs/findings/            dated experiment notes
tools/probe/              Python reference clients and probes
tools/instrument/         ScreenSharing.framework introspection
crates/rfb-proto/         portable protocol crate
crates/ss-client/         CLI client (probe/snapshot/input/gestures)
crates/ss-gui/            Wayland frontend (next)
crates/gesture-input/     Linux evdev gesture backend (next)
captures/                 untracked pcaps
artifacts/                untracked builds/screenshots
```

## Safety

This is a two-machine lab using the test account `tai`. Do not commit captures
or screenshots; they contain the remote desktop. Do not modify `/System` on
either machine. See `AGENTS.md` for the full rules.
