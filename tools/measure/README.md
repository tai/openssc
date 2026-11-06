# `tools/measure` — latency and feel measurements

Scripts behind §3, §4 and §5 of `docs/testing.md`. The methods, caveats and
recorded numbers live there; this is just the inventory.

| file | what it does |
|---|---|
| `scroll.c` | posts a synthetic two-finger scroll into the frontmost window, with a decelerating tail so macOS synthesises no momentum. Build: `clang -O2 -o scroll scroll.c -framework ApplicationServices` |
| `camera_ab.sh` | one run of the two-screen camera A/B: resets the page, starts a client, pins its window, scrolls to the top, films N identical bursts, converts to a raw gray stream |
| `parity.py` | turns recordings into `head` / `tail` / `stretch` / envelope-lag medians; derives each window's crop from the motion, prints it so it can be pinned |
| `decode_backlog.py` | turns `SS_LATDBG=1` client logs into a decode-backlog report (handles both the per-batch and older per-update log shapes) |
| `wayland-smoke.sh` | Linux/Wayland smoke test: release build, connect to the lab server, MVS dump, PNG validation, optional grim window capture, required clean exit |
| `png_probe.py` | stdlib-only PNG reader used by the smoke test: geometry, byte floor, decoded distinct colours, non-background fraction |
| `render-capture.sh` | present one filter at one window size, dump the 1:1 framebuffer, grim-capture the actual window and report p50/p95 draw time (Hyprland placement; serialized on `/tmp/ss-clone-gui.lock`) |

Typical sequence:

```sh
./camera_ab.sh --client clone --label c1 --remote tai@mm.local --url '<page>'
./camera_ab.sh --client app   --label a1 --remote tai@mm.local --url '<page>'
python3 parity.py clone=c1.raw app=a1.raw
```

Everything writes to `${TMPDIR:-/tmp}/ss-measure` by default and is disposable.
The camera harness (`scroll.c`, `camera_ab.sh`, `parity.py`) is macOS-only,
because that is where the camera and the reference client are. The client itself
targets Linux/Wayland, and `wayland-smoke.sh`/`render-capture.sh`/`png_probe.py`
are the Linux counterpart: they prove paint, presentation and filter cost, not
latency.
