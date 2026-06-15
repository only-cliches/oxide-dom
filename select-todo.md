# Select Input Plan

Recommended scope: implement `select` as a single-select dropdown first. No `multiple`, no `size > 1`, no `optgroup`, no typeahead in v1.

## Implementation Status

✅ **Phase 1: Data Model + Bridge + Display Text Sync** - COMPLETE
- Created `SelectState` and `SelectRegistry` in `src/select.rs`
- Extended JS bridge to handle `<select>` element creation and attribute syncing
- Implemented `rebuild_select_state_from_dom` to sync DOM mutations to SelectState
- Options are kept as real DOM children while a synthetic text node displays the selected label

✅ **Phase 2: Closed-State Keyboard Selection** - COMPLETE
- Implemented `apply_select_key` function for keyboard navigation
- ArrowUp/ArrowDown: cycle through options
- Home/End: jump to first/last enabled option
- Space/Enter: open dropdown
- Emits `change` events when selection is committed

✅ **Phase 3: Tab/Focus Generalization** - COMPLETE
- Renamed `focus_adjacent_input` to `focus_adjacent_control`
- Tab and Shift+Tab now traverse both inputs and selects in DOM order
- Disabled selects are properly skipped during tab navigation

✅ **Phase 4: Form Serialization** - COMPLETE
- Updated `refresh_select_text` to sync the select element's `value` attribute
- Modified `form.rs` to handle select elements in form submission
- Selected option's value is properly serialized when forms are submitted

✅ **Phase 5: Popup Overlay and Mouse Picking** - COMPLETE
- Mouse clicking on select toggles open/closed state
- Options render as a popup overlay below the select element
- Mouse hover over options updates active state with visual feedback
- Clicking on an option selects it and closes the dropdown
- Clicking outside popup closes it without selecting
- Arrow keys when open navigate with visual hover
- Enter/Space/Escape keys work correctly when open
- Popup height and layout computed from option count

---

Why this shape:
- The current runtime already has a Rust-owned control model for `<input>`, but `select` semantics are different enough that forcing it into `InputState` would get messy fast.
- `option` is currently `display: none` in the UA CSS, which is actually useful: we can keep real `<option>` nodes in the DOM for data/form submission while rendering the closed select from a synthetic display node, similar to how `<input>` works.

## Plan

1. Introduce a `SelectState` alongside `InputState`.
   - Keep a separate `SelectRegistry` or rename the current input registry to a broader control registry.
   - Store:
     - `options: Vec<SelectOption>`
     - `selected_index: Option<usize>`
     - `disabled`
     - `name`
     - `open`
     - `active_index` for keyboard hover when open
   - `SelectOption` should include `value`, `label`, `disabled`, `selected`.

2. Register `<select>` in the JS bridge.
   - In `__ox_createElement`, detect `"select"` and create a synthetic text child for the closed-label display.
   - Keep real `<option>` children as normal DOM children.
   - Extend attribute syncing for:
     - `value`
     - `disabled`
     - `name`
   - For `<option>`, parse:
     - `value`
     - `selected`
     - `disabled`
     - text content as fallback label

3. Add a “rebuild select state from DOM subtree” helper.
   - Walk a select’s child options in tree order.
   - Resolve selected option from:
     - explicit `selected`
     - otherwise first enabled option
   - Keep the synthetic display text child in sync with the current label.
   - This should run after DOM mutations that affect a select:
     - inserting/removing option nodes
     - changing option text
     - changing `selected`/`value`/`disabled`

4. Generalize focus handling from “inputs” to “managed controls”.
   - Current tab logic in `src/instance.rs` only walks registered inputs.
   - Extend that traversal to include selects so `Tab` and `Shift+Tab` move across both inputs and selects.

5. Implement closed-select interaction first.
   - Focus on click.
   - Keyboard when closed:
     - `ArrowUp` / `ArrowDown` changes selection
     - `Home` / `End` jumps
     - `Space` / `Enter` opens
   - Emit `input` and `change` when committed selection changes.

6. Implement popup dropdown as a renderer-managed overlay.
   - Store `open + active_index` in `SelectState`.
   - Render popup options in a post-pass overlay, similar in spirit to caret/selection overlays.
   - Mouse behavior:
     - click select opens
     - click option commits selection and closes
     - click outside closes
   - Keyboard when open:
     - arrows move `active_index`
     - `Enter` commits
     - `Escape` closes
     - `Tab` commits/close, then focus moves on

7. Add event payload support for selects.
   - Extend `enrich_with_input`/input-event helpers into something control-agnostic.
   - For select events expose at least:
     - `value`
     - `selectedIndex`
   - Keep `checked`/selection fields only for controls where they make sense.

8. Add form submission support.
   - Fill the existing TODO in `vendor/blitz/packages/blitz-dom/src/form.rs`.
   - Read selected options from DOM-synced state/attrs and serialize the selected value for the select name.

9. Add UA styling and paint support.
   - Keep the box itself CSS-driven like the checkbox/radio change.
   - Add a small default chevron paint for closed selects in `form_controls.rs`.
   - Add minimal popup styling in UA CSS.

10. Tests.
   - Selection via keyboard.
   - Open/close behavior.
   - Tab traversal across input + select.
   - Programmatic `value` / `selected` sync.
   - Form serialization.
   - Last-clicked scene surface still owns keyboard routing.

## Recommended sequencing

1. Data model + bridge + display text sync.
2. Closed-state keyboard selection.
3. Tab/focus generalization.
4. Form serialization.
5. Popup overlay and mouse picking.

The main design choice is whether to ship a non-popup MVP first or go straight to popup behavior. I recommend going straight to popup behavior if we want this to feel like a real `select`; otherwise the control will be technically present but awkward to use.
