# ss-clone

A protocol-compatible client for Apple Screen Sharing (`RFB 003.889` + Apple
extensions), targeting **Linux/Wayland** while using the real macOS server as
the compatibility oracle.

The reason for the project is trackpad and gesture fidelity: generic VNC/RDP
clients do not reproduce Screen Sharing's scroll feel. This one runs Apple's own
gesture and codec vocabulary, and is measured against `Screen Sharing.app`
rather than judged by eye.

## Status

| Area | Status |
|---|---|
| RFB 003.889 handshake | verified |
| Auth type 33 (RSA1 + SRP-6a) | verified, Rust + Python |
| Auth type 30 (legacy DH) | observed, not implemented |
| Rekey + AES-128-CBC/SHA-1 record layer | verified, Rust + Python |
| Framebuffer: Zlib (6), cursors (0x450/0x44c), layout (0x451/0x453) | verified |
| MultiVariant (0x3f3), server push by default | verified; keeps up at full 2560x1600 |
| Keyboard (US + JIS layouts), pointer, drag | verified |
| Scroll (subtype 11, sub-line fixed-point deltas) | verified, two axes auto-detected |
| Magnify, rotate, pan | verified live (smart-magnify implemented, not calibrated) |
| Three/four-finger swipes | reach the server; no visible action on the lab host |
| Clipboard (text + images), automatic and duplex | verified byte-identical both ways |
| File copy both directions | verified (drop to send, `Cmd+Shift+V` to pull) |
| Keyboard grab so local shortcuts reach the remote | verified |
| Visual latency vs `Screen Sharing.app` | on par (measured, see below) |
| Linux/Wayland | code paths exist and the evdev backend is tested; **never run end to end on Linux** |

## Quick start (macOS lab)

Requirements: Rust 1.97+, `mm.local` reachable, account `tai` / `$SS_PASSWORD`.

```sh
cargo build --release -p ss-gui
./target/release/ss-gui --host mm.local --mvs
```

`--mvs` is the fast path: MultiVariant plus server push. Without it the client
advertises Zlib (6), which decodes roughly ten times slower on this server.

Holding **Left Cmd+Shift for ~350 ms** releases the keyboard grab so local
shortcuts work again. The grab needs Accessibility/Input Monitoring permission;
without it the client says so and runs ungrabbed.

## Performance

Measured on the lab pair, 2026-11-06, method and caveats in
[`docs/testing.md`](docs/testing.md):

- **Receive**: 112 Mbit/s against a 114 Mbit/s link, i.e. the link, not the
  client, is the constraint.
- **Decode**: a full-resolution scroll burst decodes in 272 ms (dev profile)
  for 94 Mpx of tiles; the live decode backlog is p50 2 ms.
- **Visual latency**: envelope lag between the remote screen and the client
  window 157 ms against `Screen Sharing.app`'s 266 ms; the client tracks the
  remote (stretch 0.97) where it used to take 1.4x as long and finish 0.5 s late.

The whole `Screen Sharing.app` comparison is a camera A/B, because packet-level
byte counts are not comparable between the two clients: SS.app runs a scaled
2400x1500 Zlib stream (~1.6 MB per scroll burst), this client runs 2560x1600
MVS (0.05-1 MB).

## What a session does

- Keyboard grab with a release chord; US and JIS layouts (`--keymap`).
- Pointer, drag, and scroll; scroll axis switches between vertical and both
  automatically from recent motion, with no acceleration.
- Momentum: the OS momentum tail is forwarded by default, as the real client
  does (`--no-coast` turns it off).
- Clipboard: automatic and duplex, text and images.
- File copy: drop a file on the window to send it to the remote's `~/Downloads`;
  `Cmd+Shift+V` pulls whatever the remote clipboard names into `~/Downloads`.
- Remote cursor shape and hotspot, so hovering text shows an I-beam.

## If input seems dead

- The window forces itself to the front at startup; the title says
  `click to focus` when it is not focused.
- Run with `--debug-input` to trace input, keys, clipboard and framebuffer
  timing; `SS_LATDBG=1` adds decode/paint timing
  (`python3 tools/measure/decode_backlog.py <log>`).
- If the title says `ss-clone — disconnected`, the session reader hit a
  protocol or socket error; the terminal shows the reason.
- Pinch and rotate send a full phase series, because macOS ignores a lone
  `phase 2` frame.

## Documentation

| file | contents |
|---|---|
| [`AGENTS.md`](AGENTS.md) | operating guide: lab topology, rules, architecture, build/test |
| [`docs/testing.md`](docs/testing.md) | how this project is measured, and the recorded results |
| [`docs/protocol/`](docs/protocol/) | the protocol as verified against the real server |
| [`docs/protocol/STATUS.md`](docs/protocol/STATUS.md) | per-session handoff: verified, changed, open |
| [`docs/findings/`](docs/findings/) | dated experiment notes behind the protocol claims |

Start with `docs/protocol/framebuffer.md`, `codecs.md` (MultiVariant),
`input-and-gestures.md` and `clipboard.md`/`filecopy.md`.

## Layout

```
AGENTS.md                 operating guide for AI agents
docs/testing.md           test layers, methods, recorded measurements
docs/protocol/            verified protocol documentation
docs/findings/            dated experiment notes
tools/probe/              Python reference clients and probes
tools/instrument/         event injection and framework introspection helpers
tools/measure/            latency harness: scroll injector, camera A/B, analysis
crates/rfb-proto/         portable protocol crate (no I/O, no OS types)
crates/ss-client/         CLI client (probe/snapshot/input/gestures)
crates/ss-gui/            winit + softbuffer frontend
crates/gesture-input/     portable recogniser + Linux evdev backend
captures/, artifacts/     untracked captures and test fixtures
```

## Linux/Wayland

`rfb-proto`, `ss-client` and `ss-gui` are platform-neutral. `ss-gui` uses
`winit` + `softbuffer`, which select the native Wayland backend on Linux and
CoreGraphics on macOS.

```sh
cargo build --release -p ss-gui
./target/release/ss-gui --host <linux-host> --user tai --password <password> --mvs

# raw evdev multitouch (requires input group access)
sudo usermod -aG input "$USER"   # then log out/in
./target/release/ss-gui --host <linux-host> --evdev
```

Typical Debian/Ubuntu build dependencies: `libwayland-dev`, `libxkbcommon-dev`,
`libx11-xcb-dev`, `libxcb1-dev`, `libxkbcommon-x11-dev`; runtime packages
include `libwayland-client0` and `libxkbcommon0`.

This has not been exercised end to end on a Linux host yet. It is the last
unwalked mile for the actual target platform, and the most valuable next step.

The GUI also accepts `Ctrl+Alt+Arrow` to emit a synthetic swipe and
`Ctrl+Alt+=`/`-` to emit magnify, which calibrates the server without a physical
trackpad.

## Safety

This is a two-machine lab using the test account `tai`. Do not commit captures or
screenshots; they contain the remote desktop. Do not modify `/System` on either
machine. See [`AGENTS.md`](AGENTS.md) for the full rules.
