<div align="center">

# solite

**A UI library for games. native on every screen:**

**📱 iOS · iPadOS · Android &nbsp;·&nbsp; 🖥️ Windows · macOS · Linux**

</div>

![Solite Demo](./demo.gif)


---

## What is this?

Write your UI in TSX or JSX with fine-grained reactivity, lay it out with a real CSS engine, and composite it straight into your own `wgpu` scene — no browser, no Electron, no DOM diffing.

**solite** runs [SolidJS](https://www.solidjs.com/) inside an embedded [QuickJS](https://bellard.org/quickjs/) engine and renders the resulting HTML/CSS with [Blitz](https://github.com/DioxusLabs/blitz) onto a `wgpu` texture. The result is a self-contained, GPU-accelerated UI layer you can drop into a game, a creative-coding tool, a native app, or anything else
that can hand it a device and a frame loop.

It's **passive by design**: solite never owns the event loop or the window. Your
host pushes state in, drives `tick()` + `render()` each frame, and pulls events
back out. That makes it equally happy driving a winit window, rendering
headless to a PNG, or sitting on top of a 3D scene you drew yourself.

---

## A taste

The component is plain JSX with SolidJS semantics:

```tsx
// app.tsx
import { render } from "solite-runtime";

function Counter() {
  // `globalThis.state` is a reactive store, synced from Rust each frame.
  const count = () => Number(globalThis.state.count ?? 0);

  return (
    <button class="counter" onClick={() => sendEvent("increment", "{}")}>
      clicked {() => count()} times
    </button>
  );
}

render(() => <Counter />, __SOL_ROOT__);
```

The host owns the GPU and the frame loop (sketch — see `examples/` for the real thing):

```rust
use solite::{Instance, compile_component_source};

// Compile JSX → a runtime module (or use the live-reload source workflow).
let module = compile_component_source("app.tsx".as_ref(), APP_SRC)?;

// Mount it onto a wgpu device/queue you own.
let (mut ui, mut events) = Instance::new(config /* width, height, device, queue, … */, &module)?;

// Each frame: state in ⇄ events out.
loop {
    ui.state().set("count", json!(count));     // push reactive state
    ui.tick();                                  // run effects, apply patches
    let texture = ui.render();                  // paint to a wgpu texture
    blit(texture);                              // composite into your scene

    while let Ok(ev) = events.try_recv() {      // sendEvent(name, payload) from JS
        if ev.name == "increment" { count += 1; }
    }
}
```

State flows **down** through a reactive store; intent flows **up** through
events. No virtual DOM, no reconciliation pass — SolidJS updates exactly the
nodes that changed.

---

## Features

**⚡ Reactive, the SolidJS way**
- Fine-grained signals/stores/effects — no re-render, no diffing
- `globalThis.state` is a real Solid store, patched from Rust every `tick()`
- `sendEvent(name, payload)` streams typed events back into rust.

**🧩 Real HTML5 inspired widgets & input**
- Text inputs, `<input type=number>` with spinners, range sliders, checkboxes,
  radios, native `<select>` with popups, scrollbars
- Mouse, keyboard, wheel, and **touch** (gestures + fling momentum)
- **Accessibility**: a live [AccessKit](https://accesskit.dev/) tree for
  NVDA / VoiceOver / Orca (`a11y` feature)

**🔌 Embeds anywhere**
- Passive `tick()` / `render()` model — you own the loop
- First-class [winit](https://github.com/rust-windowing/winit) bridge that
  translates window events for you, or go fully headless
- Render to an offscreen texture and **capture to PNG**
- Composite the UI texture over/under your own `wgpu` content

**🛠️ Batteries-included tooling**
- Built-in **JSX/TS compiler** (`solite-build`, powered by
  [oxc](https://oxc.rs/)) — no Node toolchain required to ship
- **Live reload** of TSX/CSS in development; **AOT-bundled** for release
- `solite-build init ui` scaffolds a `build.rs` + mount templates

---

## Quickstart

```sh
# Run the flagship demo: a WebGPU driving scene under a live Solite HUD.
cargo run -p solite-drive-game

# Run an example (kitchen sink of widgets, inputs, and styling).
cargo run --example kitchen_sink --features "winit,capture,jsx-compiler"
```

Add it to your own crate:

```toml
[dependencies]
solite = { git = "https://github.com/only-cliches/solite", features = ["winit", "jsx-compiler"] }
```

---

## The source workflow

solite is built around one source tree that serves both development and release:

- **Debug** — `solite::workflow::SourceProject` mounts your `ui/` directory
  directly and watches it. Edits to TSX or CSS hot-reload; the watcher
  classifies each change as a full remount or a cheap imported-CSS swap.
- **Release** — `solite_build::workflow::bundle_for_cargo` (called from
  `build.rs`) compiles and bundles the same tree into a `VirtualSourceFile`
  baked into your binary. No filesystem, no compiler at runtime.

```sh
solite-build init ui   # scaffold build.rs + mount templates
```

The `demos/drive_game` crate wires up both paths end to end — read it as a
template.

---

## Feature flags

| Flag                 | What it adds                                                                 |
| -------------------- | --------------------------------------------------------------------------- |
| `jsx-compiler` *(default)* | The `solite-build` JSX/TS compiler + AOT bundler                      |
| `with_system_fonts` *(default)* | Load fonts from the OS                                          |
| `winit`              | `solite::winit` event-translation bridge                                     |
| `capture`            | `solite::capture` — read the rendered texture back to a PNG                  |
| `a11y`               | Live AccessKit tree + `accesskit_winit` adapter (implies `winit`)            |

solite renders on the GPU via Vello on `wgpu`, so `wgpu` is a core dependency:
you always hand an `Instance` a `Device`/`Queue` and it paints into a `wgpu`
texture you own. The `solite::gpu` module (device/queue + windowed-surface
bootstrap helpers) is always available. `winit` and `image` are pulled in only
when you ask for them.

---

## Examples

Each lives in [`examples/`](examples) and runs with
`cargo run --example <name> --features "<required>"`:

| Example                  | Shows off                                              |
| ------------------------ | ----------------------------------------------------- |
| `kitchen_sink`           | Inputs, selects, ranges, checkboxes, hover/focus, CSS |
| `todo`                   | A small reactive app with a system clipboard provider |
| `text_input`             | Text editing, selection, IME                          |
| `select_popup_capture`   | Native `<select>` popups, rendered headless to PNG    |
| `images`                 | Local, `data:`, and remote images                     |
| `custom_font`            | Registering and shaping a custom font                 |
| `two_instances`          | Multiple independent UI surfaces in one window        |
| `offscreen_capture`      | Headless render → PNG, no window                      |
| `a11y_touch`             | Accessibility tree + touch gestures                   |
| `perf_animate_elements`  | Many animated nodes, for profiling                    |

### Featured: `demos/drive_game`

A standalone crate that renders a ray-marched WebGPU driving scene and composites
a fully reactive Solite HUD on top — telemetry, gauges, a draggable max-speed
slider, and live-reloading TSX. It's the best end-to-end tour of mounting
solite, sharing a `wgpu` device, and round-tripping state and events between
Rust and JS.

```sh
cargo run -p solite-drive-game
```

---

## Building from source

- **Rust** (2024 edition) and a `wgpu`-capable backend (Vulkan / Metal / DX12).
- The [Blitz](vendor/blitz) packages are **vendored** under `vendor/` (their own
  workspace) and built from path.
- The SolidJS runtime ships pre-bundled at `js/dist/runtime.js`. Only contributors
  editing `js/runtime.ts` need to rebuild it:

  ```sh
  cd js && npx esbuild runtime.ts --bundle --format=esm --outfile=dist/runtime.js
  ```

### Measuring binary growth

`./scripts/measure_binary_size.sh` reports the release-binary delta of a minimal
program that links solite:

- `baseline` — empty binary
- `solite-core` — `solite` with `default-features = false`
- `solite-default` — `solite` with crate defaults

It deliberately excludes the `winit` and `wgpu` integrations, answering "how
much bigger does my binary get from the library itself?"

---

## Status

**Alpha.** The architecture is solid and the renderer paints real pixels, but
APIs may still shift and Blitz is pinned to a vendored pre-release. Expect sharp
edges; file issues with a minimal repro.

## Built on the shoulders of giants

[SolidJS](https://www.solidjs.com/) · [QuickJS](https://bellard.org/quickjs/) /
[rquickjs](https://github.com/DelSkayn/rquickjs) ·
[Blitz](https://github.com/DioxusLabs/blitz) ·
[Stylo](https://github.com/servo/stylo) ·
[Parley](https://github.com/linebender/parley) ·
[Vello](https://github.com/linebender/vello) ·
[wgpu](https://github.com/gfx-rs/wgpu) · [oxc](https://oxc.rs/) ·
[AccessKit](https://accesskit.dev/)
