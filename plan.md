# solite — Plan

A reactive UI library in Rust. HTML/CSS rendered by Blitz (Vello/wgpu), driven by SolidJS running on QuickJS. Each `Instance` paints into a GPU texture with transparency so the host can composite freely.

## Decisions (locked)

- **JS engine**: QuickJS via `rquickjs`. Small, fast-enough for Solid's reactive runtime, tiny binary.
- **JS framework**: SolidJS using `solid-js/universal` — we implement a custom renderer, not a fake DOM. ~6 ops to bridge.
- **HTML/CSS engine**: Blitz (`blitz-dom` + `blitz-renderer-vello`). Pinned commit, wrapped behind our own trait to insulate against churn.
- **GPU**: wgpu. Render target is an RGBA8 texture with premultiplied alpha. Host owns final composite.
- **Element vocabulary**: predefined HTML-ish element table (`div`, `span`, `button`, `img`, `p`, `h1`–`h6`, ...). Consumers write familiar JSX, but we never parse arbitrary HTML.
- **State + events**: bi-directional reactive `state` global on the JS side backed by Solid `createStore`, mirrored on the Rust side. JS→host events flow through a tokio `mpsc::UnboundedReceiver<Event>`.

## Architecture

```
┌────────────────────────────────────────────────────────┐
│ Host application (winit, game engine, compositor, ...) │
│  - owns wgpu Device/Queue                              │
│  - calls Instance::render(), gets a TextureView        │
│  - forwards mouse events to Instance::dispatch_*       │
│  - reads JS events from mpsc::UnboundedReceiver        │
│  - mutates Instance::state() from any tokio task       │
└────────────────────────────────────────────────────────┘
              │            ▲                ▲
              │ dispatch_  │ events.recv()  │ state().set(..)
              │   mouse    │                │
              ▼            │                │
┌────────────────────────────────────────────────────────┐
│ solite::Instance                                    │
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐  │
│  │ JS context   │  │ Blitz        │  │ Renderer     │  │
│  │ (rquickjs)   │──│ Document     │──│ (Vello→wgpu  │  │
│  │ + Solid      │  │ + layout     │  │  texture)    │  │
│  │   bundle     │  │              │  │              │  │
│  └──────────────┘  └──────────────┘  └──────────────┘  │
│         ▲                ▲                             │
│  ┌──────┴───────┐   ┌────┴─────┐                       │
│  │ StateBridge  │   │ Hit-test │                       │
│  │ (Mutex<Json>)│   │ dispatch │                       │
│  └──────────────┘   └──────────┘                       │
│         ▲                                              │
│  ┌──────┴───────┐                                      │
│  │ EventBridge  │── sendEvent(name, payload) ──▶ mpsc  │
│  └──────────────┘                                      │
└────────────────────────────────────────────────────────┘
```

## Event loop ownership

The library is **passive**. It owns no thread, no async runtime, no internal scheduler. The host drives everything via two methods plus one optional wake signal.

**What runs where:**

| Loop                    | Owner               | How it advances                                                  |
|-------------------------|---------------------|------------------------------------------------------------------|
| OS / window events      | Host (e.g. winit)   | Host's own event loop                                            |
| Render cadence          | Host                | Host calls `instance.render()` when ready                        |
| QuickJS microtask queue | Library (in `tick`) | `instance.tick()` pumps `Runtime::execute_pending_job()`         |
| Solid reactivity        | JS, synchronous     | Triggered by state writes; no loop needed                        |
| Host-side event recv    | Host (tokio task)   | `while let Some(ev) = rx.recv().await { ... }`                   |
| State mirror sync       | Library (in `tick`) | `tick()` drains pending host→JS patches before pumping JS jobs   |

**The pump — `instance.tick()`:**

One method, called by the host once per frame (or on a wake signal). In order:

1. Drain host→JS state patches (any `StateHandle::set` calls since last tick) and apply via `__sol_apply_state_patch` → Solid `setStore`.
2. Pump QuickJS pending jobs (Promise resolutions, queued microtasks) up to a budget — default 256 jobs per tick, configurable, so a runaway JS loop can't wedge the frame.
3. If anything mutated the Blitz document during 1 or 2, set an internal `needs_paint` flag.
4. Return `TickResult { needs_paint: bool, jobs_pending: bool }` so the host knows whether to call `render()` and whether to call `tick()` again soon.

```rust
pub struct TickResult {
    pub needs_paint: bool,
    pub jobs_pending: bool,  // queue still had work when budget hit
}

impl Instance {
    pub fn tick(&mut self) -> TickResult;
    pub fn render(&mut self) -> &wgpu::TextureView;
}
```

**The wake signal — `instance.wake()`:**

Optional `tokio::sync::Notify` for hosts that don't want to poll. Anything that mutates Rust-side state asynchronously (tokio task calls `StateHandle::set`, sendEvent triggered from JS in a background runtime, future M3+ async sources) calls `notify_one()`. The host awaits `.notified()` on its render thread or in a dedicated task and translates wakes into `window.request_redraw()` or equivalent.

```rust
impl Instance {
    pub fn wake_handle(&self) -> Arc<tokio::sync::Notify>;
}
```

Hosts that prefer pure polling can ignore this and call `tick()` every frame unconditionally.

**Typical host loop (winit):**

```rust
WindowEvent::RedrawRequested => {
    let tick = instance.tick();
    if tick.needs_paint {
        let view = instance.render();
        // composite view into surface frame
    }
    if tick.jobs_pending {
        window.request_redraw();  // come back soon
    }
}
// Separate tokio task:
let wake = instance.wake_handle();
loop {
    wake.notified().await;
    window.request_redraw();  // via EventLoopProxy
}
```

This keeps the library renderer-thread-agnostic: any host with a way to call two sync methods on a single thread, plus optionally a tokio runtime, can drive it.

## Crate layout

```
solite/
├── Cargo.toml
├── plan.md
├── src/
│   ├── lib.rs           # public API: Instance, InstanceConfig, MouseEvent, Event, StateHandle
│   ├── instance.rs      # Instance owns Document + JS context + texture + state + event tx
│   ├── renderer.rs      # Blitz + wgpu glue, transparent surface
│   ├── js/
│   │   ├── mod.rs       # rquickjs context, Solid bundle loader
│   │   ├── bridge.rs    # universal renderer ops (createElement, etc.)
│   │   ├── state.rs     # __sol_state_set / __sol_apply_state_patch glue
│   │   └── outbox.rs    # __sol_send_event → mpsc::UnboundedSender
│   ├── state.rs         # StateHandle (Clone + Send + Sync)
│   └── events.rs        # MouseEvent input + Event output type
├── js/
│   ├── runtime.ts       # Solid universal renderer setup + element table
│   └── dist/runtime.js  # bundled, vendored in repo (no build-time JS toolchain)
└── examples/
    └── winit_window.rs  # host harness — opens a 200x200 window, renders hello-world
```

## Dependency list

- `blitz-dom` — DOM + layout (pinned)
- `blitz-renderer-vello` — Vello-based painter (pinned)
- `wgpu` — GPU
- `rquickjs` — features: `loader`, `parallel`, `macro`
- `taffy` — transitively via Blitz, but explicit for layout helpers
- `tokio` — `sync` feature for `mpsc` channels (no full runtime required inside the library)
- `serde_json` — payload + state values
- `winit` — example only, dev-dep

## Milestones

### M1 — Vertical slice
**Goal:** `cargo run --example winit_window` opens a 200×200 transparent window showing "Hello from Solid" painted by Blitz.

- `Instance::new(InstanceConfig { width: 200, height: 200, device, queue })` allocates an RGBA8 texture (premultiplied alpha).
- Boots QuickJS, loads the vendored Solid bundle + a hardcoded hello-world component.
- Bridge implements 6 universal renderer ops, each mutating `blitz_dom::Document` directly:
  - `createElement(tag) → node_id`
  - `createTextNode(text) → node_id`
  - `setProperty(node_id, key, value)`
  - `insertNode(parent, node, anchor?)`
  - `removeNode(parent, node)`
  - `setText(node_id, value)`
- Blitz lays out + paints into the texture each frame.
- Public API: `instance.render() -> &wgpu::TextureView` for host compositing.
- Winit example: opens transparent window, blits the texture, proves alpha works.

**No events, no resize, no signals updating after first render.**

### M2 — State + events
**Goal:** host pushes state into JS and sees JS events on a tokio channel.

Public API additions:
```rust
pub struct Event { pub name: String, pub payload: serde_json::Value }

#[derive(Clone)]
pub struct StateHandle { /* Arc<Mutex<Value>> + Arc<AtomicBool> */ }
impl StateHandle {
    pub fn set(&self, path: &str, value: serde_json::Value);
    pub fn get(&self, path: &str) -> Option<serde_json::Value>;
    pub fn snapshot(&self) -> serde_json::Value;
}

impl Instance {
    pub fn new(cfg: InstanceConfig, src: &str)
        -> (Self, tokio::sync::mpsc::UnboundedReceiver<Event>);
    pub fn state(&self) -> StateHandle;          // Send + Sync, cloneable
    pub fn wake_handle(&self) -> Arc<tokio::sync::Notify>;
    pub fn tick(&mut self) -> TickResult;        // flush state, pump JS jobs, report needs_paint
}
```

**State sync — path-based patches:**
- Initial mount: Rust serializes state snapshot → JS wraps in `createStore`.
- JS write (`state.counter = 5`) → Solid store setter → `__sol_state_set("counter", 5)` → Rust mirror updates synchronously, no flag set (avoids feedback loop).
- Rust write (`handle.set("counter", json!(5))`) → mutex update + dirty flag set.
- `instance.tick()`: drains pending Rust-side patches, calls JS `__sol_apply_state_patch(path, value)` → `setStore(path, value)` → Solid re-renders affected nodes.
- Paths use dot notation; arrays use numeric indices (matches Solid `setStore` semantics).

**Events:**
- `sendEvent(name, payload)` JS global → `__sol_send_event(name, payload_json)` → `UnboundedSender::send(Event { name, payload })`.
- Unbounded for M2. Backpressure (bounded channel + `try_send` with logged drop) is a follow-up if real workloads need it.
- Channel is per-Instance. Receiver is returned from `Instance::new` so the type system forces the host to handle it (or `drop` explicitly).

### M3 — Mouse + onClick
**Goal:** Solid button with `onClick={() => setState("count", c => c+1)}` re-renders on click. Local handlers stay in JS; if the handler wants to notify the host, it calls `sendEvent`.

- `instance.dispatch_mouse(x, y, MouseEvent)` — hit-test via Blitz's layout, walk up to find an `on:click` handler ID, call into JS.
- Solid signals already drive bridge ops; mark document dirty, re-paint next `render()` call.
- Handler storage: each `on:*` attribute stores a stable JS function ID in a side map keyed by node_id.

### M4 — Resize + multi-instance
**Goal:** `instance.resize(w, h)` works; two Instances can share one wgpu device.

- Reallocate texture, tell Blitz the new viewport, repaint.
- Verify Vello state is per-Instance, not global.
- Add `examples/two_instances.rs`.

## Verified API seam

Checked docs.rs for blitz-dom, blitz-paint (0.2.1), anyrender_vello (0.11.0):

1. **DOM mutation** — green. `BaseDocument::new(DocumentConfig)`, `create_node(NodeData) -> usize`, `create_text_node(&str) -> usize`, `set_style_property(node_id, name, value)`, `resolve(time)`. Maps cleanly to the 6 Solid universal ops.
2. **Painting** — green. `blitz_paint::paint_scene(doc, &mut impl anyrender::PaintScene)`.
3. **Hit-testing** — green. `BaseDocument::hit(x, y) -> Option<HitResult>` is built in. M2 unblocked.
4. **GPU texture target** — yellow. `anyrender_vello` exposes `VelloImageRenderer` (CPU buffer) and `VelloWindowRenderer` (owns surface). Neither targets a `wgpu::Texture` directly. **Plan:**
   - **M1:** use `VelloImageRenderer` — one CPU→GPU upload per frame. Good enough to prove the Solid→Blitz bridge end-to-end.
   - **M2 onward:** swap in direct Vello `Renderer::render_to_texture` via a custom `PaintScene` capture, eliminating the roundtrip. Renderer is behind our own `Painter` trait so this swap is local.

No fork of Blitz needed.

## Non-goals (for now)

- Keyboard input, focus, IME
- Text selection
- Scrolling
- Animations / requestAnimationFrame
- Networking, fetch, timers beyond `setTimeout`
- Hot-reload of Solid components
- Accessibility tree
- Bounded event channel / backpressure (currently unbounded; revisit when a real workload hits it)
- Host → JS application events distinct from state (e.g. `onAppEvent("name", cb)`) — symmetric reverse channel; defer until a real use case shows up

These come after M4 ships.
