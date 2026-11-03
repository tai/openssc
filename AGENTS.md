# AGENTS.md — `ss-clone`

Canonical operating guide for AI agents working on this repository.
**Read this file completely before running anything.** If you learn something that
contradicts this document, update this document in the same change.

- **Project:** a protocol-compatible reimplementation of Apple Screen Sharing
  (`RFB 003.889` + Apple extensions), with a client that runs on **Linux/Wayland**.
- **Primary reason:** Apple Screen Sharing has substantially better trackpad and
  gesture fidelity than generic VNC/RDP clients. Gesture compatibility is a
  first-class requirement, not a nice-to-have.
- **Current phase:** live GUI + input + Linux evdev gesture backend working;
  magnify verified live; performance pass done (auto dirty rects, 0.5 scaling,
  coalesced input). Swipe/scroll semantic calibration, clipboard, and rekey
  handling are next.
- **Last verified:** 2026-11-03 against `md.local` and `mm.local`.

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
  (phase series 1/2/2/4, mask 31 zoomed Preview on `mm.local`). Swipe reaches
  the agent but has no visible action on this host (vertical Mission Control
  swipes disabled; possibly a single desktop space). Rotate/scroll still need
  app-level calibration.
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
- **Next:** capture a real swipe's CGEvent fields for swipe calibration;
  settle subtype-8/11 scroll; add clipboard, keepalive/rekey; package on a
  Linux Wayland host.
- **Blockers:** none. Debugging/DTrace restrictions force the
  re-signed-copy/shim route; the in-process `dlopen` + `ndisasm` method is
  already working and documented in `tools/instrument/`.
