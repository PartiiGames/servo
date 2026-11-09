/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use dom_struct::dom_struct;
use html5ever::{LocalName, Prefix, local_name};
use js::context::JSContext;
use stylo_dom::ElementState;

use crate::dom::bindings::codegen::Bindings::SVGComponentTransferFunctionElementBinding::SVGComponentTransferFunctionElementMethods;
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::root::{DomRoot, MutNullableDom};
use crate::dom::document::Document;
use crate::dom::svg::svganimatedenumeration::{SVGAnimatedEnumeration, SVGEnumeration};
use crate::dom::svg::svganimatednumber::SVGAnimatedNumber;
use crate::dom::svg::svgelement::SVGElement;
use crate::dom::svg::svgreflectedattribute::AttributePart;

/// <https://drafts.fxtf.org/filter-effects/#InterfaceSVGComponentTransferFunctionElement>
#[dom_struct]
pub(crate) struct SVGComponentTransferFunctionElement {
    svgelement: SVGElement,
    type_: MutNullableDom<SVGAnimatedEnumeration>,
    slope: MutNullableDom<SVGAnimatedNumber>,
    intercept: MutNullableDom<SVGAnimatedNumber>,
    amplitude: MutNullableDom<SVGAnimatedNumber>,
    exponent: MutNullableDom<SVGAnimatedNumber>,
    offset: MutNullableDom<SVGAnimatedNumber>,
}

impl SVGComponentTransferFunctionElement {
    pub(crate) fn new_inherited(
        local_name: LocalName,
        prefix: Option<Prefix>,
        document: &Document,
    ) -> SVGComponentTransferFunctionElement {
        SVGComponentTransferFunctionElement {
            svgelement: SVGElement::new_inherited_with_state(
                ElementState::empty(),
                local_name,
                prefix,
                document,
            ),
            type_: Default::default(),
            slope: Default::default(),
            intercept: Default::default(),
            amplitude: Default::default(),
            exponent: Default::default(),
            offset: Default::default(),
        }
    }
}

impl SVGComponentTransferFunctionElementMethods<crate::DomTypeHolder>
    for SVGComponentTransferFunctionElement
{
    /// <https://drafts.fxtf.org/filter-effects/#dom-svgcomponenttransferfunctionelement-type>
    fn Type(&self, cx: &mut JSContext) -> DomRoot<SVGAnimatedEnumeration> {
        self.type_.or_init(|| {
            SVGAnimatedEnumeration::new(
                cx,
                self.upcast(),
                local_name!("type"),
                SVGEnumeration::ComponentTransferType,
                1,
            )
        })
    }

    /// <https://drafts.fxtf.org/filter-effects/#dom-svgcomponenttransferfunctionelement-slope>
    fn Slope(&self, cx: &mut JSContext) -> DomRoot<SVGAnimatedNumber> {
        self.slope.or_init(|| {
            SVGAnimatedNumber::new(
                cx,
                self.upcast(),
                local_name!("slope"),
                AttributePart::Whole,
                1.,
            )
        })
    }

    /// <https://drafts.fxtf.org/filter-effects/#dom-svgcomponenttransferfunctionelement-intercept>
    fn Intercept(&self, cx: &mut JSContext) -> DomRoot<SVGAnimatedNumber> {
        self.intercept.or_init(|| {
            SVGAnimatedNumber::new(
                cx,
                self.upcast(),
                local_name!("intercept"),
                AttributePart::Whole,
                0.,
            )
        })
    }

    /// <https://drafts.fxtf.org/filter-effects/#dom-svgcomponenttransferfunctionelement-amplitude>
    fn Amplitude(&self, cx: &mut JSContext) -> DomRoot<SVGAnimatedNumber> {
        self.amplitude.or_init(|| {
            SVGAnimatedNumber::new(
                cx,
                self.upcast(),
                local_name!("amplitude"),
                AttributePart::Whole,
                1.,
            )
        })
    }

    /// <https://drafts.fxtf.org/filter-effects/#dom-svgcomponenttransferfunctionelement-exponent>
    fn Exponent(&self, cx: &mut JSContext) -> DomRoot<SVGAnimatedNumber> {
        self.exponent.or_init(|| {
            SVGAnimatedNumber::new(
                cx,
                self.upcast(),
                local_name!("exponent"),
                AttributePart::Whole,
                1.,
            )
        })
    }

    /// <https://drafts.fxtf.org/filter-effects/#dom-svgcomponenttransferfunctionelement-offset>
    fn Offset(&self, cx: &mut JSContext) -> DomRoot<SVGAnimatedNumber> {
        self.offset.or_init(|| {
            SVGAnimatedNumber::new(
                cx,
                self.upcast(),
                local_name!("offset"),
                AttributePart::Whole,
                0.,
            )
        })
    }
}
