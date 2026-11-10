# Input and gestures — Apple Screen Sharing

Status: **wire format VERIFIED** from the real client's `ScreenSharing.framework`
(static analysis + live server logs). Semantic acceptance of synthetic gestures
on macOS still needs calibration; see §5.

Evidence:
- class/method dump: `artifacts/framework-gestures.txt`, `tools/instrument/framework_probe.py`
- method disassembly: `artifacts/fw/`, `tools/instrument/annotate_method.py`
- live server log: agent prints the gesture struct verbatim
- live test: `tools/probe/gesture_probe.py`

## 1. Where gestures live

The client-side classes are in `ScreenSharing.framework` (cache-only on modern
macOS):

| Class | Fields |
|-------|--------|
| `SSGestureEvent` | `mSubType` (+0x28) |
| `SSGestureSwipeEvent` | progress, positionX/Y, velocityX/Y/Z, motion, swipeMask, flags, flavor |
| `SSGestureScrollEvent` | deltaX/Y/Z, naturalScrolling, gesturePhase, gestureMask |
| `SSGestureMagnifyEvent` | magnification, gesturePhase, gestureMask |
| `SSGestureRotateEvent` | rotation, gesturePhase, gestureMask |
| `SSGestureBeginEvent` / `EndEvent` / `QuickLookEvent` / `SmartMagnifyEvent` | coordinates only |

`-[SSSession stSendGestureSwipeEvent:]` etc. pass the fields to a packing
function that emits **RFB message type 0x17** (a client→server message not in
the standard RFB registry).

## 2. Message 0x17 layout

All fields are big-endian. Common header:

```
u8  type = 0x17
u8  reserved = 0
u16 length              # total bytes - 4
u16 version             # 1 (2 for magnify/rotate)
u16 subtype
```

Subtypes and total sizes:

| Subtype | Name | Total bytes | Version |
|---------|------|-------------|---------|
| 1 | Begin | 16 | 1 |
| 2 | End | 16 | 1 |
| 3 | Magnify | 36 | 2 |
| 4 | SmartMagnify | 12 | 1 |
| 5 | Rotate | 36 | 2 |
| 6 | Swipe | 52 | 1 |
| 7 | QuickLook | 12 | 1 |
| 8 | Scroll (gesture) | 36 | 1 |
| 11 | Mouse scroll (wheel) | 58 | 1 |

### 2.1 Begin / End (subtypes 1, 2)

```
+0x00 u8 0x17
+0x01 u8 0
+0x02 u16 0x000c
+0x04 u16 1
+0x06 u16 subtype
+0x08 u32 extra          # observed 0; series/mask semantics TBD
+0x0c u16 x
+0x0e u16 y
```

### 2.2 SmartMagnify / QuickLook (subtypes 4, 7)

```
+0x00 u8 0x17
+0x01 u8 0
+0x02 u16 0x0008
+0x04 u16 1
+0x06 u16 subtype
+0x08 u16 x
+0x0a u16 y
```

### 2.3 Magnify (3) and Rotate (5)

```
+0x00 u8 0x17
+0x01 u8 0
+0x02 u16 0x0020
+0x04 u16 2
+0x06 u16 subtype
+0x08 f64 value          # magnification or rotation (big-endian IEEE-754)
+0x10 u16 x
+0x12 u16 y
+0x14 u64 gesturePhase
+0x1c u64 gestureMask
```

### 2.4 Swipe (6) — the key gesture message

```
+0x00 u8 0x17
+0x01 u8 0
+0x02 u16 0x0030
+0x04 u16 1
+0x06 u16 6
+0x08 u32 motion
+0x0c u32 swipeMask
+0x10 f32 progress
+0x14 f32 positionX
+0x18 f32 positionY
+0x1c f32 velocityX
+0x20 f32 velocityY
+0x24 f32 velocityZ
+0x28 u32 flags
+0x2c u32 flavor
+0x30 u16 x
+0x32 u16 y
```

The live agent log confirms the field mapping:

```
x 640.000000 y 400.000000 motion 1 swipemask 1 progress 1.000000
pos x 0.500000 pos y 0.500000 vel x 0.000000 vel y 0.000000
vel z 0.000000 flagbits 0 flavor 0
```

The client sends backing-pixel coordinates (`2560x1600` center = `1280,800`);
the server converts to logical points (`640,400`).

### 2.5 Scroll (8) — gesture scroll

```
+0x00 u8 0x17
+0x01 u8 0
+0x02 u16 0x0020
+0x04 u16 1
+0x06 u16 8
+0x08 f32 deltaX
+0x0c f32 deltaY
+0x10 f32 deltaZ
+0x14 u32 naturalScrolling   # boolean
+0x18 u32 gesturePhase
+0x1c u32 gestureMask
+0x20 u16 x
+0x22 u16 y
```

### 2.6 Mouse scroll (11) — normal wheel / two-finger scroll

**VERIFIED live** (2026-11-03): this is the subtype that actually scrolls a
macOS app, and the `dy` line delta is the operative field. Total length is
exactly **58 bytes**; a short frame hangs the session. See §5.1.

Emitted by `-[SSSession stSendMouseScrollEvent:]` (the `scrollWheel:` path).

```
+0x00 u8  0x17
+0x01 u8  0
+0x02 u16 0x0036          # payload length (54) => 58 bytes total
+0x04 u16 1
+0x06 u16 11
+0x08 s16 deltaX
+0x0a s16 deltaY          # the operative field
+0x0c s16 deltaZ
+0x0e i32 fixedDeltaX
+0x12 i32 fixedDeltaY
+0x16 i32 fixedDeltaZ
+0x1a u32 unlabelled (0)
+0x1e i32 pointDeltaX
+0x22 i32 pointDeltaY
+0x26 i32 pointDeltaZ
+0x2a u32 unlabelled (0)
+0x2e u32 scrollPhase
+0x32 u32 momentumPhase
+0x36 u16 x
+0x38 u16 y
```

The two 4-byte words at `+0x1a` and `+0x2a` are unlabelled. Their position
matters: filling them shifts `pointDelta*`, `scrollPhase`, `momentumPhase` and
`x`/`y` into the wrong places. An earlier build appended both words at the end
instead, which left every field after `+0x1a` misaligned.

The sender reads the private CGEvent fields 118/119/120 (gesture scroll X/Y/Z),
135 (natural), 132 (phase), 133 (mask) for subtype 8; and the standard
`SSMouseScrollEvent` fields for subtype 11.

## 3. CGEvent field mapping (client producer)

`-[SSFrameBufferView swipeWithEvent:]` reads the NSEvent's CGEvent:

| Field | Constant | Wire field |
|-------|----------|------------|
| 123 | `kCGEventGestureSwipeMotion` | motion |
| 134 | `kCGEventGestureSwipeMask` | swipeMask |
| 124 | `kCGEventGestureSwipeProgress` | progress |
| 125/126 | positionX/Y | positionX/Y |
| 129/130/131 | velocityX/Y/Z | velocityX/Y/Z |
| 136 | `kCGEventSwipeGestureFlagBits` | flags |
| 138 | `kCGEventGestureFlavor` | flavor |

`kCGEventGestureSwipeMask` uses the IOHID swipe mask:
**up=0x01, down=0x02, left=0x04, right=0x08**.
`motion` is the navigation-swipe motion enum; the agent treats `motion == 2`
specially when computing `kCGEventGestureSwipeValue` (115).

## 4. Server-side pipeline

```
client 0x17 gesture
  -> screensharingd
  -> SSAgent_PostGestureEvent_rpc (MIG) + gesture struct
  -> ScreensharingAgent
  -> CGEventCreate(type 29 = gesture)
  -> CGEventSetIntegerValueField(110, 16)   # navigation swipe
  -> fields 123/134/124/125/126/129/130/131/136/138
  -> CGEventSetIntegerValueField(115, swipeValue)
  -> CGEventPost(kCGHIDEventTap)
```

The agent log line is emitted immediately after parsing the struct and before
posting.

## 5. Modifier keys on this server — Option is 0xFFE7 (VERIFIED 2026-11-05)

Modifiers must be sent as their own RFB KeyEvent messages, not folded into the
key that follows. This client forwards Control, Option and Command as discrete
key-down/key-up events and re-syncs them per key so a missed or reordered winit
`ModifiersChanged` cannot silently drop one.

The keysym choice matters and is **not** the textbook mapping on this server:

| modifier | keysym we send | notes |
|---|---|---|
| Command | `0xFFEB` (Super_L) | works as expected |
| Option | **`0xFFE7` (Meta_L)** | `0xFFE9` (Alt_L) is accepted as a key but never raises the Option flag |
| Control | `0xFFE3` (Control_L) | works as expected |
| Shift | `0xFFE1` (Shift_L) | forwarded like the others; the *server* still ignores it for printable keys because the character keysym carries case |

Getting this wrong is silent: with `0xFFE9` the remote received `Cmd+C` instead
of `Option+Cmd+C`, so Finder did a plain copy and the clipboard carried a file
name instead of a path. Nothing errors; the shortcut simply does something else.
Verified by forwarding Option+Cmd+C and reading the remote clipboard:
`/Users/tai/Desktop/rf_b.txt` (a path) with `0xFFE7`, `rf_b.txt` (a name) with
`0xFFE9`.

Note this contradicts a static reading of the daemon's `KeyMap.c` table, which
places Meta at the Command keycode. The empirical result wins; if the mapping is
ever revisited, re-test rather than trusting the table.

## 6. Keyboard layouts — send the character, the remote picks the position

VERIFIED 2026-11-05 on a JIS/JIS pair (the owner's setup). This supersedes the
naive "translate the character to a US key + Shift" approach, which is what made
`@` type `2` and `^` type `6`.

**The wire token is the character's keysym.** The daemon/agent resolves it
through the *remote* layout to a keycode and a shift level. Measured:

| typed | keysym sent | remote produced |
|---|---|---|
| `a` | `0x61` | `a` |
| `Shift+a` | `0x41` — **no Shift event** | `A` |
| `@` (JIS position) | `0x40` | `@` |
| `^` | `0x5E` | `^` |
| `:` | `0x3A` | `:` |

Two consequences worth remembering:

1. **Shift is ignored for printable keys.** The character carries its own case,
   so `A` goes as `0x41` with no Shift. Sending `Shift + 0x32` for `@` is what
   produced `2` before: the shift was dropped and the keysym named the wrong
   glyph.
2. **The local layout never has to be named.** winit reports the composed
   character under the local layout; that character *is* the token. A US
   position keysym (`0x5B` for `@`) types `[` on the remote, so translating to
   "the US key that would produce this character" is wrong.

Named keys keep the same shape: their token is their named keysym (`0xFF51`
Left, `0xFF0D` Return), and the daemon maps it. There is no separate branch.

### Layouts and `--keymap`

A layout is data: `position -> (plain, shifted)` characters. `--keymap us|jis`
selects the **remote** layout (default `us`). The table is used to validate that
the remote can type the character and to resolve the shift level; because the
token is the character, most input works even if the table is the wrong one —
which is why a JIS pair worked before `jis` was selectable.

Adding a keyboard type is therefore a new table, not new dispatch logic.

### What cannot be expressed

The daemon's keysym tables carry **no JIS-specific keysyms**: no `¥`, no
変換/無変換, no かな/英数 (verified statically in `KeyMap.c`, table at
`0x100092110` plus the 32-entry US pass). Those keys are logged as unsupported
rather than guessed. Supporting them needs a different mechanism (raw Apple
keycodes), not a better table.

### Modifier keys

Control, Option, Command and Shift are sent as discrete key events and re-synced
per key so a missed event cannot leave one stuck. **Option is `0xFFE7` (Meta_L) on
this server** — `0xFFE9` (Alt_L) is accepted as a key but never raises the Option
flag, which silently turned `Option+Cmd+C` into `Cmd+C`.

Shift needs both statements kept apart: we **do** forward it (`0xFFE1`), but the
server **ignores** it for printable keys because the keysym we send is the
character's own and already carries case (`A`, `@`, `:`). Forwarding it matters
for chords (`Cmd+Shift+V`) and for named keys; it does not produce uppercase.

## 7. Keyboard grab (macOS) — local shortcuts belong to the remote

While the openssc window is focused, a session-level `CGEventTap`
(`kCGSessionEventTap`, consume-capable) takes keyDown/keyUp/flagsChanged and
routes them to the remote instead of letting macOS handle them. Without this,
global shortcuts are consumed by the window server before any app sees them:
`Ctrl+Space` switches the local input source, `Cmd+Space` opens Spotlight,
`Cmd+Tab` switches apps — and the remote never receives them.

Consumed events are translated through the same key-mapping code as winit events
(virtual keycode → the layout tables, composed character from
`CGEventKeyboardGetUnicodeString`), so there is one place that decides what goes
on the wire. `--debug-input` prints them as `tap key ...`, distinct from winit's
`key ...`, which makes it obvious which path delivered a key.

- **Release chord**: hold **Left Command + Shift for 350 ms**. A modifier-only
  chord needs the hold, otherwise `Cmd+Shift+V` (pull from remote),
  `Cmd+Shift+3/4/5`, `Cmd+Shift+Z` and friends would drop the grab on their way
  to the remote. Any third key or extra modifier cancels the countdown.
  `--release-grab` overrides it. After it fires the grab stays off until the
  window loses and regains focus — deliberately, and because a disabled tap
  cannot observe the chord to toggle back.
- **Pointer is not grabbed.** Doing so would break winit's mouse handling,
  click-to-focus (the way out of the session) and the local cursor.
- **Permission**: needs Accessibility / Input Monitoring. `AXIsProcessTrusted`
  is checked once and the prompt requested; if it is refused the app logs the
  exact System Settings pane and runs **without** the grab rather than failing.
- **Safety**: `kCGEventTapDisabledByTimeout`/`ByUserInput` re-enable the tap and
  log (a silent disable looks like "keys stopped working"); every release path
  sends modifier key-ups so nothing sticks on the remote; `--no-grab` disables
  the subsystem.

## 8. Calibration notes / open questions

### 8.1 Two-finger scroll — VERIFIED (2026-11-03, revised 2026-11-05, `mm.local`)

The working path is **subtype 11 (MouseScroll)**, not subtype 8. Subtype 8
(gesture scroll, f32 pixels) is accepted and inert on this server; the messages
below are the only ones that move anything.

| Question | Result |
|---|---|
| Which delta field scrolls? | **Both, and they mean different things.** For *precise* (trackpad) input the client sends **`line = 0` and the whole delta in `fixedDelta`** as 16.16 lines — the server accumulates the fraction, which is what makes sub-line motion smooth instead of stepped. For *wheel* input the 16-bit line fields carry the notches and `fixedDelta` stays zero. The earlier claim that `fixedDelta` alone does nothing was an artefact of the then-wrong field offsets (the two unlabelled words at `+0x1a`/`+0x2a` shift everything after them); with the layout corrected it moves the page. |
| Why `fixedDelta` and not `line`? | Measured from the native client: 295 records of the same **82-byte** subtype-11 message over 22.9 s (~60 Hz), one per trackpad event, including deltas below one line. A locally truncated whole-line delta is what made our motion look step-wise. See `docs/findings/2026-11-06-visual-latency-parity.md` for the measurement harness. |
| Is the phase series required? | **No.** A single `phase = 2` frame with `dy = +10` scrolled TextEdit up ~8 lines. `phase = 1/4` frames are harmless but optional. |
| Sign | **`dy > 0` scrolls up (earlier content), `dy < 0` scrolls down.** |
| Magnitude | ≈ **1 line of content per unit of `line`/`fixedDelta` divided by 65536**: `dy=-10` moved 15 lines, `dy=+10` moved ~8, `dy=+5` moved ~4. The client's pixel→line divisor is `SCROLL_POINTS_PER_LINE` (24 points per line, `--scroll-speed` scales it). |
| Frame length | Must be **58 bytes** (length field `0x36`). Our earlier 50-byte frame desynchronised the server: it waited for 8 bytes that never came and the session hung. |
| Unknown bytes | Two 4-byte words at `+0x1a` and `+0x2a`; zero is accepted. |
| Where does it land? | HYPOTHESIS: the event only takes effect when the message's `x,y` fall inside the **frontmost** window. Earlier tests that appeared to confirm this had `x,y` at the wrong offsets, so re-test with the corrected layout. |

Reproduce:

```sh
# One complete scroll gesture on a single connection
cargo run -p ss-client -- gesture --host mm.local --kind wheel \
    --dy -10 --point-y 0 --x 1200 --y 800 --out artifacts/s.png
```

`openssc` maps winit `MouseWheel` and the evdev recogniser onto subtype 11: pixel
deltas are divided by `SCROLL_POINTS_PER_LINE` (24, tunable with
`--scroll-speed`), the precise path puts the whole value in `fixedDelta` with
zero lines and keeps **no** local remainder (the server accumulates), wheel input
uses whole lines, and a single event is clamped to ±40 lines. The scroll axis
filter decides vertical vs both-axis continuously from recent motion.

### 8.2 Swipe — still uncalibrated

- `mm.local` has **vertical** three/four-finger Mission Control swipes
  **disabled** (`TrackpadThreeFingerVertSwipeGesture = 0`,
  `TrackpadFourFingerVertSwipeGesture = 0`) and horizontal swipes enabled
  (`TrackpadThreeFingerHorizSwipeGesture = 2`,
  `TrackpadFourFingerHorizSwipeGesture = 2`). A vertical swipe therefore cannot
  trigger Mission Control on this host.
- Our synthetic subtype-6 messages reach the agent and are logged with the
  expected fields, but no visible action has been observed yet for
  `(motion, swipeMask, progress)` combinations tried so far. The remaining
  variables are the `motion` enum, `flags`, `flavor`, and whether macOS requires
  a valid begin/end series or a physical HID source.
- Capturing the real client's swipe values is blocked on `md.local`: with
  three-finger Mission Control swipes enabled, the local window server consumes
  the gesture before `Screen Sharing.app` sees it, and the client then sends
  nothing (verified: the agent log shows no `PostGestureEvent`). Rewriting
  `TrackpadThreeFinger*SwipeGesture` in `com.apple.*MultitouchTrackpad` and
  restarting Dock did **not** change the local behaviour.
- Next options: a suppressing CGHID event tap plus re-post into the session, or
  parameter sweeps against `mm.local` using subtype 6 directly.
- Standard keyboard/pointer input still needs a capture; the native client uses
  ordinary RFB KeyEvent/PointerEvent plus the Apple 0x10 encrypted-input variant
  when keystroke-only encryption is selected.
