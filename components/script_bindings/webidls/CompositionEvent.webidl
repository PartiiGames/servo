/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */
/*
 * The origin of this IDL file is
 * https://w3c.github.io/uievents/#idl-compositionevent
 *
 */

// https://w3c.github.io/uievents/#idl-compositionevent
[Exposed=Window, Pref="dom_composition_event_enabled"]
interface CompositionEvent : UIEvent {
  [Throws] constructor(DOMString type, optional CompositionEventInit eventInitDict = {});
  readonly attribute DOMString data;
};

// https://w3c.github.io/uievents/#idl-interface-CompositionEvent-initializers
partial interface CompositionEvent {
  // Originally introduced (and deprecated) in DOM Level 3
  undefined initCompositionEvent(DOMString typeArg, optional boolean bubblesArg = false,
                                 optional boolean cancelableArg = false, optional Window? viewArg = null,
                                 optional DOMString dataArg = "");
};

// https://w3c.github.io/uievents/#idl-compositioneventinit
dictionary CompositionEventInit : UIEventInit {
  DOMString data = "";
};

