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

Four layers, cheapest first. Use the lowest layer that can answer the question.

| layer | answers | needs lab? | cost |
|---|---|---|---|
| unit / contract tests | do the layouts and decoders match their fixtures | no | ~2 s |
| replay benchmark | how fast is decode, deterministically | no | ~1 s |
| client-internal timing (`SS_LATDBG`) | is the decoder keeping up with the wire | yes | ~30 s |
| two-screen camera A/B | does the picture trail the remote | yes + camera | ~2 min/client |
| wire capture (pcap) | what was sent, when, how many bytes | yes | ~30 s |
| native client as oracle | what should the answer look like | yes | manual |

---

## 1. Unit and contract tests

```sh
cargo test --workspace          # 73 tests; no lab, no network
```

These are the only tests that block a commit. They come in four groups:

- **`rfb-proto`** (33 = `mvs` 19 + protocol 11 + `filecopy` 3) — the MultiVariant decoder against recorded
  bodies: header/command-stream totals, block sizes and values, quant tables,
  cache-ring wrap and type-0/type-1 interaction, and the renderer. Two of them
  are the safety net for the fixed-point IDCT: `integer_idct_matches_reference_on_random_blocks`
  and `integer_idct_matches_reference_on_capture` compare the fast path against
  the old `f64` reference and require ≤1 channel of difference.
- protocol layouts in `rfb-proto/src/lib.rs` (11) — message layouts (clipboard fetch is
  exactly 8 bytes, gesture begin/end, key and pointer events, pasteboard
  archives, record cipher statefulness).
- **`filecopy`** (18 = 3 framing tests in `rfb-proto` + 15 in `ss-gui`) —
  `NewItem` layout, chunking, DFS ordering, path safety, collision handling, and
  which clipboard shapes may be pulled at all.
- **`gesture-input`** (14) — axis filter behaviour (5), momentum budget and
  decay (4), the portable recogniser (5).
- **`ss-gui`** (25 = 8 in `main.rs`, 15 in `filecopy.rs`, `grab.rs`,
  `momentum.rs`) — key mapping (US/JIS), scroll synthesis and the sub-line
  invariant, the `--coast` default, grab-chord parsing, the momentum
  classification queue, and `usage_documents_every_flag`, which fails if a flag
  is parsed but not described in `USAGE`.
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
SS_LATDBG=1 ./target/release/ss-gui --host mm.local --mvs > /tmp/dbg.log 2>&1 &
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
| ss-gui before (`2d0ea47`) | 2 | 617 ms | 733 ms | ×1.38 | 300 ms | 533 ms | 483 ms |
| ss-gui v1.5.0 | 3 | 600 ms | 483 ms | ×0.97 | 167 ms | 150 ms | 158 ms |
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
scaled 2400×1500 Zlib stream (~1.6 MB per burst) while ss-gui runs 2560×1600 MVS
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
tail and is unmeasured. Linux/Wayland has not been run end to end at all.

## 9. Checklist for a measurement or a new test

1. State the question and the number that would answer it *before* measuring.
2. Prefer the cheapest layer that can answer it (§table at the top).
3. For anything cross-client or user-visible, use the camera, and keep page,
   window geometry, crop, scroll shape and momentum identical between sides.
4. Interleave A and B runs; compare medians, not single samples.
5. When a measurement disagrees with the docs, believe the measurement, fix the
   docs, and record the number and the date in §8.
6. Prune `artifacts/` afterwards to the keep-list in §7.
