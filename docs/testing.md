# Testing and measurement

How this project is tested, what each layer proves, and what the numbers looked
like on the days they were taken. Written so a fresh agent can reproduce a
measurement instead of re-inventing it — and so nobody has to trust a claim in
`STATUS.md` that was never measured.

**Rule of precedence:** where a document and the source code disagree, the code
is right. This file describes methods and records results; the code and
`docs/protocol/*.md` describe behaviour. If you change behaviour, change the
docs in the same commit, and if you cannot reproduce a number here, say so
rather than quietly re-quoting it.

Layers, cheapest first. Use the lowest layer that can answer the question.

| layer | answers | needs lab? | cost |
|---|---|---|---|
| unit / contract tests | do the layouts and decoders match their fixtures | no | ~2 s |
| replay benchmark | how fast is decode, deterministically | no | ~1 s |
| client-internal timing (`SS_LATDBG`) | is the decoder keeping up with the wire | yes | ~30 s |
| Wayland smoke (`tools/measure/wayland-smoke.sh`) | does the release client build, connect, paint and present | yes (no camera) | ~30 s + build |
| render capture (`tools/measure/render-capture.sh`) | does the presented window match a high-quality downscale, and what does draw cost | yes + Hyprland | ~15 s/run |
| two-screen camera A/B | does the picture trail the remote | yes + camera | ~2 min/client |
| wire capture (pcap) | what was sent, when, how many bytes | yes | ~30 s |
| native client as oracle | what should the answer look like | yes | manual |

---

## 1. Unit and contract tests

```sh
cargo test --workspace          # 138 tests; no lab, no network
```

These are the only tests that block a commit. They come in four groups:

- **`rfb-proto`** (34) — the MultiVariant decoder against recorded
  bodies: header/command-stream totals, block sizes and values, quant tables,
  cache-ring wrap and type-0/type-1 interaction, and the renderer. Two of them
  are the safety net for the fixed-point IDCT: `integer_idct_matches_reference_on_random_blocks`
  and `integer_idct_matches_reference_on_capture` compare the fast path against
  the old `f64` reference and require ≤1 channel of difference.
- protocol layouts in `rfb-proto/src/lib.rs` (11) — message layouts (clipboard fetch is
  exactly 8 bytes, gesture begin/end, key and pointer events, pasteboard
  archives, record cipher statefulness).
- **`filecopy`** (18 = 3 framing tests in `rfb-proto` + 15 in `openssc`) —
  `NewItem` layout, chunking, DFS ordering, path safety, collision handling, and
  which clipboard shapes may be pulled at all.
- **`gesture-input`** (31) — axis filter behaviour, momentum budget and decay,
  the portable recogniser, evdev code mapping and the chord parser.
- **`openssc`** (72 = 22 in `manager/`, 8 in `main.rs`, and the rest in
  `filecopy.rs`, `grab.rs`, `momentum.rs`, `render.rs`) — key mapping (US/JIS),
  scroll synthesis and the sub-line invariant, the `--coast` default, grab-chord
  parsing, the momentum classification queue, render resampling, and
  `usage_documents_every_flag`, which fails if a flag is parsed but not described
  in `USAGE`. The 22 manager tests cover target parse/serialise/validation,
  `0600` file creation, focus/selection/edit/save/delete transitions, password
  masking and non-persistence, and the child argv/env.
- **integration** (1) — `tests/mvs_replay.rs`, the decode benchmark below (it
  asserts nothing on time; it prints).

Some tests read fixtures from the gitignored `artifacts/` tree and **skip
silently when the fixture is absent**. A green run on a fresh clone therefore
proves less than a green run with fixtures — see §7.

## 2. Replay benchmark

```sh
cargo test --release -p rfb-proto --test mvs_replay -- --nocapture
```

Replays one captured scroll burst through the production decoder and prints
milliseconds, Mpx/s, tile and DCT counts. It never asserts on wall time; it
exists to compare a change against the baselines below.

| fixture | before the IDCT rewrite | after |
|---|---:|---:|
| 132 bodies, 1.47 M tiles, 233 k DCT, 94.2 Mpx | 1000 ms (94 Mpx/s) | 272 ms dev / 231 ms release (~350–410 Mpx/s) |

Generate a new fixture from a live session with the `SS_DUMP_MVS=<path>` hook,
then cut it to one burst with `tools/capture/trim_mvs_dump.py`. Format is
documented at the top of `crates/rfb-proto/tests/mvs_replay.rs`.

## 3. Client-internal timing: is the decoder keeping up?

```sh
SS_LATDBG=1 ./target/release/openssc --host mm.local > /tmp/dbg.log 2>&1 &
# scroll for a few seconds, then quit
python3 tools/measure/decode_backlog.py /tmp/dbg.log
```

Other env knobs, all lab-only and off unless set: `SS_DUMP_MVS=<path>` (record
every MVS body for the replay benchmark), `SS_SCROLL_SUBTYPE=8` (gesture scroll
instead of subtype 11 — inert on this server), `SS_SCROLL_AT=<ms>` (scripted
scroll timing) and `SS_SCROLL_PROD=1` (force a capture request after each
scroll; measured, no win, kept for A/B).

The reader stamps each update's arrival, the decoder stamps each finished batch,
the UI stamps each present. The backlog is *how old the newest displayed content
is when it becomes displayable* — the number that decides whether the picture
plays a scroll back late.

| state | backlog p50 | max |
|---|---:|---:|
| before the decode rework (2560×1600) | 183 ms | 640 ms |
| after (2560×1600) | **2 ms** | 48–52 ms |
| before, at `--scale 0.5` (1280×800) | 15 ms | 88 ms |

Interpretation: p50 under ~30 ms and max under ~100 ms is real-time. A p50 in
the hundreds means decode is the problem, not the network, and the fix is in
`crates/rfb-proto/src/mvs.rs`, not in socket buffers.

Run this before reaching for a camera: it is cheaper and it localises the fault.
A still screen produces no updates, so a capture with no scrolling looks like a
dead client.

## 4. Two-screen camera A/B: does the picture trail the remote?

This is the only measurement that answers the owner's actual complaint, because
it sees the pixels the eye sees. It is also the fiddliest; read the caveats.

Setup: one camera framing the remote machine and the local client window. Both
show the same page. A synthetic scroll is posted into the client window; the
page moves on the remote screen first, then in the client window. Compare the
two motion envelopes.

```sh
clang -O2 -o tools/measure/scroll tools/measure/scroll.c -framework ApplicationServices

./tools/measure/camera_ab.sh --client clone --label c1 \
    --remote tai@mm.local --url 'https://ja.wikipedia.org/wiki/東京都'
./tools/measure/camera_ab.sh --client app   --label a1 --remote tai@mm.local --url '…'
# interleave: clone, app, clone, app — Wi-Fi and page state drift otherwise

python3 tools/measure/parity.py clone=c1.raw clone=c2.raw app=a1.raw app=a2.raw
```

What the script does, and why each part matters:

- **Page reset** before every run (`--url`), so both clients scroll the same
  content from the same position. Comparing runs recorded on different pages
  produces meaningless differences — that mistake was made once.
- **Window pinned** to the same rectangle for both clients, so the two camera
  crops cover comparable pixels. Screen Sharing.app preserves the remote aspect
  ratio, so it is given extra height to land on the same content width.
- **Decelerating scroll tail** (in `scroll.c`), so macOS synthesises no momentum.
  Momentum events would give the two clients different motion and invalidate the
  comparison; `--debug-input` shows `momentum=true` if one slips through.
- **Scroll to the top** first, so every burst starts from the same place.

`parity.py` derives each window's crop from where the motion actually is and
prints it; pin it with `--crop label=y0,y1,x0,x1` for a repeat run. Metrics per
burst: `remote`/`client` duration, `stretch` (client ÷ remote — 1.0 is
real-time), `head` (time to first visible change), `tail` (how long after the
remote settles the window settles), `env`/`cent` (envelope and centroid lag).

Recorded result, same page, like-for-like, four bursts per run, medians:

| | runs | remote | client | stretch | head | tail | envelope |
|---|---:|---:|---:|---:|---:|---:|---:|
| openssc before (`2d0ea47`) | 2 | 617 ms | 733 ms | ×1.38 | 300 ms | 533 ms | 483 ms |
| openssc v1.5.0 | 3 | 600 ms | 483 ms | ×0.97 | 167 ms | 150 ms | 158 ms |
| Screen Sharing.app | 3 | 433 ms | 467 ms | ×1.08 | 233 ms | 267 ms | 266 ms |

Those are `parity.py` runs with auto-derived crops. The recorded finding used
hand-pinned crops and got 158 ms for the client against 266 ms here and 237 ms
there — the ±30 ms crop sensitivity noted below, not a behavioural difference.

Before: the client took 1.4× as long as the remote and finished half a second
late. After: it tracks the remote, and the two clients are within a frame or two
of each other. The pre-fix row was measured by rebuilding `2d0ea47` and running
the same script, not by reusing an older recording on another page.

Caveats, in order of how much they can bite:

- **30 fps is the resolution.** One frame is 33 ms; single bursts are noisy and
  medians over 8–13 bursts are what get quoted. Do not report a difference
  smaller than ~50 ms without a repeat.
- **Crop sensitivity** is ±30 ms on the envelope metric. `parity.py` prints the
  crop it chose; keep it fixed across runs being compared.
- **Rolling shutter** reads the bottom of the frame later than the top (~10 ms),
  which biases both clients the same way, so it cancels in an A/B.
- **Clipped bursts**: a client that paints in large jumps can produce a short
  burst whose negative `tail` is a detection artefact, not the window finishing
  early. Look at the per-burst lines, not only the median.
- The camera is a macOS harness. Nothing here is required on the Linux target;
  it exists because the alternative is guessing about what the eye sees.

## 5. Wire capture

```sh
sudo tcpdump -i en0 -s 96 -w /tmp/scroll.pcap 'host mm.local and tcp port 5900'
```

Payload sizes and inter-arrival times show how much the server sends per scroll
and how fast it arrives, which is how the receive path was found to be capped at
48–85 Mbit/s against a 114 Mbit/s link (40 MB over ssh in 2.9 s) — the
socket-reading thread was decoding inline. It cannot show decode or paint cost,
and **byte counts are not comparable across clients**: Screen Sharing.app runs a
scaled 2400×1500 Zlib stream (~1.6 MB per burst) while openssc runs 2560×1600 MVS
(0.05–1 MB). Use the camera for cross-client claims.

## 6. The native client as an oracle

`Screen Sharing.app` is the compatibility and feel reference. `tools/probe/`
holds the Phase 1 probes (handshake, auth, framebuffer, MVS grammar) and
`tools/instrument/` the re-signed-copy/`dlopen` helpers used to call Apple's own
decoder in-process. Behaviour questions ("what does the real client send here")
are answered by capturing it, not by reasoning about it — several contracts in
`docs/protocol/` exist only because the real client disagreed with the guess.

## 7. Artifacts: what to keep, what to delete

`artifacts/` and `captures/` are gitignored and hold screen contents, so nothing
there is ever committed. It is a scratch tree, not an archive: it accumulates
fast and must be pruned.

Keep, because tests read them:

| path | used by |
|---|---|
| `artifacts/mvs-fixture.bin` (23 KB) | `mvs::tests::capture()` — the 2560×1600 type-0 body |
| `artifacts/mvs-fixture-quant.bin` (0.1 KB) | `mvs::tests::quant()` |
| `artifacts/mvs-scroll-burst.bin` (2.5 MB) | `tests/mvs_replay.rs` |
| `artifacts/push-pC/manifest.txt` + `artifacts/push-pC/*.bin` | `mvs::tests::replay_manifest` |
| `artifacts/push-codex-manifest.txt` + `artifacts/push-codex-seq-*.bin` | `mvs::tests::replay_manifest` |
| `artifacts/pull-ab/{file,folder}-pathname-flags0.archive`, `smoke-flags0.archive`, `folder-plain-flags0.archive` (3.5 MB) | `filecopy::tests::real_pull_ab_archives_decode_as_expected` |
| `artifacts/*.md` (recon notes, ~250 KB) | cited by code comments and `codecs.md`/`filecopy.md` as the evidence trail |

`captures/` is kept for the one pcap the auth write-up cites
(`captures/20261103-baseline-handshake.pcap`); everything else there was pruned.

Total ~9 MB. Everything else — logs, pcaps, `.bgrx` dumps, PNGs, disassembly,
recordings, one-off analysis scripts — is disposable; delete it when the
question it answered is settled and recorded in a findings note. Keep a copy of
anything you intend to cite from, and cite it by path in the findings file.

Every fixture is optional by design: the tests that read them return early when
the file is missing, so a fresh clone passes. That also means a green run proves
nothing about the codec until the fixtures exist.

**Citations to pruned paths.** The protocol docs and code comments cite
`artifacts/...` and `captures/...` paths as evidence *as it was recorded on the
day* — logs, pcaps, screenshots, disassembly. Most of those are pruned. Treat
such a citation as provenance (which session produced the claim, and what it
measured), not as a file you can open, unless it appears in the keep-list above.
The durable record is the findings note and the `STATUS.md` entry for that
date.

Regenerating the fixtures needs the lab: `SS_DUMP_MVS=<path>` on a live scroll,
`tools/capture/trim_mvs_dump.py` to cut a burst, and `ss-client stream --dump-mvs`
for the single-body fixtures. `ss-client stream` overwrites `artifacts/mvs-4.bin`
on every run, which is why the tests use a stable copy instead.

## 8. Record of campaigns

Each entry says what was asked, what was measured, and what it settled. Numbers
are as measured on that date; do not assume they still hold.

**2026-11-04 — protocol and codec reconnaissance.** Captured the real client and
server, established the handshake, authentication and MVS framing. Findings:
`2026-11-04-codex-mvs-streams.md`, `2026-11-04-push-mode-mvs.md`. Settled: the
two-stream MVS layout, the 99-byte coefficient ring, and that type-1 bodies are
what make type-0 deltas decodable.

**2026-11-05 — clipboard, file copy, JIS keyboard.** Verified on the lab pair
against the real client; recorded in `STATUS.md` per release. The measurable
outcome was byte-identical clipboard in both directions and sha1-identical file
transfers.

**2026-11-06 — visual latency.** Asked: why does the picture trail the remote
when input does not? Method, in the order it was used:

1. pcap during scroll bursts — receive throughput 48–85 Mbit/s against a
   114 Mbit/s link. Fixed by moving decode off the socket-reading thread
   (112 Mbit/s afterwards, at capacity).
2. Re-measured; still late. `SS_LATDBG` then showed the decode backlog growing
   to p50 183 ms / max 640 ms during a burst, and collapsing to 15 ms at
   `--scale 0.5`. The decoder was rendering every intermediate state of the
   scroll and falling behind; the `f64` IDCT ran at 140 Mpx/s where a burst
   needs ~190 Mpx/s before counting tile copies.
3. Fixed in the decoder (fixed-point IDCT, direct-to-framebuffer tiles, one
   publish per drained batch). Backlog p50 16 ms (later repeats on the same
   build: 2 ms), replay 1000 → 272 ms.
4. Camera A/B against Screen Sharing.app, pre-fix build rebuilt for a fair
   comparison: envelope lag 483 → 157 ms, tail 533 → 133 ms, against SS.app's
   266 / 267 ms.

Record: `2026-11-06-visual-latency-parity.md`. Traps found in the process, worth
remembering: a packet-side comparison told us nothing until the *same page* and
*the same crop* were used for both clients; and after the log format changed
from per-update to per-batch `DEC` lines, FIFO pairing of old-style pairs
silently produced nonsense (the parser in `decode_backlog.py` now handles both).

**Still open (2026-11-06).** Replay target of 150 ms for the burst is not met
(272 ms dev / 231 ms release); the live client decodes a heavy body ~2.7× slower
than the replay benchmark decodes the same bytes, which bounds the worst-case
tail and is unmeasured. Linux/Wayland now builds, connects and paints (§9), but
input, gestures and clipboard are unverified there, and there is no camera
measurement of Linux latency.

**2026-11-06 — Linux/Wayland walk-up.** Asked: can the actual target platform
build, connect and present a frame without a camera? Ran
`tools/measure/wayland-smoke.sh` on Arch/Hyprland against `mm.local` and in a
private headless weston. Measured: release build, auth, MVS decode, 2560×1600
paints, grim-captured window, clean exit 0. Also measured the runtime library
set with `ldd`/`LD_DEBUG` and corrected the README's build dependencies (winit
`dlopen`s Wayland/X11/xkbcommon, so no `-dev` packages are needed). Details and
what each check does not prove: §9.

## 9. Linux/Wayland

Everything above was developed on macOS. This section is what was actually
executed on the Linux target: Arch + Hyprland on 2026-11-06, server `mm.local`
(2560×1600), tree = `main` at `4598e4c` plus the harness branch. It extends the
layers above; it does not replace the camera for latency claims.

### 9.1 Build

```sh
export PATH="$HOME/.cargo/bin:$PATH"
cargo build --release -p openssc
cargo test --workspace
```

Measured: `cargo check --workspace` and `cargo test --workspace` pass on Linux
(138 tests: 31 `gesture-input`, 34 `rfb-proto`, 1 replay, 72 `openssc`). rustc
1.99.1 was used; the project's floor is 1.97.

**Build dependencies are smaller than the old README claimed.** Measured with
`cargo tree -p openssc -e build`: there is no `pkg-config`, `cc` build script,
`bindgen` or `cmake` in the build graph. `wayland-backend` is built with
`dlopen`, and `xkbcommon-dl` / `x11-dl` load their libraries at runtime, so no
`-dev` headers are needed to compile. A C linker and glibc are still needed for
the final link and proc-macros (`gcc`/`clang` + `binutils`; `build-essential` on
Debian, `base-devel` on Arch). The old list (`libwayland-dev`,
`libxkbcommon-dev`, `libx11-xcb-dev`, …) was a runtime package list mislabelled
as build dependencies.

If the build fails with `cannot find module or crate 'grab'`, the tree predates
the Linux platform seam (`6582290`); that is a `crates/openssc` problem, not a
harness problem. Do not patch around it in `tools/`.

### 9.2 Run

From the Hyprland session:

```sh
./target/release/openssc --host mm.local
```

Over ssh, the Wayland session variables must be exported or winit finds no
display:

```sh
export WAYLAND_DISPLAY=wayland-1
export XDG_RUNTIME_DIR=/run/user/1000
./target/release/openssc --host mm.local
```

MVS is the default (2026-11-10); `--no-mvs` advertises Zlib
(6). The Linux log line `grab: not available on this platform` is expected: the
macOS keyboard grab has no Linux counterpart yet (see §9.6).

### 9.3 Smoke test: build, connect, paint, present

```sh
tools/measure/wayland-smoke.sh                  # existing session; grim when possible
tools/measure/wayland-smoke.sh --no-grim        # no screenshot

# session-free: force the nested-compositor route even from a real session
env -u WAYLAND_DISPLAY -u XDG_RUNTIME_DIR \
  tools/measure/wayland-smoke.sh --no-grim
```

The script:

1. builds `cargo build --release -p openssc`;
2. runs `openssc --dump <png> --dump-after 6` against `mm.local` with
   `tai` / `$SS_PASSWORD` (plus `--exit-after-dump` when grim is off, and
   `--exit-after-secs 30` as the backstop);
3. waits for the log line `wrote <png>` (printed only after `write_png` returns,
   so the file cannot be read half-written). With grim enabled it then captures
   the window and closes it through the compositor; otherwise it uses the
   client's `--exit-after-dump` path. Either way the client must exit 0;
4. validates the dump with `tools/measure/png_probe.py`, a stdlib-only PNG
   reader. It checks the decoded IHDR geometry, a byte floor of
   `max(10 KiB, 0.5% of raw RGB)`, at least 8 distinct colours, and that not
   every pixel equals the first. No `pngcheck`, `file(1)`, Pillow or camera;
5. if `$WAYLAND_DISPLAY` names a live socket, finds the mapped `openssc` window
   through `hyprctl -j clients` and captures it with `grim -g`. If geometry is
   unavailable it falls back to a full-output `grim`, and if grim fails it warns
   rather than failing the run. With no real session it starts a private
   headless `weston` and skips grim.

Measured runs (2026-11-06, `--mvs`, `--dump-after 6`):

| session | dump | decoded content | grim | exit |
|---|---|---|---|---|
| Hyprland `wayland-1` | 2560×1600, 1,582,635 B | 64,559 colours, 97.8% non-background | window 360×319 → PNG | 0 |
| Hyprland `wayland-1`, `--no-grim` | 2560×1600, 1,552,952 B | 64,149 colours, 97.8% non-background | n/a | 0 |
| private headless weston | 2560×1600, 1,581,159 B | ≥65,536 colours, 97.8% non-background | n/a (headless backend) | 0 |

Sizes vary with screen content; the byte floor is a blank-frame guard, not a
quality or speed metric.

**What this proves:** the release binary starts on Wayland, resolves `mm.local`,
completes Apple auth and session setup, decodes an MVS frame, paints at least six
frames, dumps a non-trivial image at the expected geometry, and exits 0 on the
normal teardown. With grim it also proves the compositor has a mapped window
whose pixels can be photographed.

**What it deliberately does not prove:** input (keyboard, pointer, gestures),
clipboard and file copy, frame rate, latency, colour fidelity, or that the frame
is current — a stale but non-blank dump passes. grim's full-output fallback can
photograph other windows, so it is evidence of a mapped window, not a correct
one. There is no camera harness on Linux yet, so there is no Linux counterpart
to §4 and no Linux visual-latency number.

### 9.4 Runtime dependencies (measured, not guessed)

`ldd target/release/openssc` and `readelf -d` show the direct dependencies:

```text
linux-vdso.so.1
libgcc_s.so.1
libm.so.6
libc.so.6
ld-linux-x86-64.so.2
```

That is the whole `DT_NEEDED` list: the GUI does **not** link Wayland, X11 or
xkbcommon. They are `dlopen`ed. Measured by running a real Wayland session with
`LD_DEBUG=libs` (2026-11-06), the client also loads:

```text
libwayland-client.so.0
libxkbcommon.so.0
libX11.so.6
libX11-xcb.so.1
libxcb.so.1
libXau.so.6
libXdmcp.so.6
```

The X11 set is loaded by the same winit/softbuffer build even in a Wayland
session. The Wayland path needs `libwayland-client` and `libxkbcommon`; the X11
set is present on a normal desktop and is only used if the client falls back to
XWayland. A Wayland-only container without any X11 libraries is untested
(HYPOTHESIS: winit's Wayland backend would still run).

Package mapping. The Arch column was confirmed on this host with `pacman -Qo`;
the Debian names are the standard split (VERIFIED library names, HYPOTHESIS
package names — this host cannot query Debian):

| loaded library | Debian/Ubuntu package | Arch package |
|---|---|---|
| `libc.so.6`, `libm.so.6`, `ld-linux-x86-64.so.2` | `libc6` | `glibc` |
| `libgcc_s.so.1` | `libgcc-s1` | `libgcc` (older: `gcc-libs`) |
| `libwayland-client.so.0` | `libwayland-client0` | `wayland` |
| `libxkbcommon.so.0` | `libxkbcommon0` | `libxkbcommon` |
| `libX11.so.6`, `libX11-xcb.so.1` | `libx11-6`, `libx11-xcb1` | `libx11` |
| `libxcb.so.1` | `libxcb1` | `libxcb` |
| `libXau.so.6` | `libxau6` | `libxau` |
| `libXdmcp.so.6` | `libxdmcp6` | `libxdmcp` |

Optional, because the client shells out to them:

| command | Debian package | Arch package | used for |
|---|---|---|---|
| `wl-copy`, `wl-paste` | `wl-clipboard` | `wl-clipboard` | text/image clipboard |
| `grim` | `grim` | `grim` | smoke-test screenshot only |
| `weston` | `weston` | `weston` | session-free smoke-test mode only |
| `hyprctl` | n/a | `hyprland` | window geometry for grim |

**The clipboard shells out** on Linux: `wl-paste --no-newline` for text,
`wl-paste --type image/png` for images, and `wl-copy` (optionally
`--type <mime>`) to write. The watcher polls every 700 ms. Measured with
`PATH` pointing at an empty directory: the session connects, decodes and dumps
normally, and there is **no warning and no crash** — clipboard reads return
`None` and writes silently do nothing. In other words, `wl-clipboard` is a soft
dependency: without it the remote session works but local clipboard sync is
absent. The `clipboard: automatic duplex` log line still appears because the
protocol side is still trying. `--no-clipboard` disables the 700 ms poll.

### 9.5 Packaging

`packaging/openssc.desktop` used to call `openssc` without `--mvs`, silently
selecting the slow Zlib path. MVS is the binary default since 2026-11-10; the
launcher now opens the connection manager on Linux (`--manager`), and each
Connect child gets MVS and `--evdev` by default (`--no-mvs` still opts out on a
direct CLI run). It keeps stderr in
`$XDG_STATE_HOME/openssc/openssc.log` and sends a desktop notification if the
manager exits non-zero. Saved targets are in
`$XDG_CONFIG_HOME/openssc/targets.conf`; `SS_HOST` is no longer consulted by
the launcher, because the target list owns host selection now.

```sh
./packaging/install.sh                  # ~/.local, builds release first
./packaging/install.sh --no-build       # use target/release/openssc
./packaging/install.sh --prefix /usr/local
```

The installer copies `openssc`, `openssc-launch`, the desktop entry and
`openssc.svg`, and rewrites `Exec=`/`TryExec=` to absolute paths because
`~/.local/bin` is not always on a GUI session's `PATH`. Installed to a temporary
prefix on 2026-11-06 and the generated entry inspected; launching it from a
desktop menu was not automated. The manager launch path itself was exercised
directly (§9.7).

### 9.6 Known gaps on Wayland

- **No camera harness on Linux.** `tools/measure/camera_ab.sh` is
  macOS/avfoundation. There is no Linux visual-latency measurement yet; use
  `SS_LATDBG`/`decode_backlog.py` for decoder health and the camera on the macOS
  harness for feel.
- **Window geometry pinning and `focus_window` do not exist on Wayland.** A
  Wayland client cannot place itself at a chosen rectangle or raise/focus itself;
  the compositor owns that. Measured on Hyprland: the window was tiled at
  360×319 (the size varies with the workspace) instead of the 800×600 the client
  asks for. The macOS A/B pins both clients to the
  same rectangle before filming, which is why cross-client crops are comparable;
  that step has no Wayland equivalent. `--dump` sidesteps it for correctness
  checks, and grim captures whatever the compositor presents, which is enough to
  prove presentation but not to compare two clients on screen geometry.
- **Raw evdev is partial and unexercised.** `--evdev` in `openssc` does spawn
  the `gesture-input` MT-protocol-B reader on Linux and does **not** grab the
  device, but it could not be run here: this user has no access to
  `/dev/input/event*` (no `input` group, no seat ACL). It also discovers devices
  by ioctl capability probing and takes the first MT device, rather than by udev
  `ID_INPUT_TOUCHPAD`. `EVIOCGRAB`, a release chord and a keyboard grab are the
  next phase; the prerequisites are in
  [`docs/findings/2026-11-06-evdev-prerequisites.md`](findings/2026-11-06-evdev-prerequisites.md).
- **No keyboard grab.** `grab: not available on this platform` at startup is
  expected. Local shortcuts reach the compositor, not the remote; the macOS
  Left-Cmd+Shift release chord does not exist here.

## 10. Render quality and presentation

This is the layer that answers "why did the text look rough when the remote
desktop is crisp?". It is separate from decode: a correct framebuffer can still
be presented badly.

### 10.1 The measured root cause

On 2026-11-06 the lab server sent a 2560x1600 framebuffer, Hyprland put the
window on `DP-1` (1920x1080, scale 1), and the default window was 1280x800
logical. `draw()` mapped each output pixel to one source pixel with
`floor(dst * src / dst_len)`: at exactly 2:1 that keeps every other row and
column and **discards three of every four source pixels**. On a 2x backing
(macOS, or this host's eDP-1 at scale 2) the logical 1280x800 becomes physical
2560x1600, the 1:1 `copy_from_slice` path runs, and the same window is crisp.
That is the whole discrepancy.

Before/after grim captures and 2x text crops are in the gitignored
`artifacts/render-quality/` (screen contents are never committed). The
`before-*` files are `--filter nearest`, which reproduces the old mapping; the
`after-*` files are the new default.

### 10.2 What the client does now

`crates/openssc/src/render.rs` owns the resampler and `draw()` calls it:

- **1:1 whole-frame memcpy** is unchanged and still the first branch.
- **`auto`** (default): exact integer downscale -> area/box averaging; every
  other ratio -> bilinear. 2560x1600 -> 1280x800 is the unrolled 2x2 box case.
- **`nearest`** preserves the old `floor(dst * src / dst_len)` mapping exactly,
  for A/B tests and when the user wants the old look.
- **`box`** uses area averaging when both axes divide exactly, otherwise falls
  back to bilinear. **`bilinear`** forces bilinear even for the 2:1 case.
- Maps and weights are built in `Scaler::new`, only on a window/framebuffer
  resize. `apply()` allocates nothing. On x86_64 the bilinear loop processes
  four output pixels at a time with SSE2; a scalar implementation is kept for
  other targets and is compared against the SIMD output by a unit test.

`--filter nearest|auto|box|bilinear` selects the mode. `SS_DRAW_TIMING=1` makes
`--debug-input` print every present time, not only the >5 ms warnings; the
budget warning itself is unchanged.

### 10.3 Default window size

`--window-size WxH` pins a logical size. Without it the client starts at a safe
1280x800 request and, once the compositor tells it which monitor it landed on,
asks for:

- **1:1 with the framebuffer** when that fits inside 95% of the monitor's
  logical area (eDP-1 2880x1800 at scale 2 -> 1280x800 logical = 2560x1600
  physical, so no resampling at all);
- otherwise the largest **16:10** size that fits, starting from 1600x1000
  (DP-1 1920x1080 at scale 1 -> 1600x1000).

Measured with `--debug-input`: `window: 1600x1000 logical for monitor scale 1`
on DP-1 and `window: 1280x800 logical for monitor scale 2` on eDP-1. A tiling
compositor may ignore the request entirely (Hyprland tiled the test window to
331x584); this is a request, not a geometry pin. `--window-size` is the
deterministic option for a capture.

### 10.4 Measured filter quality and draw cost

Method: one run per filter/size, 2560x1600 source, window on DP-1, grim capture
of the actual presented window, and the same run's 1:1 `--dump` downscaled with
an area filter as the reference. The metric is normalized MAE/RMSE of a text
crop against that reference (lower is closer), plus distinct colours in the
crop. This is a consistency check, not a perceptual score.

| window | filter | MAE | RMSE | colours | draw p50 | draw p95 |
|---|---|---|---:|---:|---:|---:|
| 1280x800 | nearest (old) | 0.0300 | 0.0729 | 4,975 | 1.85 ms | 3.90 ms |
| 1280x800 | auto / box | **0.0096** | **0.0104** | 5,556 | 2.80 ms | 4.90 ms |
| 1600x1000 | nearest | 0.0381 | 0.1084 | 6,831 | 2.60 ms | 5.10 ms |
| 1600x1000 | auto / bilinear | **0.0140** | **0.0226** | 7,647 | 7.00 ms | 11.20 ms |

The 2:1 box path is 3.1x closer to the reference in MAE and 7.0x closer in
RMSE than nearest, while staying under the 5 ms budget at p95. The 1600x1000
bilinear path is 2.7x closer in MAE and 4.8x in RMSE than nearest, at p50
7.0 ms: above the 5 ms warning but well inside a 16.7 ms frame. The biggest
perceived gain is the larger window, because 1600x1000 keeps more source pixels
than 1280x800 even before the filter.

Reproduce with the committed capture harness (Hyprland lab; it serializes on
`/tmp/openssc-gui.lock`, uses `--no-clipboard`, bounds the run and verifies the
process is gone):

```sh
cargo build --release -p openssc
tools/measure/render-capture.sh --filter nearest --size 1280x800 --workspace 12 \
    --out artifacts/render-quality/nearest-1280
tools/measure/render-capture.sh --filter auto --size 1280x800 --workspace 12 \
    --out artifacts/render-quality/box-1280
tools/measure/render-capture.sh --filter nearest --size 1600x1000 --workspace 12 \
    --out artifacts/render-quality/nearest-1600
tools/measure/render-capture.sh --filter auto --size 1600x1000 --workspace 12 \
    --out artifacts/render-quality/bilinear-1600
```

`--workspace 12` is the lab's DP-1 workspace. On another desktop omit it (the
window opens on the active workspace) and use `--size` to compare like with
like. The 1:1 `dump.png` in each output directory is the unfiltered reference.

**What this proves:** the presented pixels come from all source pixels in the
2:1 case (box) or a weighted four-sample interpolation otherwise, not one in
four; the filter choice is deterministic and testable; the 1:1 path is still a
memcpy; the draw cost at the common sizes is measured.

**What it cannot prove:** perceptual quality needs the human's eyes. The
reference is generated by another filter, not from a ground-truth display, and
the text crop is only one region. grim captures the compositor's output, so a
wrong window geometry or an overlapping window changes the image; the 1:1 dump
cannot. There is no colour-management/HDR check, no frame-pacing measurement,
and no camera here. Finally, the numbers are this host and this 2560x1600
stream; a different source size, monitor scale or compositor changes both the
chosen filter and the draw cost.

## 11. Checklist for a measurement or a new test

1. State the question and the number that would answer it *before* measuring.
2. Prefer the cheapest layer that can answer it (§table at the top).
3. For anything cross-client or user-visible, use the camera, and keep page,
   window geometry, crop, scroll shape and momentum identical between sides.
4. Interleave A and B runs; compare medians, not single samples.
5. When a measurement disagrees with the docs, believe the measurement, fix the
   docs, and record the number and the date in §8.
6. Prune `artifacts/` afterwards to the keep-list in §7.

### 9.7 Connection manager (`--manager`)

The manager is deliberately a separate window and state machine
(`crates/openssc/src/manager/`) rather than part of the session `App`. Its unit
tests cover the target file format, validation, `0600` creation, focus and
selection, add/save/delete, password masking and the refusal to persist it, and
the exact child argv/env. The abstract layout is tested at the requested size
and at two tiled sizes.

Smoke test on od (Arch/Hyprland, 2026-11-10):

```sh
export WAYLAND_DISPLAY=wayland-1 XDG_RUNTIME_DIR=/run/user/1000
cargo build --release -p openssc
SS_TARGETS_FILE=/tmp/openssc-targets.conf ./target/release/openssc --manager
```

Measured: the window presented and painted at a tiled 457x599 logical (Hyprland
ignored the 900x560 request); a grim capture at 914x1198 showed the title, list,
five fields, all four buttons and the status line. The layout reflows at that
width (47% list, 2x2 buttons) rather than drawing off the surface. A Connect
action spawned `target/release/openssc --host mm.local --port 5900 --user tai
--evdev`; the child connected and painted 2560x1600 while the manager stayed
open. The target file was `-rw-------` and contained only name/host/port/user;
no password appeared in argv or the log.

What this does not prove: scripted mouse input, a successful password
authentication through the field (the smoke run's password field was empty),
or a long-list scroll. Those paths are covered by the unit tests, not by a
compositor-level harness.
