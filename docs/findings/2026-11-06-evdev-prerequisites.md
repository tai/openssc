# Prerequisites for raw evdev on Wayland (next phase)

**Date:** 2026-11-06. **Status:** research only for the permission, grab and
keyboard parts — no system rule or group membership was changed, and `EVIOCGRAB`
is not called anywhere. A basic ungrabbed MT-protocol-B reader already exists in
`crates/gesture-input` and is reachable with `--evdev`; it could not be exercised
on this host because the user cannot open the event nodes. This note records what
the host looks like and what is needed for the next phase: reliable discovery,
optional grabbing and a keyboard grab with a release path.

Wayland deliberately does not hand clients raw input: the compositor owns the
devices and clients receive only `wl_pointer`/`wl_keyboard` events addressed to
their own surface. That is enough for two-finger scroll and gives no
three-/four-finger swipes, no pinch/rotate with finger count, and no keyboard
grab. Reading Linux `evdev` directly is the route to both, at the cost of a
privilege and an escape hatch. Everything marked VERIFIED was measured on this
machine (Arch, Apple internal keyboard + `bcm5974` trackpad) on 2026-11-06.
Kernel semantics cite the source as read on that date; they are not guesses
about behaviour.

## 1. What this host looks like

```
$ id -nG
tai uucp wheel                     # no `input` group
$ ls -l /dev/input/event10
crw-rw---- 1 root input 13, 74 ... event10
$ getfacl /dev/input/event10
user::rw-  group::rw-  other::---  # no user ACL for tai
```

So the current user cannot open the event nodes. Note that this is not a
`sudo`-less inconvenience to work around with `chmod 666`: an evdev fd can read
**every keystroke on the machine**, including passwords, so access must be
scoped to the device and to the seat user.

Measured with the current build:

```text
$ ./target/release/ss-gui --host mm.local --mvs --evdev --exit-after-secs 3
grab: not available on this platform
evdev: no multitouch device found under /dev/input (need input group access)
```

The client exits 0; the message comes from `gesture-input` failing to open any
candidate, so it is a permission failure presented as "not found". The next
phase should distinguish EACCES and name the fix.

Devices on this host (VERIFIED):

| node | name | class |
|---|---|---|
| `/dev/input/event7` | `Apple Inc. Apple Internal Keyboard / Trackpad` | keyboard (`ID_INPUT_KEYBOARD=1`) |
| `/dev/input/event10` | `bcm5974` | **trackpad** (`ID_INPUT_TOUCHPAD=1`, `..._INTEGRATION=internal`) |

The trackpad is a separate event node from the keyboard even though the USB
product name is shared. Do not hardcode `event10`: numbering changes with
hotplug and across boots.

## 2. Finding the trackpad without guessing

Do not pick the node whose name looks like a trackpad, and do not prefer the
first `event*`. Ask udev, which has already classified it:

```sh
for e in /sys/class/input/event*; do
  n=$(cat "$e/device/name" 2>/dev/null) || continue
  props=$(udevadm info --query=property --name="/dev/input/${e##*/}" 2>/dev/null)
  case "$props" in
    *ID_INPUT_TOUCHPAD=1*) echo "${e##*/}: $n (touchpad)";;
  esac
done
```

On this host that prints exactly `event10: bcm5974 (touchpad)`. The same query
also reports `ID_INPUT_TOUCHPAD_INTEGRATION=internal`; an external trackpad does
not have the `internal` value.

Supporting evidence from `/sys/class/input/event10/device/capabilities/`:
`abs` has the MT bits (`ABS_MT_SLOT`/`ABS_MT_POSITION_X`/`Y`/`ABS_MT_TRACKING_ID`),
`key` has `BTN_TOOL_FINGER`/`BTN_TOUCH`; `rel` is zero. That is the shape of an
MT-protocol-B touchpad, which is what `crates/gesture-input` already parses.

`/dev/input/by-id/...-if02-event-mouse` and `/dev/input/by-path/...-if02-event-mouse`
exist on this host, but they encode the model and USB port. Use them to inspect,
not as the discovery rule; re-enumerate with udev when the device appears.

## 3. Permission: group or seat ACL

Two ways to get read access, both requiring a deliberate system change. AGENTS
§4.10 applies: ask a human before installing a rule.

### Option A — `input` group (coarse, persistent)

```sh
sudo usermod -aG input "$USER"
# log out and back in, or at least start a new login session
id -nG | tr ' ' '\n' | grep -qx input && echo ok
```

All event nodes already belong to `input` with mode `0660`. This grants every
device, not just the trackpad, and new group membership does not affect already
running processes. It is the simplest option for a lab machine.

### Option B — udev `uaccess` rule (per-device, per-seat user)

`uaccess` is the logind mechanism: udev adds an ACL for the user who owns the
active seat, and removes it on logout. The default rules only apply it to
joysticks, not touchpads. A local rule placed **before** `73-seat-late.rules`
so the tag is set before the ACL is applied:

```udev
# /etc/udev/rules.d/69-ss-clone-trackpad.rules
SUBSYSTEM=="input", KERNEL=="event*", ENV{ID_INPUT_TOUCHPAD}=="1", TAG+="uaccess"
```

```sh
sudo udevadm control --reload-rules
sudo udevadm trigger --subsystem-match=input
getfacl /dev/input/event10      # expect user:tai:rw-
```

The rule matches by the udev property, not by node number or model. It does not
grant the keyboard, which is the right default while gestures are the goal.

Avoid: `MODE="0666"`, running the client as root, or a broad rule matching all
of `SUBSYSTEM=="input"` — each exposes the keyboard.

## 4. `EVIOCGRAB` semantics

From `include/uapi/linux/input.h` (VERIFIED against Linux master, 2026-11-06):

```c
#define EVIOCGRAB   _IOW('E', 0x90, int)   /* Grab/Release device */
#define EVIOCREVOKE _IOW('E', 0x91, int)   /* Revoke device access */
```

`ioctl(fd, EVIOCGRAB, 1)` grabs, `0` releases. What the kernel does
(`drivers/input/evdev.c`, same date):

- **Only one grabber per device.** `evdev_grab()` returns `-EBUSY` if
  `evdev->grab` is already set. Another client (including a future helper) cannot
  take it while you hold it.
- **All events go to the grab client and nobody else.** `evdev_events()` checks
  `evdev->grab` first: if set it calls `evdev_pass_values()` only for that
  client, and does **not** walk the normal client list. The compositor's
  libinput fd therefore receives nothing — the pointer/keyboard simply stops
  moving as far as the desktop is concerned. It is not told the device was
  removed; it just goes silent.
- **Closing the fd releases the grab.** `evdev_release()` calls
  `evdev_ungrab()`. A crash, `SIGKILL`, or normal process exit therefore returns
  the device to the compositor. A live but wedged process keeps it.
- **`EVIOCREVOKE` is the forced eviction.** It marks the client revoked,
  releases its grab, flushes the device and wakes readers with `EPOLLHUP`; reads
  then fail with `ENODEV`. This is how a privileged supervisor could take the
  device back from a stuck client. It is not something the client can do to
  itself as a recovery path (it would need a second privileged process or a
  logind action).
- **Release must re-synchronise state.** The compositor missed every event while
  grabbed. Keys held at release can look stuck to it; the client must send
  key-ups to the remote and release modifiers before dropping the grab. Do not
  release in a signal handler while a read/write is half-finished without a
  clear owner.

## 5. What to grab, and the escape-chord problem

Two different grabs, with different risk:

- **Trackpad only** is the safe first step for gestures. The keyboard stays with
  the compositor, so local shortcuts and the window manager still work. The cost
  is that the local pointer freezes while grabbed; the remote pointer keeps
  moving because the client forwards deltas. Release when the remote window
  loses focus, on an explicit UI toggle, and on a timeout.
- **Keyboard too** is what makes local shortcuts reach the remote, as the macOS
  keyboard grab does. On Wayland there is no `CGEventTap` equivalent: to see the
  keys you must take the device, and then the compositor cannot see the chord
  that would give it back.

The escape chord is therefore a client-side responsibility, and it has failure
modes:

1. The client must recognise the release chord **from its own evdev stream**
   (for example hold Left-Ctrl + Left-Alt + Esc for 350 ms). The compositor will
   never see it, so it cannot be a compositor shortcut.
2. If the client is running but wedged, the chord is not processed and the
   desktop has no keyboard while the grab lasts. `SIGKILL`/crash recovers
   because the fd closes; a hung process does not. A watchdog thread that calls
   `EVIOCGRAB 0` and closes the fd after a missed heartbeat is required, and it
   must not depend on the wedged input thread.
3. Do not rely on `Ctrl+Alt+F1`/VT switching or Magic SysRq as the escape hatch
   while a keyboard grab is held: EVDEV grabs are at the input-handler level, so
   the VT keyboard handler may not see the combination (HYPOTHESIS — test
   before relying on it). The Power button and lid switch are separate devices
   on this host (`event0`–`event2`), but a keyboard's own power key is not.
4. Make the grab explicit and visible: a "capture input" action, an on-screen
   indicator, and release on window blur, app exit and panic. Never grab at
   startup.
5. If a chord is also forwarded while held (the macOS `Cmd+Shift+V` lesson),
   the release path must consume it, not send it to the remote.

For the first implementation, grab only the trackpad and leave the keyboard
alone. That gets swipes without making the machine unusable if the gesture
thread dies; keyboard grab can follow with the watchdog and the explicit
release chord.

## 6. Next steps for the implementation

1. Replace the current "first MT-capable event node" scan with udev
   `ID_INPUT_TOUCHPAD=1` discovery, subscribed to add/remove (or at least
   re-scanned on hotplug); never hardcode `eventN`.
2. Add a startup permission check and a clear error instead of the generic
   "no multitouch device found" when the cause is EACCES; document the `input`
   group and the `uaccess` rule in the error.
3. Keep the reader ungrabbed until there is an explicit capture action; then
   add `EVIOCGRAB` on the trackpad only, with the watchdog and release paths in
   §5. The existing `gesture-input` MT-B parser and recogniser can be reused.
4. Do not open the keyboard node in the first version. A keyboard grab needs the
   release-chord design in §5 and a guaranteed release on panic, exit and a
   missed heartbeat; only then does it belong in `ss-gui`.
5. Log only device identity and frame counts, never raw keystrokes; the fd sees
   passwords.
6. Measure: gesture recogniser output against the macOS harness for the same
   physical gesture, then latency from the trackpad event to the RFB send.
   `--evdev` could not be run on this host because the user lacks access, so
   even the existing ungrabbed path is unverified end to end.

The smoke test and packaging in this branch do not depend on any of this.
