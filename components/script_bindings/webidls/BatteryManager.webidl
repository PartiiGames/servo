/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

// https://w3c.github.io/battery/

[SecureContext]
partial interface Navigator {
  [Pref="dom_battery_enabled"] Promise<BatteryManager> getBattery();
};

[SecureContext, Exposed=Window, Pref="dom_battery_enabled"]
interface BatteryManager : EventTarget {
  readonly attribute boolean charging;
  readonly attribute unrestricted double chargingTime;
  readonly attribute unrestricted double dischargingTime;
  readonly attribute double level;
  attribute EventHandler onchargingchange;
  attribute EventHandler onchargingtimechange;
  attribute EventHandler ondischargingtimechange;
  attribute EventHandler onlevelchange;
};
