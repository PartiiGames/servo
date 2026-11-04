/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Partii: a small subset of Permissions Policy, per frame, for the features a game frame must
//! declare: `fullscreen`, `pointer-lock` and `gamepad`.
//!
//! The embedding document computes a [`ContainerPolicy`] from its `<iframe>`'s `allow` (and
//! `allowfullscreen`) attribute when it navigates the frame, already intersected with its own
//! policy and with `'self'`/`'src'` resolved to origins. The policy travels in the `LoadData`; the
//! constellation keeps it per browsing context so that navigations started inside the frame keep
//! it. The nested document then enables a feature iff the allowlist matches its own origin.
//!
//! <https://w3c.github.io/webappsec-permissions-policy/>

use serde::{Deserialize, Serialize};
use servo_url::{ImmutableOrigin, ServoUrl};

/// The policy-controlled features this fork enforces.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum PolicyFeature {
    /// <https://fullscreen.spec.whatwg.org/#permissions-policy-integration>
    Fullscreen,
    /// Pointer lock has no standard policy-controlled feature; Partii frames must declare
    /// `allow="pointer-lock"`, as Chromium also accepts.
    PointerLock,
    /// <https://w3c.github.io/gamepad/#permission-policy>
    Gamepad,
}

impl PolicyFeature {
    pub const ALL: [PolicyFeature; 3] = [
        PolicyFeature::Fullscreen,
        PolicyFeature::PointerLock,
        PolicyFeature::Gamepad,
    ];

    /// The feature's name in an `allow` attribute.
    pub fn name(self) -> &'static str {
        match self {
            PolicyFeature::Fullscreen => "fullscreen",
            PolicyFeature::PointerLock => "pointer-lock",
            PolicyFeature::Gamepad => "gamepad",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|feature| feature.name().eq_ignore_ascii_case(name))
    }
}

/// <https://w3c.github.io/webappsec-permissions-policy/#allowlists>, resolved to origins.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub enum Allowlist {
    /// `*`: every origin.
    All,
    /// The listed origins (`'self'` and `'src'` already resolved).
    Origins(Vec<ImmutableOrigin>),
    /// `'none'`, or a feature the embedder itself may not use.
    #[default]
    None,
}

impl Allowlist {
    /// Does this allowlist match a document with `origin` (whose URL is `url`)? An opaque origin
    /// (a sandboxed frame without `allow-same-origin`) matches `*`, and otherwise only when it is
    /// the frame's declared `src` document (`src_url`) and that URL's origin is listed ('src'):
    /// a later navigation gets a new opaque origin, which no listed origin matches.
    pub fn matches(
        &self,
        origin: &ImmutableOrigin,
        url: &ServoUrl,
        src_url: Option<&ServoUrl>,
    ) -> bool {
        match self {
            Allowlist::All => true,
            Allowlist::None => false,
            Allowlist::Origins(origins) => {
                if origin.is_tuple() {
                    origins.contains(origin)
                } else {
                    let url_origin = url.origin();
                    src_url == Some(url) && url_origin.is_tuple() && origins.contains(&url_origin)
                }
            },
        }
    }
}

/// <https://w3c.github.io/webappsec-permissions-policy/#container-policy>, for the features in
/// [`PolicyFeature`], computed by the embedding document.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct ContainerPolicy {
    /// The URL in the `<iframe src>` when the policy was made ('src'); `None` for srcdoc and
    /// about:blank.
    pub src_url: Option<ServoUrl>,
    pub fullscreen: Allowlist,
    pub pointer_lock: Allowlist,
    pub gamepad: Allowlist,
}

impl ContainerPolicy {
    pub fn allowlist(&self, feature: PolicyFeature) -> &Allowlist {
        match feature {
            PolicyFeature::Fullscreen => &self.fullscreen,
            PolicyFeature::PointerLock => &self.pointer_lock,
            PolicyFeature::Gamepad => &self.gamepad,
        }
    }

    pub fn allowlist_mut(&mut self, feature: PolicyFeature) -> &mut Allowlist {
        match feature {
            PolicyFeature::Fullscreen => &mut self.fullscreen,
            PolicyFeature::PointerLock => &mut self.pointer_lock,
            PolicyFeature::Gamepad => &mut self.gamepad,
        }
    }
}
