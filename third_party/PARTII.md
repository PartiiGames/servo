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
- `:has()` invalidation (`invalidation/element/relative_selector.rs`, `AncestorEarlierSibling`):
  an ancestor whose earlier siblings aren't on the sibling search path ends the walk over those
  siblings only (`break`), not the whole upward walk (`return`), so `:has(~ div .test)` still
  finds its anchor when `.test` sits in a later sibling of a child of `div` (WPT
  has-in-sibling-position / has-in-adjacent-position).

`webrender/` is crates.io `webrender 0.70.0` (the version Servo 0.7.0 locks; MPL-2.0, `webrender/LICENSE`),
patched in the same way. The vendoring commit copies the published crate unchanged (without the registry's
`.cargo_vcs_info.json`, `Cargo.toml.orig` and `Cargo.lock`), so `git diff <that commit> -- third_party/webrender`
shows every Partii patch. `webrender_api` and `webrender_build` stay on crates.io.

Patches on top of webrender 0.70.0:
- `PARTII_WR_BACKDROP_LOCAL` (on by default; `=0` is upstream): a backdrop-filter sub-graph
  splits and is a dependency of only the picture-cache tiles it is drawn on, so backdrops on
  different tiles render in the same passes instead of one chain after another
  (src/surface.rs, src/invalidation/cached_surface.rs, src/tile_cache/mod.rs, src/picture.rs,
  src/prepare.rs).
- `PARTII_WR_BACKDROP_UNDERLAY` (on by default; `=0` is upstream): a backdrop-filter over a
  compositor-surface candidate (a WebGL canvas) no longer makes its slice atomic; an underlay copy
  of the surface, clipped to the backdrop plus the blur's reach, is drawn under it when the surface
  is promoted (src/scene_building.rs, src/tile_cache/slice_builder.rs, src/tile_cache/mod.rs,
  src/prim_store/image.rs).
