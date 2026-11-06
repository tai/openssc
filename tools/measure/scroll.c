/*
 * scroll.c — inject a synthetic two-finger scroll into the frontmost window.
 *
 * Used by the visual-latency harness (docs/testing.md, "Camera A/B"). It is
 * deliberately not a gesture recogniser: it posts pixel scroll wheel events the
 * way a trackpad would, so a client sees an ordinary continuous scroll.
 *
 * The gesture ends with a decelerating tail (4, 3, 2, 1 px at 50 ms). macOS
 * synthesises a momentum phase only when a gesture ends at speed, and momentum
 * would make two clients receive different motion, which invalidates a
 * side-by-side comparison.
 *
 * Build:
 *   clang -O2 -o scroll scroll.c -framework ApplicationServices
 *
 * Usage:
 *   ./scroll <x> <y> <total_px> <steps> <dir>
 *     x, y      screen point to post at (must be over the client window)
 *     total_px  total distance in pixels
 *     steps     number of events (16 ms apart); total_px/steps per event
 *     dir       -1 scrolls down (content moves up), 1 scrolls up
 *
 *   ./scroll 700 500 6000 40 1     # scroll to the top of a page
 *   ./scroll 700 500 300 20 -1     # one 300 px measurement burst
 *
 * Needs Accessibility/Input Monitoring permission for the calling terminal.
 */

#include <ApplicationServices/ApplicationServices.h>
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>

static void post(double x, double y, int dy) {
    CGEventRef e = CGEventCreateScrollWheelEvent(NULL, kCGScrollEventUnitPixel, 2, dy, 0);
    if (!e) {
        fprintf(stderr, "scroll: could not create event\n");
        exit(1);
    }
    CGEventSetIntegerValueField(e, kCGScrollWheelEventIsContinuous, 1);
    CGEventPost(kCGHIDEventTap, e);
    CFRelease(e);
}

int main(int argc, char **argv) {
    if (argc != 6) {
        fprintf(stderr, "usage: %s <x> <y> <total_px> <steps> <dir +1|-1>\n", argv[0]);
        return 2;
    }
    double x = atof(argv[1]);
    double y = atof(argv[2]);
    int total = atoi(argv[3]);
    int steps = atoi(argv[4]);
    int dir = atoi(argv[5]);
    if (steps <= 0 || total == 0 || (dir != 1 && dir != -1)) {
        fprintf(stderr, "scroll: steps>0, total!=0 and dir=+1|-1 required\n");
        return 2;
    }

    /* Park the pointer so the events land in the intended window, and give the
     * focus change a moment to settle before the first event. */
    CGWarpMouseCursorPosition(CGPointMake(x, y));
    CGEventRef move = CGEventCreateMouseEvent(NULL, kCGEventMouseMoved, CGPointMake(x, y), kCGMouseButtonLeft);
    CGEventPost(kCGHIDEventTap, move);
    CFRelease(move);
    usleep(120000);

    int per = total / steps;
    if (per == 0)
        per = 1;
    for (int i = 0; i < steps; i++) {
        post(x, y, dir * per);
        usleep(16000);
    }

    int tail[4] = {4, 3, 2, 1};
    for (int i = 0; i < 4; i++) {
        post(x, y, dir * tail[i]);
        usleep(50000);
    }
    return 0;
}
