# oxide-dom — CSS / browser-parity TODO

Goal: make the project feel like a regular browser for SolidJS + CSS code.

## Current state

- **Styling** flows only through `style="..."` inline attributes set via `__ox_setProperty` (src/js/bridge.rs:67). No way to ship a stylesheet at boot or mutate one at runtime.
- **`:hover` plumbing is half-wired**: `dispatch_mouse(Move)` already calls `doc.snapshot_node_and(id, node.hover())` (src/instance.rs:409–435). Blitz's style system *should* match `:hover` selectors against that flag — it just has no stylesheets to match against. Same for `:focus` (focused_node_id is tracked, no CSS to fire).
- Blitz exposes the APIs we need: `BaseDocument::add_user_agent_stylesheet`, `make_stylesheet` + `add_stylesheet_for_node`, `process_style_element`, `upsert_stylesheet_for_node`, `remove_user_agent_stylesheet` (vendored at packages/blitz-dom/src/document.rs:873–920).

Both named gaps collapse to one root cause: **no stylesheet ingress**. Fix that and `:hover` / `:focus` / `:active` / class selectors all light up via the existing Blitz pipeline.

## Gaps (ordered by user-visible impact)

### G1 — Stylesheet ingress (the big one)

Three entry points, each thin:

- [x] Rust API on `Instance`:
  - [x] `add_stylesheet(&str) -> StylesheetId`
  - [x] `replace_stylesheet(id, &str)`
  - [x] `remove_stylesheet(id)`
  - Backed by `add_user_agent_stylesheet` plus a side map `id → contents` so removal works. Mark `needs_paint`.
- [x] `InstanceConfig.stylesheets: Vec<String>` so the host can boot with CSS in place before first paint.
- [x] JS-side `<style>` element support:
  - [x] When `__ox_insertNode` parents a node whose tag is `style`, call `upsert_stylesheet_for_node(id)` on insert.
  - [x] When its text content changes via `__ox_setText`, call `upsert_stylesheet_for_node(id)`.
  - Lets Solid components emit `<style>` blocks idiomatically.
- [x] **CSS file imports** auto-register: `import "./styles.css"` runs `__ox_register_stylesheet(text)` as a side effect of module evaluation. Default export still exposes the raw text.

### G2 — `class` / `className` attribute

- [x] Normalize `className` → `class` in the `__ox_setProperty` JS dispatcher (src/js/bridge.rs:105). Today it dispatches to `__ox_setAttr` writing attr `"classname"` — wrong key; Blitz's selector matcher looks at `class`.
- [x] Support Solid's `class:foo={cond}` directive — toggle class token in the existing class list.
- [x] (Bonus) Support Solid's `style:foo={value}` directive — set a single style declaration.

### G3 — `:hover` / `:focus` / `:active` repaint

Mostly free once G1 lands, but verify:

- [x] After flipping `hover`/`unhover`, confirm `resolve(0.0)` re-evaluates pseudo-class style. (Verified by the `hover_pseudo_class_changes_computed_color` test — Blitz's `snapshot_node_and` already feeds the restyling pass.)
- [ ] Confirm focus blur/focus snapshots trigger restyle (`:focus`, `:focus-within`). (Not yet covered by a test — likely works since the dispatch already uses snapshots; add a regression test in M6.)
- [x] Add `:active`:
  - [x] Flip an "active" snapshot on `MouseEvent::Down { Left }`.
  - [x] Clear it on `Up`, or when hover leaves the active node.

### G4 — `<link rel="stylesheet">` from disk

Optional but matches browser feel.

- [ ] Resolve `href` against the component module's base path (already tracked for hot-reload).
- [ ] Read synchronously, register as a named stylesheet keyed on the resolved path so replace-on-change works.
- [ ] Skip `http(s)` schemes for now.

### G5 — CSS hot-reload via FileWatch

- [ ] Extend `FileWatch` (src/instance.rs:52) to re-read changed `.css` files and call `replace_stylesheet`.
- [ ] Pairs naturally with G4 (same key lookup).

### G6 — `@media` queries

- [ ] `Instance::set_color_scheme(Light | Dark)` — re-pass the new `ColorScheme` to Blitz and trigger restyle for `@media (prefers-color-scheme: ...)`.
- [ ] Confirm `resize()` already causes `@media (min-width: ...)` to re-evaluate; add a test.

### G7 — CSS custom properties from host

- [ ] `Instance::set_css_var(name, value)` — implement as a small UA stylesheet `:root { --foo: ...; }` that we rewrite on each call.
- [ ] Useful for theming from native code without touching the component.

## Non-goals (defer)

- `@font-face` over network, `url()` images other than local files.
- Full `getComputedStyle`.
- CSS animations / transitions (Blitz has partial support; revisit after G1–G3).
- CSSOM from JS (`document.styleSheets`, `el.style.color = ...`). Solid drives styles via attributes / class toggles, so this isn't needed for the "feels like a browser" target.

## Milestones

- [x] **M5 — Stylesheets in:** G1 + G2 + CSS-import auto-register. Rust API (`add/replace/remove_stylesheet`, `InstanceConfig.stylesheets`), `<style>` element handling, `className`→`class`, `class:foo`/`style:foo` directives, CSS-module auto-register via `import "./x.css"`. Tests in `src/instance.rs` and `src/js/mod.rs` cover class selector matching, stylesheet replace/remove, `<style>` mount + text refresh, CSS import auto-register, `:hover` and `:active` flips. `kitchen_sink` example registers a baseline `.demo-hover` stylesheet.
- [~] **M6 — Pseudo-class completeness:** `:hover` and `:active` shipped in M5 (verified by tests). Still TODO: a `:focus` regression test and `:focus-within` coverage.
- [ ] **M7 — Source-of-truth CSS files:** G4 + G5 (`<link>` + hot-reload), one example with a sibling `.css` file.
- [ ] **M8 — Theming hooks:** G6 + G7.

**M5 status:** shipped. CSS rules can be registered at boot or at runtime from Rust, components can emit `<style>` blocks, `className`/`class:foo` work, `:hover`/`:active` flip computed style automatically.
