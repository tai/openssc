# AGENTS.md — `ss-clone` — **v1.4.1**

Canonical operating guide for AI agents working on this repository.
**Read this file completely before running anything.** If you learn something that
contradicts this document, update this document in the same change.

> **v1.4.1 — 2026-11-05.** Local shortcuts now go to the remote. While the
> ss-gui window is focused a session-level `CGEventTap` captures the keyboard, so
> `Ctrl+Space` (input source), `Cmd+Space`, `Cmd+Tab` and the rest reach the
> remote instead of being eaten by macOS. Hold **Left Cmd+Shift for 350 ms** to
> give them back — a modifier-only chord needs the hold, or `Cmd+Shift+V` would
> release the grab on its way through. It needs Accessibility permission and
> falls back to ungrabbed operation without it; the pointer is deliberately not
> grabbed. `--help` exists now.

> **v1.4.0 — 2026-11-05.** Japanese JIS keyboards now type correctly. The client
> used to translate a character into a US physical key plus Shift; on JIS that is
> the wrong key, and this server ignores Shift for printable keys anyway, so `@`
> arrived as `2` and `^` as `6`. The wire token is now the character's own
> keysym and the remote layout resolves the position. Layouts are data
> (`--keymap us|jis` for the remote layout), so adding a keyboard type is a table
> rather than new dispatch logic. JIS-specific keys (yen, 変換, 無変換, かな)
> have no keysym in the server's tables at all and are logged as unsupported.

> **v1.3.0 — 2026-11-05.** Pulling files back from the remote. `Cmd+Shift+V` in
> ss-gui pulls whatever the remote clipboard names into `~/Downloads`. Getting
> there exposed an input bug that had nothing to do with the clipboard: Option
> was forwarded as `0xFFE9` (Alt_L), which this server accepts as a key but never
> treats as Option, so the remote saw a plain `Cmd+C` and copied only the item
> name. Option is now sent as `0xFFE7` (Meta_L) and modifiers are re-synced per
> key. Drag-out of the window stays out of scope by decision.

> **v1.2.0 — 2026-11-05.** File copy. Drop a file on the window and it appears
> on the remote; incoming files land in `~/Downloads`. This is Apple's separate
> `0x22` file-copy stream (not a pasteboard flavour) with its own helper process
> on the far end, and the piece that took the longest was one field: `NewItem`
> carries the POSIX mode at `+0x66`, so a zeroed metadata block creates the file
> mode `000` in the in-progress state and every data write fails. The layout was
> recovered by capturing a genuine server-sent `NewItem` rather than guessing.
> Drag *out* of the window is deliberately not implemented.

> **v1.1.0 — 2026-11-05.** Remote cursor and a real clipboard. The cursor is now
> the *remote* one: shape, hotspot and size come from the server (Apple `0x450`
> alpha cursor, `0x44c` position, `0x451` for point-vs-pixel scale), so hovering
> text gives an I-beam instead of a local arrow. The clipboard is automatic and
> bidirectional on one connection — text and images, byte-identical both ways —
> after finding that `0x0b ClipboardFetch` is an 8-byte message and that our
> 1-byte fetch was leaving the server 7 bytes short and desyncing the stream.
> File copy (`0x22 RFBFileCopy_*`) is the next milestone and is not started.

> **v1.0.0 — 2026-11-04.** First release where the client is subjectively on par
> with the real `Screen Sharing.app` in normal use, confirmed by the owner on the
> lab pair. The milestone is the MultiVariant (MVS) fast path: full-frame latency
> went from 632–880 ms (deflate tiers) to 44–49 ms, push mode with type-0 deltas,
> spatial records and type-1 refinement decodes correctly, and the end-to-end
> measured keystroke latency is now within a few ms of Apple's own client at the
> packet boundary. What is *not* in v1.0.0 is listed in §13 — most importantly
> the Linux/Wayland packaging, which is still the last unwalked mile for the
> actual target platform.

- **Project:** a protocol-compatible reimplementation of Apple Screen Sharing
  (`RFB 003.889` + Apple extensions), with a client that runs on **Linux/Wayland**.
- **Primary reason:** Apple Screen Sharing has substantially better trackpad and
  gesture fidelity than generic VNC/RDP clients. Gesture compatibility is a
  first-class requirement, not a nice-to-have.
- **Current phase:** v1.4.1 on macOS. Live GUI, keyboard/pointer input (with
  correct modifier forwarding), gesture backend, remote cursor, automatic duplex
  clipboard (text + images), file copy both ways (drop to send, Cmd+Shift+V to
  pull), JIS keyboard support with layout tables, keyboard grab so local
  shortcuts reach the remote, and MVS with **server push as the default**
  including the full push vocabulary. Next: **Linux/Wayland packaging** — still the actual target
  platform and still unwalked — then swipe semantic calibration and frame pacing.
- **Last verified:** 2026-11-04 against `md.local` and `mm.local`.

---

## 1. Mission and scope

Build a client that can connect to a real macOS Screen Sharing server and provide:

1. Correct RFB 003.889 handshake, Apple authentication, and session setup.
2. Framebuffer rendering with acceptable latency (Raw/ZRLE first, Apple codecs later).
3. Keyboard and pointer input, including modifier and dead-key behaviour.
4. **High-fidelity trackpad input:** two-finger scroll, pinch/zoom, rotate,
   three-/four-finger swipes, force click, and drag variants.
5. Clipboard (text first, images later).
6. A native Linux/Wayland window, with a macOS build used only as a development
   harness.

### Explicit non-goals (for now)

- Reimplementing the macOS **server** (`screensharingd`). The real server is the
  compatibility oracle.
- Apple Remote Desktop (ARD) management protocol on port 3283, unless a feature
  we need turns out to require it.
- iOS/iPadOS clients, account enrollment, Screen Sharing invitations, or iCloud
  identity flows.
- Audio, file transfer, and display-arrangement reconfiguration are **later
  milestones**, not part of the first working client.
- Supporting third-party VNC servers. Standard RFB behaviour may be reused where
  convenient, but Apple's dialect is the target.

---

## 2. Lab topology and access

| | `md.local` (this machine) | `mm.local` (remote) |
|---|---|---|
| Role | Analysis workstation + client under development | Real Screen Sharing server / oracle |
| macOS | 13.7.8 (22H730) | 12.7.6 (21H1320) |
| Arch | x86_64 | x86_64 (Intel Iris) |
| Console user | `tai` | `tai` |
| Display | 2880×1800 Retina | 2560×1600 Retina |
| `Screen Sharing.app` | 3.0 (build 587.3) | 2.0 |
| `screensharingd` | 3.9.8 | 3.9.8 |
| RFB banner | `RFB 003.889\n` | `RFB 003.889\n` |
| SSH | local | key-based, no password |

Both machines use the local account `tai`; set `SS_PASSWORD` in the environment for the lab password.

### 2.1 Access commands

```sh
# Remote shell (key-based)
ssh tai@mm.local

# md.local: passwordless sudo
sudo <command>

# mm.local: passwordless sudo (verified after the sudoers fix; `-n` succeeds)
ssh tai@mm.local 'sudo -n <command>'
ssh -t tai@mm.local 'sudo <command>'   # interactive equivalent if ever needed

# Open the real client against the server (GUI; may prompt for credentials)
open "vnc://tai@mm.local"
```

`mm.local` runs `screensharingd` and listens on TCP 5900. `Screen Sharing`
(port 5900) is also enabled on `md.local`, which is useful for loopback tests.

### 2.2 SIP / security posture

`csrutil status` on both machines reports a **Custom Configuration**:

```
Apple Internal:        disabled
Kext Signing:          disabled
Filesystem Protections: disabled
Debugging Restrictions: enabled
DTrace Restrictions:   enabled
NVRAM Protections:     enabled
BaseSystem Verification: enabled
```

Consequences:

- We *can* write to `/System`, but **must not**. Treat the OS installation as
  read-only and work on copies under `~/ss-lab/`.
- `lldb`/`dtrace` attaching to Apple platform binaries will likely be denied.
  Use a **re-signed copy** of the client (add `get-task-allow`) or a
  `DYLD_INSERT_LIBRARIES` shim, not SIP changes.
- Do not disable SIP, change boot arguments, or modify `/System` without
  explicit human approval.

---

## 3. Verified lab facts (Phase 0)

These were observed directly. Treat them as ground truth until re-verified.

### 3.1 Listener and daemon

- TCP **5900** is the Screen Sharing/RFB listener.
- launchd job: `system/com.apple.screensharing`
  (`/System/Library/LaunchDaemons/com.apple.screensharing.plist`).
- Program:
  `/System/Library/CoreServices/RemoteManagement/screensharingd.bundle/Contents/MacOS/screensharingd`
- Socket activation uses Bonjour service type `rfb`, service name `vnc-server`.
- `screensharingd` is stripped (essentially no exported symbols). It is a
  universal/Intel binary with extensive Apple-internal source-path strings.
- A logged-in per-user agent runs as `tai`:
  `/System/Library/CoreServices/RemoteManagement/ScreensharingAgent.bundle/Contents/MacOS/ScreensharingAgent`
  (bundle id `com.apple.screensharing.agent`, version 3.9.8).

### 3.2 Client components

- App:
  `/System/Library/CoreServices/Applications/Screen Sharing.app`
  (bundle id `com.apple.ScreenSharing`, URL scheme `vnc://`).
- Private framework:
  `/System/Library/PrivateFrameworks/ScreenSharing.framework`
  **has no on-disk Mach-O binary** on this OS. Only resources are present; the
  code lives in the dyld shared cache. Do not assume `strings` on the framework
  path will work. Use the app binary, the agent binary, the daemon binary, or
  extract from the shared cache.
- The app is sandboxed. Its effective preferences are under:
  `~/Library/Containers/com.apple.ScreenSharing/Data/Library/Preferences/com.apple.ScreenSharing.plist`
- Relevant entitlements include `com.apple.private.rfbeventhelper.xpcaccepted`
  and Mach service `com.apple.RFBEventHelper`. This is part of the input/gesture
  event path.

### 3.3 Protocol hints found in binaries

From `screensharingd` and `ScreensharingAgent` strings:

- Banner: `RFB 003.889`.
- Server internals reference
  `RemoteDesktop/RFBCommon/...` and `RemoteDesktop/RFBServer/...`, including:
  `ViewerMessages.c`, `EncodeRaw.c`, `EncodeZRLE.c`, `EncodeZlib.c`,
  `EncodeMultiVariant.c`, `EncodeDisplayInfo.c`, `EncodeUserInfo.c`,
  `EncodeCursor.c`, `HuffmanEncode.c`, `ScreenCopy.c`, `SendFrameBuffer.c`,
  `TranslatePixels.c`, `Clipboard.c`, `KeyMap.c`, `MouseEventUtils.c`,
  `DisplayStream.c`, `hidpi.c`.
- Named server concepts: `RFBServerInfo2`, `HandleSetEncodingsMessage`,
  `HandleViewerClipboardRequest`, `HandleViewerClipboardSend`,
  `RFBAutoPasteboardMessage`, `RFBFileCopy_StartFileSendMessage`,
  `RFBFileCopy_StartFileReceiveMessage`, `kRFBFlagLastBuffer`.
- Encodings/codecs referenced: Raw, ZRLE, Zlib, **MultiVariant (MVS)** tile
  codec (8×8 tiles, DCT, OpenCL kernel), Cursor, DisplayInfo/DisplayInfo2,
  UserInfo, and a "lossless codec" identifier.
- Gesture pipeline (critical):
  - Server: `SSAgent_PostGestureEvent_rpc failed, %s dest port %d`
  - Agent: `agent_SSAgent_PostGestureEvent_rpc`,
    `agent_SSAgent_PostMouseEvent_rpc`, `agent_SSAgent_PostScrollEvent_rpc`,
    `agent_SSAgent_PostKeyEvent_rpc`
  - Gesture log format exposes the structure:
    `x %f y %f motion %lld swipemask %llx progress %f pos x %f pos y %f
     vel x %f vel y %f vel z %f flagbits %llx flavor %lld`
  - Other gesture strings: `start gesture %u`, `magnify %f`, `rotation %f x %d y %d`,
    `lookup gesture`.
  - Drag request codes: `kRFBDragRequestCodePostMouseDrag`,
    `kRFBDragRequestCodePostMouseUp`.
- The server appears to forward gesture events to the logged-in agent, which
  posts real system events into the user session. That means the wire format
  carries structured gesture data, not merely synthetic pointer deltas.

### 3.4 Client-side switches that make analysis easier

The app binary contains these preference/menu strings:

- `LogRFBPackets` (routing: `/tmp/ScreenSharing.log`, plus
  `com.apple.screensharing.logConfiguration`)
- `RFBEncryption Off` / `RFBEncryption On`
- `encryptRFBDataStream`
- `encryptKeystrokesOnly Off` / `encryptKeystrokesOnly On`
- `screenQualityMode` (current value 4 on both machines; `Quality Adaptive`,
  `Quality Full`)

Server preferences on `mm.local`
(`/Library/Preferences/com.apple.RemoteManagement.plist`):

```
ARD_AllLocalUsers            = 1
allowInsecureDH              = 1
DisableKerberos              = 0
ScreenSharingReqPermEnabled  = 1
VNCLegacyConnectionsEnabled  = 0
```

`allowInsecureDH = 1` is promising: it may allow a plaintext or weak-DH session,
which would make full packet capture feasible. Verify, do not assume.

**Hypothesis to test next:** set `encryptRFBDataStream` /
`encryptKeystrokesOnly` off in the client container defaults **and**
`LogRFBPackets` on, then capture a complete session in plaintext.

---

## 4. Non-negotiable rules

1. **Do not modify `/System`, `/Library`, or SIP settings.** Copy binaries to
   `~/ss-lab/` and re-sign/instrument the copies.
2. **Do not reboot either machine** without asking the human. Do not log out the
   console user on `mm.local`; it is needed for a usable desktop session.
3. **Do not change Screen Sharing settings on `mm.local`** (users, permissions,
   ports, legacy VNC) without recording the prior value and restoring it.
4. **Never commit captures or dumps.** `captures/`, `artifacts/`, `secrets/`,
   `.pcap`, `.pcapng`, logs, screenshots, and crash dumps are gitignored.
   They contain screen contents, usernames, and potentially credentials.
5. **Test account only.** Use `tai` / `$SS_PASSWORD`; never touch other accounts,
   keychains, iCloud identities, or personal files.
6. **One experiment per capture.** Change one variable at a time; record the
   exact commands, timestamps, app build numbers, and gesture performed.
7. **Leave the lab clean.** Quit test clients, kill stray `tcpdump`/`lldb`
   processes, and restore preferences you changed.
8. **No silent guesses in docs.** Mark statements as `VERIFIED`, `HYPOTHESIS`,
   or `TODO`. Protocol docs must cite the capture/experiment ID that proved it.
9. **Tcpdump only on the lab interface and port.** Prefer
   `host mm.local and tcp port 5900` over broad filters. Do not capture unrelated
   traffic.
10. **Ask before installing system-wide tools.** User-space tooling under
    `~/ss-lab/` is fine.

---

## 5. Analysis methodology

Work from least invasive to most invasive. Do not jump to a debugger before
trying Apple's own logging toggles.

### Step 1 — Black-box baseline

1. Capture the TCP handshake and RFB banner/negotiation with `tcpdump`.
2. Reproduce with the real client, one gesture/action at a time.
3. Parse the stream with a small Python parser (not only Wireshark; the Apple
   extensions are not in stock dissectors).
4. Record: client/server build, prefs, auth method chosen, security result,
   ServerInit fields, SetEncodings list, and first FramebufferUpdate.

Suggested capture wrapper:

```sh
# On md.local, capture traffic to/from mm.local
sudo tcpdump -i en0 -s 0 -w "captures/$(date +%Y%m%d-%H%M%S)-mm-5900.pcap" \
  'host mm.local and tcp port 5900'
```

`en0` is the route to `mm.local` (192.168.3.0/24).

### Step 2 — Turn on Apple's own RFB logging

Try, in this order, and verify each:

```sh
# Preferred: write into the sandbox container's prefs
P="$HOME/Library/Containers/com.apple.ScreenSharing/Data/Library/Preferences/com.apple.ScreenSharing.plist"
defaults write "$P" LogRFBPackets -bool true
defaults write "$P" encryptRFBDataStream -bool false
defaults write "$P" encryptKeystrokesOnly -bool false

# Also try the host domain; note which one actually takes effect
defaults write com.apple.ScreenSharing LogRFBPackets -bool true

killall cfprefsd 2>/dev/null || true
# Then connect and inspect:
tail -f /private/tmp/ScreenSharing.log
```

If the value names above are wrong, discover the real keys by diffing the
container plist before/after toggling the corresponding menu items in the GUI.

### Step 3 — Instrument the client (re-signed copy)

Only if Steps 1–2 are insufficient.

- Copy the app to `~/ss-lab/app/ScreenSharing.app`.
- Add `com.apple.security.get-task-allow` to its entitlements and re-sign
  ad-hoc (`codesign --force --deep --sign -`). Private `com.apple.private.*`
  entitlements may stop working after re-signing; validate the basic session
  still connects before relying on it.
- Use `lldb` breakpoints on RFB send/recv call sites once symbols are located,
  or a `DYLD_INSERT_LIBRARIES` shim that interposes `send`/`recv`/`write`/
  `read` and logs structured frames.
- If `lldb` attach still fails due to debugging restrictions, prefer the shim
  on a re-signed copy over changing SIP.

### Step 4 — Server-side corroboration

`screensharingd` is stripped but has very informative strings. For the server
side, prefer:

- Unified logging (`log stream`/`log show`) with the screensharing subsystems.
- The per-user `ScreensharingAgent` log messages (gesture struct format).
- A client-side shim that logs the exact bytes sent, correlated with server
  log lines.

Do not run a modified `screensharingd` as the system daemon. If a server-side
experiment is unavoidable, run a copy on a non-standard port and restore
everything afterwards.

### Step 5 — Differential experiments

For each unknown, capture A/B pairs:

- Encryption off vs on.
- One gesture vs no gesture (boolean diff of client→server bytes).
- Mouse move only vs drag.
- Key down/up vs key repeat.
- Clipboard text set vs read.
- One display resolution vs another.

Store experiment notes in `docs/findings/YYYY-MM-DD-<slug>.md` with the pcap
filename (pcaps themselves stay untracked).

---

## 6. Repository layout

Create these as work progresses; keep the layout stable.

```
AGENTS.md                 # this file — canonical operating guide
README.md                 # human-facing overview (later)
docs/
  lab.md                  # machine/account/topology details, reset procedures
  protocol/
    overview.md           # layering, ports, session lifecycle
    transport.md          # TCP framing, banner, version negotiation
    authentication.md     # Apple security types, DH/AES, failure modes
    handshake.md          # ClientInit/ServerInit, pixel formats, encodings
    encodings.md          # Raw, ZRLE, Zlib, Cursor, MultiVariant, DisplayInfo
    input-and-gestures.md # key/pointer/scroll/gesture messages + event struct
    clipboard.md          # text/image pasteboard messages
    audio.md              # later
    display.md            # resolution/scale/display-info messages
    glossary.md
  findings/               # dated experiment notes (no captures committed)
tools/
  capture/                # pcap helpers, RFB stream parser
  probe/                  # minimal handshake/auth probes
  instrument/             # dylib shims, lldb scripts, re-sign helpers
crates/
  rfb-proto/              # pure protocol types, codecs, no I/O
  ss-client/              # transport + session state machine + CLI
  ss-gui/                 # winit/wgpu (or softbuffer) frontend
  gesture-input/          # macOS test backend + Linux evdev/libinput backend
scripts/                  # reproducible lab commands
captures/                 # UNTRACKED: pcaps, logs, screenshots
artifacts/                # UNTRACKED: builds, shims, extracted binaries
```

---

## 7. Client architecture

### 7.1 Language and stack

- **Rust** (stable; 1.97.x is installed) for the client and protocol crates.
- **Python 3.9** for one-off capture parsing and experiments.
- Async I/O: `tokio`; errors: `thiserror`; logging: `tracing`.
- Crypto: start with well-reviewed crates (`aes`, `sha1`, `hmac`, `num-bigint`)
  or OpenSSL bindings; do not hand-roll primitives.
- GUI: `winit` (native Wayland) + `softbuffer` for the first framebuffer
  milestone, then `wgpu` for scaling/colour conversion if needed.
- Keep `rfb-proto` free of OS and async dependencies so it can be unit-tested
  against captured byte fixtures.

### 7.2 Wayland gesture reality check

Standard Wayland clients receive pointer axis/scroll and keyboard events, but
**three-/four-finger trackpad gestures are generally consumed by the compositor**
and are not exposed through `wl_pointer`. To match Apple's gesture support:

- Implement a **raw input backend** that reads Linux `evdev` multitouch events
  (`/dev/input/event*`, MT protocol B), optionally with `EVIOCGRAB` when the
  user explicitly asks the client to own the trackpad.
- Implement gesture recognition locally (fingers, centroid, scale, rotation,
  velocity, swipe direction/progress) and map it to Apple's gesture message.
- Provide a **Wayland-native fallback**: two-finger scroll and pinch where the
  compositor exposes them; otherwise degrade gracefully.
- Keep all of this behind a `gesture-input` trait so the macOS dev harness can
  inject synthetic gestures without a physical trackpad.

Document permission requirements (typically `input` group or a udev rule) and
never silently grab a device.

### 7.3 Session layers

```
TCP :5900
  └─ RFB version + security handshake (Apple dialect)
      └─ ClientInit / ServerInit / SetPixelFormat / SetEncodings
          ├─ FramebufferUpdate loop  → decoder → renderer
          ├─ KeyEvent / PointerEvent / Apple gesture messages
          ├─ Clipboard (CutText / Apple pasteboard messages)
          └─ Control messages (display info, quality, keepalive)
```

Design the state machine so each message type can be enabled independently and
so unknown message types are logged and skipped without killing the session.

---

## 8. Milestones and definitions of done

| ID | Milestone | Definition of done |
|----|-----------|--------------------|
| M0 | Probe | CLI connects to `mm.local:5900`, performs auth, prints server name/version/desktop size, exits cleanly. |
| M1 | Framebuffer | Decodes Raw + ZRLE, writes a correct PPM/PNG of the remote desktop; live window at ≥15 fps on LAN. (Python snapshot done; Rust/live pending.) |
| M2 | Input | Keyboard and absolute/relative pointer work end-to-end; modifier keys and drag are correct. |
| M3 | Clipboard | Text clipboard syncs both directions. |
| M4 | Gestures | Scroll, pinch, rotate, 3-/4-finger swipes, force click, and drag gestures are accepted by macOS and behave like the real client. |
| M5 | Wayland | Native Wayland window + evdev gesture backend, packaged and runnable on a Linux host. |
| M6 | Fidelity | Apple codecs (MultiVariant/ZRLE tuning), latency/quality controls, reconnect, multi-display basics. |

Each milestone requires: protocol doc updates, capture/experiment IDs, automated
tests for parsers, and a short demo command in the PR/commit message.

---

## 9. Build and test

```sh
# Rust workspace
cargo build --workspace
cargo test --workspace

# Protocol probe against the real server
cargo run -p ss-client -- probe --host mm.local

# Framebuffer snapshot / input / gesture
cargo run -p ss-client -- snapshot --host mm.local --out artifacts/rust.png
cargo run -p ss-client -- demo-input --host mm.local --x 1000 --y 800 --text hello
cargo run -p ss-client -- gesture --host mm.local --kind swipe \
    --motion 1 --swipe-mask 4 --progress 1.0 --series --out artifacts/swipe.png
cargo run -p ss-client -- gesture --host mm.local --kind magnify \
    --value 0.2 --mask 31 --series --out artifacts/magnify.png

# Live GUI (0.5 server scale by default) and headless compositing check
cargo run -p ss-gui -- --host mm.local
cargo run -p ss-gui -- --host mm.local --scale 1.0   # full fidelity
cargo run -p ss-gui -- --host mm.local --dump artifacts/gui.png --dump-after 1

# Linux raw multitouch (input group required)
cargo run -p ss-gui --release -- --host <linux-host> --evdev

# Python reference probes
python3 tools/probe/apple_client.py --host mm.local
python3 tools/probe/apple_framebuffer.py --host mm.local --out artifacts/remote.png

# Real-client smoke test
open "vnc://tai@mm.local"
```

Until the client exists, the "test" is a documented manual session with the real
client plus a pcap and experiment note.

---

## 10. Working agreement for agents

1. **Start every session** by reading `AGENTS.md`, `docs/protocol/STATUS.md`
   (once it exists), and the latest file in `docs/findings/`.
2. **State the experiment.** Before touching the lab, write down: hypothesis,
   commands, expected observable, and rollback.
3. **Prefer read-only first.** `strings`, `otool`, `codesign -d`, `defaults
   read`, `lsof`, `log show`, and pcaps before debuggers or modified binaries.
4. **Record evidence.** Every protocol claim in `docs/protocol/*` must link to a
   capture ID or experiment note.
5. **Keep the oracle working.** After each session, verify `mm.local:5900` still
   answers `RFB 003.889` and the console user is still logged in.
6. **Update this file** when you discover a durable fact, change the stack, or
   finish a milestone. Do not let `AGENTS.md` drift.
7. **Leave a handoff note** in `docs/protocol/STATUS.md`: what is verified, what
   is hypothesized, what is blocked, and the next command to run.

### Handoff template

```md
## YYYY-MM-DD — <agent/session>
- Phase / milestone:
- Verified:
- Changed:
- Captures / experiments (untracked paths):
- Open questions:
- Next command:
- Lab state (clients killed, prefs restored, mm.local healthy):
```

---

## 11. Open questions (prioritized)

1. Which RFB security type(s) does the real client negotiate with this server
   (`allowInsecureDH=1`, `VNCLegacyConnectionsEnabled=0`)? What exactly happens
   when `encryptRFBDataStream`/`encryptKeystrokesOnly` are off?
2. What are the exact Apple client→server message type numbers and layouts for
   gestures? Which fields map to `swipemask`, `progress`, `flavor`, `flagbits`,
   and the velocity components?
3. Are gestures sent as one message per phase (begin/update/end), or as a
   complete event after local recognition? What is the coordinate space?
4. What is the exact auth key derivation and AES framing? Can the DH exchange be
   MITM'd, or must we instrument an endpoint?
5. Which encodings does the client actually advertise to this server, and in
   what order? Which one does the server choose for a static vs moving desktop?
6. Is MultiVariant required for the "Adaptive" quality mode, and how are its
   tiles framed and decoded? Is there a fallback ZRLE path we can ship first?
7. What does the server require for "control" vs "observe" permission, and how
   is that signalled in the handshake?
8. How are Retina/HiDPI scale and display changes communicated (DisplayInfo vs
   DisplayInfo2), and how should a Wayland client map them to fractional scale?
9. Does this build support audio? If so, over which channel and codec?
10. What is the minimum viable keepalive/ACK behaviour to avoid server timeouts?

---

## 12. Glossary

- **RFB** — Remote Framebuffer protocol; VNC's wire protocol. Apple uses version
  `003.889` with private extensions.
- **ServerInit** — server's initial desktop name/size/pixel-format message.
- **MultiVariant (MVS)** — Apple's tile-based codec (8×8 tiles, DCT/OpenCL
  paths) seen in `screensharingd`.
- **ZRLE** — Zlib Run-Length Encoding, a standard RFB encoding.
- **DisplayInfo / DisplayInfo2** — Apple messages carrying display geometry,
  scale, and arrangement.
- **ARD** — Apple Remote Desktop; management protocol on port 3283, distinct
  from Screen Sharing on 5900.
- **ScreensharingAgent** — per-user agent that receives RPCs from
  `screensharingd` and posts real input/gesture events into the session.
- **`com.apple.RFBEventHelper`** — Mach service used by the client-side input
  path.

---

## 13. Current status snapshot

### v1.4.1 — 2026-11-05

- **Keyboard grab**: a `CGEventTap` consumes key events while the window is
  focused and routes them through the same key mapping as winit, so global
  shortcuts reach the remote. Release by holding Left Cmd+Shift 350 ms
  (`--release-grab`), which passes through and is never forwarded. Pointer
  deliberately not grabbed. Needs Accessibility/Input Monitoring; without it the
  app logs the pane and runs ungrabbed. The tap re-enables itself if macOS
  disables it, and every release path sends modifier key-ups.
- **`--help` / `-h`** for ss-gui with grouped usage and a test that fails if a
  parser flag is missing from the text. `ss-client` usage updated.
- Not in v1.4.1: pointer grabbing, JIS-specific keys (no keysym exists), Wayland.

### v1.4.0 — 2026-11-05

JIS keyboard support, and the key-mapping model that makes layouts data.

- **The wire token is the character.** The server resolves it through the remote
  layout to a keycode and shift level, and **ignores Shift for printable keys**:
  `A` goes as `0x41` with no modifier, `@` as `0x40`, `:` as `0x3A`. The old
  code sent `Shift + 0x32` for `@`, which is why it typed `2`.
- **Layouts are tables**, `position -> (plain, shifted)`, selected with
  `--keymap us|jis` meaning the REMOTE layout. Named keys use their named keysym
  and need no special path. The local layout never has to be named because winit
  hands us the composed character.
- **Not expressible**: the daemon's keysym tables contain no yen, 変換/無変換 or
  かな/英数 keysyms, so those keys are logged as unsupported rather than guessed.
  That is a server-side ceiling, not a table we can complete.
- Verified on a JIS/JIS pair: `a A @ ^ :` and the wider matrix all correct.

### v1.3.0 — 2026-11-05

Pulling a file or folder from the remote, and the input bug that blocked it.

- **Reverse direction**: `Cmd+Shift+V` requests a fresh clipboard archive
  (0x0b flags 0, with a short retry), takes the first text flavour that is an
  absolute path, and pulls that file/folder over the file-copy channel into
  `~/Downloads`. When there is no path it names what it saw and says what to do.
- **The real bug was Option**: we forwarded `0xFFE9` (Alt_L); this server accepts
  it as a key but never raises the Option flag, so the remote performed a plain
  `Cmd+C` and the clipboard carried the item name. `0xFFE7` (Meta_L) is what
  works. Modifiers are now re-synced per key so a missed `ModifiersChanged`
  cannot silently drop one. This contradicts a static reading of the daemon's
  KeyMap table - empirical wins (`input-and-gestures.md` §6).
- **A plain Finder Cmd+C can never carry a path**: the remote pasteboard holds a
  file URL plus the name, and the server strips the URL from the archive. No
  client recovers it, Apple's included. *Copy as Pathname* is the gesture.
- **Not in v1.3.0**: drag-out of the window (declined), the xattr/resource-fork
  trailer, `cmd 200` status frames.

### v1.2.0 — 2026-11-05

File copy, both directions, verified by sha1 on the lab pair.

- **Wire**: `0x22` + u32 length, then `{u16 version, u16 cmd, u32 sessionID}`,
  then cmd 2 `StartFileReceive` (the viewer names the destination folder on the
  remote), 100 `ItemInfo`, 101 `NewItem` per item, 102 `FileData` in 65536-byte
  chunks, 104 `EndSession`. Contract in `docs/protocol/filecopy.md`.
- **The trap worth remembering**: `NewItem` carries the item metadata and the
  POSIX mode is at `+0x66` (plus `nameLen` excludes the NUL, and the dates are
  `UTCDateTime` 16.32 fixed point). Zeroed, the remote creates the file in the
  in-progress state with mode `000` and every write fails. Recovered from a
  captured server-sent `NewItem`, not from the disassembly alone.
- **Behaviour**: drop a file on the window to send it (winit `DroppedFile`) to
  `/Users/<remote user>/Downloads`; incoming files are written to `~/Downloads`
  with `" 2"` on a name collision. Directory drops are refused. Transfers run on
  a worker thread, so painting and input stay responsive. `--send-file` /
  `--recv-file` are test hooks, not user features.
- **Not in v1.2.0**: drag-out of the window (declined; `StartFileSend` works but
  has no gesture), folder drops (recursion via `level`), the xattr/resource-fork
  trailer, and `cmd 200` status frames (arrive after a send, logged as unknown,
  transfer completes regardless).

### v1.1.0 — 2026-11-05

Remote cursor and duplex clipboard, both owner-verified on the lab pair.

- **Cursor**: advertise Apple `0x450` (alpha) and `0x44c` (position); decode
  hotspot + ARGB bitmap + alpha plane, cache per connection (server ids are
  monotonic), hide the local pointer and composite the bitmap ourselves.
  Sizing comes from `0x451`, which carries both the point and the pixel
  geometry: cursor bitmaps are in points (arrow 17x23, I-beam 9x18) while the
  framebuffer is 2x, so scaling by `target_px / point_px` is what makes it the
  right size. Shape changes arrive without pointer motion.
- **Clipboard**: automatic and duplex, no flags. `AutoPasteboard(1)` at
  connect, an 8-byte `0x0b` fetch at connect and on `0x14 cmd=2`, text via
  `0x06`, images via a viewer `0x1f`, echo suppression on both. Images verified
  byte-identical in both directions. `--no-clipboard` disables it all.
- **The bug worth remembering**: `0x0b ClipboardFetch` is 8 bytes, byte 1 bit 0
  = promises only. A 1-byte fetch leaves the server 7 bytes short, so it parses
  the *next* message as `SetPixelFormat` and closes ~13 ms later. That, not any
  server misbehaviour, is why the old client kept clipboard off.
- **Not in v1.1.0**: file copy (`0x22`, a dedicated channel, not a pasteboard
  flavour), clipboard image types beyond PNG/TIFF/JPEG preference, and the
  local pasteboard polling (~700 ms, one process spawn per poll) which an
  NSPasteboard change-count watcher should replace.

### v1.0.0 — 2026-11-04 (milestone)

**What it is:** a working macOS client that talks to a real macOS Screen Sharing
server over RFB 003.889 with Apple's extensions, renders it live, accepts input
and gestures, syncs the text clipboard, and uses MultiVariant (the server's fast
codec) with server push — in normal use it now feels on par with the real
`Screen Sharing.app` (owner-verified on the lab pair).

**Measured, same host, same 12-keystroke protocol:**

| | full-frame latency | keystroke → painted (p50) |
|---|---:|---:|
| Raw / Zlib / 0x3e8–0x3ea | 632–1590 ms | — |
| MVS polling | 44–49 ms | 85–98 ms |
| MVS push (v1.0.0 default) | 44–49 ms | 92 ms |
| native `Screen Sharing.app` | — | 65–72 ms at the packet boundary |

The remaining ~20–30 ms against Apple is server capture/encode cadence, not our
request path: the applied rects already arrive 6–9 ms apart during a repaint, and
a paired packet capture found no native client advantage (native was 13–14 ms
*later*, not earlier). Recorded in `docs/findings/2026-11-04-push-mode-mvs.md`.

**Verified end to end:** authentication (type 33 RSA1 + SRP-6a), record layer
(0x44f rekey, AES-128-CBC, SHA-1), ServerInit/pixel formats, Zlib, Raw and the
0x3e8–0x3ea tiers, MVS type-0 (dual streams) and type-1 (refinement) with the
spatial-record table and 99-byte coefficient ring, server push arming
(`0x09 selected_screen=0` + one non-incremental `0x03`), keyboard, pointer,
scroll, magnify/rotate/pan gestures, and text clipboard both directions.

**Deliberately not in v1.0.0:** Linux/Wayland packaging (the actual target
platform — the code paths exist and the evdev backend is tested, but it has not
been run end to end on a Linux host); swipe semantic calibration; force click
and drag-gesture variants; clipboard images; audio; multi-display arrangement;
keepalive/rekey hardening for long sessions.

### Earlier phases

- **Phase 0 (recon):** complete.
- **Phase 1 (capture):** complete. Verified protocol docs now live in
  `docs/protocol/`:
  - `authentication.md` — type 33 RSA1 + SRP-6a, confirmed by matching the real
    client's `M1`.
  - `record-layer.md` — 0x44f rekey, AES-128-CBC, SHA-1 records.
  - `framebuffer.md` — ServerInit, 0x451/0x453, Zlib encoding 6.
  - `input-and-gestures.md` — gesture message 0x17 and all subtypes.
- **M0:** complete in Python and Rust. `cargo run -p ss-client -- probe`
  authenticates to `mm.local` and prints ServerInit (2560×1600).
- **M1:** complete in Python and Rust. `ss-client snapshot` decodes a full
  2560×1600 Zlib frame to PNG (`artifacts/rust.png`).
- **M2:** complete in Rust, CLI and GUI. `ss-client demo-input` and a local
  click/keystrokes into the `ss-gui` window both type into remote TextEdit.
- **M4 (gestures):** message 0x17 implemented. **Magnify is verified live**
  (phase series 1/2/2/4, mask 31 zoomed Preview on `mm.local`) and
  **two-finger scroll is verified live** via subtype 11: the 16-bit `dy` line
  delta is the operative field (`+` up, `-` down, ≈1 line/unit), one `phase=2`
  frame suffices, and the frame must be exactly 58 bytes or the server
  desynchronises. Swipe reaches the agent but has no visible action on this
  host (vertical Mission Control swipes disabled; possibly a single desktop
  space). Rotate still needs app-level calibration.
- **M5 (GUI):** `ss-gui` (winit + softbuffer) runs on macOS and uses the
  native Wayland backend on Linux. It presents BGRX frames, forwards keyboard
  and pointer input, handles pinch/rotate/pan/smart-zoom, and has a headless
  `--dump` path that verifies compositing. Server-driven `0x09` updates with
  `0x451` re-arm, and full-resolution dirty-rect polling make it interactive.
  `--scale <1.0` (`0x08`) is experimental because this server build stops
  incremental updates in scaled mode. The window forces focus at startup and on
  first click; `--debug-input` traces winit events, `--ime` enables CJK input,
  and the title reports focus/disconnect state. `--scale` scales only the
  image; input coordinates stay in the server's original backing space.
- **Gesture input:** `gesture-input` has a tested pure-Rust recognizer and a
  Linux evdev MT-protocol-B reader (`--evdev`, input group required).
- **MVS (2026-11-04):** the frame splits into **two independent streams** —
  commands in `body[6:data_off]` (reader at `state+0x98`) and *all* payloads in
  `body[data_off:]` (reader at `state+0xa8`). A full frame validates exactly:
  64,000 tiles, 1,534 commands, 2,352 DCT blocks, each stream ending with a
  bit-aligned `0x6d` marker plus zero padding. The block decoder
  (`0x7ff922480b28`) is callable in-process; only bit consumption is validated,
  not pixels. See `docs/findings/2026-11-04-codex-mvs-streams.md`.
- **MVS is the fast path.** A full 2560x1600 frame is 44–49 ms end to end
  (14 ms decode) versus 632–675 ms for the deflate tiers, which is the
  difference between a responsive mirror and one that stalls on repaints.
- **Push mode (2026-11-04, VERIFIED):** server push (`0x09` with
  `selected_screen = 0`, then one non-incremental `0x03`) emits **type-0
  deltas** that reference earlier tiles through a 99-byte coefficient ring
  (`viewer+0xda0`) plus **type-1 refinement** bodies that are the only writers
  of that ring. Both are decoded (`rfb-proto::mvs`), and the result agrees with
  an independent native-assisted renderer to max channel diff 1. Contract:
  `codecs.md` §2.20; record: `docs/findings/2026-11-04-push-mode-mvs.md`.
- **Latency reality:** polling MVS answers a keystroke in 85–98 ms p50, and the
  floor is one server capture interval — not the request loop (applied rects
  arrive 6–9 ms apart during a repaint). Whether push beats that is still
  unmeasured; `ss-gui` does not yet expose a correct push path.
- **Clipboard:** text sync is verified in both directions
  (`docs/protocol/clipboard.md`). Reads need no AutoPasteboard; writes require
  `0x15 AutoPasteboard(1)`, which puts the session in promise mode, so `ss-gui`
  defaults to read and offers `--send-clipboard` for write.
- **Next:** wire `ss-gui --push` to the now-working decoder and measure
  keystroke latency against the 85–98 ms polling baseline; capture a real
  swipe's CGEvent fields for swipe calibration (local Mission Control steals the
  gesture, so this needs a suppressing tap or a parameter sweep); clipboard
  duplex on one connection; keepalive/rekey; package on a Linux Wayland host.
- **Blockers:** none. Debugging/DTrace restrictions force the
  re-signed-copy/shim route; the in-process `dlopen` + `ndisasm` method is
  already working and documented in `tools/instrument/`.
