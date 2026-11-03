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

Emitted by `-[SSSession stSendMouseScrollEvent:]` (the `scrollWheel:` path).

```
+0x00 u8 0x17
+0x01 u8 0
+0x02 u16 0x0036
+0x04 u16 1
+0x06 u16 11
+0x08 s16 deltaX
+0x0a s16 deltaY
+0x0c s16 deltaZ
+0x0e i32 fixedDeltaX
+0x12 i32 fixedDeltaY
+0x16 i32 fixedDeltaZ
+0x1e i32 pointDeltaX
+0x22 i32 pointDeltaY
+0x26 i32 pointDeltaZ
+0x2e u32 scrollPhase
+0x32 u32 momentumPhase
+0x36 u16 x               # note: count/flags fields are still being mapped
+0x38 u16 y
```

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

## 5. Calibration notes / open questions

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
- A one-time capture of a real trackpad swipe's CGEvent fields (via an event tap
  on `md.local`) will settle the values; the wire format itself is known.
- Standard keyboard/pointer input still needs a capture; the native client uses
  ordinary RFB KeyEvent/PointerEvent plus the Apple 0x10 encrypted-input variant
  when keystroke-only encryption is selected.
