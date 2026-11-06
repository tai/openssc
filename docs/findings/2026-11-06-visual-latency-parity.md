# 2026-11-06 — Visual latency: ss-gui vs Screen Sharing.app, measured on camera

Owner asked whether the client is on parity with the real `Screen Sharing.app`
for *visual* latency (how far the picture in the window trails the actual remote
screen), and to improve it if not. Input latency is not the complaint: both
clients apply the scroll to the remote immediately.

Verdict: **not on parity**. The remote screen performs the scroll in ~500-650 ms
and SS.app shows it in about the same time, while ss-gui keeps painting for
1.5-1.9 s. The cause is decode throughput at full resolution, not the network.

## Method

Camera on both screens, 30 fps, 640x480 (`ffmpeg -f avfoundation`). Bottom screen
is `md.local` with the client window, top screen is `mm.local` (the target).
A synthetic pixel scroll (`scroll3`, same 300 px / 20 events for both clients) is
posted into the client window; the page moves on the remote, then later in the
window. Per-screen motion energy is the mean absolute frame difference inside a
crop over the article list; scroll bursts are runs above threshold; the lag is
the shift that best aligns the two envelopes (sub-frame by parabola).

The scroll tool ends with a decelerating tail so the OS synthesises no momentum
events (`momentum=true` count zero in `--debug-input`), otherwise the two
clients would receive different motion and the envelopes would not be comparable.

Files (untracked): `r1.raw` (ss-gui), `s1.raw` (SS.app), analysis `parity.py`.
Scratch dir `/private/var/folders/7v/sfdz0gqx5j55_vsb9n338yr80000gn/T/opencode`.

    python3 parity.py clone:r1 app:s1

## Baseline (before the decode fix)

| | remote motion | client presentation | stretch | head lag | tail lag |
|---|---|---:|---:|---:|---:|
| SS.app | 500 ms | 633 ms | x1.27 | 250 ms | 383 ms |
| ss-gui | 650 ms | 1733 ms | x2.67 | 250 ms | 1300 ms |

`head` is the offset between the two onsets, `tail` the offset between the two
offsets (how long after the remote settles the window settles). Head lag is the
same in both clients; tail lag is 3.4x ours.

Per burst, ss-gui 1467/1633/1833/1867 ms against a remote 567-767 ms; SS.app
533-700 ms against a remote 500 ms. The first frame lands at roughly the same
time in both clients — what differs is that ours keeps moving long after the
remote has stopped.

Note the framebuffers are not the same: SS.app runs a scaled 2400x1500 Zlib
stream (~1.6 MB per burst), ss-gui runs 2560x1600 MVS (~52 KiB-1 MB). Byte
counts and arrival times are therefore not comparable across clients; only the
camera is.

## Mechanism (camera-independent)

`SS_LATDBG=1` timestamps each update's arrival (`ARR`), decode completion
(`DEC`) and paint (`PAINT`); pairing ARR and DEC FIFO-wise gives the decode
backlog. During one 300 px scroll burst at full resolution:

| | backlog p50 | backlog max |
|---|---:|---:|
| 2560x1600 | 183 ms | 640 ms |
| `--scale 0.5` (1280x800) | 15 ms | 88 ms |

So the backlog is proportional to pixels and the decoder, not I/O, is the
constraint. `sample` on the client during a scroll puts the hot work in
`memmove` (tile copies and frame publishing), `mvs::Idct::plane`,
`mvs::rgb_from_idct` and `decode_partial`.

Work counted from `--debug-input` for one 300 px burst at full resolution:
75 rects, 1,870,946 tiles (119.7 Mpx of tile area), 273,181 DCT blocks
(17.5 Mpx). Only ~15% of tiles carry a DCT block; the rest are fill, copy,
bitmap or colour commands. `Idct::plane` is a naive `f64` separable 8x8
transform measuring ~140 Mpx/s; the burst needs ~190 Mpx/s before counting
copies. The decoder cannot consume the stream in real time, so the picture plays
the scroll back over roughly 2.5x its real duration.

## After the fix (measured like-for-like)

Same page (a long Japanese Wikipedia article), same window geometry (pinned to
1280 px wide at the same screen position for both clients), same synthetic
scroll, four bursts per run. The pre-fix client was rebuilt from `2d0ea47` and
recorded on that same page, so the before and after rows are the same content,
not two different pages.

| | runs / bursts | remote motion | client presentation | stretch | envelope lag | head | tail |
|---|---|---:|---:|---:|---:|---:|---:|
| ss-gui before (`2d0ea47`) | 2 / 8 | 617 ms | 733 ms | x1.38 | 483 ms | 300 ms | 533 ms |
| ss-gui after | 3 / 12 | 600 ms | 467 ms | x0.94 | **157 ms** | **167 ms** | **133 ms** |
| SS.app | 3 / 13 | 433 ms | 467 ms | x1.06 | 237 ms | 233 ms | 267 ms |

Envelope lag fell from 483 ms to 157 ms (3.1x) and is now lower than SS.app's.
The client presents the motion in about the time the remote takes to do it
(stretch 0.94), where before it took 1.4x as long and finished half a second
late. `head` and `tail` are within a frame or two of SS.app's, so the two are
indistinguishable on this measurement; if anything ours starts marginally
sooner, which is consistent with SS.app paying ~1.6 MB of transfer per burst
where we send 0.05-1 MB.

Remote durations differ per client row because each client forwards the input
slightly differently (SS.app coalesces the wheel stream differently and its
scaled framebuffer moves a different number of remote pixels), which is why
`stretch` is the comparable number and raw milliseconds are not.

Method caveats: 30 fps gives 33 ms granularity, so medians over 8-13 bursts are
quoted; the camera reads the bottom of the frame later than the top (rolling
shutter, ~10 ms), which biases both clients the same way; a client that paints
in large jumps can produce a clipped burst whose negative `tail` is an artefact
of boundary detection rather than the window finishing early. None of these
change the ordering, and the internal backlog measurement below is independent
of the camera.

## Follow-up

- Integer fixed-point IDCT, direct fb-to-fb tile copies for CopyPrevious/
  CopyAbove, and one publish per drained batch instead of one per update.
- Re-measure with the same harness; report the same table.
- Remaining known gap: the live client decodes a heavy body ~2.7x slower than
  the replay benchmark for the same bytes, which bounds the worst-case tail.
  Worth isolating before chasing further absolute gains.

## What the camera settled about the server's reaction time

An earlier packet-level decomposition put the server's first byte after a scroll
at ~76 ms for us against 7-52 ms for SS.app, and it was suspected that the
server reacts to SS.app sooner. The camera says that is not what the owner sees:
the time from the scroll to the *first* visible change is the same for both
clients (~233 ms, both on the remote screen and in the window). The difference
is entirely in how long each client takes to finish presenting the motion. So
the first-byte term is not worth chasing before the decode backlog is gone.

## Not the cause (checked)

- Network: receive throughput already measures 112 Mbit/s against a 114 Mbit/s
  link (fixed earlier by moving decode off the socket-reading thread).
- Debug build: the workspace sets `opt-level = 3` for `rfb-proto` and `ss-gui`
  in the dev profile, so the debug binary is optimised.
- Momentum asymmetry: the input tool emits no momentum.
