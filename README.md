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
| Linux/Wayland | builds, connects, decodes and paints end to end (smoke test, 2026-11-06); input/gestures/clipboard not yet verified on Linux |

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
tools/measure/            latency harness: camera A/B, scroll, decode backlog,
                          Wayland smoke test and PNG probe
packaging/                .desktop entry, launcher, icon, user installer
crates/rfb-proto/         portable protocol crate (no I/O, no OS types)
crates/ss-client/         CLI client (probe/snapshot/input/gestures)
crates/ss-gui/            winit + softbuffer frontend
crates/gesture-input/     portable recogniser + Linux evdev backend
captures/, artifacts/     untracked captures and test fixtures
```

## Linux/Wayland

Linux/Wayland is the target platform; the macOS lab is the development harness.
As of 2026-11-06 the client builds and runs natively on Wayland: release build,
Apple auth, MVS decode, a 2560×1600 paint and a compositor-presented window were
measured on Arch/Hyprland against `mm.local`. Input, gestures and clipboard have
not yet been verified on Linux. Method, numbers and caveats:
[`docs/testing.md` §9](docs/testing.md#9-linuxwayland).

### Build

```sh
export PATH="$HOME/.cargo/bin:$PATH"   # rustup installs user-space
cargo build --release -p ss-gui
cargo test --workspace
```

Build requirements are **Rust 1.97+ and a C linker** (`build-essential` on
Debian, `base-devel` on Arch), plus a normal glibc. No `-dev` packages are
needed: `winit`/`softbuffer` load Wayland, X11 and xkbcommon with `dlopen` at
runtime. The old list of `libwayland-dev`, `libxkbcommon-dev`, `libx11-xcb-dev`
and friends was a runtime list mislabelled as build dependencies.

Runtime libraries (measured with `ldd`, `readelf -d` and `LD_DEBUG=libs`):

| library | Debian/Ubuntu | Arch |
|---|---|---|
| `libc.so.6`, `libm.so.6` | `libc6` | `glibc` |
| `libgcc_s.so.1` | `libgcc-s1` | `libgcc` (older: `gcc-libs`) |
| `libwayland-client.so.0` | `libwayland-client0` | `wayland` |
| `libxkbcommon.so.0` | `libxkbcommon0` | `libxkbcommon` |
| `libX11.so.6`, `libX11-xcb.so.1`, `libxcb.so.1` | `libx11-6`, `libx11-xcb1`, `libxcb1` | `libx11`, `libxcb` |

The X11 libraries are loaded by the same winit build even under Wayland; the
Wayland path needs `libwayland-client` and `libxkbcommon`. Clipboard sync shells
out to `wl-copy`/`wl-paste` from `wl-clipboard`; without it the session still
works but local clipboard sync is silently absent.

### Run

```sh
./target/release/ss-gui --host mm.local --mvs
```

`--mvs` is the fast path; without it the client advertises Zlib (6), the slow
path. A Wayland session started from ssh needs both variables exported:

```sh
export WAYLAND_DISPLAY=wayland-1
export XDG_RUNTIME_DIR=/run/user/1000
```

Presentation defaults to the sharp path: an exact integer downscale (the common
2:1 case) uses box/area averaging, any other ratio uses bilinear, and a 1:1
window is still a plain memcpy. `--filter nearest` reproduces the old
sample-one-pixel mapping for A/B tests; `--filter box|bilinear` force a filter.

The initial window size is chosen for the monitor the window lands on: 1:1 with
the framebuffer when it fits (e.g. 1280x800 logical on a 2x 2880x1800 panel),
otherwise the largest 16:10 size up to 1600x1000 that fits. `--window-size WxH`
pins it. Measured filter quality, draw cost and caveats:
[`docs/testing.md` §10](docs/testing.md#10-render-quality-and-presentation).

### Verify without a camera

```sh
tools/measure/wayland-smoke.sh              # build + connect + paint + grim, if present
tools/measure/wayland-smoke.sh --no-grim    # headless
env -u WAYLAND_DISPLAY -u XDG_RUNTIME_DIR \
  tools/measure/wayland-smoke.sh --no-grim  # private headless weston
```

The script builds release, dumps six painted frames with the built-in
`--dump`/`--dump-after` path, decodes the PNG with stdlib Python (geometry, byte
floor, distinct colours), requires a clean exit, and in a real session captures
the window with grim before closing it. It proves build/connect/paint/present/
teardown; it does not prove input, gestures, clipboard, frame rate or latency.

For the presentation filter specifically, `tools/measure/render-capture.sh`
runs one filter at one window size, keeps the 1:1 dump and a grim capture of the
actual window, and reports p50/p95 draw time:

```sh
tools/measure/render-capture.sh --filter nearest --size 1280x800 \
    --workspace 12 --out artifacts/render-quality/nearest-1280
tools/measure/render-capture.sh --filter auto --size 1600x1000 \
    --workspace 12 --out artifacts/render-quality/bilinear-1600
```

### Desktop entry

```sh
./packaging/install.sh                  # ~/.local, builds first
./packaging/install.sh --no-build       # install the existing release binary
```

The entry launches `ss-clone-launch`, which always passes `--mvs`, logs stderr to
`${XDG_STATE_HOME:-~/.local/state}/ss-clone/ss-gui.log`, and notifies on a
failed connect. Set `SS_HOST` in the environment or in
`~/.config/ss-clone/config`.

### Not on Wayland yet

- **No keyboard grab.** macOS's `CGEventTap` and release chord have no Wayland
  equivalent; the log says `grab: not available on this platform`.
- **No window geometry pinning or `focus_window`.** The compositor owns
  placement and focus, so the macOS camera A/B's pinned rectangles cannot be
  reproduced here. `--dump` is the geometry-independent check.
- **Raw evdev is partial.** `--evdev` reads MT-protocol-B without grabbing, but
  this host's user cannot open `/dev/input/event*` (no `input` group), so it was
  not exercised; `EVIOCGRAB`, udev-based discovery, a release chord and a
  keyboard grab are the next phase. The prerequisites and escape-chord
  constraints are written down in
  [`docs/findings/2026-11-06-evdev-prerequisites.md`](docs/findings/2026-11-06-evdev-prerequisites.md).

The GUI also accepts `Ctrl+Alt+Arrow` to emit a synthetic swipe and
`Ctrl+Alt+=`/`-` to emit magnify, which calibrates the server without a physical
trackpad.

## Safety

This is a two-machine lab using the test account `tai`. Do not commit captures or
screenshots; they contain the remote desktop. Do not modify `/System` on either
machine. See [`AGENTS.md`](AGENTS.md) for the full rules.
