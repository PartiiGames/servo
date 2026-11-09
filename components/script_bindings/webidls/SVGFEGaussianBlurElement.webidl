/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

// https://drafts.fxtf.org/filter-effects/#InterfaceSVGFEGaussianBlurElement
[Exposed=Window]
interface SVGFEGaussianBlurElement : SVGElement {
  // Edge Mode Values
  const unsigned short SVG_EDGEMODE_UNKNOWN = 0;
  const unsigned short SVG_EDGEMODE_DUPLICATE = 1;
  const unsigned short SVG_EDGEMODE_WRAP = 2;
  const unsigned short SVG_EDGEMODE_NONE = 3;

  readonly attribute SVGAnimatedString in1;
  readonly attribute SVGAnimatedNumber stdDeviationX;
  readonly attribute SVGAnimatedNumber stdDeviationY;
  readonly attribute SVGAnimatedEnumeration edgeMode;

  undefined setStdDeviation(float stdDeviationX, float stdDeviationY);
};

SVGFEGaussianBlurElement includes SVGFilterPrimitiveStandardAttributes;
