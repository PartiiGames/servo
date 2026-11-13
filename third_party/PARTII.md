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
- `PARTII_WR_BACKDROP_HALF_RES` (on by default; `=0` is upstream): a backdrop-filter that is one
  `blur()` the blur chain would halve anyway (std deviation above `PARTII_WR_BACKDROP_BLUR_SIGMA`,
  2 device px) runs at half resolution: its capture/blur picture and the intermediate surface
  `BackdropRender` draws from get half the device pixel scale, so the resolve blit halves the
  backdrop as it copies it and two full-size targets a frame (the full-size capture with its
  scaling pass, and the full-size intermediate) become quarter-size ones
  (src/picture.rs `partii_backdrop_half_res`, src/scene_building.rs `add_backdrop_filter`).
- `PARTII_WR_FILTER_CACHE` (on by default; `=0` is upstream): a `drop-shadow()` picture (shadows
  sharing one blur radius) drawn straight onto a picture cache keeps its content and blur in the
  render task cache while its content hash (the per-primitive data tile invalidation compares)
  is unchanged; a tile dirtied by something else composites the cached result instead of
  re-running the picture and its downscale-and-blur chain (src/picture.rs,
  src/picture_composite_mode.rs, src/render_task_cache.rs, src/resource_cache.rs, src/surface.rs,
  src/tile_cache/mod.rs, src/invalidation/vert_buffer.rs, src/batch.rs).
- `PARTII_WR_INVAL_LOG=<passes>` (off by default; measurement only): logs why picture-cache tiles are
  redrawn (a summary every 2 s, and a dump of frames with at least <passes> render passes)
  (src/invalidation/, src/picture.rs, src/frame_builder.rs).
- `PARTII_WR_WARMUP` (on by default on a software GL only: llvmpipe, softpipe, swrast; `=0` off, `=1` on any
  GL): the device records each new (program, draw state: target format, raw blend/depth/colour-mask
  state, bound texture targets/formats/filters/swizzles) to `$MESA_SHADER_CACHE_DIR/partii-wr-warmup.txt`
  (else `~/.cache/partii/wr-warmup.txt`, or `PARTII_WR_WARMUP_FILE`), and the renderer replays those
  entries at start, one degenerate instance each into 4x4 scratch targets, so the software GL builds
  its draw-time shader variants before the first frame (`PARTII_WR_WARMUP_MS` caps it, default 4000)
  (src/device/partii_warm.rs, src/device/gl.rs, src/renderer/shade.rs, src/renderer/init.rs).
