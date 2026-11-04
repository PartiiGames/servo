/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

// https://w3c.github.io/requestidlecallback/#the-idledeadline-interface
[Exposed=Window, Pref="dom_request_idle_callback_enabled"]
interface IdleDeadline {
  DOMHighResTimeStamp timeRemaining();
  readonly attribute boolean didTimeout;
};

// https://w3c.github.io/requestidlecallback/#the-requestidlecallback-method
dictionary IdleRequestOptions {
  unsigned long timeout;
};

callback IdleRequestCallback = undefined (IdleDeadline deadline);
