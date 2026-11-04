# 2026-11-04 — Push mode: why MVS deltas sheared, and what type-1 is

Three-agent session (Codex `wK:pB`, OpenCode `wK:pC`, owner/verifier OpenCode
`wK:p9`). Protocol contract is written up in `docs/protocol/codecs.md` §2.20;
this file is the experiment record.

## Symptom

With `--mvs` polling the GUI was correct but typing cost ~85–100 ms. Turning on
server push (`0x09`) cut that, but rendered garbage:

```
85 x  mvs decode failed (2560x200+0+1200): unsupported MVS update type 1
screenshot artifacts/gui-push-window.png: sheared bands, stale strips
```

Two failures, assumed to be separate bugs (type-1 undecoded; type-0 deltas
applied to an accumulated framebuffer). They turned out to be one dependency.

## Root cause

1. `rfb-proto::mvs` implemented type-0 payload widths for codes 6 and 7 and then
   **did nothing with them** (`Command::Cache16 => {}`), because the original
   reference capture never exercised them.
2. Codes 6/7 are 99-byte **coefficient-ring** reads. The ring is inserted into
   **only** by type-1 code 1. So a type-0 delta that references previously-sent
   tiles needs type-1 bodies to have been applied first.

Measured on `artifacts/push-pC/` (89 pushed bodies):

| | code 6 | code 7 | tiles |
|---|---:|---:|---:|
| pushed type-0 deltas (51 bodies) | 18,768 | 7,727 | 214,732 |
| polling type-0 bodies | 0 | 0 | — |

That is the whole V3/V4 split from the earlier investigation: a full-refresh
body needs no cache (perfect), a delta references ~12 % of its tiles through it
(garbage), and polling never emits those references (why polling was clean).

## What the framework actually does (Codex, instruction-level)

Full detail in `docs/protocol/codecs.md` §2.20 and the annotated disassembly at
`artifacts/push-codex-cache-annotated.asm`. Highlights, including two
corrections to our earlier notes:

- **Two tables**, not one: 84-byte spatial records at `viewer+0xd80` (base
  coefficients per tile, refined by type-1) and the 99-byte ring at
  `viewer+0xda0` (65,000 entries, `Y64 / Cr15 / Cb20`).
- **Code 1 = left/previous, code 2 = above.** Our docs had them swapped.
- Ring insertion happens at exactly one call site, `0x3b9df9`, in the type-1
  path. Type-0 never inserts.
- Type-1 is *refinement*, not a standalone frame: 2-bit code per tile, terminated
  by three `mvs` bytes, and code 2 is a generation-checked pixel copy.
- The chroma ACs in type-1 use the **luma** AC Huffman table for both planes.

## Validation

Independent paths, deliberately not sharing state:

| check | result |
|---|---|
| Python parser over Codex's 222-body capture | all bodies reach their terminators, 34,608 ring insertions |
| Python parser over pC's 89-body capture | 20,082 insertions |
| Rust decoder, same 89-body capture | 20,082 insertions, `last_read` 20,073, cursor 20,082, FNV `0xe065e9ab` |
| Native cache oracle (`481cd3` insert / `481ae3` read) | exact 99-byte copy, pixel identity, cursor wrap 64999→1, no insert on read |
| Huffman symbols cross-check | Rust `LUMA_AC_SYMBOLS` matches all 162 framework bytes |
| Renderer vs Rust, final frame | 178,464 px differ (4.4 %), **max channel diff 1**, 0 px above 1 |
| Renderer vs Rust, prefixes at bodies 12 / 45 / 78 | same: max channel diff 1, 0 px above 1 |
| Rendered sequence | coherent desktop, no shear (`artifacts/verify-push-rust.png`) |

The ±1 is the documented native-vs-portable IDCT rounding difference, so the
comparison is "agrees within fixed-point rounding", not "bit-identical". The
prefix checks matter more than the final frame: they show the accumulated
record/ring state agrees throughout the sequence, not just at the end.

Polling pin: the native-assisted renderer on a quant + `mvs-fixture.bin` manifest
differs from the already-verified portable output on 0.558 % of pixels, max
channel error 1, zero pixels above 1 — consistent, and it confirms the oracle's
type-0 half.

## Latency

Same host, same instrumentation, twelve keystrokes into the live window:

| configuration | p50 | min | max |
|---|---:|---:|---:|
| banding + no standing pipeline | 85 ms | 43 ms | 209 ms |
| MVS, banding off + standing pipeline | 98 ms | 46 ms | 201 ms |

No measurable difference, and that is informative: during a repaint the applied
rects arrive **6–9 ms apart**, so the request loop is already sub-frame. The
floor is the server's capture cadence after it injects the keystroke, not our
request scheduling. **Push latency is still unmeasured** — it needs the ss-gui
push path wired to the now-working decoder.

## Lab notes

- Two 12-second push captures; Codex's used a transient
  `osascript display dialog` on the console session to force a screen change.
  The dialog auto-dismissed. No preferences, daemon state, or settings were
  changed; the console user stayed logged in.
- `screensharingd` was killed once on `mm.local` early in the session, before
  the "do not restart the oracle" instruction was issued. launchd respawned it;
  banner and console user verified afterwards. Recorded here because it happened.

## Open questions

1. Does push actually beat the ~85 ms polling floor? Measure with keystrokes.
2. Where does the ~1-capture-interval floor come from — capture rate, or the
   input-injection path? (Affects whether SS.app parity is reachable at all.)
3. Type-1 `cr_ac_count` / `cb_ac_count` are always 0 or small in our captures;
   a capture with real chroma AC content would exercise the AC path harder.
4. The capture fixtures are untracked (they contain screen contents), so the
   two replay tests skip when `artifacts/` is absent. A synthetic, content-free
   fixture would make CI exercise the cache and type-1 grammar.
