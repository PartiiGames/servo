/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

// https://w3c.github.io/uievents/#dictdef-eventmodifierinit
dictionary EventModifierInit : UIEventInit {
    boolean ctrlKey = false;
    boolean shiftKey = false;
    boolean altKey = false;
    boolean metaKey = false;
    boolean modifierAltGraph = false;
    boolean modifierCapsLock = false;
    boolean modifierFn = false;
    boolean modifierFnLock = false;
    boolean modifierHyper = false;
    boolean modifierNumLock = false;
    boolean modifierScrollLock = false;
    boolean modifierSuper = false;
    boolean modifierSymbol = false;
    boolean modifierSymbolLock = false;
};
