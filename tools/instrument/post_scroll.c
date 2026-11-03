// Post a synthetic scroll wheel event into the frontmost window owned by a
// given process name. Used to drive ss-gui without a physical trackpad.
// Build: clang -framework CoreGraphics -framework CoreFoundation -framework AppKit post_scroll.c -o post_scroll
// Run:   ./post_scroll ss-gui 5
#include <CoreFoundation/CoreFoundation.h>
#include <CoreGraphics/CoreGraphics.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

int main(int argc, char **argv) {
    if (argc < 3) {
        fprintf(stderr, "usage: %s <owner-name> <lines> [dx]\n", argv[0]);
        return 2;
    }
    const char *owner = argv[1];
    int lines = atoi(argv[2]);
    int dx = argc > 3 ? atoi(argv[3]) : 0;

    CFArrayRef windows = CGWindowListCopyWindowInfo(
        kCGWindowListOptionOnScreenOnly | kCGWindowListExcludeDesktopElements, kCGNullWindowID);
    if (!windows) {
        fprintf(stderr, "no window list\n");
        return 1;
    }
    int found = 0;
    CGPoint center = CGPointZero;
    CGRect bounds = CGRectZero;
    for (CFIndex i = 0; i < CFArrayGetCount(windows); i++) {
        CFDictionaryRef w = CFArrayGetValueAtIndex(windows, i);
        CFStringRef name = CFDictionaryGetValue(w, kCGWindowOwnerName);
        char buf[256] = {0};
        if (name) CFStringGetCString(name, buf, sizeof(buf), kCFStringEncodingUTF8);
        if (strcmp(buf, owner) != 0) continue;
        CFDictionaryRef rectDict = CFDictionaryGetValue(w, kCGWindowBounds);
        if (!rectDict) continue;
        CGRect r;
        if (!CGRectMakeWithDictionaryRepresentation(rectDict, &r)) continue;
        // Skip menu-bar extras / tiny windows.
        if (r.size.width < 200 || r.size.height < 200) continue;
        bounds = r;
        center = CGPointMake(CGRectGetMidX(r), CGRectGetMidY(r));
        found = 1;
        break;
    }
    CFRelease(windows);
    if (!found) {
        fprintf(stderr, "no window for %s\n", owner);
        return 1;
    }
    printf("window %.0f,%.0f %.0fx%.0f center %.0f,%.0f\n", bounds.origin.x, bounds.origin.y,
           bounds.size.width, bounds.size.height, center.x, center.y);

    CGEventRef move = CGEventCreateMouseEvent(NULL, kCGEventMouseMoved, center, kCGMouseButtonLeft);
    CGEventPost(kCGHIDEventTap, move);
    CFRelease(move);
    usleep(120000);

    CGEventRef scroll = CGEventCreateScrollWheelEvent(NULL, kCGScrollEventUnitLine, 2, lines, dx);
    CGEventPost(kCGHIDEventTap, scroll);
    CFRelease(scroll);
    return 0;
}
