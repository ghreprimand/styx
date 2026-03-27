// Edge detection is handled inline by Injector::inject_mouse_motion(),
// which returns true when the cursor hits the right edge of the rightmost
// display. No separate CGEventTap is needed since we know the cursor
// position from injection.
