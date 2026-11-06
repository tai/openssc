# 2026-11-06 — MVS decode throughput at full resolution

Experiment record for the scroll-burst decode work. Protocol contract unchanged;
the additions worth knowing are summarised in `docs/protocol/codecs.md` §2.21.

## Symptom (measured, not guessed)

`ss-gui` at 2560x1600 with MVS push could not decode as fast as the server sent
during a scroll. `SS_LATDBG=1` pairs each body's arrival with the time its batch
finished decoding, which gives the visible backlog directly.

Baseline, one 300 px synthetic scroll, same host, same remote content
(`--scroll-test medium` after pre-scrolling the remote document so an upward
scroll has room):

| | p50 | p90 | max |
|---|---:|---:|---:|
| before | 681 ms | 1081 ms | 1373 ms |
| after (two runs) | 16-17 ms | 56-58 ms | 88-106 ms |

The numbers depend on what is on the remote screen; the heavy frame of this
capture is 2560x1432 with ~30,000 DCT blocks (52% of tiles), far more DCT than
the 2026-11-04 text-page burst.

## Replay benchmark

`SS_DUMP_MVS=<file>` records every `0x3f3` body with its rect, encoding and
arrival ms (format in `crates/rfb-proto/tests/mvs_replay.rs`).
`tools/capture/trim_mvs_dump.py` cuts one arrival-time window out of it.

Fixture: `artifacts/mvs-scroll-burst.bin` (untracked), 132 bodies / 1,471,236
tiles / 233,522 DCT blocks / 94.2 Mpx, captured 2026-11-06 against `mm.local`.

`cargo test -p rfb-proto --test mvs_replay -- --nocapture`:

| | time | Mpx/s |
|---|---:|---:|
| baseline (f64 IDCT, staged tiles) | 1000.2 ms | 94.1 |
| final | 271.6-275.9 ms | 341-347 |

A DCT-dense second fixture (19 bodies / 687,360 tiles / 359,395 DCT blocks,
`artifacts` untracked) went 280.7 ms -> 237.5 ms.

## What was slow

`sample` on the decoder thread, before:

- `Idct::plane` 33% (f64 separable, 1,024 multiply-adds per plane)
- `rgb_from_idct` + `render_coeff` 27%
- `decode_partial` 12%, bit reader + block decoder 26%

Two implementation bugs mattered more than the algorithm:

1. **Overflow checks.** The dev profile is optimised (`opt-level = 3`) but
   `overflow-checks` stay on, so every `+` in fixed-point code carried a `jo`
   branch. The IDCT runs on `wrapping_*` now, which is also the correct
   semantics (it mirrors the native SSE).
2. **The flat-chroma test was wrong.** `coeff[64..]` includes the two DC
   coefficients, so "chroma is flat" was never true and every DCT tile ran three
   IDCTs. Checking `65..128` and `129..192` (the AC ranges) took one plane
   render from 393 ns to 296 ns on the spot.

## What changed

- Fixed-point integer IDCT (`islow` butterfly, 18-bit constants, i64
  accumulators, one rounding at the end). 127 ns/plane vs 450 ns for f64;
  agrees with the f64 basis within one rendered channel on the capture and on
  randomised blocks up to the full i16 range.
- Tiles render straight into the framebuffer: no 256-byte staging tile, no
  `fb -> tile -> fb` copy for CopyPrevious/CopyAbove.
- Run-level fill spans instead of per-tile fills.
- Chroma constants hoisted out of the pixel loop when both chroma planes are DC
  only (the normal type-0 case); the pixel loop auto-vectorises with SSE2
  (`paddsw`/`packssdw`) at 4 pixels per iteration, so no intrinsics were needed.
- Per-tile `index % tiles_x` / `index / tiles_x` replaced by an incremental
  raster position.
- Decoder thread publishes once per batch of drained updates instead of once per
  update. Bodies are still applied strictly in arrival order.

## Deliberately not changed

- Wire format, command grammar, spatial-record semantics and the 99-byte ring.
- `idct_8` lane vectorisation and per-architecture intrinsics: a hand-written
  4-lane i64 version was 40% *slower* (register spills), and the compiler
  already vectorises the pixel path.
- A cached-bit-buffer `BitReader`: measured 7% slower than the eight-byte-load
  version because of the extra state and branch.

## Where the remaining time goes

Final profile of the replay: render 20%, IDCT 17%, block decoder + bit reader
30%, command loop 12%, tile copies 10%. Meeting a 150 ms target on this fixture
would need roughly another 2x across all of those, not one fix.
