Plan for `<img>` support

Most of the lower-level pipeline already exists. `blitz-dom` already has `SpecialElementData::Image`, `img[src]` triggers `load_image()`, loaded images are cached/applied back onto nodes, replaced-element sizing is implemented, and `blitz-paint` already knows how to paint raster images. So this is not a greenfield feature; it is mostly an integration and behavior-completion task.

Recommended scope for v1:
1. Get static `<img src="...">` working end-to-end in this app.
2. Support `src`, `width`, `height`, CSS sizing, and natural aspect ratio.
3. Support dynamic `src` changes.
4. Add `onLoad` / `onError` events.
5. Add sane broken/empty-image behavior.
6. Leave out `srcset`, `sizes`, `picture`, lazy loading, decoding hints, and `loading=` for now.

Implementation plan:
1. Verify the current bridge/runtime path with a minimal example.
   - JSX/bridge likely already creates `<img>` correctly because `__sol_createElement` is generic.
   - The first task is to confirm whether a plain `<img src>` already renders, or whether the remaining gap is just event dispatch / invalidation / example coverage.

2. Wire image lifecycle events into the app-facing event layer.
   - `blitz-dom` already emits internal `ResourceLoad` handling, but there is no obvious JS-facing `load` / `error` dispatch for images yet.
   - Add node-targeted `load` when an image resolves and `error` when fetch/decode fails.
   - Keep this specific to `<img>` first.

3. Make `src` mutation semantics explicit and robust.
   - `mutator.rs` already reloads on `img[src]` changes.
   - Audit clearing/removing `src` as well:
     - empty `src`
     - removed `src`
     - changing from good URL to bad URL
     - changing between two valid URLs
   - Ensure stale image data is cleared or replaced consistently.

4. Define broken/empty-image behavior.
   - Current layout/paint path uses `ImageData::None`, but user-facing behavior needs to be explicit.
   - For v1:
     - no `src` or empty `src` => no rendered image
     - failed load => broken image state, still participates in layout via attrs/CSS if sized
   - Decide whether to show only alt text later; I would defer full alt-text rendering in-box to v2 unless needed immediately.

5. Add host/example coverage.
   - Add a kitchen-sink image block with:
     - one valid image
     - one broken image
     - one dynamically swapped image
   - Add a headless capture example for image rendering similar to the select capture flow.

6. Add tests in three layers.
   - Bridge/runtime:
     - setting `src` on `<img>` triggers resource load path
     - changing `src` replaces prior image state
   - Layout:
     - intrinsic size is used when no CSS size is given
     - `width` / `height` attrs override as expected
     - aspect ratio is preserved
   - Events:
     - `onLoad` fires on success
     - `onError` fires on failure

7. Only after v1, consider richer HTML image features.
   - `alt` fallback rendering
   - `srcset` / `sizes`
   - `picture`
   - decoding/lazy-loading hints
   - image dragging / selection behavior

Recommended build order:
1. Minimal `<img src>` proof with local/known-good asset.
2. Dynamic `src` mutation audit.
3. `load` / `error` event dispatch.
4. Broken-image behavior.
5. Tests and kitchen sink coverage.

The main takeaway is that the renderer already has image storage, sizing, caching, and paint. The likely missing pieces are app-level verification, lifecycle events, and edge-case semantics, not raw raster support.
