/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

// https://drafts.fxtf.org/filter-effects/#InterfaceSVGFEDiffuseLightingElement
[Exposed=Window]
interface SVGFEDiffuseLightingElement : SVGElement {
  readonly attribute SVGAnimatedString in1;
  readonly attribute SVGAnimatedNumber surfaceScale;
  readonly attribute SVGAnimatedNumber diffuseConstant;
  readonly attribute SVGAnimatedNumber kernelUnitLengthX;
  readonly attribute SVGAnimatedNumber kernelUnitLengthY;
};

SVGFEDiffuseLightingElement includes SVGFilterPrimitiveStandardAttributes;
