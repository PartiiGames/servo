# Vendored crates (Partii fork)

`stylo/` and `stylo_static_prefs/` are crates.io `stylo 0.22.0` and `stylo_static_prefs 0.22.0`
(the versions Servo 0.7.0 locks), patched in via `[patch.crates-io]` in the root `Cargo.toml`.
An embedder workspace (partii-runtime) must patch them the same way.

Patches on top of 0.22.0:
- `backdrop-filter` is gated on its own pref `layout.css.backdrop-filter.enabled` (Servo pref
  `layout_css_backdrop_filter_enabled`) instead of `layout.unimplemented`.
- `text-wrap-style` and the `text-wrap` shorthand are enabled for Servo (Gecko only upstream);
  Servo's inline layout implements `text-wrap-style: balance`.
- Container query length units (cqw, cqh, cqi, cqb, cqmin, cqmax) parse for Servo too (Gecko only
  upstream); Servo's layout supplies container sizes (`query_container_size`).
