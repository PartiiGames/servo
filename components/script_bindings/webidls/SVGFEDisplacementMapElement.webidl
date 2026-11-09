/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

// https://drafts.fxtf.org/filter-effects/#InterfaceSVGFEDisplacementMapElement
[Exposed=Window]
interface SVGFEDisplacementMapElement : SVGElement {
  // Channel Selectors
  const unsigned short SVG_CHANNEL_UNKNOWN = 0;
  const unsigned short SVG_CHANNEL_R = 1;
  const unsigned short SVG_CHANNEL_G = 2;
  const unsigned short SVG_CHANNEL_B = 3;
  const unsigned short SVG_CHANNEL_A = 4;

  readonly attribute SVGAnimatedString in1;
  readonly attribute SVGAnimatedString in2;
  readonly attribute SVGAnimatedNumber scale;
  readonly attribute SVGAnimatedEnumeration xChannelSelector;
  readonly attribute SVGAnimatedEnumeration yChannelSelector;
};

SVGFEDisplacementMapElement includes SVGFilterPrimitiveStandardAttributes;
