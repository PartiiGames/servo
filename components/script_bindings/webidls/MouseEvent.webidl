/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

// https://w3c.github.io/pointerevents/#dom-mouseevent
// Partii: coordinates are double (CSSOM View); MouseEvent::coordinate floors them except on PointerEvents.
[Exposed=Window]
interface MouseEvent : UIEvent {
    [Throws] constructor(DOMString typeArg, optional MouseEventInit mouseEventInitDict = {});
    readonly    attribute double         screenX;
    readonly    attribute double         screenY;
    readonly    attribute double         clientX;
    readonly    attribute double         clientY;
    readonly    attribute double         pageX;
    readonly    attribute double         pageY;
    readonly    attribute double         x;
    readonly    attribute double         y;
    readonly    attribute double         offsetX;
    readonly    attribute double         offsetY;
    readonly    attribute boolean        ctrlKey;
    readonly    attribute boolean        shiftKey;
    readonly    attribute boolean        altKey;
    readonly    attribute boolean        metaKey;
    readonly    attribute short          button;
    readonly    attribute EventTarget?   relatedTarget;
    // Introduced in DOM Level 3
    readonly    attribute unsigned short buttons;
    boolean getModifierState (DOMString keyArg);
    // https://w3c.github.io/pointerlock/#extensions-to-the-mouseevent-interface
    readonly    attribute double         movementX;
    readonly    attribute double         movementY;
};

// https://w3c.github.io/pointerevents/#dom-mouseeventinit
dictionary MouseEventInit : EventModifierInit {
    double         screenX = 0;
    double         screenY = 0;
    double         clientX = 0;
    double         clientY = 0;
    short          button = 0;
    unsigned short buttons = 0;
    EventTarget?   relatedTarget = null;
    // https://w3c.github.io/pointerlock/#extensions-to-the-mouseeventinit-dictionary
    double         movementX = 0;
    double         movementY = 0;
};

// https://w3c.github.io/pointerevents/#dom-mouseevent-initmouseevent
partial interface MouseEvent {
    // Deprecated in DOM Level 3
    undefined initMouseEvent (DOMString typeArg, boolean bubblesArg, boolean cancelableArg,
                         Window? viewArg, long detailArg,
                         long screenXArg, long screenYArg,
                         long clientXArg, long clientYArg,
                         boolean ctrlKeyArg, boolean altKeyArg,
                         boolean shiftKeyArg, boolean metaKeyArg,
                         short buttonArg, EventTarget? relatedTargetArg);
};
