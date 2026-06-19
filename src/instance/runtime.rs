use std::sync::Arc;

use blitz_dom::{BaseDocument, LocalName, Node, QualName, ns};
use blitz_traits::shell::{ColorScheme, Viewport};
use parley::{Affinity, Cursor, Selection};
use style_dom::ElementState;

use super::{Instance, StylesheetId};
use crate::events::{KeyboardEvent, MouseButton, MouseEvent};
use crate::img::ImgEvent;
use crate::js::TickResult;
use crate::renderer::{InputCaret, InputSelection};
use crate::scrollbar::{
    self, ScrollAxis, ScrollbarDrag, ScrollbarHit, ScrollbarTheme, collect_scrollbar_regions,
};
use crate::state::StateHandle;

impl Instance {
    pub fn tick(&mut self) -> TickResult {
        let mut result = self.js.tick(&self.state, 256);

        // Advance the caret blink on the focused native input, if any. The
        // toggle is driven from inside tick() rather than from a separate
        // host timer so anyone calling `tick()` regularly (every ~50–500 ms)
        // gets blinking for free; hosts that want a tighter cadence can call
        // `tick()` more often.
        let blink_flipped = self.advance_input_blink();
        if blink_flipped {
            self.needs_paint = true;
        }

        // Drain resource fetch outcomes from the NetProvider and turn them
        // plus the document's current image state into `load` / `error`
        // events on the JS side. Image fetches happen synchronously inside
        // mutator hooks; the decoded bytes only land on the node during the
        // next `BaseDocument::resolve()` (which `render()` calls), so this
        // is the first place where the `load` event can fire after a render
        // cycle has run.
        let fetch_events = self.net_provider.drain_events();
        let img_events: Vec<ImgEvent> = {
            let mut watcher = self.js.img_watcher.borrow_mut();
            watcher.ingest_fetch_events(fetch_events);
            watcher.collect_pending(&self.doc.borrow())
        };
        for ev in img_events {
            let (node_id, name) = match ev {
                ImgEvent::Load { node_id } => (node_id, "load"),
                ImgEvent::Error { node_id } => (node_id, "error"),
            };
            let r = self.js.dispatch_image_event(node_id, name);
            if r.needs_paint {
                self.needs_paint = true;
                result.needs_paint = true;
            }
            if r.jobs_pending {
                result.jobs_pending = true;
            }
        }

        let needs_paint = result.needs_paint || self.needs_paint;
        if result.needs_paint {
            self.needs_paint = true;
        }
        TickResult {
            needs_paint,
            jobs_pending: result.jobs_pending,
        }
    }

    /// Flip caret visibility on the currently-focused input if its blink
    /// interval has elapsed. Returns true if anything changed.
    fn advance_input_blink(&mut self) -> bool {
        let Some(focused) = self.focused_node_id else {
            return false;
        };
        self.js
            .inputs
            .borrow_mut()
            .get_mut(&focused)
            .is_some_and(|state| state.tick_blink(std::time::Instant::now()))
    }

    /// Resolve layout and paint the document into the output texture.
    ///
    /// Returns a reference to the [`wgpu::TextureView`] the host can composite.
    pub fn render(&mut self) -> &wgpu::TextureView {
        // Resolve CSS + layout.
        self.sync_input_render_text_before_layout();
        self.doc.borrow_mut().resolve(0.0);
        self.sync_input_render_text_before_layout();

        // Compute scrollbar geometry from the resolved layout — final_layout
        // and scroll_offset are now current. Reused by `dispatch_mouse` for
        // scrollbar hit-testing this frame.
        self.scrollbars = collect_scrollbar_regions(&self.doc.borrow());
        let input_selections = self.collect_input_selections();
        let input_carets = self.collect_input_carets();

        // Paint into the wgpu texture, layering scrollbars on top.
        {
            let mut doc = self.doc.borrow_mut();
            let masked_focus = self.mask_blitz_text_input_focus_for_paint(&mut doc);
            self.painter.paint(
                &mut doc,
                &self.scrollbars,
                &input_selections,
                &input_carets,
                self.scrollbar_theme,
                &self.texture,
            );
            self.restore_blitz_text_input_focus_after_paint(&mut doc, masked_focus);
        }
        self.needs_paint = false;

        &self.texture_view
    }

    /// Access the instance output texture backing the last paint.
    pub fn texture(&self) -> &wgpu::Texture {
        &self.texture
    }

    /// Resize the output texture and viewport.
    ///
    /// The next `render()` call will repaint at the new size.
    pub fn resize(&mut self, width: u32, height: u32) {
        self.width = width;
        self.height = height;

        // Reallocate texture.
        self.texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("solite"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_DST
                | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        self.texture_view = self
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());

        // Update blitz viewport.
        let viewport = Viewport {
            window_size: (width, height),
            hidpi_scale: 1.0,
            zoom: 1.0,
            color_scheme: ColorScheme::Light,
        };
        self.doc.borrow_mut().set_viewport(viewport);

        if self.document_scroll {
            let mut doc = self.doc.borrow_mut();
            let mut mutate = doc.mutate();
            mutate.set_style_property(self.container_id, "height", &format!("{height}px"));
            mutate.set_style_property(self.container_id, "overflow-y", "auto");
        }

        // Update painter output buffers with new dimensions.
        self.painter.resize(width, height);
        self.needs_paint = true;
    }

    fn document_coords_for_client(&self, x: f32, y: f32) -> (f32, f32) {
        if x < 0.0 || y < 0.0 || x >= self.width as f32 || y >= self.height as f32 {
            return (x, y);
        }

        let scroll = self.doc.borrow().viewport_scroll();
        (x + scroll.x as f32, y + scroll.y as f32)
    }

    fn hit_node_id(&self, x: f32, y: f32) -> Option<usize> {
        let (document_x, document_y) = self.document_coords_for_client(x, y);
        // Open popups are absolutely-positioned children of their <select> and
        // extend below it; the regular tree hit-test (`hit_visible_node_id`)
        // bails when the click is outside the select's bounding box and never
        // descends into the popup. Check popups first using their absolute
        // positions so option clicks land on the popup nodes.
        if let Some(id) = self.hit_popup_node(document_x, document_y) {
            return Some(id);
        }
        let doc = self.doc.borrow();
        let root_id = doc.root_element().id;
        hit_visible_node_id(&doc, root_id, document_x, document_y)
    }

    /// Look for a hit against any open `<select>` popup overlay, using each
    /// popup option's resolved absolute position. Returns the deepest matching
    /// option node id, falling back to the popup root if the point is inside
    /// the popup background but not over any option.
    fn hit_popup_node(&self, document_x: f32, document_y: f32) -> Option<usize> {
        let doc = self.doc.borrow();
        let selects = self.js.selects.borrow();
        for (_, state) in selects.iter() {
            let Some(popup_id) = state.popup_root_id else {
                continue;
            };
            let Some(popup_node) = doc.get_node(popup_id) else {
                continue;
            };
            let abs = popup_node.absolute_position(0.0, 0.0);
            let size = popup_node.final_layout.size;
            if document_x < abs.x
                || document_x > abs.x + size.width
                || document_y < abs.y
                || document_y > abs.y + size.height
            {
                continue;
            }
            for opt_id in state.option_node_ids.iter().flatten().copied() {
                let Some(opt_node) = doc.get_node(opt_id) else {
                    continue;
                };
                let opt_abs = opt_node.absolute_position(0.0, 0.0);
                let opt_size = opt_node.final_layout.size;
                if document_x >= opt_abs.x
                    && document_x <= opt_abs.x + opt_size.width
                    && document_y >= opt_abs.y
                    && document_y <= opt_abs.y + opt_size.height
                {
                    return Some(opt_id);
                }
            }
            return Some(popup_id);
        }
        None
    }

    /// Read the current scroll offset of a node along one axis.
    fn node_scroll(&self, node_id: usize, axis: ScrollAxis) -> f32 {
        self.doc
            .borrow()
            .get_node(node_id)
            .map(|node| match axis {
                ScrollAxis::Vertical => node.scroll_offset.y as f32,
                ScrollAxis::Horizontal => node.scroll_offset.x as f32,
            })
            .unwrap_or(0.0)
    }

    /// Move a node's scroll offset along one axis to an absolute target value.
    ///
    /// Note: `BaseDocument::scroll_node_by` uses an inverted sign convention
    /// (positive delta scrolls *back* toward the start), so we negate `delta`.
    fn set_node_scroll(&mut self, node_id: usize, axis: ScrollAxis, target: f32) {
        let current = self.node_scroll(node_id, axis);
        let delta = (target - current) as f64;
        if delta == 0.0 {
            return;
        }
        let (dx, dy) = match axis {
            ScrollAxis::Vertical => (0.0, -delta),
            ScrollAxis::Horizontal => (-delta, 0.0),
        };
        self.doc
            .borrow_mut()
            .scroll_node_by(node_id, dx, dy, |_| {});
    }

    fn set_active_to_node(&mut self, new_active_id: Option<usize>) -> bool {
        if new_active_id == self.active_node_id {
            return false;
        }
        let old_active_id = self.active_node_id;
        let mut doc = self.doc.borrow_mut();
        let old_path = existing_node_layout_ancestors(&doc, old_active_id);
        let new_path = existing_node_layout_ancestors(&doc, new_active_id);
        let same_count = old_path
            .iter()
            .zip(&new_path)
            .take_while(|(o, n)| o == n)
            .count();
        for &id in old_path.iter().skip(same_count) {
            doc.snapshot_node_and(id, |node| node.unactive());
        }
        for &id in new_path.iter().skip(same_count) {
            doc.snapshot_node_and(id, |node| node.active());
        }
        drop(doc);
        self.active_node_id = new_active_id;
        true
    }

    /// Apply hover snapshot + state updates by deferring to Blitz's canonical
    /// `BaseDocument::set_hover_to`. This also updates the document's own
    /// `hover_node_id` — selector matching for `:hover` reads it, and any
    /// rolled-our-own snapshot path that doesn't update it leaves invalidation
    /// in a state where the pseudo-class flip never reaches subsequent
    /// restyles.
    ///
    /// We always defer to Blitz: if we gated on our local `hovered_node_id`
    /// matching the new id, any disagreement between our custom hit-test
    /// (`hit_visible_node_id`) and Blitz's (`BaseDocument::hit`) would mask
    /// out the call entirely. `new_id` is provided only to keep our local
    /// tracker (used for JS `mouseover`/`mouseout` dispatch) in sync.
    fn set_hover_to_node(&mut self, x: f32, y: f32, new_id: Option<usize>) -> bool {
        let (doc_x, doc_y) = self.document_coords_for_client(x, y);
        let changed = self.doc.borrow_mut().set_hover_to(doc_x, doc_y);
        let tracker_changed = new_id != self.hovered_node_id;
        self.hovered_node_id = new_id;
        changed || tracker_changed
    }

    // ── Mouse input ──────────────────────────────────────────────────────────

    /// Forward a mouse event to the document.
    ///
    /// Hit-tests the resolved layout to find the deepest node under `(x, y)`,
    /// walks up the ancestor chain to find a registered event handler, and
    /// calls it. `MouseEvent::Move` updates hover state and dispatches
    /// transition events (`mouseover`, `mouseout`, `mouseenter`, `mouseleave`,
    /// `hover`, `hoverenter`, `hoverleave`). Only `MouseEvent::Down { button:
    /// Left }` triggers `"click"`
    /// handlers; other events are accepted for future extension.
    ///
    /// **Requires layout to be current** — call `render()` before dispatching.
    ///
    /// Returns a [`TickResult`] so the host knows whether to call `render()`
    /// and whether to tick again.
    pub fn dispatch_mouse(&mut self, x: f32, y: f32, event: MouseEvent) -> TickResult {
        // ── Scrollbar interaction takes priority over document hit-testing.
        //
        // While the user is dragging a scrollbar thumb, every Move event
        // updates that node's scroll_offset directly. Up ends the drag.
        if let Some(drag) = self.scrollbar_drag {
            match event {
                MouseEvent::Move { x, y } => {
                    let pointer = match drag.axis {
                        ScrollAxis::Vertical => y,
                        ScrollAxis::Horizontal => x,
                    };
                    let target = drag.pointer_to_scroll(pointer);
                    self.set_node_scroll(drag.node_id, drag.axis, target);
                    self.needs_paint = true;
                    return TickResult {
                        needs_paint: true,
                        jobs_pending: false,
                    };
                }
                MouseEvent::Up { .. } => {
                    self.scrollbar_drag = None;
                    return TickResult::default();
                }
                _ => {}
            }
        }

        // Range drag takes priority: every Move updates the value, Up ends the drag.
        if let Some(drag_id) = self.range_drag_id {
            match event {
                MouseEvent::Move { x, y } => {
                    let (doc_x, _) = self.document_coords_for_client(x, y);
                    if let Some(result) = self.update_range_from_x(drag_id, doc_x) {
                        self.needs_paint = true;
                        return result;
                    }
                    return TickResult::default();
                }
                MouseEvent::Up { .. } => {
                    self.range_drag_id = None;
                    return TickResult::default();
                }
                _ => {}
            }
        }

        // MouseDown on a scrollbar thumb or track: start a drag or page-step.
        if let MouseEvent::Down {
            button: MouseButton::Left,
            ..
        } = event
        {
            let (doc_x, doc_y) = self.document_coords_for_client(x, y);
            match scrollbar::hit_scrollbar(&self.scrollbars, doc_x, doc_y) {
                Some(ScrollbarHit::Thumb(region)) => {
                    self.scrollbar_drag = Some(ScrollbarDrag::from_thumb_hit(region, doc_x, doc_y));
                    return TickResult::default();
                }
                Some(ScrollbarHit::Track(region)) => {
                    // Page step: jump by ~80% of the visible track length
                    // along the scrollbar's axis.
                    let (pointer, thumb_start, track_length) = match region.axis {
                        ScrollAxis::Vertical => (doc_y, region.thumb.1, region.track.3),
                        ScrollAxis::Horizontal => (doc_x, region.thumb.0, region.track.2),
                    };
                    let direction = if pointer < thumb_start { -1.0 } else { 1.0 };
                    let step = (track_length * 0.8).max(20.0);
                    let current = self.node_scroll(region.node_id, region.axis);
                    let target = (current + direction * step).clamp(0.0, region.max_scroll);
                    self.set_node_scroll(region.node_id, region.axis, target);
                    self.needs_paint = true;
                    return TickResult {
                        needs_paint: true,
                        jobs_pending: false,
                    };
                }
                None => {}
            }
        }

        match event {
            MouseEvent::Move { x, y } => {
                let old_hover_id = self.hovered_node_id;
                let new_hover_id = self.hit_node_id(x, y);

                // While a popup is open, hovering over an option updates the
                // active-index highlight on that popup.
                if let Some(hover_id) = new_hover_id {
                    if let Some((sel_id, opt_idx)) = self.popup_option_for_hit(hover_id) {
                        let changed = {
                            let mut selects = self.js.selects.borrow_mut();
                            match selects.get_mut(&sel_id) {
                                Some(state) if state.active_index() != Some(opt_idx) => {
                                    state.set_active_index(Some(opt_idx));
                                    true
                                }
                                _ => false,
                            }
                        };
                        if changed {
                            self.sync_select_popup_highlights(sel_id);
                            self.needs_paint = true;
                        }
                    }
                }
                let hover_changed = self.set_hover_to_node(x, y, new_hover_id);

                // Browser parity: if the mouse moves off the actively-pressed
                // node while held, the :active state drops. (When it moves
                // back over before release, browsers re-engage :active; we
                // don't track that yet since we no longer know which button
                // is held — Down/Up are the source of truth here.)
                if let Some(active) = self.active_node_id
                    && new_hover_id != Some(active)
                    && self.set_active_to_node(None)
                {
                    self.needs_paint = true;
                }

                let move_target = new_hover_id;
                let mut result = TickResult::default();

                if old_hover_id != new_hover_id {
                    if let Some(old_id) = old_hover_id {
                        result = combine_tick_result(
                            result,
                            self.js.dispatch_event_with_related(
                                old_id,
                                "mouseout",
                                x,
                                y,
                                old_id,
                                new_hover_id,
                            ),
                        );
                        result = combine_tick_result(
                            result,
                            self.js.dispatch_event_at_with_target(
                                old_id,
                                "mouseleave",
                                x,
                                y,
                                old_id,
                                new_hover_id,
                            ),
                        );
                        result = combine_tick_result(
                            result,
                            self.js.dispatch_event_at_with_target(
                                old_id,
                                "hoverleave",
                                x,
                                y,
                                old_id,
                                new_hover_id,
                            ),
                        );
                    }

                    if let Some(new_id) = new_hover_id {
                        result = combine_tick_result(
                            result,
                            self.js.dispatch_event_with_related(
                                new_id,
                                "mouseover",
                                x,
                                y,
                                new_id,
                                old_hover_id,
                            ),
                        );
                        result = combine_tick_result(
                            result,
                            self.js.dispatch_event_at_with_target(
                                new_id,
                                "mouseenter",
                                x,
                                y,
                                new_id,
                                old_hover_id,
                            ),
                        );
                        result = combine_tick_result(
                            result,
                            self.js.dispatch_event_at_with_target(
                                new_id,
                                "hover",
                                x,
                                y,
                                new_id,
                                old_hover_id,
                            ),
                        );
                        result = combine_tick_result(
                            result,
                            self.js.dispatch_event_at_with_target(
                                new_id,
                                "hoverenter",
                                x,
                                y,
                                new_id,
                                old_hover_id,
                            ),
                        );
                    }
                }

                if let Some(target_id) = move_target {
                    result = combine_tick_result(
                        result,
                        self.js.dispatch_event_at(target_id, "mousemove", x, y),
                    );
                }

                if hover_changed || result.needs_paint {
                    self.needs_paint = true;
                    result.needs_paint = true;
                }
                return result;
            }
            MouseEvent::Wheel {
                x,
                y,
                delta_x,
                delta_y,
            } => return self.dispatch_wheel(x, y, delta_x, delta_y),
            MouseEvent::Down { .. } | MouseEvent::Up { .. } => {}
        };

        let event_name = match event {
            MouseEvent::Down {
                button: MouseButton::Left,
                ..
            } => {
                let hit_id = self.hit_node_id(x, y);

                // Click landed on a popup option: commit the selection and close.
                if let Some(hit_id) = hit_id {
                    if let Some((sel_id, opt_idx)) = self.popup_option_for_hit(hit_id) {
                        let disabled = self
                            .js
                            .selects
                            .borrow()
                            .get(&sel_id)
                            .and_then(|s| s.options.get(opt_idx).map(|o| o.disabled))
                            .unwrap_or(true);
                        if !disabled {
                            if let Some(state) = self.js.selects.borrow_mut().get_mut(&sel_id) {
                                state.set_selected_index(Some(opt_idx));
                            }
                            self.set_select_open(sel_id, false);
                            self.refresh_select_text(sel_id);
                            let select_snapshot = self
                                .js
                                .selects
                                .borrow()
                                .get(&sel_id)
                                .map(|s| (s.value().unwrap_or_default(), s.selected_index()));
                            if let Some((value, selected_index)) = select_snapshot {
                                return self.js.dispatch_select_change_event(
                                    sel_id,
                                    &value,
                                    selected_index,
                                );
                            }
                            return TickResult::default();
                        }
                        // Disabled option click: swallow and keep open.
                        return TickResult {
                            needs_paint: false,
                            jobs_pending: false,
                        };
                    }
                }

                // Click landed somewhere outside any open select+popup: close
                // those popups (a click on the select itself is dispatched via
                // handle_select_click below).
                let owning_select = hit_id.and_then(|id| self.select_owning_hit(id));
                let open_selects: Vec<usize> = self
                    .js
                    .selects
                    .borrow()
                    .iter()
                    .filter(|(_, s)| s.is_open())
                    .map(|(id, _)| *id)
                    .collect();
                for select_id in open_selects {
                    if Some(select_id) != owning_select {
                        self.set_select_open(select_id, false);
                    }
                }

                let old_focus = self.focused_node_id;
                let hit_id = self.hit_node_id(x, y);
                let focus_id = hit_id.map(|hit_id| {
                    let doc = self.doc.borrow();
                    self.js
                        .find_handler_up(&doc, hit_id, "focus")
                        .or_else(|| self.js.find_handler_up(&doc, hit_id, "keydown"))
                        .unwrap_or(hit_id)
                });

                // :active pseudo-class flips on while the mouse button is held
                // over `hit_id`. Repaint is needed so any matching CSS rule
                // re-evaluates.
                if self.set_active_to_node(hit_id) {
                    self.needs_paint = true;
                }

                let mut result = TickResult::default();

                match focus_id {
                    Some(focus_id) => {
                        if old_focus != Some(focus_id) {
                            result = combine_tick_result(
                                result,
                                self.set_focused_node(Some(focus_id), x, y),
                            );
                        }
                    }
                    None => {
                        result = combine_tick_result(result, self.set_focused_node(None, x, y));
                        if result.needs_paint {
                            self.needs_paint = true;
                        }
                        return result;
                    }
                }

                let Some(hit_id) = hit_id else {
                    return result;
                };

                // Native inputs: handle before JS click dispatch.
                let input_kind = self
                    .js
                    .inputs
                    .borrow()
                    .get(&hit_id)
                    .map(|s| (s.is_checked_like(), s.is_range()));

                match input_kind {
                    Some((true, _)) => {
                        // Checkbox / radio: toggle on click (mirrors Space/Enter).
                        result =
                            combine_tick_result(result, self.handle_checked_input_click(hit_id));
                        self.needs_paint = true;
                        result.needs_paint = true;
                        return result;
                    }
                    Some((_, true)) => {
                        // Range slider: set value from click x, begin drag.
                        let (doc_x, _) = self.document_coords_for_client(x, y);
                        if let Some(r) = self.update_range_from_x(hit_id, doc_x) {
                            result = combine_tick_result(result, r);
                        }
                        self.range_drag_id = Some(hit_id);
                        self.needs_paint = true;
                        result.needs_paint = true;
                        return result;
                    }
                    _ => {}
                }

                // Native selects: toggle open state on click.
                if self.js.selects.borrow().contains_key(&hit_id) {
                    result = combine_tick_result(result, self.handle_select_click(hit_id));
                    return result;
                }

                let handler_node = {
                    let doc = self.doc.borrow();
                    self.js.find_handler_up(&doc, hit_id, "click")
                };

                if let Some(handler_node) = handler_node {
                    return combine_tick_result(
                        result,
                        self.js.dispatch_event(handler_node, "click", x, y),
                    );
                }

                if result.needs_paint {
                    self.needs_paint = true;
                }
                return result;
            }
            MouseEvent::Down {
                button: MouseButton::Right,
                ..
            } => "contextmenu",
            MouseEvent::Down {
                button: MouseButton::Middle,
                ..
            } => "auxclick",
            MouseEvent::Up { .. } => "mouseup",
            MouseEvent::Move { .. } | MouseEvent::Wheel { .. } => unreachable!(),
        };

        // Any mouse-up clears :active. We do this regardless of which button
        // released — browser :active is cleared on any release that ends the
        // press, and tracking which button started it isn't worth the bytes
        // for the rare multi-button case.
        if matches!(event, MouseEvent::Up { .. }) && self.set_active_to_node(None) {
            self.needs_paint = true;
        }

        // Hit-test: find the deepest node at (x, y).
        let hit_id = self.hit_node_id(x, y);
        let Some(hit_id) = hit_id else {
            return TickResult::default();
        };

        // Walk ancestors for a registered handler.
        let handler_node = {
            let doc = self.doc.borrow();
            self.js.find_handler_up(&doc, hit_id, event_name)
        };
        let Some(handler_node) = handler_node else {
            return TickResult::default();
        };

        let result = self.js.dispatch_event(handler_node, event_name, x, y);
        if result.needs_paint {
            self.needs_paint = true;
        }
        result
    }

    /// Forward a wheel event to the document.
    ///
    /// The wheel delta is applied to the nearest scrollable node under `(x, y)`.
    /// If node scrolling bubbles to an ancestor or the viewport, those scroll
    /// offsets are updated as needed and `scroll` events are dispatched for the
    /// node where the offset changed.
    pub fn dispatch_wheel(&mut self, x: f32, y: f32, delta_x: f32, delta_y: f32) -> TickResult {
        let start_id = self.hit_node_id(x, y);
        let Some(start_id) = start_id else {
            return TickResult::default();
        };

        let mut before_offsets = Vec::new();
        let before_viewport = {
            let doc = self.doc.borrow();
            let mut node_id = Some(start_id);
            while let Some(id) = node_id {
                if let Some(node) = doc.get_node(id) {
                    before_offsets.push((id, node.scroll_offset.x, node.scroll_offset.y));
                }
                if id == 0 {
                    break;
                }
                node_id = doc.get_node(id).and_then(|node| node.parent);
            }
            doc.viewport_scroll()
        };

        {
            let mut doc = self.doc.borrow_mut();
            doc.scroll_node_by(start_id, f64::from(delta_x), f64::from(delta_y), |_| {});
        }

        let after_offsets = {
            let doc = self.doc.borrow();
            let mut values = Vec::with_capacity(before_offsets.len());
            for (node_id, _, _) in before_offsets.iter().copied() {
                if let Some(node) = doc.get_node(node_id) {
                    values.push((node_id, node.scroll_offset.x, node.scroll_offset.y));
                }
            }
            values
        };
        let after_viewport = self.doc.borrow().viewport_scroll();

        let mut changed_node = None;
        let mut changed_scroll = (0.0_f64, 0.0_f64);
        for ((node_id, before_x, before_y), (_, after_x, after_y)) in
            before_offsets.iter().zip(after_offsets.iter())
        {
            if before_x != after_x || before_y != after_y {
                changed_node = Some(*node_id);
                changed_scroll = (*after_x, *after_y);
                break;
            }
        }

        let viewport_changed = before_viewport != after_viewport;
        let has_scrolled = { changed_node.is_some() || viewport_changed };
        let target_scroll = after_offsets
            .iter()
            .find(|(node_id, _, _)| *node_id == start_id)
            .map(|(_, scroll_x, scroll_y)| (*scroll_x, *scroll_y))
            .or_else(|| viewport_changed.then_some((after_viewport.x, after_viewport.y)))
            .unwrap_or((0.0, 0.0));

        let mut result = TickResult::default();

        // Resolve the handler in its own statement so the `Ref` from
        // `doc.borrow()` is dropped *before* we re-enter JS — otherwise a
        // reactive effect triggered from the wheel handler that needs
        // `doc.borrow_mut()` (e.g. `__sol_setText`) would panic.
        let wheel_handler_id = self
            .js
            .find_handler_up(&self.doc.borrow(), start_id, "wheel");
        if let Some(wheel_handler_id) = wheel_handler_id {
            result = combine_tick_result(
                result,
                self.js.dispatch_wheel_event(
                    wheel_handler_id,
                    "wheel",
                    x,
                    y,
                    delta_x,
                    delta_y,
                    start_id,
                    None,
                    target_scroll.0,
                    target_scroll.1,
                ),
            );
        }

        let should_dispatch_scroll = if let Some(node_id) = changed_node {
            Some((node_id, changed_scroll))
        } else if viewport_changed {
            Some((self.container_id, (after_viewport.x, after_viewport.y)))
        } else {
            None
        };

        if should_dispatch_scroll.is_none() && !result.needs_paint && !has_scrolled {
            return TickResult::default();
        }

        if let Some((scroll_node_id, (scroll_left, scroll_top))) = should_dispatch_scroll {
            result = combine_tick_result(
                result,
                self.js
                    .dispatch_scroll_event(scroll_node_id, x, y, scroll_left, scroll_top),
            );
        }

        if result.needs_paint || has_scrolled {
            self.needs_paint = true;
            result.needs_paint = true;
        }

        result
    }

    // ── Keyboard input ─────────────────────────────────────────────────────

    /// Forward a key-down event to the focused node.
    pub fn dispatch_key_down(&mut self, event: KeyboardEvent) -> TickResult {
        self.dispatch_key("keydown", event)
    }

    /// Forward a key-up event to the focused node.
    pub fn dispatch_key_up(&mut self, event: KeyboardEvent) -> TickResult {
        self.dispatch_key("keyup", event)
    }

    fn set_focused_node(&mut self, next_focus: Option<usize>, x: f32, y: f32) -> TickResult {
        let old_focus = self.focused_node_id;
        if old_focus == next_focus {
            return TickResult::default();
        }

        let mut result = TickResult::default();

        if let Some(previous) = old_focus {
            if self
                .js
                .selects
                .borrow()
                .get(&previous)
                .is_some_and(|state| state.is_open())
            {
                self.set_select_open(previous, false);
            }
            result = combine_tick_result(result, self.js.dispatch_event(previous, "blur", x, y));
            if self.js.inputs.borrow().contains_key(&previous) {
                self.refresh_input_text(previous);
                self.needs_paint = true;
            }
            self.doc.borrow_mut().clear_focus();
            self.focused_node_id = None;
        }

        let Some(focus_id) = next_focus else {
            if result.needs_paint {
                self.needs_paint = true;
            }
            return result;
        };

        self.focused_node_id = Some(focus_id);
        self.doc.borrow_mut().set_focus_to(focus_id);
        if self.js.inputs.borrow().contains_key(&focus_id) {
            if let Some(state) = self.js.inputs.borrow_mut().get_mut(&focus_id) {
                state.place_caret_at_end();
            }
            self.refresh_input_text(focus_id);
            self.needs_paint = true;
        }
        result = combine_tick_result(result, self.js.dispatch_event(focus_id, "focus", x, y));
        if result.needs_paint {
            self.needs_paint = true;
        }

        result
    }

    fn focus_adjacent_control(&mut self, backwards: bool) -> TickResult {
        let control_order =
            crate::focus::collect_tab_order(&self.doc.borrow(), &self.js.inputs, &self.js.selects);

        if control_order.is_empty() {
            return TickResult::default();
        }

        let next_index = match self
            .focused_node_id
            .and_then(|id| control_order.iter().position(|candidate| *candidate == id))
        {
            Some(current) if backwards => (current + control_order.len() - 1) % control_order.len(),
            Some(current) => (current + 1) % control_order.len(),
            None if backwards => control_order.len() - 1,
            None => 0,
        };

        let mut result = self.set_focused_node(Some(control_order[next_index]), 0.0, 0.0);
        result.needs_paint = true;
        result
    }

    fn dispatch_key(&mut self, event_name: &str, event: KeyboardEvent) -> TickResult {
        if event_name == "keydown" && event.key == "Tab" {
            let mut result = TickResult::default();
            if let Some(focused_id) = self.focused_node_id {
                if self
                    .js
                    .selects
                    .borrow()
                    .get(&focused_id)
                    .is_some_and(|state| state.is_open())
                {
                    let (edited, emits_change) = self.apply_select_key(focused_id, &event);
                    if edited {
                        self.refresh_select_text(focused_id);
                    }
                    if emits_change {
                        let select_snapshot = self
                            .js
                            .selects
                            .borrow()
                            .get(&focused_id)
                            .map(|s| (s.value().unwrap_or_default(), s.selected_index()));
                        if let Some((value, selected_index)) = select_snapshot {
                            let change_result = self.js.dispatch_select_change_event(
                                focused_id,
                                &value,
                                selected_index,
                            );
                            result = combine_tick_result(result, change_result);
                        }
                    }
                }
            }

            return combine_tick_result(result, self.focus_adjacent_control(event.shift_key));
        }

        let Some(focused_id) = self.focused_node_id else {
            return TickResult::default();
        };

        // If the focused node is a native `<input>`, the engine owns editing:
        // apply the keystroke to the InputState, refresh the visible text
        // node, and emit `input` after the user-defined handler so it sees
        // the updated value via `event.value` / `event.target.value`.
        // Caret-only edits (arrows/home/end etc.) refresh visual text but do
        // not dispatch `input` to match browser semantics.
        let (edited, emits_input_event, focus_target) = if event_name == "keydown"
            && self.js.inputs.borrow().contains_key(&focused_id)
        {
            if self
                .js
                .inputs
                .borrow()
                .get(&focused_id)
                .is_some_and(|state| state.is_radio())
                && matches!(
                    event.key.as_str(),
                    "ArrowLeft" | "ArrowRight" | "ArrowUp" | "ArrowDown" | "Home" | "End"
                )
            {
                self.apply_radio_navigation_key(focused_id, &event)
            } else {
                let (edited, emits_input_event) =
                    apply_input_key(&self.js.inputs, focused_id, &event);
                (edited, emits_input_event, None)
            }
        } else if event_name == "keydown" && self.js.selects.borrow().contains_key(&focused_id) {
            let (edited, emits_change) = self.apply_select_key(focused_id, &event);
            (edited, emits_change, None)
        } else {
            (false, false, None)
        };

        let mut result = self.js.dispatch_key_event(focused_id, event_name, &event);

        // Button keyboard activation. Browsers fire `click` on:
        //   - `keydown` for Enter (Repeat included — long-press repeats the
        //     activation), AND
        //   - `keyup` for Space (the keydown shows :active visual state, the
        //     keyup fires the actual click).
        // Only the unmodified keys count; Ctrl/Alt/Meta+Enter is reserved
        // for the user's own shortcut handlers via `onKeyDown`.
        if is_button_node(&self.doc.borrow(), focused_id) {
            let no_mods = !event.ctrl_key && !event.alt_key && !event.meta_key;
            let activate = no_mods
                && match (event_name, event.key.as_str()) {
                    ("keydown", "Enter") => true,
                    ("keyup", " " | "Space") => true,
                    _ => false,
                };
            if activate {
                let click = self.js.dispatch_event_at(focused_id, "click", 0.0, 0.0);
                result = combine_tick_result(result, click);
            }
        }
        if result.needs_paint {
            self.needs_paint = true;
        }

        if let Some(next_focus) = focus_target {
            let focus_result = self.set_focused_node(Some(next_focus), 0.0, 0.0);
            result = combine_tick_result(result, focus_result);
        }

        if edited {
            let target_id = focus_target.unwrap_or(focused_id);
            self.refresh_input_text(target_id);
            self.refresh_select_text(target_id);
            self.needs_paint = true;
        }

        if emits_input_event {
            // Refresh visible text + emit input event for inputs.
            let target_id = focus_target.unwrap_or(focused_id);
            let snapshot = self.js.inputs.borrow().get(&target_id).map(|s| {
                (
                    s.value().to_string(),
                    s.checked(),
                    s.selection_start(),
                    s.selection_end(),
                )
            });
            if let Some((value, checked, selection_start, selection_end)) = snapshot {
                let input_result = self.js.dispatch_input_event(
                    target_id,
                    &value,
                    checked,
                    selection_start,
                    selection_end,
                );
                return combine_tick_result(result, input_result);
            }

            // Refresh visible text + emit change event for selects.
            let select_snapshot = self
                .js
                .selects
                .borrow()
                .get(&focus_target.unwrap_or(focused_id))
                .map(|s| (s.value().unwrap_or_default(), s.selected_index()));
            if let Some((value, selected_index)) = select_snapshot {
                let change_result = self.js.dispatch_select_change_event(
                    focus_target.unwrap_or(focused_id),
                    &value,
                    selected_index,
                );
                return combine_tick_result(result, change_result);
            }
        }

        result
    }

    /// Refresh the visible text child of an `<input>` from its InputState.
    /// The child text node is the first child of the input (seeded in
    /// `__sol_createElement` when the tag is "input").
    pub(super) fn refresh_input_text(&mut self, input_id: usize) {
        if self
            .js
            .inputs
            .borrow()
            .get(&input_id)
            .is_some_and(|state| state.is_range())
        {
            return;
        }

        let focused = self.focused_node_id == Some(input_id);
        let display = self
            .js
            .inputs
            .borrow()
            .get(&input_id)
            .map(|s| s.render(focused).0);
        let Some(text) = display else { return };
        let child = self
            .doc
            .borrow()
            .get_node(input_id)
            .and_then(|n| n.children.first().copied());
        if let Some(child_id) = child {
            self.doc
                .borrow_mut()
                .mutate()
                .set_node_text(child_id, &text);
        }
    }

    /// Refresh the visible text child of a `<select>` from its SelectState.
    /// The child text node is the first child of the select (seeded in
    /// `__sol_createElement` when the tag is "select").
    /// Also update the select element's value attribute for form submission.
    pub(super) fn refresh_select_text(&mut self, select_id: usize) {
        let display = self
            .js
            .selects
            .borrow()
            .get(&select_id)
            .map(|s| s.current_label());
        let Some(text) = display else { return };
        let child = self
            .doc
            .borrow()
            .get_node(select_id)
            .and_then(|n| n.children.first().copied());
        if let Some(child_id) = child {
            self.doc
                .borrow_mut()
                .mutate()
                .set_node_text(child_id, &text);
        }

        // Sync the value attribute for form submission
        let value = self
            .js
            .selects
            .borrow()
            .get(&select_id)
            .and_then(|s| s.selected_value().map(str::to_owned));
        self.doc.borrow_mut().mutate().set_attribute(
            select_id,
            blitz_dom::QualName::new(None, blitz_dom::ns!(), blitz_dom::LocalName::from("value")),
            value.as_deref().unwrap_or(""),
        );
    }

    fn sync_input_render_text_before_layout(&self) {
        let focused_id = self.focused_node_id;
        let inputs: Vec<(usize, String, bool, usize)> = self
            .js
            .inputs
            .borrow()
            .iter()
            .map(|(input_id, state)| {
                let (text, placeholder) = state.render(focused_id == Some(*input_id));
                (
                    *input_id,
                    text,
                    placeholder,
                    state.display_caret_byte_index(),
                )
            })
            .collect();
        if inputs.is_empty() {
            return;
        }

        let mut doc = self.doc.borrow_mut();
        for (input_id, text, placeholder, caret_byte) in inputs {
            let Some(has_text_input) = doc.get_node(input_id).map(|node| {
                node.element_data()
                    .and_then(|element| element.text_input_data())
                    .is_some()
            }) else {
                continue;
            };

            if has_text_input {
                doc.with_text_input(input_id, |mut driver| {
                    if driver.editor.raw_text() != text {
                        driver.editor.set_text(&text);
                        driver.refresh_layout();
                    }
                    driver.move_to_byte(caret_byte);
                });
            } else if let Some(element) = doc
                .get_node_mut(input_id)
                .and_then(|node| node.element_data_mut())
            {
                element.attrs.set(attr_qual("value"), &text);
            }

            if let Some(element) = doc
                .get_node_mut(input_id)
                .and_then(|node| node.element_data_mut())
            {
                if placeholder {
                    element
                        .attrs
                        .set(attr_qual("data-ox-placeholder-active"), "true");
                } else {
                    element
                        .attrs
                        .remove(&attr_qual("data-ox-placeholder-active"));
                }
            }
        }
    }

    pub(super) fn collect_input_carets(&self) -> Vec<InputCaret> {
        let Some(input_id) = self.focused_node_id else {
            return Vec::new();
        };

        let inputs = self.js.inputs.borrow();
        let Some(state) = inputs.get(&input_id) else {
            return Vec::new();
        };
        if !state.blink_visible() {
            return Vec::new();
        }

        let doc = self.doc.borrow();
        let Some(input_node) = doc.get_node(input_id) else {
            return Vec::new();
        };
        let Some(input_data) = input_node
            .element_data()
            .and_then(|element| element.text_input_data())
        else {
            return Vec::new();
        };
        let Some(cursor) = input_data.editor.cursor_geometry(1.5) else {
            return Vec::new();
        };

        let input_origin = input_node.absolute_position(0.0, 0.0);
        let layout = input_node.final_layout;
        let content_x = input_origin.x + layout.border.left + layout.padding.left;
        let content_y = input_origin.y + layout.border.top + layout.padding.top;
        let content_w = layout.content_box_width().max(0.0);
        let content_h = layout.content_box_height().max(1.0);
        let y_offset = input_node.text_input_v_centering_offset(1.0) as f32;
        let cursor_w = (cursor.x1 - cursor.x0).max(0.0) as f32;
        let cursor_h = (cursor.y1 - cursor.y0).max(0.0) as f32;

        let (x, y, caret_w, caret_h) = if cursor_h > 0.0 {
            (
                (content_x + cursor.x0 as f32).clamp(content_x, content_x + content_w),
                (content_y + y_offset + cursor.y0 as f32).clamp(content_y, content_y + content_h),
                cursor_w.max(1.0),
                cursor_h.max(1.0),
            )
        } else {
            let caret_w = cursor_w.max(1.5);
            let caret_h = (content_h * 0.7).max(1.0);
            let x = content_x + estimated_input_char_width(input_node) * state.caret() as f32;
            (
                x.clamp(content_x, content_x + content_w),
                content_y + ((content_h - caret_h).max(0.0) * 0.5),
                caret_w,
                caret_h,
            )
        };

        vec![InputCaret {
            x,
            y,
            width: caret_w,
            height: caret_h,
            color: input_caret_color(input_node),
        }]
    }

    pub(super) fn collect_input_selections(&self) -> Vec<InputSelection> {
        let Some(input_id) = self.focused_node_id else {
            return Vec::new();
        };

        let inputs = self.js.inputs.borrow();
        let Some(state) = inputs.get(&input_id) else {
            return Vec::new();
        };
        let (selection_start_chars, selection_end_chars) =
            (state.selection_start(), state.selection_end());
        if selection_start_chars == selection_end_chars {
            return Vec::new();
        }
        let selection_len = (selection_end_chars - selection_start_chars) as f32;
        if !state.is_text_like() {
            return Vec::new();
        }

        let doc = self.doc.borrow();
        let Some(input_node) = doc.get_node(input_id) else {
            return Vec::new();
        };
        let Some(input_data) = input_node
            .element_data()
            .and_then(|element| element.text_input_data())
        else {
            return Vec::new();
        };

        let layout = input_node.final_layout;
        let input_origin = input_node.absolute_position(0.0, 0.0);
        let content_x = input_origin.x + layout.border.left + layout.padding.left;
        let content_y = input_origin.y + layout.border.top + layout.padding.top;
        let content_w = layout.content_box_width().max(0.0);
        let content_h = layout.content_box_height().max(1.0);
        let y_offset = input_node.text_input_v_centering_offset(1.0) as f32;

        let display_text = state.render(true).0;
        let selection_start = char_index_to_byte_index(&display_text, selection_start_chars);
        let selection_end = char_index_to_byte_index(&display_text, selection_end_chars);
        let Some(layout_data) = input_data.editor.try_layout() else {
            return vec![];
        };
        let anchor = Cursor::from_byte_index(layout_data, selection_start, Affinity::Downstream);
        let focus = Cursor::from_byte_index(layout_data, selection_end, Affinity::Downstream);
        let selection = Selection::new(anchor, focus);

        let mut selections = Vec::new();
        selection.geometry_with(layout_data, |rect, _line_idx| {
            let x0 = (content_x + rect.x0 as f32).clamp(content_x, content_x + content_w);
            let x1 = (content_x + rect.x1 as f32).clamp(content_x, content_x + content_w);
            let y0 =
                (content_y + y_offset + rect.y0 as f32).clamp(content_y, content_y + content_h);
            let y1 =
                (content_y + y_offset + rect.y1 as f32).clamp(content_y, content_y + content_h);
            let width = (x1 - x0).max(0.0);
            let height = (y1 - y0).max(0.0);
            if width <= 0.0 || height <= 0.0 {
                return;
            }
            selections.push(InputSelection {
                x: x0,
                y: y0,
                width,
                height,
            });
        });

        if selections.is_empty() {
            let width = (estimated_input_char_width(input_node) * selection_len).max(1.0);
            let height = (content_h * 0.7).max(1.0);
            let y = content_y + ((content_h - height).max(0.0) * 0.5);
            selections.push(InputSelection {
                x: (content_x
                    + estimated_input_char_width(input_node) * selection_start_chars as f32)
                    .clamp(content_x, content_x + content_w),
                y,
                width,
                height,
            });
        }

        selections
    }

    fn mask_blitz_text_input_focus_for_paint(&self, doc: &mut BaseDocument) -> Option<usize> {
        let input_id = self.focused_node_id?;
        if !self.js.inputs.borrow().contains_key(&input_id) {
            return None;
        }
        if doc.get_focussed_node_id() != Some(input_id) {
            return None;
        }

        doc.get_node_mut(input_id)?
            .element_state
            .remove(ElementState::FOCUS);
        Some(input_id)
    }

    fn restore_blitz_text_input_focus_after_paint(
        &self,
        doc: &mut BaseDocument,
        input_id: Option<usize>,
    ) {
        let Some(input_id) = input_id else {
            return;
        };
        if let Some(node) = doc.get_node_mut(input_id) {
            node.element_state.insert(ElementState::FOCUS);
        }
    }

    // ── State & events ────────────────────────────────────────────────────────

    /// A clone of the state handle. Can be sent to any thread; writes are
    /// applied on the next `tick()`.
    pub fn state(&self) -> StateHandle {
        self.state.clone()
    }

    /// Dispatch a custom event from the Rust host into the JS runtime.
    ///
    /// JS code can subscribe with `addEventListener(name, listener)` or
    /// `__sol_addEventListener(name, listener)`. The listener receives an object
    /// containing `type`, `detail`, and `payload`; `payload` is an alias for
    /// `detail` for convenience.
    pub fn dispatch_runtime_event(
        &mut self,
        name: impl AsRef<str>,
        payload: serde_json::Value,
    ) -> TickResult {
        let result = self.js.dispatch_runtime_event(name.as_ref(), &payload);
        if result.needs_paint {
            self.needs_paint = true;
        }
        result
    }

    /// Returns and clears the latest JS boundary error captured by the host bridge.
    pub fn take_send_event_error(&self) -> Option<String> {
        self.js.take_send_event_error()
    }

    /// If a focused native `<input>` is blinking, returns the deadline at
    /// which the host should wake up to advance the blink (so it can set
    /// `ControlFlow::WaitUntil(deadline)` and call `tick()`). `None` means
    /// nothing is blinking right now and the host can idle indefinitely.
    pub fn next_blink_deadline(&self) -> Option<std::time::Instant> {
        let focused = self.focused_node_id?;
        let map = self.js.inputs.borrow();
        let state = map.get(&focused)?;
        Some(state.next_blink_at())
    }

    /// A `Notify` handle that fires whenever an async source (e.g. a tokio task
    /// calling `StateHandle::set`) mutates state. The host can await this to
    /// know when to schedule a tick.
    pub fn wake_handle(&self) -> Arc<tokio::sync::Notify> {
        Arc::clone(&self.wake)
    }

    // ── Stylesheets ──────────────────────────────────────────────────────────

    /// Register a stylesheet with the document.
    ///
    /// The returned [`StylesheetId`] can be passed to
    /// [`replace_stylesheet`](Self::replace_stylesheet) or
    /// [`remove_stylesheet`](Self::remove_stylesheet) to update or drop it.
    /// Marks the document as needing repaint.
    pub fn add_stylesheet(&mut self, css: &str) -> StylesheetId {
        let id = StylesheetId(self.next_stylesheet_id);
        self.next_stylesheet_id += 1;
        self.doc.borrow_mut().add_user_agent_stylesheet(css);
        self.stylesheets.insert(id, css.to_string());
        self.needs_paint = true;
        id
    }

    /// Replace the contents of a previously-registered stylesheet.
    ///
    /// Returns `true` if the stylesheet was found and replaced, `false`
    /// otherwise. Marks the document as needing repaint on success.
    pub fn replace_stylesheet(&mut self, id: StylesheetId, css: &str) -> bool {
        let Some(old) = self.stylesheets.get_mut(&id) else {
            return false;
        };
        let mut doc = self.doc.borrow_mut();
        doc.remove_user_agent_stylesheet(old);
        doc.add_user_agent_stylesheet(css);
        *old = css.to_string();
        drop(doc);
        self.needs_paint = true;
        true
    }

    /// Remove a previously-registered stylesheet.
    ///
    /// Returns `true` if the stylesheet was found and removed, `false`
    /// otherwise. Marks the document as needing repaint on success.
    pub fn remove_stylesheet(&mut self, id: StylesheetId) -> bool {
        let Some(old) = self.stylesheets.remove(&id) else {
            return false;
        };
        self.doc.borrow_mut().remove_user_agent_stylesheet(&old);
        self.needs_paint = true;
        true
    }

    /// Replace an existing stylesheet, or insert it when the id is not known.
    ///
    /// Returns the stylesheet id to use for subsequent updates.
    pub fn upsert_stylesheet(
        &mut self,
        stylesheet_id: Option<StylesheetId>,
        css: &str,
    ) -> StylesheetId {
        let id = match stylesheet_id {
            Some(id) => {
                if self.replace_stylesheet(id, css) {
                    id
                } else {
                    let _ = self.remove_stylesheet(id);
                    self.add_stylesheet(css)
                }
            }
            None => self.add_stylesheet(css),
        };

        id
    }

    // ── Native inputs ────────────────────────────────────────────────────────

    /// Returns the current value of the `<input>` registered at `node_id`,
    /// or `None` if no input is registered there. Useful for tests and for
    /// hosts that want to read the field directly without round-tripping
    /// through a JS handler.
    pub fn input_value(&self, node_id: usize) -> Option<String> {
        self.js.inputs.borrow().get(&node_id).map(|state| {
            if state.is_checked_like() {
                state.render(false).0
            } else {
                state.value().to_string()
            }
        })
    }

    /// Set the value of the `<input>` registered at `node_id`. Mirrors what
    /// `__sol_setAttr(node, "value", v)` does from JS — the caret moves to the
    /// end of the new text and the visible text is refreshed on next render.
    /// Returns false if no input is registered at `node_id`.
    pub fn set_input_value(&mut self, node_id: usize, value: impl Into<String>) -> bool {
        let mut map = self.js.inputs.borrow_mut();
        let Some(state) = map.get_mut(&node_id) else {
            return false;
        };
        state.set_value(value);
        drop(map);
        self.refresh_input_text(node_id);
        self.needs_paint = true;
        true
    }

    // ── Scrollbars ───────────────────────────────────────────────────────────

    /// Override scrollbar colours for every scroll container in this instance.
    ///
    /// Pass `Some(theme)` to use the host-supplied colours, or `None` to fall
    /// back to the default heuristic (the container's computed `color` tinted
    /// at a low alpha for the track and higher alpha for the thumb).
    ///
    /// Full CSS scrollbar theming (`scrollbar-color`, `::-webkit-scrollbar`)
    /// awaits a stylo build that exposes the property in servo mode.
    pub fn set_scrollbar_theme(&mut self, theme: Option<ScrollbarTheme>) {
        self.scrollbar_theme = theme.map(|t| t.to_colors());
        self.needs_paint = true;
    }

    // ── Geometry ─────────────────────────────────────────────────────────────

    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    pub fn container_id(&self) -> usize {
        self.container_id
    }

    /// Iterate registered `<select>` node ids. Useful for tests and host code
    /// that wants to drive popups programmatically.
    pub fn select_node_ids(&self) -> Vec<usize> {
        self.js.selects.borrow().keys().copied().collect()
    }

    /// Programmatically open or close a `<select>` dropdown. Used by tests
    /// and by host code that drives selects without a real pointer.
    pub fn set_select_dropdown_open(&mut self, select_id: usize, open: bool) {
        self.set_select_open(select_id, open);
    }
}

/// True when `node_id` is a `<button>` element. Used by keyboard
/// activation to decide whether `Enter`/`Space` should fire `click`.
fn is_button_node(doc: &BaseDocument, node_id: usize) -> bool {
    doc.get_node(node_id)
        .and_then(|n| n.element_data())
        .is_some_and(|e| e.name.local.as_ref() == "button")
}

fn hit_visible_node_id(doc: &BaseDocument, node_id: usize, x: f32, y: f32) -> Option<usize> {
    let node = doc.get_node(node_id)?;
    let local_x = x - node.final_layout.location.x;
    let local_y = y - node.final_layout.location.y;
    let size = node.final_layout.size;
    let content_size = node.final_layout.content_size;
    let overflow = node.scrollable_overflow;
    let has_scrollable_content = node.final_layout.scroll_width() > size.width
        || node.final_layout.scroll_height() > size.height
        || node.scroll_offset.x != 0.0
        || node.scroll_offset.y != 0.0;

    let matches_self =
        local_x >= 0.0 && local_x <= size.width && local_y >= 0.0 && local_y <= size.height;
    let matches_content = local_x >= 0.0
        && local_x <= content_size.width
        && local_y >= 0.0
        && local_y <= content_size.height;
    let matches_overflow = local_x >= overflow.x0 as f32
        && local_x <= overflow.x1 as f32
        && local_y >= overflow.y0 as f32
        && local_y <= overflow.y1 as f32;
    let matches_node = if has_scrollable_content {
        matches_self
    } else {
        matches_self || matches_content || matches_overflow
    };

    if !matches_node {
        return None;
    }

    let child_x = local_x + node.scroll_offset.x as f32;
    let child_y = local_y + node.scroll_offset.y as f32;
    let children = node
        .paint_children
        .borrow()
        .as_ref()
        .cloned()
        .unwrap_or_else(|| node.children.clone());

    for child_id in children.iter().rev() {
        if let Some(hit_id) = hit_visible_node_id(doc, *child_id, child_x, child_y) {
            return Some(hit_id);
        }
    }

    matches_self.then_some(node.id)
}

fn existing_node_layout_ancestors(doc: &BaseDocument, node_id: Option<usize>) -> Vec<usize> {
    node_id
        .filter(|id| doc.get_node(*id).is_some())
        .map(|id| doc.node_layout_ancestors(id))
        .unwrap_or_default()
}

fn attr_qual(name: &str) -> QualName {
    QualName::new(None, ns!(), LocalName::from(name))
}

pub(super) fn estimated_input_char_width(node: &Node) -> f32 {
    node.primary_styles()
        .map(|styles| styles.clone_font_size().used_size().px() * 0.6)
        .filter(|width| width.is_finite() && *width > 0.0)
        .unwrap_or(8.0)
}

pub(super) fn char_index_to_byte_index(text: &str, char_idx: usize) -> usize {
    text.char_indices()
        .nth(char_idx)
        .map(|(i, _)| i)
        .unwrap_or(text.len())
}

fn input_caret_color(node: &Node) -> peniko::Color {
    let Some(styles) = node.primary_styles() else {
        return peniko::Color::BLACK;
    };
    let srgb = styles
        .clone_color()
        .to_color_space(style::color::ColorSpace::Srgb);
    let to_u8 = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    peniko::Color::from_rgba8(
        to_u8(srgb.components.0),
        to_u8(srgb.components.1),
        to_u8(srgb.components.2),
        255,
    )
}

/// Apply a single keystroke to the `InputState` registered for `input_id`.
/// Returns `(changed, emits_input_event)` where:
/// - `changed`: caret or value changed and rendered text should update.
/// - `emits_input_event`: value changed and an `"input"` event should dispatch.
fn apply_input_key(
    inputs: &crate::input::InputRegistry,
    input_id: usize,
    event: &KeyboardEvent,
) -> (bool, bool) {
    let mut map = inputs.borrow_mut();
    let Some(state) = map.get_mut(&input_id) else {
        return (false, false);
    };

    let has_modifier = event.ctrl_key || event.meta_key || event.alt_key;
    let with_shift = event.shift_key;

    if state.is_checked_like() {
        return match event.key.as_str() {
            // Browser checkboxes/radios toggle on Space/Enter.
            " " | "Space" | "Enter" => {
                if has_modifier {
                    return (false, false);
                }

                if state.is_radio() {
                    if state.checked() {
                        return (false, false);
                    }

                    let group_name = state.name().map(str::to_owned);
                    state.set_checked(true);

                    if let Some(group_name) = group_name {
                        for (other_id, other_state) in map.iter_mut() {
                            if *other_id == input_id {
                                continue;
                            }
                            if other_state.kind() != crate::input::InputType::Radio {
                                continue;
                            }
                            if other_state.name() == Some(group_name.as_str()) {
                                other_state.set_checked(false);
                            }
                        }
                    }

                    (true, true)
                } else {
                    let edited = state.toggle_checked();
                    (edited, edited)
                }
            }
            _ => (false, false),
        };
    }

    if state.is_range() {
        return match event.key.as_str() {
            "ArrowLeft" | "ArrowDown" => {
                let edited = state.step_number(-1);
                (edited, edited)
            }
            "ArrowRight" | "ArrowUp" => {
                let edited = state.step_number(1);
                (edited, edited)
            }
            "PageDown" => {
                let edited = state.step_number(-10);
                (edited, edited)
            }
            "PageUp" => {
                let edited = state.step_number(10);
                (edited, edited)
            }
            "Home" => {
                let edited = state.move_range_to_extreme(false);
                (edited, edited)
            }
            "End" => {
                let edited = state.move_range_to_extreme(true);
                (edited, edited)
            }
            _ => (false, false),
        };
    }

    let key = event.key.as_str();
    let word_modifier = (event.ctrl_key || event.meta_key) && !event.alt_key;

    if word_modifier {
        match key {
            "a" | "A" => {
                let edited = state.select_all();
                return (edited, false);
            }
            "ArrowLeft" => {
                let edited = state.move_word_left_extending(with_shift);
                return (edited, false);
            }
            "ArrowRight" => {
                let edited = state.move_word_right_extending(with_shift);
                return (edited, false);
            }
            "Backspace" => {
                let edited = state.delete_word_left();
                return (edited, edited);
            }
            "Delete" => {
                let edited = state.delete_word_right();
                return (edited, edited);
            }
            _ => {}
        }
    }

    match event.key.as_str() {
        "Backspace" => {
            let edited = state.backspace();
            (edited, edited)
        }
        "Delete" => {
            let edited = state.delete_forward();
            (edited, edited)
        }
        "ArrowLeft" => {
            let edited = state.move_left_extending(with_shift);
            (edited, false)
        }
        "ArrowRight" => {
            let edited = state.move_right_extending(with_shift);
            (edited, false)
        }
        "Home" => {
            let edited = state.move_home_extending(with_shift);
            (edited, false)
        }
        "End" => {
            let edited = state.move_end_extending(with_shift);
            (edited, false)
        }
        " " => {
            if state.is_numeric_like() {
                let edited = false;
                (edited, edited)
            } else {
                let edited = state.insert(' ');
                (edited, edited)
            }
        }
        "Space" => {
            if state.is_numeric_like() {
                let edited = false;
                (edited, edited)
            } else {
                let edited = state.insert(' ');
                (edited, edited)
            }
        }
        // Modifier-only keys produce empty / multi-char `key` strings we
        // shouldn't insert. Single-char printable keys *do* go in.
        key if key.chars().count() == 1 => {
            if event.ctrl_key || event.meta_key || event.alt_key {
                return (false, false);
            }
            let ch = match key.chars().next() {
                Some(ch) => ch,
                None => return (false, false),
            };
            if ch.is_control() {
                return (false, false);
            }
            let edited = if state.is_numeric_like() {
                state.insert_numeric_char(ch)
            } else {
                state.insert(ch)
            };
            (edited, edited)
        }
        _ => (false, false),
    }
}

fn combine_tick_result(a: TickResult, b: TickResult) -> TickResult {
    TickResult {
        needs_paint: a.needs_paint || b.needs_paint,
        jobs_pending: a.jobs_pending || b.jobs_pending,
    }
}
