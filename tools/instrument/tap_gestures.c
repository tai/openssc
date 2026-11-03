// Global listen-only tap for gesture/scroll events.
// Build: clang -framework CoreGraphics -framework CoreFoundation tap_gestures.c -o tap_gestures
// Run:   sudo ./tap_gestures 600 > gestures.log
#include <CoreGraphics/CoreGraphics.h>
#include <CoreFoundation/CoreFoundation.h>
#include <stdio.h>
#include <stdlib.h>
#include <time.h>

static const CGEventField INT_FIELDS[] = {
    110, 111, 115, 117, 123, 132, 133, 134, 135, 136, 138, 143, 144,
    11, 12, 13, 93, 94, 95, 96, 97, 98, 99, 100, 101, 102,
};
static const CGEventField DBL_FIELDS[] = {
    113, 114, 116, 118, 119, 120, 124, 125, 126, 129, 130, 131,
    139, 140, 142, 147, 148,
};

static CGEventRef callback(CGEventTapProxy proxy, CGEventType type, CGEventRef event, void *refcon) {
    (void)proxy;
    (void)refcon;
    CGPoint loc = CGEventGetLocation(event);
    double t = CFAbsoluteTimeGetCurrent();
    printf("EV type=%d t=%.3f loc=%.1f,%.1f flags=%llx", (int)type, t, loc.x, loc.y,
           (unsigned long long)CGEventGetFlags(event));
    for (size_t i = 0; i < sizeof(INT_FIELDS) / sizeof(INT_FIELDS[0]); i++) {
        int64_t v = CGEventGetIntegerValueField(event, INT_FIELDS[i]);
        if (v != 0) printf(" i%u=%lld", (unsigned)INT_FIELDS[i], (long long)v);
    }
    for (size_t i = 0; i < sizeof(DBL_FIELDS) / sizeof(DBL_FIELDS[0]); i++) {
        double v = CGEventGetDoubleValueField(event, DBL_FIELDS[i]);
        if (v != 0.0) printf(" d%u=%.6f", (unsigned)DBL_FIELDS[i], v);
    }
    printf("\n");
    fflush(stdout);
    return event;
}

int main(int argc, char **argv) {
    double seconds = argc > 1 ? atof(argv[1]) : 600.0;
    // kCGEventGesture is a private enum value (29).
    CGEventMask mask = (1ULL << 29) | (1ULL << kCGEventScrollWheel);
    CFMachPortRef tap = CGEventTapCreate(kCGSessionEventTap, kCGHeadInsertEventTap,
                                         kCGEventTapOptionListenOnly, mask, callback, NULL);
    if (!tap) {
        fprintf(stderr, "tap failed: grant Accessibility to the parent terminal/root, or run as root\n");
        return 1;
    }
    CFRunLoopSourceRef src = CFMachPortCreateRunLoopSource(NULL, tap, 0);
    CFRunLoopAddSource(CFRunLoopGetCurrent(), src, kCFRunLoopCommonModes);
    CGEventTapEnable(tap, true);
    fprintf(stderr, "gesture tap running for %.0f s\n", seconds);
    fflush(stderr);
    CFAbsoluteTime end = CFAbsoluteTimeGetCurrent() + seconds;
    while (CFAbsoluteTimeGetCurrent() < end) {
        CFRunLoopRunInMode(kCFRunLoopDefaultMode, 0.5, false);
    }
    CFRelease(src);
    CFRelease(tap);
    return 0;
}
