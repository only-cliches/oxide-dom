use crate::events::KeyboardEvent;
use crate::js::TickResult;
use blitz_dom::{LocalName, QualName, ns};

use super::Instance;

impl Instance {
    pub(super) fn apply_radio_navigation_key(
        &mut self,
        input_id: usize,
        event: &KeyboardEvent,
    ) -> (bool, bool, Option<usize>) {
        let direction = match event.key.as_str() {
            "ArrowLeft" | "ArrowUp" => Some(-1),
            "ArrowRight" | "ArrowDown" => Some(1),
            "Home" => Some(i32::MIN),
            "End" => Some(i32::MAX),
            _ => None,
        };
        let Some(direction) = direction else {
            return (false, false, None);
        };

        let group_name = {
            let inputs = self.js.inputs.borrow();
            let Some(state) = inputs.get(&input_id) else {
                return (false, false, None);
            };
            if !state.is_radio() || state.disabled() {
                return (false, false, None);
            }
            state.name().map(str::to_owned)
        };
        let Some(group_name) = group_name else {
            return (false, false, None);
        };

        let members = {
            let inputs = self.js.inputs.borrow();
            let doc = self.doc.borrow();
            let mut ids = Vec::new();
            doc.visit(|node_id, _node| {
                if inputs.get(&node_id).is_some_and(|state| {
                    state.is_radio()
                        && !state.disabled()
                        && state.name() == Some(group_name.as_str())
                }) {
                    ids.push(node_id);
                }
            });
            ids
        };
        if members.is_empty() {
            return (false, false, None);
        }

        let current_index = members.iter().position(|candidate| *candidate == input_id);
        let Some(current_index) = current_index else {
            return (false, false, None);
        };

        let next_index = match direction {
            d if d == i32::MIN => 0,
            d if d == i32::MAX => members.len() - 1,
            1 => (current_index + 1) % members.len(),
            -1 => (current_index + members.len() - 1) % members.len(),
            _ => current_index,
        };
        let next_id = members[next_index];

        if next_id == input_id
            && self
                .js
                .inputs
                .borrow()
                .get(&input_id)
                .is_some_and(|state| state.checked())
        {
            return (false, false, None);
        }

        {
            let mut inputs = self.js.inputs.borrow_mut();
            for radio_id in &members {
                if let Some(state) = inputs.get_mut(radio_id) {
                    state.set_checked(*radio_id == next_id);
                }
            }
        }

        {
            let mut doc = self.doc.borrow_mut();
            for radio_id in &members {
                if let Some(node) = doc.get_node_mut(*radio_id) {
                    if let Some(el) = node.element_data_mut() {
                        if let Some(slot) = el.checkbox_input_checked_mut() {
                            *slot = *radio_id == next_id;
                        }
                    }
                }
            }
        }

        (true, true, Some(next_id))
    }
}

/// Outcome of a keyboard event applied to a select.
#[derive(Default, Clone, Copy)]
struct SelectKeyOutcome {
    /// State changed and the synthetic display text needs re-syncing.
    edited: bool,
    /// `change` event should be dispatched.
    emits_change: bool,
    /// Popup overlay should be opened (after this returns).
    open_popup: bool,
    /// Popup overlay should be closed (after this returns).
    close_popup: bool,
    /// Popup active-index changed and class lists need re-syncing.
    sync_highlights: bool,
}

/// Pure state transition: given a select and a keyboard event, return what
/// the caller should do. The state mutations that don't touch the DOM (moving
/// selection, setting active_index) are applied in place; DOM-affecting steps
/// (opening/closing the popup, restyling options) are deferred to the caller.
fn select_key_outcome(
    state: &mut crate::select::SelectState,
    event: &KeyboardEvent,
) -> SelectKeyOutcome {
    let mut out = SelectKeyOutcome::default();
    let is_popup_selectable = |idx: usize| {
        state
            .options
            .get(idx)
            .is_some_and(|opt| !opt.disabled && !opt.hidden)
    };
    if !state.is_open() {
        // Alt+ArrowDown opens the dropdown (browser parity).
        if event.alt_key && matches!(event.key.as_str(), "ArrowDown" | "Down") {
            out.edited = true;
            out.open_popup = true;
            return out;
        }
        match event.key.as_str() {
            "ArrowDown" | "Down" => {
                let edited = state.move_selection(1);
                out.edited = edited;
                out.emits_change = edited;
            }
            "ArrowUp" | "Up" => {
                let edited = state.move_selection(-1);
                out.edited = edited;
                out.emits_change = edited;
            }
            "PageDown" => {
                let edited = state.step_selection(10);
                out.edited = edited;
                out.emits_change = edited;
            }
            "PageUp" => {
                let edited = state.step_selection(-10);
                out.edited = edited;
                out.emits_change = edited;
            }
            "Home" => {
                let edited = state.jump_to_extreme(false);
                out.edited = edited;
                out.emits_change = edited;
            }
            "End" => {
                let edited = state.jump_to_extreme(true);
                out.edited = edited;
                out.emits_change = edited;
            }
            " " | "Space" | "Enter" => {
                out.edited = true;
                out.open_popup = true;
            }
            _ => {
                if let Some(ch) = type_ahead_char(event) {
                    if state.type_ahead(ch, std::time::Instant::now()).is_some() {
                        out.edited = true;
                        out.emits_change = true;
                    }
                }
            }
        }
    } else {
        // Alt+ArrowUp commits the highlighted option (if any) and closes.
        // Browser parity (Chrome/Firefox <select> open state).
        if event.alt_key && matches!(event.key.as_str(), "ArrowUp" | "Up") {
            if let Some(active) = state.active_index() {
                if is_popup_selectable(active) && state.selected_index() != Some(active) {
                    state.set_selected_index(Some(active));
                    out.edited = true;
                    out.emits_change = true;
                }
            }
            out.close_popup = true;
            return out;
        }
        match event.key.as_str() {
            "ArrowDown" | "Down" => {
                let idx = state
                    .active_index()
                    .unwrap_or_else(|| state.selected_index().unwrap_or(0));
                let len = state.options.len() as i32;
                if len > 0 {
                    let mut next = ((idx as i32 + 1).rem_euclid(len)) as usize;
                    let mut attempts = 0;
                    while attempts < len as usize && !is_popup_selectable(next) {
                        next = ((next as i32 + 1).rem_euclid(len)) as usize;
                        attempts += 1;
                    }
                    if is_popup_selectable(next) {
                        state.set_active_index(Some(next));
                        out.edited = true;
                        out.sync_highlights = true;
                    }
                }
            }
            "ArrowUp" | "Up" => {
                let idx = state
                    .active_index()
                    .unwrap_or_else(|| state.selected_index().unwrap_or(0));
                let len = state.options.len() as i32;
                if len > 0 {
                    let mut next = ((idx as i32 - 1).rem_euclid(len)) as usize;
                    let mut attempts = 0;
                    while attempts < len as usize && !is_popup_selectable(next) {
                        next = ((next as i32 - 1).rem_euclid(len)) as usize;
                        attempts += 1;
                    }
                    if is_popup_selectable(next) {
                        state.set_active_index(Some(next));
                        out.edited = true;
                        out.sync_highlights = true;
                    }
                }
            }
            "Home" => {
                if let Some(first_enabled) = state.find_first_enabled() {
                    state.set_active_index(Some(first_enabled));
                    out.edited = true;
                    out.sync_highlights = true;
                }
            }
            "End" => {
                if let Some(idx) = state
                    .options
                    .iter()
                    .rposition(|opt| !opt.disabled && !opt.hidden)
                {
                    state.set_active_index(Some(idx));
                    out.edited = true;
                    out.sync_highlights = true;
                }
            }
            "Enter" | " " | "Space" => {
                if let Some(active) = state.active_index() {
                    if is_popup_selectable(active) && state.selected_index() != Some(active) {
                        state.set_selected_index(Some(active));
                        out.edited = true;
                        out.emits_change = true;
                    }
                }
                out.close_popup = true;
            }
            "Escape" => {
                out.edited = true;
                out.close_popup = true;
            }
            "Tab" => {
                if let Some(active) = state.active_index() {
                    if is_popup_selectable(active) && state.selected_index() != Some(active) {
                        state.set_selected_index(Some(active));
                        out.edited = true;
                        out.emits_change = true;
                    }
                }
                out.close_popup = true;
            }
            _ => {
                if let Some(ch) = type_ahead_char(event) {
                    if let Some(idx) = state.type_ahead(ch, std::time::Instant::now()) {
                        // Type-ahead in an open popup highlights but does not
                        // commit — committing happens on Enter or click.
                        state.set_active_index(Some(idx));
                        out.edited = true;
                        out.sync_highlights = true;
                    }
                }
            }
        }
    }
    out
}

/// Return `Some(ch)` when `event` represents a single printable character
/// that should drive select type-ahead. Modifier keys (other than Shift)
/// and multi-char `key` values (`"ArrowDown"`, `"Tab"`, …) are filtered out.
fn type_ahead_char(event: &KeyboardEvent) -> Option<char> {
    if event.ctrl_key || event.meta_key || event.alt_key {
        return None;
    }
    let mut chars = event.key.chars();
    let ch = chars.next()?;
    if chars.next().is_some() {
        return None;
    }
    if ch.is_control() {
        return None;
    }
    Some(ch)
}

impl Instance {
    /// Handle a mouse click on a checkbox or radio input.
    ///
    /// Mirrors the Space/Enter path in `apply_input_key`: toggles the
    /// `InputState`, syncs blitz-dom's `CheckboxInput`, deselects radio group
    /// siblings, and dispatches an `"input"` event.
    pub(super) fn handle_checked_input_click(&mut self, input_id: usize) -> TickResult {
        let toggle_info = {
            let mut map = self.js.inputs.borrow_mut();
            let Some(state) = map.get_mut(&input_id) else {
                return TickResult::default();
            };
            if state.is_radio() {
                if state.checked() {
                    return TickResult::default(); // already selected
                }
                let group = state.name().map(str::to_owned);
                state.set_checked(true);
                Some((true, true, group))
            } else {
                let toggled = state.toggle_checked();
                let new_checked = state.checked();
                toggled.then_some((new_checked, false, None))
            }
        };

        let Some((new_checked, is_radio, group_name)) = toggle_info else {
            return TickResult::default();
        };

        // Sync blitz-dom's CheckboxInput for this node.
        if let Some(node) = self.doc.borrow_mut().get_node_mut(input_id) {
            if let Some(el) = node.element_data_mut() {
                if let Some(slot) = el.checkbox_input_checked_mut() {
                    *slot = new_checked;
                }
            }
        }

        // For radio: deselect siblings in the same group.
        if is_radio {
            if let Some(ref group) = group_name {
                // InputState side.
                let sibling_ids: Vec<usize> = {
                    let map = self.js.inputs.borrow();
                    map.iter()
                        .filter(|(id, s)| {
                            **id != input_id && s.is_radio() && s.name() == Some(group.as_str())
                        })
                        .map(|(id, _)| *id)
                        .collect()
                };
                for sid in sibling_ids {
                    if let Some(s) = self.js.inputs.borrow_mut().get_mut(&sid) {
                        s.set_checked(false);
                    }
                    // blitz-dom side.
                    if let Some(node) = self.doc.borrow_mut().get_node_mut(sid) {
                        if let Some(el) = node.element_data_mut() {
                            if let Some(slot) = el.checkbox_input_checked_mut() {
                                *slot = false;
                            }
                        }
                    }
                }
            }
        }

        // Dispatch the "input" event.
        let snapshot = self.js.inputs.borrow().get(&input_id).map(|s| {
            (
                s.value().to_string(),
                s.checked(),
                s.selection_start(),
                s.selection_end(),
            )
        });
        if let Some((value, checked, sel_start, sel_end)) = snapshot {
            return self
                .js
                .dispatch_input_event(input_id, &value, checked, sel_start, sel_end);
        }
        TickResult::default()
    }

    /// Apply a keyboard event to a focused `<select>`. Mutates state, drives
    /// the popup overlay open/closed, and reports back whether the synthetic
    /// label and `change` event need to fire.
    pub(super) fn apply_select_key(
        &mut self,
        select_id: usize,
        event: &KeyboardEvent,
    ) -> (bool, bool) {
        let outcome = {
            let mut map = self.js.selects.borrow_mut();
            let Some(state) = map.get_mut(&select_id) else {
                return (false, false);
            };
            if state.disabled() {
                return (false, false);
            }
            select_key_outcome(state, event)
        };
        if outcome.open_popup {
            self.set_select_open(select_id, true);
        }
        if outcome.close_popup {
            self.set_select_open(select_id, false);
        }
        if outcome.sync_highlights {
            self.sync_select_popup_highlights(select_id);
        }
        if outcome.edited || outcome.open_popup || outcome.close_popup || outcome.sync_highlights {
            self.needs_paint = true;
        }
        (outcome.edited, outcome.emits_change)
    }

    /// Handle a mouse click on a select element: toggle open state.
    pub(super) fn handle_select_click(&mut self, select_id: usize) -> TickResult {
        let was_open = self
            .js
            .selects
            .borrow()
            .get(&select_id)
            .map(|s| s.is_open())
            .unwrap_or(false);
        self.set_select_open(select_id, !was_open);
        self.refresh_select_text(select_id);

        TickResult {
            needs_paint: true,
            jobs_pending: false,
        }
    }

    /// Open or close `select_id`'s dropdown. Keeps `SelectState.open` and the
    /// DOM popup overlay in sync — never call `state.set_open` directly from
    /// outside the select state itself.
    pub(super) fn set_select_open(&mut self, select_id: usize, open: bool) {
        let was_open = {
            let mut map = self.js.selects.borrow_mut();
            let Some(state) = map.get_mut(&select_id) else {
                return;
            };
            let was = state.is_open();
            if open && !was {
                let active_index = state
                    .selected_index()
                    .filter(|&idx| {
                        state
                            .options
                            .get(idx)
                            .is_some_and(|opt| !opt.disabled && !opt.hidden)
                    })
                    .or_else(|| state.find_first_enabled());
                state.set_active_index(active_index);
            }
            state.set_open(open);
            was
        };
        if open && !was_open {
            self.mount_select_popup(select_id);
        } else if !open && was_open {
            self.unmount_select_popup(select_id);
        }
        self.needs_paint = true;
    }

    /// Build the popup `<div>` for `select_id` and append it as the last child
    /// of the select. Stores the popup root and per-option node ids on the
    /// `SelectState` so mouse hit-tests can map a hovered node back to an
    /// option index.
    fn mount_select_popup(&mut self, select_id: usize) {
        // Snapshot what the popup needs without holding the selects borrow
        // while we mutate the DOM. `hidden` options keep their slot in the
        // snapshot so indices line up with `SelectState::options`.
        let snapshot: Option<(
            Vec<(String, bool, bool)>,
            Option<usize>,
            Option<usize>,
            String,
        )> = self.js.selects.borrow().get(&select_id).and_then(|s| {
            let doc = self.doc.borrow();
            let select_node = doc.get_node(select_id)?;
            let layout = select_node.final_layout;
            let popup_left = -(layout.border.left + layout.padding.left);
            let popup_top =
                layout.content_box_height() + layout.padding.bottom + layout.border.bottom;
            let popup_width = layout.size.width;
            let popup_style =
                format!("left: {popup_left}px; top: {popup_top}px; width: {popup_width}px;");
            (
                s.options
                    .iter()
                    .map(|o| (o.label.clone(), o.disabled, o.hidden))
                    .collect(),
                s.selected_index(),
                s.active_index(),
                popup_style,
            )
                .into()
        });
        let Some((entries, selected_idx, active_idx, popup_style)) = snapshot else {
            return;
        };

        let mut doc = self.doc.borrow_mut();
        let popup_id = doc.mutate().create_element(
            QualName::new(None, ns!(html), LocalName::from("div")),
            vec![],
        );
        doc.mutate().set_attribute(
            popup_id,
            QualName::new(None, ns!(), LocalName::from("class")),
            crate::select::POPUP_CLASS,
        );
        doc.mutate().set_attribute(
            popup_id,
            QualName::new(None, ns!(), LocalName::from("style")),
            &popup_style,
        );

        let mut option_ids: Vec<Option<usize>> = Vec::with_capacity(entries.len());
        for (i, (label, disabled, hidden)) in entries.iter().enumerate() {
            if *hidden {
                option_ids.push(None);
                continue;
            }
            let opt_id = doc.mutate().create_element(
                QualName::new(None, ns!(html), LocalName::from("div")),
                vec![],
            );
            let mut classes = String::from(crate::select::POPUP_OPTION_CLASS);
            if *disabled {
                classes.push(' ');
                classes.push_str(crate::select::POPUP_OPTION_DISABLED_CLASS);
            }
            if Some(i) == selected_idx {
                classes.push(' ');
                classes.push_str(crate::select::POPUP_OPTION_SELECTED_CLASS);
            }
            if Some(i) == active_idx {
                classes.push(' ');
                classes.push_str(crate::select::POPUP_OPTION_ACTIVE_CLASS);
            }
            doc.mutate().set_attribute(
                opt_id,
                QualName::new(None, ns!(), LocalName::from("class")),
                &classes,
            );
            let text_id = doc.create_text_node(label);
            doc.mutate().append_children(opt_id, &[text_id]);
            doc.mutate().append_children(popup_id, &[opt_id]);
            option_ids.push(Some(opt_id));
        }

        doc.mutate().append_children(select_id, &[popup_id]);
        drop(doc);

        if let Some(state) = self.js.selects.borrow_mut().get_mut(&select_id) {
            state.popup_root_id = Some(popup_id);
            state.option_node_ids = option_ids;
        }
    }

    /// Remove the popup overlay rooted at the recorded popup id and clear the
    /// stored ids on the `SelectState`.
    fn unmount_select_popup(&mut self, select_id: usize) {
        let popup_id = {
            let mut map = self.js.selects.borrow_mut();
            let Some(state) = map.get_mut(&select_id) else {
                return;
            };
            state.option_node_ids.clear();
            state.popup_root_id.take()
        };
        if let Some(popup_id) = popup_id {
            self.doc
                .borrow_mut()
                .mutate()
                .remove_and_drop_node(popup_id);
        }
    }

    /// Rewrite the popup option class lists so the active/selected highlights
    /// match the current `SelectState`. Called after keyboard or mouse
    /// navigation while the popup is open.
    pub(super) fn sync_select_popup_highlights(&mut self, select_id: usize) {
        let snapshot = self.js.selects.borrow().get(&select_id).map(|s| {
            (
                s.option_node_ids.clone(),
                s.options.iter().map(|o| o.disabled).collect::<Vec<bool>>(),
                s.selected_index(),
                s.active_index(),
            )
        });
        let Some((option_ids, disabled, selected_idx, active_idx)) = snapshot else {
            return;
        };
        let mut doc = self.doc.borrow_mut();
        for (i, opt_id) in option_ids.iter().enumerate() {
            let Some(opt_id) = opt_id else { continue };
            let mut classes = String::from(crate::select::POPUP_OPTION_CLASS);
            if disabled.get(i).copied().unwrap_or(false) {
                classes.push(' ');
                classes.push_str(crate::select::POPUP_OPTION_DISABLED_CLASS);
            }
            if Some(i) == selected_idx {
                classes.push(' ');
                classes.push_str(crate::select::POPUP_OPTION_SELECTED_CLASS);
            }
            if Some(i) == active_idx {
                classes.push(' ');
                classes.push_str(crate::select::POPUP_OPTION_ACTIVE_CLASS);
            }
            doc.mutate().set_attribute(
                *opt_id,
                QualName::new(None, ns!(), LocalName::from("class")),
                &classes,
            );
        }
    }

    /// Map a hovered node back to the popup option it belongs to, walking up
    /// the parent chain until we hit a popup option whose id was recorded by
    /// `mount_select_popup`. Returns `(select_id, option_index)`.
    pub(super) fn popup_option_for_hit(&self, hit_id: usize) -> Option<(usize, usize)> {
        let doc = self.doc.borrow();
        let selects = self.js.selects.borrow();
        let mut current = Some(hit_id);
        while let Some(id) = current {
            for (sel_id, state) in selects.iter() {
                if let Some(pos) = state
                    .option_node_ids
                    .iter()
                    .position(|opt| *opt == Some(id))
                {
                    return Some((*sel_id, pos));
                }
            }
            current = doc.get_node(id).and_then(|n| n.parent);
        }
        None
    }

    /// Returns the id of the select that owns `hit_id`, treating both the
    /// select element itself and the open popup overlay as belonging to it.
    pub(super) fn select_owning_hit(&self, hit_id: usize) -> Option<usize> {
        let doc = self.doc.borrow();
        let selects = self.js.selects.borrow();
        let mut current = Some(hit_id);
        while let Some(id) = current {
            if selects.contains_key(&id) {
                return Some(id);
            }
            for (sel_id, state) in selects.iter() {
                if state.popup_root_id == Some(id) {
                    return Some(*sel_id);
                }
            }
            current = doc.get_node(id).and_then(|n| n.parent);
        }
        None
    }

    /// Compute a new range value from a document-space x coordinate and apply
    /// it to the `InputState`. Returns `Some(TickResult)` if the value changed
    /// and an `"input"` event was dispatched, `None` if the node is not a
    /// range input or the value didn't change.
    pub(super) fn update_range_from_x(
        &mut self,
        input_id: usize,
        doc_x: f32,
    ) -> Option<TickResult> {
        // Compute the fraction from the element's absolute position.
        let (abs_x, content_h, pad_left, pad_right, size_w) = {
            let doc = self.doc.borrow();
            let node = doc.get_node(input_id)?;
            let l = &node.final_layout;
            let abs = node.absolute_position(0.0, 0.0);
            let content_h =
                l.size.height - l.padding.top - l.padding.bottom - l.border.top - l.border.bottom;
            (
                abs.x,
                content_h,
                l.padding.left,
                l.padding.right,
                l.size.width,
            )
        };

        let content_x0 = abs_x + pad_left;
        let content_x1 = abs_x + size_w - pad_right;
        let thumb_r = (content_h / 2.0).min(8.0).max(3.0);
        let usable_x0 = content_x0 + thumb_r;
        let usable_x1 = content_x1 - thumb_r;
        let usable_w = (usable_x1 - usable_x0).max(0.0);

        let fraction = if usable_w > 0.0 {
            ((doc_x - usable_x0) / usable_w).clamp(0.0, 1.0) as f64
        } else {
            0.5
        };

        let changed = self
            .js
            .inputs
            .borrow_mut()
            .get_mut(&input_id)?
            .set_value_from_range_fraction(fraction);

        if !changed {
            return Some(TickResult::default());
        }

        self.refresh_input_text(input_id);

        let snapshot = self.js.inputs.borrow().get(&input_id).map(|s| {
            (
                s.value().to_string(),
                s.checked(),
                s.selection_start(),
                s.selection_end(),
            )
        })?;

        Some(self.js.dispatch_input_event(
            input_id,
            &snapshot.0,
            snapshot.1,
            snapshot.2,
            snapshot.3,
        ))
    }

    /// Step a `<input type="number">` value by `direction` steps and fire an
    /// `input` event. Mirrors `update_range_from_x` for spinner button clicks.
    pub(super) fn step_number_input(
        &mut self,
        node_id: usize,
        direction: i8,
    ) -> Option<crate::js::TickResult> {
        let changed = {
            let mut inputs = self.js.inputs.borrow_mut();
            let state = inputs.get_mut(&node_id)?;
            state.step_number(direction)
        };
        if !changed {
            return Some(crate::js::TickResult::default());
        }
        self.refresh_input_text(node_id);
        let snapshot = self.js.inputs.borrow().get(&node_id).map(|s| {
            (
                s.value().to_string(),
                s.checked(),
                s.selection_start(),
                s.selection_end(),
            )
        })?;
        Some(self.js.dispatch_input_event(
            node_id,
            &snapshot.0,
            snapshot.1,
            snapshot.2,
            snapshot.3,
        ))
    }
}
