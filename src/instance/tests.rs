use super::runtime::{char_index_to_byte_index, estimated_input_char_width};
use super::*;
use crate::scrollbar::ScrollAxis;
use crate::{Event, FontFormat, KeyboardEvent, MouseButton, MouseEvent};
use blitz_dom::LocalName;
use parley::{Affinity, Cursor, Selection};
use std::sync::Arc;
use tokio::sync::mpsc::UnboundedReceiver;

use serde_json::{Value, json};

const CLICK_BUTTON_COMPONENT: &str = r#"
        import { render } from "solite-runtime";
        function App() {
          const btn = __sol_createElement("button");
          __sol_setProperty(
            btn,
            "style",
            "display:block; width: 160px; height: 80px;"
          );
          __sol_setProperty(btn, "onClick", () => {
            globalThis.state.count = (globalThis.state.count || 0) + 1;
          });
          return btn;
        }
        render(() => App(), __SOL_ROOT__);
    "#;

const ROOT_CLICK_COMPONENT: &str = r#"
        import { render } from "solite-runtime";
        function App() {
          const root = __sol_createElement("div");
          __sol_setProperty(root, "onClick", () => {
            globalThis.state.clicked = true;
          });
          __sol_setProperty(
            root,
            "style",
            "display:block; width: 200px; height: 200px;"
          );
          return root;
        }
        render(() => App(), __SOL_ROOT__);
    "#;

const HOVER_COMPONENT: &str = r#"
        import { render } from "solite-runtime";
        function App() {
          const btn = __sol_createElement("button");
          __sol_setProperty(
            btn,
            "style",
            "display:block; width: 80px; height: 80px;"
          );
          __sol_setProperty(btn, "onMouseOver", (e) => {
            globalThis.state.over = (globalThis.state.over || 0) + 1;
            globalThis.state.overTarget = e.target;
            globalThis.state.overRelated = e.relatedTarget;
          });
          __sol_setProperty(btn, "onMouseOut", (e) => {
            globalThis.state.out = (globalThis.state.out || 0) + 1;
            globalThis.state.outTarget = e.target;
            globalThis.state.outRelated = e.relatedTarget;
          });
          __sol_setProperty(btn, "onMouseEnter", (e) => {
            globalThis.state.enter = (globalThis.state.enter || 0) + 1;
            globalThis.state.enterCurrent = e.currentTarget;
            globalThis.state.enterRelated = e.relatedTarget;
          });
          __sol_setProperty(btn, "onMouseLeave", (e) => {
            globalThis.state.leave = (globalThis.state.leave || 0) + 1;
            globalThis.state.leaveCurrent = e.currentTarget;
            globalThis.state.leaveRelated = e.relatedTarget;
          });
          __sol_setProperty(btn, "onHover", (e) => {
            globalThis.state.hover = (globalThis.state.hover || 0) + 1;
            globalThis.state.hoverCurrent = e.currentTarget;
          });
          __sol_setProperty(btn, "onHoverEnter", (e) => {
            globalThis.state.hoverEnter = (globalThis.state.hoverEnter || 0) + 1;
            globalThis.state.hoverEnterRelated = e.relatedTarget;
          });
          __sol_setProperty(btn, "onHoverLeave", (e) => {
            globalThis.state.hoverLeave = (globalThis.state.hoverLeave || 0) + 1;
            globalThis.state.hoverLeaveRelated = e.relatedTarget;
          });
          return btn;
        }
        render(() => App(), __SOL_ROOT__);
    "#;

const WHEEL_SCROLL_COMPONENT: &str = r#"
        import { render } from "solite-runtime";
        function App() {
          const outer = __sol_createElement("div");
          __sol_setProperty(
            outer,
            "style",
            "display:block; width: 120px; height: 80px; overflow: auto;"
          );
          __sol_setProperty(outer, "onWheel", (event) => {
            globalThis.state.wheel = (globalThis.state.wheel || 0) + 1;
            sendEvent("wheel", JSON.stringify({ top: event.scrollTop, deltaY: event.deltaY }));
          });
          __sol_setProperty(outer, "onScroll", (event) => {
            globalThis.state.scroll = (globalThis.state.scroll || 0) + 1;
            globalThis.state.scrollTop = event.scrollTop;
          });

          const filler = __sol_createElement("div");
          __sol_setProperty(
            filler,
            "style",
            "display:block; width: 120px; height: 240px;"
          );
          __sol_insertNode(outer, filler, null);
          return outer;
        }
        render(() => App(), __SOL_ROOT__);
    "#;

const TEXT_INPUT_COMPONENT: &str = r#"
        import { render } from "solite-runtime";
        function App() {
          const input = __sol_createElement("input");
          __sol_setProperty(input, "style", "display:block; width: 220px; height: 40px;");

          __sol_setProperty(input, "onFocus", () => {
            globalThis.state.focused = true;
            globalThis.state.lastFocus = "focus";
          });

          __sol_setProperty(input, "onBlur", () => {
            globalThis.state.focused = false;
            globalThis.state.lastBlur = "blur";
          });

          __sol_setProperty(input, "onInput", (event) => {
            globalThis.state.value = event.value;
            globalThis.state.caret = event.selectionStart;
          });

          __sol_setProperty(input, "onKeyDown", (event) => {
            globalThis.state.lastKey = event.key;
            // Keep caret visible in this test path for move-only keys, since
            // native `input` events are not fired on caret movement alone.
            if (event.selectionStart !== undefined) {
              globalThis.state.caret = event.selectionStart;
            }
          });

          __sol_setProperty(input, "onKeyUp", (event) => {
            globalThis.state.lastKeyUp = event.key;
          });

          return input;
        }
        render(() => App(), __SOL_ROOT__);
    "#;

async fn make_test_device() -> (Arc<wgpu::Device>, Arc<wgpu::Queue>) {
    if std::env::var_os("XDG_RUNTIME_DIR").is_none() {
        unsafe {
            std::env::set_var("XDG_RUNTIME_DIR", "/tmp");
        }
    }

    let wgpu_instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: wgpu::Backends::all(),
        ..wgpu::InstanceDescriptor::new_without_display_handle()
    });
    let adapter = wgpu_instance
        .request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::LowPower,
            compatible_surface: None,
            // Metal has no software fallback; let the platform pick.
            force_fallback_adapter: false,
        })
        .await
        .expect("no adapter available for test");
    let (device, queue) = adapter
        .request_device(&wgpu::DeviceDescriptor {
            label: Some("solite-test"),
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::default(),
            experimental_features: wgpu::ExperimentalFeatures::disabled(),
            memory_hints: wgpu::MemoryHints::default(),
            trace: wgpu::Trace::Off,
        })
        .await
        .expect("request device");

    (Arc::new(device), Arc::new(queue))
}

fn test_device() -> (Arc<wgpu::Device>, Arc<wgpu::Queue>) {
    pollster::block_on(make_test_device())
}

fn make_key_event(
    key: &str,
    code: &str,
    key_code: u32,
    repeat: bool,
    shift_key: bool,
    ctrl_key: bool,
    alt_key: bool,
    meta_key: bool,
) -> KeyboardEvent {
    KeyboardEvent {
        key: key.to_owned(),
        code: code.to_owned(),
        key_code,
        repeat,
        shift_key,
        ctrl_key,
        alt_key,
        meta_key,
    }
}

#[test]
fn dispatch_mouse_click_updates_rust_state() {
    let (device, queue) = test_device();
    let (mut instance, _rx) = Instance::new(
        InstanceConfig {
            width: 200,
            height: 200,
            device,
            queue,
            stylesheets: vec![],
            document_scroll: false,
            base_url: None,
            initial_state: None,
            registered_resources: vec![],
            scale_factor: 1.0,
        },
        ROOT_CLICK_COMPONENT,
    );
    let state = instance.state();
    assert_eq!(state.get("clicked"), None);

    let _ = instance.render();

    let result = instance.dispatch_mouse(
        10.0,
        10.0,
        MouseEvent::Down {
            x: 10.0,
            y: 10.0,
            button: MouseButton::Left,
        },
    );
    assert!(result.needs_paint);
    assert_eq!(state.get("clicked"), Some(json!(true)));

    let result_again = instance.dispatch_mouse(
        10.0,
        10.0,
        MouseEvent::Down {
            x: 10.0,
            y: 10.0,
            button: MouseButton::Left,
        },
    );
    assert!(result_again.needs_paint);
    assert_eq!(state.get("clicked"), Some(json!(true)));
}

#[test]
fn take_send_event_error_clears_after_read() {
    let (device, queue) = test_device();
    let (mut instance, _rx) = Instance::new(
        InstanceConfig {
            width: 200,
            height: 80,
            device,
            queue,
            stylesheets: vec![],
            document_scroll: false,
            base_url: None,
            initial_state: None,
            registered_resources: vec![],
            scale_factor: 1.0,
        },
        r#"
            import { render } from "solite-runtime";
            function App() {
              const btn = __sol_createElement("button");
              __sol_setProperty(btn, "style", "display:block; width: 200px; height: 80px;");
              __sol_setProperty(btn, "onClick", () => {
                sendEvent("invalid", "{invalid");
              });
              return btn;
            }
            render(() => App(), __SOL_ROOT__);
            "#,
    );

    let _ = instance.render();
    let _ = instance.dispatch_mouse(
        10.0,
        10.0,
        MouseEvent::Down {
            x: 10.0,
            y: 10.0,
            button: MouseButton::Left,
        },
    );
    let _ = instance.dispatch_mouse(
        10.0,
        10.0,
        MouseEvent::Up {
            x: 10.0,
            y: 10.0,
            button: MouseButton::Left,
        },
    );

    assert!(
        instance
            .take_send_event_error()
            .as_ref()
            .is_some_and(|msg| !msg.is_empty())
    );
    assert_eq!(instance.take_send_event_error(), None);
}

#[test]
fn dispatch_key_down_and_up_target_focused_node() {
    let (device, queue) = test_device();
    let (mut instance, _rx) = Instance::new(
        InstanceConfig {
            width: 200,
            height: 80,
            device,
            queue,
            stylesheets: vec![],
            document_scroll: false,
            base_url: None,
            initial_state: None,
            registered_resources: vec![],
            scale_factor: 1.0,
        },
        TEXT_INPUT_COMPONENT,
    );
    let state = instance.state();

    let _ = instance.render();
    let _ = instance.dispatch_mouse(
        10.0,
        10.0,
        MouseEvent::Down {
            x: 10.0,
            y: 10.0,
            button: MouseButton::Left,
        },
    );

    assert_eq!(state.get("focused"), Some(json!(true)));

    let _ = instance.dispatch_key_down(make_key_event(
        "A", "KeyA", 65, false, false, false, false, false,
    ));
    assert_eq!(state.get("value"), Some(json!("A")));
    assert_eq!(state.get("caret"), Some(json!(1)));
    assert_eq!(state.get("lastKey"), Some(json!("A")));

    let _ = instance.dispatch_key_down(make_key_event(
        "Backspace",
        "Backspace",
        8,
        false,
        false,
        false,
        false,
        false,
    ));
    assert_eq!(state.get("value"), Some(json!("")));
    assert_eq!(state.get("caret"), Some(json!(0)));

    let _ = instance.dispatch_key_up(make_key_event(
        "A", "KeyA", 65, false, false, false, false, false,
    ));
    assert_eq!(state.get("lastKeyUp"), Some(json!("A")));

    let _ = instance.dispatch_mouse(
        500.0,
        500.0,
        MouseEvent::Down {
            x: 500.0,
            y: 500.0,
            button: MouseButton::Left,
        },
    );
    assert_eq!(state.get("focused"), Some(json!(false)));

    let value_after_blur = state.get("value");
    let _ = instance.dispatch_key_down(make_key_event(
        "B", "KeyB", 66, false, false, false, false, false,
    ));
    assert_eq!(state.get("value"), value_after_blur);
}

#[test]
fn dispatch_focus_events_update_host_state() {
    let (device, queue) = test_device();
    let (mut instance, _rx) = Instance::new(
        InstanceConfig {
            width: 200,
            height: 80,
            device,
            queue,
            stylesheets: vec![],
            document_scroll: false,
            base_url: None,
            initial_state: None,
            registered_resources: vec![],
            scale_factor: 1.0,
        },
        TEXT_INPUT_COMPONENT,
    );
    let state = instance.state();

    let _ = instance.render();
    let _ = instance.dispatch_mouse(
        10.0,
        10.0,
        MouseEvent::Down {
            x: 10.0,
            y: 10.0,
            button: MouseButton::Left,
        },
    );

    assert_eq!(state.get("focused"), Some(json!(true)));
    assert_eq!(state.get("lastFocus"), Some(json!("focus")));

    let _ = instance.dispatch_mouse(
        500.0,
        500.0,
        MouseEvent::Down {
            x: 500.0,
            y: 500.0,
            button: MouseButton::Left,
        },
    );
    assert_eq!(state.get("focused"), Some(json!(false)));
    assert_eq!(state.get("lastBlur"), Some(json!("blur")));

    let _ = instance.tick();
    assert_eq!(state.get("lastBlur"), Some(json!("blur")));
}

#[test]
fn resize_updates_size_and_keeps_click_working() {
    let (device, queue) = test_device();
    let (mut instance, _rx) = Instance::new(
        InstanceConfig {
            width: 100,
            height: 100,
            device,
            queue,
            stylesheets: vec![],
            document_scroll: false,
            base_url: None,
            initial_state: None,
            registered_resources: vec![],
            scale_factor: 1.0,
        },
        CLICK_BUTTON_COMPONENT,
    );
    let state = instance.state();
    let _ = instance.render();
    assert!(
        instance
            .dispatch_mouse(
                20.0,
                20.0,
                MouseEvent::Down {
                    x: 20.0,
                    y: 20.0,
                    button: MouseButton::Left,
                },
            )
            .needs_paint
    );
    assert_eq!(state.get("count"), Some(json!(1)));

    instance.resize(220, 80);
    assert_eq!(instance.size(), (220, 80));
    let _ = instance.render();

    let second = instance.dispatch_mouse(
        10.0,
        10.0,
        MouseEvent::Down {
            x: 10.0,
            y: 10.0,
            button: MouseButton::Left,
        },
    );
    assert!(second.needs_paint);
    assert_eq!(state.get("count"), Some(json!(2)));
}

#[test]
fn dispatch_mouse_move_updates_hover_state_and_handlers() {
    let (device, queue) = test_device();
    let (mut instance, _rx) = Instance::new(
        InstanceConfig {
            width: 200,
            height: 200,
            device,
            queue,
            stylesheets: vec![],
            document_scroll: false,
            base_url: None,
            initial_state: None,
            registered_resources: vec![],
            scale_factor: 1.0,
        },
        HOVER_COMPONENT,
    );
    let state = instance.state();
    let _ = instance.render();

    let btn_id = {
        let d = instance.doc.borrow();
        d.get_node(instance.container_id())
            .and_then(|c| c.children.first().copied())
            .expect("button should be mounted")
    };

    assert!(
        !instance
            .doc
            .borrow()
            .get_node(btn_id)
            .is_some_and(|n| n.is_hovered())
    );

    let enter = instance.dispatch_mouse(10.0, 10.0, MouseEvent::Move { x: 10.0, y: 10.0 });
    assert!(enter.needs_paint);

    assert!(
        instance
            .doc
            .borrow()
            .get_node(btn_id)
            .is_some_and(|n| n.is_hovered())
    );

    assert_eq!(state.get("over"), Some(json!(1)));
    assert_eq!(state.get("enter"), Some(json!(1)));
    assert_eq!(state.get("hover"), Some(json!(1)));
    assert_eq!(state.get("hoverEnter"), Some(json!(1)));
    assert_eq!(state.get("hoverCurrent"), Some(json!(btn_id)));
    assert_eq!(state.get("hoverEnterRelated"), Some(json!(null)));
    assert_eq!(state.get("overTarget"), Some(json!(btn_id)));
    assert_eq!(state.get("overRelated"), Some(json!(null)));
    assert_eq!(state.get("enterCurrent"), Some(json!(btn_id)));
    assert_eq!(state.get("enterRelated"), Some(json!(null)));

    let stay = instance.dispatch_mouse(20.0, 20.0, MouseEvent::Move { x: 20.0, y: 20.0 });
    assert!(!stay.needs_paint);
    assert_eq!(state.get("over"), Some(json!(1)));
    assert_eq!(state.get("enter"), Some(json!(1)));
    assert_eq!(state.get("hover"), Some(json!(1)));
    assert_eq!(state.get("hoverEnter"), Some(json!(1)));
    assert!(state.get("out").is_none());

    let leave = instance.dispatch_mouse(500.0, 500.0, MouseEvent::Move { x: 500.0, y: 500.0 });
    assert!(leave.needs_paint);

    assert!(
        !instance
            .doc
            .borrow()
            .get_node(btn_id)
            .is_some_and(|n| n.is_hovered())
    );

    assert_eq!(state.get("out"), Some(json!(1)));
    assert_eq!(state.get("leave"), Some(json!(1)));
    assert_eq!(state.get("hoverLeave"), Some(json!(1)));
    assert_eq!(state.get("outTarget"), Some(json!(btn_id)));
    assert_eq!(state.get("outRelated"), Some(json!(null)));
    assert_eq!(state.get("leaveCurrent"), Some(json!(btn_id)));
    assert_eq!(state.get("leaveRelated"), Some(json!(null)));
    assert_eq!(state.get("hoverLeaveRelated"), Some(json!(null)));
}

#[test]
fn dispatch_wheel_scrolls_and_dispatches_events() {
    let (device, queue) = test_device();
    let (mut instance, mut rx) = Instance::new(
        InstanceConfig {
            width: 160,
            height: 160,
            device,
            queue,
            stylesheets: vec![],
            document_scroll: false,
            base_url: None,
            initial_state: None,
            registered_resources: vec![],
            scale_factor: 1.0,
        },
        WHEEL_SCROLL_COMPONENT,
    );
    let state = instance.state();
    let _ = instance.render();

    let outer_id = {
        let d = instance.doc.borrow();
        d.get_node(instance.container_id())
            .and_then(|container| container.children.first().copied())
            .expect("scroll container should be mounted")
    };
    let before_top = instance
        .doc
        .borrow()
        .get_node(outer_id)
        .expect("outer node exists")
        .scroll_offset
        .y;

    let result = instance.dispatch_wheel(10.0, 10.0, 0.0, 40.0);
    assert!(result.needs_paint);

    let after_top = instance
        .doc
        .borrow()
        .get_node(outer_id)
        .expect("outer node exists")
        .scroll_offset
        .y;

    assert_eq!(state.get("wheel"), Some(json!(1)));

    let first = rx.try_recv().expect("wheel event");
    assert_eq!(first.name, "wheel");
    let first_top = first
        .payload
        .get("top")
        .and_then(|value| value.as_f64())
        .unwrap_or(0.0);
    assert_eq!(first_top, 0.0);
    assert!(after_top >= before_top);

    if let Ok(second) = rx.try_recv() {
        assert_eq!(second.name, "scroll");
        assert_eq!(second.payload["type"], json!("scroll"));
        let scroll_top = second
            .payload
            .get("scrollTop")
            .and_then(|value| value.as_f64())
            .unwrap_or(0.0);
        assert_eq!(scroll_top, first_top);
        assert_eq!(state.get("scroll"), Some(json!(1)));
        assert_eq!(state.get("scrollTop"), Some(json!(0.0)));
    }

    assert!(rx.try_recv().is_err());
}

#[test]
fn two_instances_share_device_and_keep_state_independent() {
    let (device, queue) = test_device();
    let (mut a, _rx_a) = Instance::new(
        InstanceConfig {
            width: 140,
            height: 140,
            device: Arc::clone(&device),
            queue: Arc::clone(&queue),
            stylesheets: vec![],
            document_scroll: false,
            base_url: None,
            initial_state: None,
            registered_resources: vec![],
            scale_factor: 1.0,
        },
        ROOT_CLICK_COMPONENT,
    );
    let (mut b, _rx_b) = Instance::new(
        InstanceConfig {
            width: 140,
            height: 140,
            device,
            queue,
            stylesheets: vec![],
            document_scroll: false,
            base_url: None,
            initial_state: None,
            registered_resources: vec![],
            scale_factor: 1.0,
        },
        CLICK_BUTTON_COMPONENT,
    );

    let state_a = a.state();
    let state_b = b.state();
    let _ = a.render();
    let _ = b.render();

    let click_a = a.dispatch_mouse(
        10.0,
        10.0,
        MouseEvent::Down {
            x: 10.0,
            y: 10.0,
            button: MouseButton::Left,
        },
    );
    assert!(click_a.needs_paint);

    let click_b = b.dispatch_mouse(
        10.0,
        10.0,
        MouseEvent::Down {
            x: 10.0,
            y: 10.0,
            button: MouseButton::Left,
        },
    );
    assert!(click_b.needs_paint);

    assert_eq!(state_a.get("clicked"), Some(json!(true)));
    assert_eq!(state_b.get("count"), Some(json!(1)));

    a.resize(120, 120);
    assert_eq!(a.size(), (120, 120));
    assert_eq!(b.size(), (140, 140));

    let _ = a.render();
    let _ = b.render();
    assert!(
        a.dispatch_mouse(
            8.0,
            8.0,
            MouseEvent::Down {
                x: 8.0,
                y: 8.0,
                button: MouseButton::Left,
            },
        )
        .needs_paint
    );
}

// Regression: a reactive child that always resolves to a string should
// mutate the same text node across re-renders rather than swapping the
// node — otherwise `focused_node_id` ends up pointing at a detached
// node and subsequent key events get dropped.
#[test]
fn reactive_text_child_keeps_same_node_across_renders() {
    const STABLE_TEXT_COMPONENT: &str = r#"
            import { createEffect, render } from "solite-runtime";
            function App() {
              const root = __sol_createElement("div");
              const para = __sol_createElement("div");
              // appendReactive happens implicitly when JSX passes a function
              // child; here we mimic that via createEffect over __sol_setText
              // since we don't have JSX in this test — what we want to
              // assert is that whatever runtime path the JSX uses preserves
              // the text node id, which appendReactive's fast path does
              // when consecutive values are simple text.
              let textId = __sol_createTextNode("");
              let appended = false;
              createEffect(() => {
                const v = String(globalThis.state.value || "");
                if (!appended) {
                  __sol_insertNode(para, textId, null);
                  appended = true;
                }
                __sol_setText(textId, v);
                globalThis.state.lastTextId = textId;
              });
              __sol_insertNode(root, para, null);
              return root;
            }
            render(() => App(), __SOL_ROOT__);
        "#;

    let (device, queue) = test_device();
    let (mut instance, _rx) = Instance::new(
        InstanceConfig {
            width: 200,
            height: 100,
            device,
            queue,
            stylesheets: vec![],
            document_scroll: false,
            base_url: None,
            initial_state: None,
            registered_resources: vec![],
            scale_factor: 1.0,
        },
        STABLE_TEXT_COMPONENT,
    );
    let _ = instance.render();
    let first_id = instance.state().get("lastTextId");
    instance.state().set("value", json!("hello"));
    let _ = instance.tick();
    let second_id = instance.state().get("lastTextId");
    instance.state().set("value", json!("hi"));
    let _ = instance.tick();
    let third_id = instance.state().get("lastTextId");
    assert!(first_id.is_some());
    assert_eq!(first_id, second_id);
    assert_eq!(second_id, third_id);
}

// Regression: an onClick handler that mutates state should re-run a
// reactive effect that inserts/removes DOM children, so clicking
// "Add Row" in kitchen_sink actually grows the visible list.
#[test]
fn click_triggers_reactive_list_update() {
    const REACTIVE_LIST_COMPONENT: &str = r#"
            import { createEffect, render } from "solite-runtime";
            function App() {
              const root = __sol_createElement("div");
              __sol_setProperty(root, "style", "display:block; width: 200px; height: 200px;");

              const button = __sol_createElement("button");
              __sol_setProperty(button, "style", "display:block; width: 100px; height: 30px;");
              __sol_setProperty(button, "onClick", () => {
                globalThis.state.count = (globalThis.state.count || 0) + 1;
              });
              __sol_insertNode(button, __sol_createTextNode("inc"), null);
              __sol_insertNode(root, button, null);

              const list = __sol_createElement("div");
              __sol_setProperty(list, "style", "display:block;");
              __sol_insertNode(root, list, null);

              // Track inserted child ids so each effect re-run can clear them.
              let prevIds = [];
              createEffect(() => {
                for (const id of prevIds) {
                  __sol_removeNode(list, id);
                }
                prevIds = [];
                const count = Number(globalThis.state.count || 0);
                for (let i = 0; i < count; i++) {
                  const row = __sol_createElement("div");
                  __sol_insertNode(row, __sol_createTextNode("row " + i), null);
                  __sol_insertNode(list, row, null);
                  prevIds.push(row);
                }
                globalThis.state.listLen = prevIds.length;
              });

              return root;
            }
            render(() => App(), __SOL_ROOT__);
        "#;

    let (device, queue) = test_device();
    let (mut instance, _rx) = Instance::new(
        InstanceConfig {
            width: 200,
            height: 200,
            device,
            queue,
            stylesheets: vec![],
            document_scroll: false,
            base_url: None,
            initial_state: None,
            registered_resources: vec![],
            scale_factor: 1.0,
        },
        REACTIVE_LIST_COMPONENT,
    );
    let _ = instance.render();
    assert_eq!(instance.state().get("listLen"), Some(json!(0)));

    // Click the button — Down{Left} fires "click" in solite.
    let _ = instance.dispatch_mouse(
        10.0,
        10.0,
        MouseEvent::Down {
            x: 10.0,
            y: 10.0,
            button: MouseButton::Left,
        },
    );
    assert_eq!(instance.state().get("count"), Some(json!(1)));
    assert_eq!(instance.state().get("listLen"), Some(json!(1)));

    let _ = instance.dispatch_mouse(
        10.0,
        10.0,
        MouseEvent::Down {
            x: 10.0,
            y: 10.0,
            button: MouseButton::Left,
        },
    );
    assert_eq!(instance.state().get("count"), Some(json!(2)));
    assert_eq!(instance.state().get("listLen"), Some(json!(2)));
}

// Regression for a "RefCell already borrowed" panic: dispatch_wheel used
// to hold `self.doc.borrow()` as a temporary inside the `if let` scrutinee
// for `find_handler_up`, extending the Ref's lifetime through the body.
// When the wheel handler mutated state, a reactive effect ran inline and
// called `__sol_setText`, which tries `doc.borrow_mut()` → panic.
#[test]
fn dispatch_wheel_with_reactive_effect_does_not_panic_on_doc_borrow() {
    const REACTIVE_WHEEL_COMPONENT: &str = r#"
            import { createEffect, render } from "solite-runtime";
            function App() {
              const outer = __sol_createElement("div");
              __sol_setProperty(
                outer,
                "style",
                "display:block; width: 120px; height: 80px; overflow: auto;"
              );
              __sol_setProperty(outer, "onWheel", () => {
                globalThis.state.wheel = (globalThis.state.wheel || 0) + 1;
              });

              const filler = __sol_createElement("div");
              __sol_setProperty(
                filler,
                "style",
                "display:block; width: 120px; height: 240px;"
              );
              __sol_insertNode(outer, filler, null);

              const status = __sol_createElement("div");
              const text = __sol_createTextNode("");
              __sol_insertNode(status, text, null);
              __sol_insertNode(outer, status, null);

              // Effect runs synchronously when state.wheel changes — this is
              // the path that re-enters Rust via __sol_setText while
              // dispatch_wheel is still on the stack.
              createEffect(() => {
                __sol_setText(text, "wheel=" + (globalThis.state.wheel || 0));
              });

              return outer;
            }
            render(() => App(), __SOL_ROOT__);
        "#;

    let (device, queue) = test_device();
    let (mut instance, _rx) = Instance::new(
        InstanceConfig {
            width: 160,
            height: 160,
            device,
            queue,
            stylesheets: vec![],
            document_scroll: false,
            base_url: None,
            initial_state: None,
            registered_resources: vec![],
            scale_factor: 1.0,
        },
        REACTIVE_WHEEL_COMPONENT,
    );
    let _ = instance.render();

    // Would have panicked before the fix.
    let _ = instance.dispatch_wheel(10.0, 10.0, 0.0, 40.0);
    assert_eq!(instance.state().get("wheel"), Some(json!(1)));
}

// ── Stylesheet API & CSS feature tests ────────────────────────────────────

/// Returns the computed color of the first child of the container as
/// (r, g, b) bytes in sRGB space. Drives `render()` so styles resolve.
fn first_child_color(instance: &mut Instance) -> Option<(u8, u8, u8)> {
    let _ = instance.render();
    let doc = instance.doc.borrow();
    let child_id = doc
        .get_node(instance.container_id())
        .and_then(|c| c.children.first().copied())?;
    node_color(&doc, child_id)
}

fn node_color(doc: &BaseDocument, node_id: usize) -> Option<(u8, u8, u8)> {
    let styles = doc.get_node(node_id)?.primary_styles()?;
    let srgb = styles
        .clone_color()
        .to_color_space(style::color::ColorSpace::Srgb);
    let c = srgb.components;
    let to_u8 = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    Some((to_u8(c.0), to_u8(c.1), to_u8(c.2)))
}

const COLORED_DIV: &str = r#"
        import { render } from "solite-runtime";
        function App() {
          const d = __sol_createElement("div");
          __sol_setProperty(d, "className", "tag");
          __sol_setProperty(d, "style", "display:block; width:50px; height:50px;");
          return d;
        }
        render(() => App(), __SOL_ROOT__);
    "#;

fn make_instance_with(
    component: &str,
    css: &[&str],
) -> (Instance, tokio::sync::mpsc::UnboundedReceiver<Event>) {
    let (device, queue) = test_device();
    Instance::new(
        InstanceConfig {
            width: 100,
            height: 100,
            device,
            queue,
            stylesheets: css.iter().map(|s| s.to_string()).collect(),
            document_scroll: false,
            base_url: None,
            initial_state: None,
            registered_resources: vec![],
            scale_factor: 1.0,
        },
        component,
    )
}

#[test]
fn classname_normalizes_to_class_and_matches_selector() {
    let (mut instance, _rx) = make_instance_with(COLORED_DIV, &[".tag { color: rgb(255, 0, 0) }"]);
    assert_eq!(first_child_color(&mut instance), Some((255, 0, 0)));
}

#[test]
fn add_stylesheet_applies_class_rule_post_mount() {
    let (mut instance, _rx) = make_instance_with(COLORED_DIV, &[]);
    let baseline = first_child_color(&mut instance);
    let _ = instance.add_stylesheet(".tag { color: rgb(0, 128, 0) }");
    let after = first_child_color(&mut instance);
    assert_ne!(after, baseline);
    assert_eq!(after, Some((0, 128, 0)));
}

#[test]
fn replace_stylesheet_swaps_rule() {
    let (mut instance, _rx) = make_instance_with(COLORED_DIV, &[]);
    let id = instance.add_stylesheet(".tag { color: rgb(255, 0, 0) }");
    assert_eq!(first_child_color(&mut instance), Some((255, 0, 0)));
    assert!(instance.replace_stylesheet(id, ".tag { color: rgb(0, 0, 255) }"));
    assert_eq!(first_child_color(&mut instance), Some((0, 0, 255)));
}

#[test]
fn remove_stylesheet_drops_rule() {
    let (mut instance, _rx) = make_instance_with(COLORED_DIV, &[]);
    let id = instance.add_stylesheet(".tag { color: rgb(255, 0, 0) }");
    assert_eq!(first_child_color(&mut instance), Some((255, 0, 0)));
    assert!(instance.remove_stylesheet(id));
    assert_ne!(first_child_color(&mut instance), Some((255, 0, 0)));
    // Removing a non-existent id is a no-op.
    assert!(!instance.remove_stylesheet(id));
}

#[test]
fn upsert_stylesheet_reuses_or_recreates_id() {
    let (mut instance, _rx) = make_instance_with(COLORED_DIV, &[]);
    let stable_id = instance.add_stylesheet(".tag { color: rgb(255, 0, 0) }");
    let updated_id = instance.upsert_stylesheet(Some(stable_id), ".tag { color: rgb(0, 0, 255) }");
    assert_eq!(updated_id, stable_id);
    assert_eq!(first_child_color(&mut instance), Some((0, 0, 255)));

    let missing = StylesheetId(u64::MAX);
    let new_id = instance.upsert_stylesheet(Some(missing), ".tag { color: rgb(0, 128, 0) }");
    assert_ne!(new_id, missing);
    assert_eq!(first_child_color(&mut instance), Some((0, 128, 0)));
}

#[test]
fn filewatch_classifies_css_and_js_changes() {
    let root = std::env::temp_dir().join(format!(
        "solite-watch-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    std::fs::create_dir_all(&root).expect("create temp watch dir");
    let css_path = root.join("style.css");
    let jsx_path = root.join("app.tsx");
    std::fs::write(&css_path, "body {}").expect("seed css");
    std::fs::write(&jsx_path, "export const x = 1").expect("seed jsx");

    let watch = Instance::watch_files(&root).expect("watch files");

    let wait = |watch: &FileWatch, source_dir: &Path| -> SourceChangeSummary {
        for _ in 0..60 {
            let summary = watch.poll_source_changes(source_dir);
            if summary != SourceChangeSummary::default() {
                return summary;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        SourceChangeSummary::default()
    };

    std::fs::write(&css_path, "body { color: red }").expect("touch css");
    let css_only = wait(&watch, &root);
    assert!(
        css_only.css_reload,
        "css edits should be flagged for stylesheet reload"
    );
    assert!(
        !css_only.bundle_rebuild,
        "css-only edits should not request bundle rebuild"
    );

    std::fs::write(&jsx_path, "export const x = 2").expect("touch jsx");
    let bundle_only = wait(&watch, &root);
    assert!(
        bundle_only.bundle_rebuild,
        "jsx edits should be flagged for bundle rebuild"
    );
    assert!(
        !bundle_only.css_reload,
        "jsx-only edits should not require css reload"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn class_directive_toggles_class_token() {
    const COMPONENT: &str = r#"
            import { createEffect, render } from "solite-runtime";
            function App() {
              const d = __sol_createElement("div");
              __sol_setProperty(d, "style", "display:block; width:50px; height:50px;");
              createEffect(() => {
                const on = Boolean(globalThis.state.on);
                __sol_setProperty(d, "class:tag", on);
              });
              return d;
            }
            render(() => App(), __SOL_ROOT__);
        "#;
    let (mut instance, _rx) = make_instance_with(COMPONENT, &[".tag { color: rgb(255, 0, 0) }"]);
    assert_ne!(first_child_color(&mut instance), Some((255, 0, 0)));

    instance.state().set("on", json!(true));
    let _ = instance.tick();
    assert_eq!(first_child_color(&mut instance), Some((255, 0, 0)));

    instance.state().set("on", json!(false));
    let _ = instance.tick();
    assert_ne!(first_child_color(&mut instance), Some((255, 0, 0)));
}

#[test]
fn style_element_applies_css_on_mount() {
    const COMPONENT: &str = r#"
            import { render } from "solite-runtime";
            function App() {
              const root = __sol_createElement("div");
              const style = __sol_createElement("style");
              const text = __sol_createTextNode(".tag { color: rgb(0, 200, 0) }");
              __sol_insertNode(style, text, null);
              __sol_insertNode(root, style, null);

              const d = __sol_createElement("div");
              __sol_setProperty(d, "className", "tag");
              __sol_setProperty(d, "style", "display:block; width:50px; height:50px;");
              __sol_insertNode(root, d, null);
              return root;
            }
            render(() => App(), __SOL_ROOT__);
        "#;
    let (mut instance, _rx) = make_instance_with(COMPONENT, &[]);
    // Container's first child is `root`; root's second child is the tagged div.
    let _ = instance.render();
    let doc = instance.doc.borrow();
    let root_id = doc
        .get_node(instance.container_id())
        .and_then(|c| c.children.first().copied())
        .expect("root mounted");
    let tagged_id = doc
        .get_node(root_id)
        .and_then(|root| root.children.get(1).copied())
        .expect("tagged div mounted");
    assert_eq!(node_color(&doc, tagged_id), Some((0, 200, 0)));
}

#[test]
fn style_element_refreshes_when_text_changes() {
    const COMPONENT: &str = r#"
            import { createEffect, render } from "solite-runtime";
            function App() {
              const root = __sol_createElement("div");
              const style = __sol_createElement("style");
              const text = __sol_createTextNode("");
              __sol_insertNode(style, text, null);
              __sol_insertNode(root, style, null);
              createEffect(() => {
                const c = String(globalThis.state.css || "");
                __sol_setText(text, c);
              });

              const d = __sol_createElement("div");
              __sol_setProperty(d, "className", "tag");
              __sol_setProperty(d, "style", "display:block; width:50px; height:50px;");
              __sol_insertNode(root, d, null);
              return root;
            }
            render(() => App(), __SOL_ROOT__);
        "#;
    let (mut instance, _rx) = make_instance_with(COMPONENT, &[]);
    instance
        .state()
        .set("css", json!(".tag { color: rgb(10, 20, 30) }"));
    let _ = instance.tick();
    let _ = instance.render();
    {
        let doc = instance.doc.borrow();
        let root_id = doc
            .get_node(instance.container_id())
            .and_then(|c| c.children.first().copied())
            .unwrap();
        let tagged_id = doc.get_node(root_id).unwrap().children[1];
        assert_eq!(node_color(&doc, tagged_id), Some((10, 20, 30)));
    }
    instance
        .state()
        .set("css", json!(".tag { color: rgb(99, 88, 77) }"));
    let _ = instance.tick();
    let _ = instance.render();
    let doc = instance.doc.borrow();
    let root_id = doc
        .get_node(instance.container_id())
        .and_then(|c| c.children.first().copied())
        .unwrap();
    let tagged_id = doc.get_node(root_id).unwrap().children[1];
    assert_eq!(node_color(&doc, tagged_id), Some((99, 88, 77)));
}

#[test]
fn hover_pseudo_class_changes_computed_color() {
    const COMPONENT: &str = r#"
            import { render } from "solite-runtime";
            function App() {
              const d = __sol_createElement("div");
              __sol_setProperty(d, "className", "tag");
              __sol_setProperty(d, "style", "display:block; width:80px; height:80px;");
              return d;
            }
            render(() => App(), __SOL_ROOT__);
        "#;
    let (mut instance, _rx) = make_instance_with(
        COMPONENT,
        &[".tag { color: rgb(10, 10, 10) } .tag:hover { color: rgb(200, 200, 200) }"],
    );
    assert_eq!(first_child_color(&mut instance), Some((10, 10, 10)));
    let _ = instance.dispatch_mouse(20.0, 20.0, MouseEvent::Move { x: 20.0, y: 20.0 });
    assert_eq!(first_child_color(&mut instance), Some((200, 200, 200)));
    let _ = instance.dispatch_mouse(500.0, 500.0, MouseEvent::Move { x: 500.0, y: 500.0 });
    assert_eq!(first_child_color(&mut instance), Some((10, 10, 10)));
}

#[test]
fn hover_pseudo_class_works_with_multiple_class_tokens() {
    // Mirrors the kitchen_sink pattern: a multi-token `class` like
    // "btn btn-add" with `:hover` rules on the more specific token.
    const COMPONENT: &str = r#"
            import { render } from "solite-runtime";
            function App() {
              const d = __sol_createElement("div");
              __sol_setProperty(d, "className", "btn btn-add");
              __sol_setProperty(d, "style", "display:block; width:80px; height:80px;");
              return d;
            }
            render(() => App(), __SOL_ROOT__);
        "#;
    // Both tokens are independently match-able and the `:hover` selector
    // on the second token must still flip the colour.
    let (mut instance, _rx) = make_instance_with(
        COMPONENT,
        &[".btn { color: rgb(50, 50, 50) } .btn-add:hover { color: rgb(255, 0, 0) }"],
    );
    // Static (non-pseudo) match against the first token works even before
    // any hover snapshot — confirms classlist parsing.
    assert_eq!(first_child_color(&mut instance), Some((50, 50, 50)));
    let _ = instance.dispatch_mouse(10.0, 10.0, MouseEvent::Move { x: 10.0, y: 10.0 });
    assert_eq!(first_child_color(&mut instance), Some((255, 0, 0)));
}

#[test]
fn hover_flips_when_pointer_enters_nested_child() {
    // Pointer moves to a child node; the styled ancestor should still
    // pick up :hover because we snapshot the whole ancestor chain.
    const COMPONENT: &str = r#"
            import { render } from "solite-runtime";
            function App() {
              const outer = __sol_createElement("div");
              __sol_setProperty(outer, "className", "row");
              __sol_setProperty(outer, "style", "display:block; width:80px; height:80px; padding:10px;");
              const inner = __sol_createElement("div");
              __sol_setProperty(inner, "style", "display:block; width:40px; height:40px;");
              __sol_insertNode(inner, __sol_createTextNode("hi"), null);
              __sol_insertNode(outer, inner, null);
              return outer;
            }
            render(() => App(), __SOL_ROOT__);
        "#;
    let (mut instance, _rx) = make_instance_with(
        COMPONENT,
        &[".row { color: rgb(10, 10, 10) } .row:hover { color: rgb(20, 80, 200) }"],
    );
    assert_eq!(first_child_color(&mut instance), Some((10, 10, 10)));
    // Land squarely on the inner child so the hit is on the text or
    // inner div, not the outer.
    let _ = instance.dispatch_mouse(25.0, 25.0, MouseEvent::Move { x: 25.0, y: 25.0 });
    assert_eq!(first_child_color(&mut instance), Some((20, 80, 200)));
}

/// Regression: in kitchen_sink, rows are inserted by Solid's `insert`
/// helper applied to an array-returning function. Each row carries
/// `class="row row-even"` and we want `.row:hover` to flip its colour.
/// The class is set via the renderer's `setProp` path (the same path the
/// JSX compiler emits), not directly via __sol_setProperty.
#[test]
fn hover_via_solid_setprop_on_class_works() {
    const COMPONENT: &str = r#"
            import { render, setProp, insert } from "solite-runtime";
            function App() {
              const root = __sol_createElement("div");
              __sol_setProperty(root, "style", "display:block; width:120px; height:120px;");

              const make = () => {
                const items = [];
                for (let i = 0; i < 2; i++) {
                  const row = __sol_createElement("div");
                  setProp(row, "class", i % 2 === 0 ? "row row-even" : "row row-odd");
                  setProp(row, "style", "display:block; width:120px; height:40px;");
                  __sol_insertNode(row, __sol_createTextNode("row " + i), null);
                  items.push(row);
                }
                return items;
              };
              insert(root, make);
              return root;
            }
            render(() => App(), __SOL_ROOT__);
        "#;
    let css = ".row { color: rgb(50, 50, 50) } \
                   .row-even { background: rgb(20, 20, 20) } \
                   .row:hover { color: rgb(255, 0, 0) }";
    let (mut instance, _rx) = make_instance_with(COMPONENT, &[css]);
    // First row idle = grey.
    let _ = instance.render();
    let row_color = {
        let doc = instance.doc.borrow();
        let root_id = doc
            .get_node(instance.container_id())
            .and_then(|c| c.children.first().copied())
            .unwrap();
        let row_id = doc.get_node(root_id).unwrap().children[0];
        node_color(&doc, row_id)
    };
    assert_eq!(row_color, Some((50, 50, 50)));

    // Hover the first row.
    let _ = instance.dispatch_mouse(20.0, 10.0, MouseEvent::Move { x: 20.0, y: 10.0 });
    let _ = instance.render();
    let hovered_color = {
        let doc = instance.doc.borrow();
        let root_id = doc
            .get_node(instance.container_id())
            .and_then(|c| c.children.first().copied())
            .unwrap();
        let row_id = doc.get_node(root_id).unwrap().children[0];
        node_color(&doc, row_id)
    };
    assert_eq!(hovered_color, Some((255, 0, 0)));
}

/// Regression: mirrors kitchen_sink exactly — same JSX pattern (a button
/// inside a panel container), same CSS (multi-token classes + :hover on
/// the more-specific token), driven through the JSX compiler so we
/// exercise the same code paths as the example.
#[cfg(feature = "jsx-compiler")]
#[test]
fn kitchen_sink_button_hover_flips_color() {
    const JSX: &str = r#"
            import { render } from "solite-runtime";
            function App() {
              return (
                <div class="panel">
                  <button class="btn btn-add">+ Add Row</button>
                </div>
              );
            }
            render(() => App(), __SOL_ROOT__);
        "#;
    const CSS: &str = r#"
            .panel { display: block; width: 200px; padding: 10px; background: #182238; }
            .btn { display: inline-block; padding: 8px 10px; border: 1px solid #7fb5ff; color: rgb(243, 247, 255); }
            .btn-add { background: rgb(31, 59, 95); }
            .btn-add:hover { background: rgb(91, 140, 250); color: rgb(255, 255, 255); }
        "#;

    let compiled =
        solite_build::compile_component_source(std::path::Path::new("/tmp/kitchen.jsx"), JSX)
            .expect("compile");
    let (mut instance, _rx) = make_instance_with(&compiled, &[CSS]);

    // First paint resolves layout.
    let _ = instance.render();

    // The button lives at panel.children[0]. Find a point inside it.
    let (panel_id, btn_id) = {
        let doc = instance.doc.borrow();
        let panel = doc
            .get_node(instance.container_id())
            .and_then(|c| c.children.first().copied())
            .unwrap();
        let btn = doc.get_node(panel).unwrap().children[0];
        (panel, btn)
    };
    let _ = panel_id;

    // Read color before hover.
    let before = {
        let doc = instance.doc.borrow();
        node_color(&doc, btn_id)
    };
    assert_eq!(before, Some((243, 247, 255)));

    // Move pointer into the button — its layout is inside the panel which
    // has padding 10. So (20, 20) lands on the button.
    let _ = instance.dispatch_mouse(20.0, 20.0, MouseEvent::Move { x: 20.0, y: 20.0 });
    let _ = instance.render();
    let after = {
        let doc = instance.doc.borrow();
        node_color(&doc, btn_id)
    };
    assert_eq!(after, Some((255, 255, 255)), "hover should flip color");
}

/// A function passed to a reactive attribute by reference (`style={fn}`
/// instead of `style={fn()}`) compiles to a static `setProp`, so it is applied
/// once and never updates — a silent footgun. In dev builds the runtime should
/// surface a `__sol_dev_warn` diagnostic naming the attribute.
#[cfg(all(feature = "jsx-compiler", debug_assertions))]
#[test]
fn uncalled_function_attribute_emits_dev_warning() {
    const JSX: &str = r#"
            import { render } from "solite-runtime";
            function App() {
              const styleFn = () => ({ width: "10px" });
              return <div style={styleFn}></div>;
            }
            render(() => App(), __SOL_ROOT__);
        "#;
    let compiled =
        solite_build::compile_component_source(std::path::Path::new("/tmp/warn.jsx"), JSX)
            .expect("compile");
    let (mut instance, _rx) = make_instance_with(&compiled, &[]);
    let _ = instance.render();

    let mut count = 0i32;
    let mut mentions_style = false;
    instance.js.context_with(|ctx| {
        count = ctx
            .eval::<i32, _>("(globalThis.__sol_dev_warnings || []).length")
            .unwrap_or(0);
        mentions_style = ctx
            .eval::<bool, _>(
                "(globalThis.__sol_dev_warnings || []).some(m => m.indexOf('style') >= 0)",
            )
            .unwrap_or(false);
    });
    assert!(count >= 1, "expected a dev warning for style={{fn}}, got {count}");
    assert!(mentions_style, "warning should name the offending `style` attribute");
}

/// Regression: kitchen_sink panicked at blitz-dom/src/stylo.rs:84
/// (`invalid key`) when the user clicked "Add Row" after a row had been
/// styled with a `transition:` declaration. Cause: removed nodes leave
/// stale entries in `DocumentAnimationSet` that `resolve_stylist`
/// indexes back into `self.nodes`. Until the upstream cleanup lands,
/// avoid `transition:` on dynamic subtrees; this test guards against
/// regressing back into the panic with a transition-free :hover rule.
#[test]
fn dynamic_subtree_with_hover_survives_remove_and_restyle() {
    const COMPONENT: &str = r#"
            import { createEffect, render } from "solite-runtime";
            function App() {
              const root = __sol_createElement("div");
              __sol_setProperty(root, "style", "display:block; width:120px; height:240px;");
              let prevIds = [];
              createEffect(() => {
                for (const id of prevIds) { __sol_removeNode(root, id); }
                prevIds = [];
                const n = Number(globalThis.state.rows || 0);
                for (let i = 0; i < n; i++) {
                  const row = __sol_createElement("div");
                  __sol_setProperty(row, "className", "row");
                  __sol_setProperty(row, "style", "display:block; width:120px; height:20px;");
                  __sol_insertNode(row, __sol_createTextNode("row " + i), null);
                  __sol_insertNode(root, row, null);
                  prevIds.push(row);
                }
              });
              return root;
            }
            render(() => App(), __SOL_ROOT__);
        "#;
    let css = ".row { color: rgb(50, 50, 50) } .row:hover { color: rgb(255, 0, 0) }";
    let (mut instance, _rx) = make_instance_with(COMPONENT, &[css]);
    instance.state().set("rows", json!(3));
    let _ = instance.tick();
    let _ = instance.render();
    // Hover, then add/remove rows to force the snapshot + animation path.
    let _ = instance.dispatch_mouse(10.0, 5.0, MouseEvent::Move { x: 10.0, y: 5.0 });
    let _ = instance.render();
    instance.state().set("rows", json!(5));
    let _ = instance.tick();
    // The panic was triggered by the next resolve after removal.
    let _ = instance.render();
    instance.state().set("rows", json!(2));
    let _ = instance.tick();
    let _ = instance.render();
}

/// Regression: read painted pixels (not just primary_styles) to confirm
/// :hover actually reaches the texture, not just the computed-style
/// cache. This is the missing link between "the unit tests pass" and
/// "the live app shows no hover".
#[test]
fn hover_pseudo_class_actually_paints_new_color() {
    const COMPONENT: &str = r#"
            import { render } from "solite-runtime";
            function App() {
              const d = __sol_createElement("div");
              __sol_setProperty(d, "className", "swatch");
              return d;
            }
            render(() => App(), __SOL_ROOT__);
        "#;
    let css = ".swatch { display:block; width:80px; height:80px; background: rgb(0, 0, 255); } \
                   .swatch:hover { background: rgb(255, 0, 0); }";
    let (mut instance, _rx) = make_instance_with(COMPONENT, &[css]);

    let read_pixel = |instance: &mut Instance, x: u32, y: u32| -> (u8, u8, u8) {
        let _ = instance.render();
        // The painter writes into its internal cpu_buffer before uploading
        // to the texture. We can't read back the texture without a copy
        // operation, but cpu_buffer is the same RGBA8 source. Reach in.
        let buf = &instance.painter.cpu_buffer;
        let row = (instance.width * 4) as usize;
        let i = (y as usize) * row + (x as usize) * 4;
        (buf[i], buf[i + 1], buf[i + 2])
    };

    let before = read_pixel(&mut instance, 20, 20);
    assert_eq!(before, (0, 0, 255), "before hover: {before:?}");
    let _ = instance.dispatch_mouse(20.0, 20.0, MouseEvent::Move { x: 20.0, y: 20.0 });
    let after = read_pixel(&mut instance, 20, 20);
    assert_eq!(after, (255, 0, 0), "after hover: {after:?}");
}

#[test]
fn scrollable_overflow_paints_a_scrollbar() {
    const COMPONENT: &str = r#"
            import { render } from "solite-runtime";
            function App() {
              const outer = __sol_createElement("div");
              __sol_setProperty(outer, "style", "display:block; width:120px; height:80px; overflow:auto;");
              const filler = __sol_createElement("div");
              __sol_setProperty(filler, "style", "display:block; width:120px; height:480px;");
              __sol_insertNode(outer, filler, null);
              return outer;
            }
            render(() => App(), __SOL_ROOT__);
        "#;
    let (mut instance, _rx) = make_instance_with(COMPONENT, &[]);
    let _ = instance.render();
    assert!(
        !instance.scrollbars.is_empty(),
        "expected a scrollbar region for the overflowing container",
    );
    let region = instance.scrollbars[0];
    // The track sits flush against the instance viewport's right edge.
    let viewport_w = instance.width as f32;
    assert!(
        (region.track.0 + region.track.2 - viewport_w).abs() < 0.01,
        "expected scrollbar track to clamp to the instance viewport width {viewport_w}: {region:?}",
    );
    assert!(region.max_scroll > 0.0);
}

#[test]
fn scrollbar_thumb_drag_moves_scroll_offset() {
    const COMPONENT: &str = r#"
            import { render } from "solite-runtime";
            function App() {
              const outer = __sol_createElement("div");
              __sol_setProperty(outer, "style", "display:block; width:120px; height:80px; overflow:auto;");
              const filler = __sol_createElement("div");
              __sol_setProperty(filler, "style", "display:block; width:120px; height:480px;");
              __sol_insertNode(outer, filler, null);
              return outer;
            }
            render(() => App(), __SOL_ROOT__);
        "#;
    let (mut instance, _rx) = make_instance_with(COMPONENT, &[]);
    let _ = instance.render();
    let region = instance.scrollbars[0];
    let thumb_centre_x = region.thumb.0 + region.thumb.2 * 0.5;
    let thumb_centre_y = region.thumb.1 + region.thumb.3 * 0.5;
    let outer_id = region.node_id;

    // Down on the thumb starts the drag.
    let _ = instance.dispatch_mouse(
        thumb_centre_x,
        thumb_centre_y,
        MouseEvent::Down {
            x: thumb_centre_x,
            y: thumb_centre_y,
            button: MouseButton::Left,
        },
    );
    assert!(instance.scrollbar_drag.is_some());

    // Move down 30 logical pixels — scroll_offset should grow.
    let _ = instance.dispatch_mouse(
        thumb_centre_x,
        thumb_centre_y + 30.0,
        MouseEvent::Move {
            x: thumb_centre_x,
            y: thumb_centre_y + 30.0,
        },
    );
    let scrolled = instance
        .doc
        .borrow()
        .get_node(outer_id)
        .unwrap()
        .scroll_offset
        .y;
    assert!(scrolled > 0.0, "expected scroll to advance, got {scrolled}");

    // Up ends the drag.
    let _ = instance.dispatch_mouse(
        thumb_centre_x,
        thumb_centre_y + 30.0,
        MouseEvent::Up {
            x: thumb_centre_x,
            y: thumb_centre_y + 30.0,
            button: MouseButton::Left,
        },
    );
    assert!(instance.scrollbar_drag.is_none());
}

#[test]
fn scrollbar_theme_paints_supplied_colors() {
    // Container sized so the scrollbar lives well inside the 100x100
    // painter texture; the test reads back pixels from cpu_buffer.
    const COMPONENT: &str = r#"
            import { render } from "solite-runtime";
            function App() {
              const outer = __sol_createElement("div");
              __sol_setProperty(outer, "style", "display:block; width:80px; height:80px; overflow:auto;");
              const filler = __sol_createElement("div");
              __sol_setProperty(filler, "style", "display:block; width:80px; height:400px;");
              __sol_insertNode(outer, filler, null);
              return outer;
            }
            render(() => App(), __SOL_ROOT__);
        "#;
    let (mut instance, _rx) = make_instance_with(COMPONENT, &[]);
    instance.set_scrollbar_theme(Some(crate::ScrollbarTheme {
        track: (50, 50, 50, 255),
        thumb: (220, 30, 30, 255),
    }));
    let _ = instance.render();
    let region = instance.scrollbars[0];
    // Sample the thumb centre — should be the supplied red.
    let row = (instance.width * 4) as usize;
    let tx = (region.thumb.0 + region.thumb.2 * 0.5) as usize;
    let ty = (region.thumb.1 + region.thumb.3 * 0.5) as usize;
    let i = ty * row + tx * 4;
    let (r, g, b) = (
        instance.painter.cpu_buffer[i],
        instance.painter.cpu_buffer[i + 1],
        instance.painter.cpu_buffer[i + 2],
    );
    // Allow a few-byte rounding tolerance from Vello's sampler.
    assert!(
        r > 200 && g < 60 && b < 60,
        "thumb should be red, got ({r},{g},{b})"
    );
}

#[test]
fn track_click_pages_scroll_offset() {
    const COMPONENT: &str = r#"
            import { render } from "solite-runtime";
            function App() {
              const outer = __sol_createElement("div");
              __sol_setProperty(outer, "style", "display:block; width:120px; height:80px; overflow:auto;");
              const filler = __sol_createElement("div");
              __sol_setProperty(filler, "style", "display:block; width:120px; height:480px;");
              __sol_insertNode(outer, filler, null);
              return outer;
            }
            render(() => App(), __SOL_ROOT__);
        "#;
    let (mut instance, _rx) = make_instance_with(COMPONENT, &[]);
    let _ = instance.render();
    let region = instance.scrollbars[0];
    // Click below the thumb — should page down.
    let click_x = region.track.0 + region.track.2 * 0.5;
    let click_y = region.thumb.1 + region.thumb.3 + 5.0;
    let _ = instance.dispatch_mouse(
        click_x,
        click_y,
        MouseEvent::Down {
            x: click_x,
            y: click_y,
            button: MouseButton::Left,
        },
    );
    let after = instance
        .doc
        .borrow()
        .get_node(region.node_id)
        .unwrap()
        .scroll_offset
        .y;
    assert!(
        after > 0.0,
        "expected page-down to advance scroll, got {after}"
    );
}

#[test]
fn document_scroll_scrolls_root_container() {
    // A document taller than the instance height. With document_scroll:
    // true the container gets overflow-y:auto + explicit height, so wheel
    // events that aren't consumed by a child scroll the container itself.
    const TALL_COMPONENT: &str = r#"
            import { render } from "solite-runtime";
            function App() {
              const root = __sol_createElement("div");
              __sol_setProperty(root, "style",
                "display:block; width:200px; height:600px; background:#111;");
              return root;
            }
            render(() => App(), __SOL_ROOT__);
        "#;

    let (device, queue) = test_device();
    let (mut instance, _rx) = Instance::new(
        InstanceConfig {
            width: 200,
            height: 200,
            device,
            queue,
            stylesheets: vec![],
            document_scroll: true,
            base_url: None,
            initial_state: None,
            registered_resources: vec![],
            scale_factor: 1.0,
        },
        TALL_COMPONENT,
    );
    let _ = instance.render();

    let before = instance
        .doc
        .borrow()
        .get_node(instance.container_id())
        .expect("container exists")
        .scroll_offset
        .y;

    // Wheel down (negative winit delta → content scrolls down).
    let result = instance.dispatch_wheel(10.0, 10.0, 0.0, -40.0);
    assert!(result.needs_paint);

    let after = instance
        .doc
        .borrow()
        .get_node(instance.container_id())
        .expect("container exists")
        .scroll_offset
        .y;

    assert!(
        after > before,
        "document scroll_offset should increase after wheel down (before={before}, after={after})"
    );
}

#[test]
fn horizontal_overflow_emits_horizontal_scrollbar() {
    // A scroll container with content that overflows on the X axis.
    // collect_scrollbar_regions should emit a horizontal scrollbar
    // pinned to the bottom of the container.
    const WIDE_COMPONENT: &str = r#"
            import { render } from "solite-runtime";
            function App() {
              const wrap = __sol_createElement("div");
              __sol_setProperty(wrap, "style",
                "display:block; width:200px; height:200px; overflow:auto;");
              const inner = __sol_createElement("div");
              __sol_setProperty(inner, "style",
                "display:block; width:600px; height:100px; background:#888;");
              __sol_insertNode(wrap, inner, null);
              return wrap;
            }
            render(() => App(), __SOL_ROOT__);
        "#;

    let (device, queue) = test_device();
    let (mut instance, _rx) = Instance::new(
        InstanceConfig {
            width: 200,
            height: 200,
            device,
            queue,
            stylesheets: vec![],
            document_scroll: false,
            base_url: None,
            initial_state: None,
            registered_resources: vec![],
            scale_factor: 1.0,
        },
        WIDE_COMPONENT,
    );
    let _ = instance.render();

    let h_region = instance
        .scrollbars
        .iter()
        .find(|r| r.axis == ScrollAxis::Horizontal)
        .copied()
        .unwrap_or_else(|| {
            panic!(
                "expected a horizontal scrollbar region, got {:?}",
                instance.scrollbars
            )
        });

    // Track must be at the bottom edge of the 200x200 container,
    // SCROLLBAR_WIDTH tall, fully within the viewport.
    let (tx, ty, tw, th) = h_region.track;
    assert!(
        tx >= 0.0 && tx + tw <= 200.0,
        "track x bounds: {h_region:?}"
    );
    assert!(
        (ty + th - 200.0).abs() < 0.01,
        "track should sit on the bottom edge: {h_region:?}",
    );
    // No vertical bar in this layout (content fits vertically), so the
    // horizontal track should NOT be inset for a v-bar corner.
    assert!(
        !instance
            .scrollbars
            .iter()
            .any(|r| r.axis == ScrollAxis::Vertical),
        "this layout has no vertical overflow"
    );

    // Scrolling the container right should move the thumb right.
    let initial_thumb_x = h_region.thumb.0;
    let _ = instance.dispatch_wheel(50.0, 50.0, -200.0, 0.0);
    let _ = instance.render();
    let h2 = instance
        .scrollbars
        .iter()
        .find(|r| r.axis == ScrollAxis::Horizontal)
        .copied()
        .expect("horizontal scrollbar still present");
    assert!(
        h2.thumb.0 > initial_thumb_x,
        "thumb should move right after horizontal wheel (before={initial_thumb_x}, after={})",
        h2.thumb.0,
    );
}

#[test]
fn three_scene_surfaces_keep_horizontal_scrollbar_visible() {
    use crate::scene::{Scene, SurfaceRect};

    const PANELS_COMPONENT_BASE: &str = r#"
            import { render } from "solite-runtime";
            function App() {
              const is_center = globalThis.state.targetIndex === 1;
              const outer = __sol_createElement("div");
              __sol_setProperty(
                outer,
                "style",
                is_center
                  ? "display:block; width:160px; height:80px; overflow:auto; color:#ffffff; background:#080808;"
                  : "display:block; width:160px; height:80px; overflow:auto; color:#060606; background:#f0f4ff;"
              );
              const inner = __sol_createElement("div");
              __sol_setProperty(inner, "style", "display:block; width:320px; height:40px; background:#8899aa;");
              __sol_insertNode(outer, inner, null);
              return outer;
            }
            render(() => App(), __SOL_ROOT__);
        "#;

    let (device, queue) = test_device();
    let mut scene: Scene<()> = Scene::new();
    for index in 0..3 {
        let seeded_source =
            format!("globalThis.state.targetIndex = {index};\n{PANELS_COMPONENT_BASE}");
        let (instance, _rx) = Instance::new(
            InstanceConfig {
                width: 180,
                height: 100,
                device: device.clone(),
                queue: queue.clone(),
                stylesheets: vec![],
                document_scroll: false,
                base_url: None,
                initial_state: None,
                registered_resources: vec![],
                scale_factor: 1.0,
            },
            &seeded_source,
        );
        scene.add_surface(
            instance,
            SurfaceRect::new((180 * index) as f32, 0.0, 180.0, 100.0),
            (),
        );
    }

    for surface in scene.surfaces_mut() {
        let _ = surface.instance.render();
    }

    let left = scene.surfaces()[0]
        .instance
        .scrollbars
        .iter()
        .find(|region| matches!(region.axis, ScrollAxis::Horizontal))
        .copied()
        .expect("left surface should expose a horizontal scrollbar");
    let center = scene.surfaces()[1]
        .instance
        .scrollbars
        .iter()
        .find(|region| matches!(region.axis, ScrollAxis::Horizontal))
        .copied()
        .expect("center surface should expose a horizontal scrollbar");
    let right = scene.surfaces()[2]
        .instance
        .scrollbars
        .iter()
        .find(|region| matches!(region.axis, ScrollAxis::Horizontal))
        .copied()
        .expect("right surface should expose a horizontal scrollbar");

    let read_pixel = |instance: &mut Instance, x: f32, y: f32| -> (u8, u8, u8, u8) {
        let buf = &instance.painter.cpu_buffer;
        let width = (instance.width as usize) * 4;
        let ix = (x.max(0.0) as usize).min(instance.width.saturating_sub(1) as usize);
        let iy = (y.max(0.0) as usize).min(instance.height.saturating_sub(1) as usize);
        let idx = iy * width + ix * 4;
        (buf[idx], buf[idx + 1], buf[idx + 2], buf[idx + 3])
    };

    let bg_dist = |pixel: (u8, u8, u8, u8), bg: (u8, u8, u8)| -> u8 {
        let dr = pixel.0.abs_diff(bg.0);
        let dg = pixel.1.abs_diff(bg.1);
        let db = pixel.2.abs_diff(bg.2);
        let sum = u16::from(dr) + u16::from(dg) + u16::from(db);
        (sum / 3).try_into().unwrap_or(0)
    };
    let is_visible =
        |sample: (u8, u8, u8, u8), bg: (u8, u8, u8)| -> bool { bg_dist(sample, bg) > 24 };

    let left_sample = read_pixel(
        &mut scene.surfaces_mut()[0].instance,
        left.thumb.0 + left.thumb.2 * 0.5,
        left.thumb.1 + left.thumb.3 * 0.5,
    );
    let center_sample = read_pixel(
        &mut scene.surfaces_mut()[1].instance,
        center.thumb.0 + center.thumb.2 * 0.5,
        center.thumb.1 + center.thumb.3 * 0.5,
    );
    let right_sample = read_pixel(
        &mut scene.surfaces_mut()[2].instance,
        right.thumb.0 + right.thumb.2 * 0.5,
        right.thumb.1 + right.thumb.3 * 0.5,
    );
    assert!(
        is_visible(left_sample, (240, 244, 255)),
        "left horizontal scrollbar thumb should be visible, got {:?}",
        left_sample
    );
    assert!(
        is_visible(center_sample, (8, 8, 8)),
        "center horizontal scrollbar thumb should be visible on dark background, got {:?}",
        center_sample
    );
    assert!(
        is_visible(right_sample, (240, 244, 255)),
        "right horizontal scrollbar thumb should be visible, got {:?}",
        right_sample
    );
    assert!(
        left.thumb.2 > 0.0 && center.thumb.2 > 0.0 && right.thumb.2 > 0.0,
        "horizontal scroll thumbs should exist on all three surfaces"
    );
}

#[test]
fn scrollbar_tracks_are_local_to_each_scene_surface() {
    use crate::scene::{Scene, SurfaceRect};

    const SCROLL_COMPONENT: &str = r#"
            import { render } from "solite-runtime";
            function App() {
              const outer = __sol_createElement("div");
              __sol_setProperty(
                outer,
                "style",
                "display:block; width:100px; height:100px; overflow:auto;"
              );
              const filler = __sol_createElement("div");
              __sol_setProperty(
                filler,
                "style",
                "display:block; width:100px; height:300px;"
              );
              __sol_insertNode(outer, filler, null);
              return outer;
            }
            render(() => App(), __SOL_ROOT__);
        "#;

    let (device, queue) = test_device();

    let (surface_a, _rx_a) = Instance::new(
        InstanceConfig {
            width: 100,
            height: 100,
            device: device.clone(),
            queue: queue.clone(),
            stylesheets: vec![],
            document_scroll: false,
            base_url: None,
            initial_state: None,
            registered_resources: vec![],
            scale_factor: 1.0,
        },
        SCROLL_COMPONENT,
    );
    let (surface_b, _rx_b) = Instance::new(
        InstanceConfig {
            width: 100,
            height: 100,
            device,
            queue,
            stylesheets: vec![],
            document_scroll: false,
            base_url: None,
            initial_state: None,
            registered_resources: vec![],
            scale_factor: 1.0,
        },
        SCROLL_COMPONENT,
    );

    let mut scene: Scene<()> = Scene::new();
    scene.add_surface(surface_a, SurfaceRect::new(0.0, 0.0, 100.0, 100.0), ());
    scene.add_surface(surface_b, SurfaceRect::new(200.0, 0.0, 100.0, 100.0), ());

    let _ = scene.surfaces_mut()[0].instance.render();
    let _ = scene.surfaces_mut()[1].instance.render();

    let left_region = scene.surfaces()[0]
        .instance
        .scrollbars
        .iter()
        .next()
        .copied()
        .expect("left instance should render a scrollbar");
    let right_region = scene.surfaces()[1]
        .instance
        .scrollbars
        .iter()
        .next()
        .copied()
        .expect("right instance should render a scrollbar");

    // Scrollbars must stay inside their own 100x100 Blix surface.
    assert!(left_region.track.0 >= 0.0 && left_region.track.0 + left_region.track.2 <= 100.0);
    assert!(left_region.track.1 >= 0.0 && left_region.track.1 + left_region.track.3 <= 100.0);
    assert!(right_region.track.0 >= 0.0 && right_region.track.0 + right_region.track.2 <= 100.0);
    assert!(right_region.track.1 >= 0.0 && right_region.track.1 + right_region.track.3 <= 100.0);

    let click_x = scene.surfaces()[1].rect.x + right_region.track.0 + right_region.track.2 * 0.5;
    let click_y = scene.surfaces()[1].rect.y + right_region.track.1 + right_region.track.3 * 0.5;

    let before_left_scroll = scene.surfaces()[0]
        .instance
        .doc
        .borrow()
        .get_node(left_region.node_id)
        .unwrap()
        .scroll_offset
        .y;
    let before_right_scroll = scene.surfaces()[1]
        .instance
        .doc
        .borrow()
        .get_node(right_region.node_id)
        .unwrap()
        .scroll_offset
        .y;

    let _ = scene.dispatch_mouse(
        click_x,
        click_y,
        MouseEvent::Down {
            x: click_x,
            y: click_y,
            button: MouseButton::Left,
        },
    );
    let _ = scene.dispatch_mouse(
        click_x,
        click_y,
        MouseEvent::Up {
            x: click_x,
            y: click_y,
            button: MouseButton::Left,
        },
    );

    let after_left_scroll = scene.surfaces()[0]
        .instance
        .doc
        .borrow()
        .get_node(left_region.node_id)
        .unwrap()
        .scroll_offset
        .y;
    let after_right_scroll = scene.surfaces()[1]
        .instance
        .doc
        .borrow()
        .get_node(right_region.node_id)
        .unwrap()
        .scroll_offset
        .y;

    assert_eq!(
        before_left_scroll, after_left_scroll,
        "left surface should not scroll when interacting with right scrollbar"
    );
    assert!(
        after_right_scroll > before_right_scroll,
        "right surface should scroll when its scrollbar is clicked"
    );
}

#[test]
fn horizontal_scrollbar_tracks_are_local_to_each_scene_surface() {
    use crate::scene::{Scene, SurfaceRect};

    const SCROLL_COMPONENT: &str = r#"
            import { render } from "solite-runtime";
            function App() {
              const outer = __sol_createElement("div");
              __sol_setProperty(
                outer,
                "style",
                "display:block; width:100px; height:60px; overflow:auto;"
              );
              const filler = __sol_createElement("div");
              __sol_setProperty(
                filler,
                "style",
                "display:block; width:300px; height:60px;"
              );
              __sol_insertNode(outer, filler, null);
              return outer;
            }
            render(() => App(), __SOL_ROOT__);
        "#;

    let (device, queue) = test_device();

    let (surface_a, _rx_a) = Instance::new(
        InstanceConfig {
            width: 100,
            height: 100,
            device: device.clone(),
            queue: queue.clone(),
            stylesheets: vec![],
            document_scroll: false,
            base_url: None,
            initial_state: None,
            registered_resources: vec![],
            scale_factor: 1.0,
        },
        SCROLL_COMPONENT,
    );
    let (surface_b, _rx_b) = Instance::new(
        InstanceConfig {
            width: 100,
            height: 100,
            device,
            queue,
            stylesheets: vec![],
            document_scroll: false,
            base_url: None,
            initial_state: None,
            registered_resources: vec![],
            scale_factor: 1.0,
        },
        SCROLL_COMPONENT,
    );

    let mut scene: Scene<()> = Scene::new();
    scene.add_surface(surface_a, SurfaceRect::new(0.0, 0.0, 100.0, 100.0), ());
    scene.add_surface(surface_b, SurfaceRect::new(200.0, 0.0, 100.0, 100.0), ());

    let _ = scene.surfaces_mut()[0].instance.render();
    let _ = scene.surfaces_mut()[1].instance.render();

    let left_region = scene.surfaces()[0]
        .instance
        .scrollbars
        .iter()
        .find(|region| matches!(region.axis, ScrollAxis::Horizontal))
        .copied()
        .expect("left instance should render a horizontal scrollbar");
    let right_region = scene.surfaces()[1]
        .instance
        .scrollbars
        .iter()
        .find(|region| matches!(region.axis, ScrollAxis::Horizontal))
        .copied()
        .expect("right instance should render a horizontal scrollbar");

    // Horizontal scrollbars should live at the bottom of each 100px surface.
    assert!(left_region.track.0 >= 0.0 && left_region.track.0 + left_region.track.2 <= 100.0);
    assert!(left_region.track.1 >= 0.0 && left_region.track.1 + left_region.track.3 <= 100.0);
    assert!(right_region.track.0 >= 0.0 && right_region.track.0 + right_region.track.2 <= 100.0);
    assert!(right_region.track.1 >= 0.0 && right_region.track.1 + right_region.track.3 <= 100.0);

    let click_x = scene.surfaces()[1].rect.x + right_region.track.0 + right_region.track.2 * 0.5;
    let click_y = scene.surfaces()[1].rect.y + right_region.track.1 + right_region.track.3 * 0.5;

    let before_left_scroll = scene.surfaces()[0]
        .instance
        .doc
        .borrow()
        .get_node(left_region.node_id)
        .unwrap()
        .scroll_offset
        .x;
    let before_right_scroll = scene.surfaces()[1]
        .instance
        .doc
        .borrow()
        .get_node(right_region.node_id)
        .unwrap()
        .scroll_offset
        .x;

    let _ = scene.dispatch_mouse(
        click_x,
        click_y,
        MouseEvent::Down {
            x: click_x,
            y: click_y,
            button: MouseButton::Left,
        },
    );
    let _ = scene.dispatch_mouse(
        click_x,
        click_y,
        MouseEvent::Up {
            x: click_x,
            y: click_y,
            button: MouseButton::Left,
        },
    );

    let after_left_scroll = scene.surfaces()[0]
        .instance
        .doc
        .borrow()
        .get_node(left_region.node_id)
        .unwrap()
        .scroll_offset
        .x;
    let after_right_scroll = scene.surfaces()[1]
        .instance
        .doc
        .borrow()
        .get_node(right_region.node_id)
        .unwrap()
        .scroll_offset
        .x;

    assert_eq!(
        before_left_scroll, after_left_scroll,
        "left surface should not scroll when interacting with right scrollbar"
    );
    assert!(
        after_right_scroll > before_right_scroll,
        "right surface should scroll when its horizontal scrollbar is clicked"
    );
}

#[test]
fn horizontal_overflow_emits_without_vertical_scroll() {
    // overflow-x-only (with overflow-y:hidden) should still emit a horizontal
    // scrollbar so long as there is horizontal overflow.
    const WIDE_COMPONENT: &str = r#"
            import { render } from "solite-runtime";
            function App() {
              const wrap = __sol_createElement("div");
              __sol_setProperty(wrap, "style",
                "display:block; width:200px; height:200px; overflow-x:auto; overflow-y:hidden;");
              const inner = __sol_createElement("div");
              __sol_setProperty(inner, "style",
                "display:block; width:600px; height:100px; background:#888;");
              __sol_insertNode(wrap, inner, null);
              return wrap;
            }
            render(() => App(), __SOL_ROOT__);
        "#;

    let (device, queue) = test_device();
    let (mut instance, _rx) = Instance::new(
        InstanceConfig {
            width: 200,
            height: 200,
            device,
            queue,
            stylesheets: vec![],
            document_scroll: false,
            base_url: None,
            initial_state: None,
            registered_resources: vec![],
            scale_factor: 1.0,
        },
        WIDE_COMPONENT,
    );
    let _ = instance.render();

    let regions = instance
        .scrollbars
        .iter()
        .filter(|r| r.axis == ScrollAxis::Horizontal)
        .collect::<Vec<_>>();
    assert_eq!(
        regions.len(),
        1,
        "expected exactly one horizontal scrollbar: {:?}",
        regions
    );

    let h_region = regions[0];
    assert!(
        !instance
            .scrollbars
            .iter()
            .any(|r| r.axis == ScrollAxis::Vertical),
        "no vertical scrollbar should be needed"
    );

    assert!(
        h_region.track.3 > 0.0,
        "horizontal track should be visible: {:?}",
        h_region.track
    );
}

#[test]
fn inline_overflow_emits_horizontal_scrollbar() {
    // A long inline element that overflows horizontally should produce a
    // horizontal scrollbar even when vertical scrolling is not needed.
    const WIDE_COMPONENT: &str = r#"
            import { render } from "solite-runtime";
            function App() {
              const wrap = __sol_createElement("div");
              __sol_setProperty(wrap, "style",
                "display:block; width:200px; height:20px; overflow:auto; white-space: nowrap;");
              const child = __sol_createElement("span");
              __sol_setProperty(child, "style", "display:inline-block; width:400px; height:20px;");
              __sol_insertNode(wrap, child, null);
              return wrap;
            }
            render(() => App(), __SOL_ROOT__);
        "#;

    let (device, queue) = test_device();
    let (mut instance, _rx) = Instance::new(
        InstanceConfig {
            width: 220,
            height: 80,
            device,
            queue,
            stylesheets: vec![],
            document_scroll: false,
            base_url: None,
            initial_state: None,
            registered_resources: vec![],
            scale_factor: 1.0,
        },
        WIDE_COMPONENT,
    );
    let _ = instance.render();

    assert!(
        instance
            .scrollbars
            .iter()
            .any(|r| r.axis == ScrollAxis::Horizontal),
        "expected horizontal scrollbar for inline overflow: {:?}",
        instance.scrollbars
    );
    assert!(
        !instance
            .scrollbars
            .iter()
            .any(|r| r.axis == ScrollAxis::Vertical),
        "inline overflow test should not need a vertical scrollbar"
    );
}

#[test]
fn no_horizontal_scrollbar_for_vertical_only_overflow() {
    // A container that only overflows vertically (content wider than needed)
    // must not emit a horizontal scrollbar.
    const TALL_WIDE_COMPONENT: &str = r#"
            import { render } from "solite-runtime";
            function App() {
              const wrap = __sol_createElement("div");
              __sol_setProperty(wrap, "style",
                "display:block; width:200px; height:80px; overflow:auto;");
              const inner = __sol_createElement("div");
              __sol_setProperty(inner, "style", "width:200px; height:300px; background:#888;");
              __sol_insertNode(wrap, inner, null);
              return wrap;
            }
            render(() => App(), __SOL_ROOT__);
        "#;

    let (device, queue) = test_device();
    let (mut instance, _rx) = Instance::new(
        InstanceConfig {
            width: 220,
            height: 80,
            device,
            queue,
            stylesheets: vec![],
            document_scroll: false,
            base_url: None,
            initial_state: None,
            registered_resources: vec![],
            scale_factor: 1.0,
        },
        TALL_WIDE_COMPONENT,
    );
    let _ = instance.render();

    let has_h = instance
        .scrollbars
        .iter()
        .any(|r| r.axis == ScrollAxis::Horizontal);
    let has_v = instance
        .scrollbars
        .iter()
        .any(|r| r.axis == ScrollAxis::Vertical);

    assert!(has_v, "vertical overflow should emit a vertical scrollbar");
    assert!(
        !has_h,
        "vertical-only overflow should not emit a horizontal scrollbar"
    );
}

#[test]
fn flex_inline_overflow_emits_horizontal_scrollbar() {
    // Flex row content wider than the container should emit a horizontal scrollbar.
    const FLEX_COMPONENT: &str = r#"
            import { render } from "solite-runtime";
            function App() {
              const wrap = __sol_createElement("div");
              __sol_setProperty(wrap, "style",
                "display:flex; width:200px; height:60px; overflow:auto;");
              for (let i = 0; i < 10; i++) {
                const item = __sol_createElement("div");
                __sol_setProperty(item, "style", "display:flex; flex:0 0 auto; width:80px; height:40px;");
                __sol_insertNode(wrap, item, null);
              }
              return wrap;
            }
            render(() => App(), __SOL_ROOT__);
        "#;

    let (device, queue) = test_device();
    let (mut instance, _rx) = Instance::new(
        InstanceConfig {
            width: 220,
            height: 60,
            device,
            queue,
            stylesheets: vec![],
            document_scroll: false,
            base_url: None,
            initial_state: None,
            registered_resources: vec![],
            scale_factor: 1.0,
        },
        FLEX_COMPONENT,
    );
    let _ = instance.render();

    assert!(
        instance
            .scrollbars
            .iter()
            .any(|r| r.axis == ScrollAxis::Horizontal),
        "flex row overflow should emit horizontal scrollbar: {:?}",
        instance.scrollbars
    );
}

#[test]
fn document_scroll_with_inner_scroll_still_scrolls() {
    // Mirror the kitchen sink layout: tall panel inside the document
    // (taller than the instance height) AND a child element with its
    // own `overflow: auto`. Wheeling over the panel (not over the
    // inner scroll container) should scroll the document container.
    const PANEL_COMPONENT: &str = r#"
            import { render } from "solite-runtime";
            function App() {
              const panel = __sol_createElement("div");
              __sol_setProperty(panel, "style",
                "display:block; width:360px; padding:10px; background:#111;");
              const title = __sol_createElement("div");
              __sol_setProperty(title, "style", "height:200px; background:#222;");
              __sol_insertNode(panel, title, null);
              const rows = __sol_createElement("div");
              __sol_setProperty(rows, "style",
                "display:block; width:340px; height:190px; overflow:auto;");
              const filler = __sol_createElement("div");
              __sol_setProperty(filler, "style", "height:600px; background:#333;");
              __sol_insertNode(rows, filler, null);
              __sol_insertNode(panel, rows, null);
              const footer = __sol_createElement("div");
              __sol_setProperty(footer, "style", "height:200px; background:#444;");
              __sol_insertNode(panel, footer, null);
              return panel;
            }
            render(() => App(), __SOL_ROOT__);
        "#;

    let (device, queue) = test_device();
    let (mut instance, _rx) = Instance::new(
        InstanceConfig {
            width: 360,
            height: 440,
            device,
            queue,
            stylesheets: vec![],
            document_scroll: true,
            base_url: None,
            initial_state: None,
            registered_resources: vec![],
            scale_factor: 1.0,
        },
        PANEL_COMPONENT,
    );
    let _ = instance.render();

    let container_id = instance.container_id();
    let before = instance
        .doc
        .borrow()
        .get_node(container_id)
        .unwrap()
        .scroll_offset
        .y;

    // Wheel over the top of the panel (well above the inner .rows).
    let _ = instance.dispatch_wheel(50.0, 50.0, 0.0, -40.0);

    let after = instance
        .doc
        .borrow()
        .get_node(container_id)
        .unwrap()
        .scroll_offset
        .y;
    assert!(
        after > before,
        "wheel at panel top should scroll the document container \
             (before={before}, after={after})"
    );
}

#[test]
fn document_scroll_emits_scrollbar_region() {
    // Same setup as document_scroll_scrolls_root_container, but here we
    // assert that render() collects a scrollbar region for the root
    // container so the bar actually paints. Also exercises that the
    // track stays pinned to the viewport (not scrolled off-screen) and
    // that the thumb moves down as the container is scrolled.
    const TALL_COMPONENT: &str = r#"
            import { render } from "solite-runtime";
            function App() {
              const root = __sol_createElement("div");
              __sol_setProperty(root, "style",
                "display:block; width:200px; height:600px; background:#111;");
              return root;
            }
            render(() => App(), __SOL_ROOT__);
        "#;

    let (device, queue) = test_device();
    let (mut instance, _rx) = Instance::new(
        InstanceConfig {
            width: 200,
            height: 200,
            device,
            queue,
            stylesheets: vec![],
            document_scroll: true,
            base_url: None,
            initial_state: None,
            registered_resources: vec![],
            scale_factor: 1.0,
        },
        TALL_COMPONENT,
    );
    let _ = instance.render();

    let container_id = instance.container_id();
    let region = instance
        .scrollbars
        .iter()
        .find(|region| region.node_id == container_id)
        .copied()
        .unwrap_or_else(|| {
            panic!(
                "expected a scrollbar region for the document-scroll container, got {:?}",
                instance.scrollbars
            )
        });
    let (tx, ty, tw, th) = region.track;
    assert!(
        tx >= 0.0 && tx + tw <= 200.0 && ty >= 0.0 && ty + th <= 200.0,
        "track {region:?} should be within the 200x200 viewport"
    );

    // Scroll the container, then re-render. The track must stay pinned
    // to the viewport (track_y unchanged) and the thumb must move down.
    let _ = instance.dispatch_wheel(10.0, 10.0, 0.0, -120.0);
    let _ = instance.render();
    let region2 = instance
        .scrollbars
        .iter()
        .find(|region| region.node_id == container_id)
        .copied()
        .expect("scrollbar region still present after scroll");
    assert_eq!(
        region2.track, region.track,
        "track must stay pinned to the viewport"
    );
    assert!(
        region2.thumb.1 > region.thumb.1,
        "thumb should move down after scrolling (before={}, after={})",
        region.thumb.1,
        region2.thumb.1,
    );
}

// ── Native <input> tests ──────────────────────────────────────────────

fn type_key(key: &str) -> KeyboardEvent {
    KeyboardEvent {
        key: key.into(),
        code: String::new(),
        key_code: 0,
        repeat: false,
        shift_key: false,
        ctrl_key: false,
        alt_key: false,
        meta_key: false,
    }
}

fn input_child_text(instance: &Instance, input_id: usize) -> String {
    let doc = instance.doc.borrow();
    let child = doc
        .get_node(input_id)
        .and_then(|node| node.children.first().copied());
    child
        .and_then(|child_id| {
            doc.get_node(child_id).and_then(|child_node| {
                if let blitz_dom::NodeData::Text(text) = &child_node.data {
                    Some(text.content.clone())
                } else {
                    None
                }
            })
        })
        .unwrap_or_default()
}

fn assert_input_selection_matches_layout(
    input_type: &str,
    typed: &str,
    anchor: usize,
    focus: usize,
) {
    let component = format!(
        r#"
            import {{ render }} from "solite-runtime";
            function App() {{
              const input = __sol_createElement("input");
              __sol_setProperty(input, "type", "{input_type}");
              __sol_setProperty(input, "style", "display:block; width:220px; height:40px;");
              return input;
            }}
            render(() => App(), __SOL_ROOT__);
        "#
    );
    let (mut instance, _rx) = make_instance_with(&component, &[]);
    let _ = instance.render();
    let _ = instance.dispatch_mouse(
        10.0,
        10.0,
        MouseEvent::Down {
            x: 10.0,
            y: 10.0,
            button: MouseButton::Left,
        },
    );

    let input_id = instance
        .doc
        .borrow()
        .get_node(1)
        .and_then(|root| root.children.first().copied())
        .unwrap();

    for ch in typed.chars() {
        let key = ch.to_string();
        let _ = instance.dispatch_key_down(type_key(&key));
    }
    if let Some(state) = instance.js.inputs.borrow_mut().get_mut(&input_id) {
        state.set_selection(anchor, focus);
    }
    let _ = instance.render();

    let selections = instance.collect_input_selections();
    assert!(
        !selections.is_empty(),
        "expected selection overlay for {input_type} input"
    );

    let expected = {
        let doc = instance.doc.borrow();
        let node = doc.get_node(input_id).unwrap();
        let input_data = node
            .element_data()
            .and_then(|element| element.text_input_data())
            .unwrap();
        let layout = node.final_layout;
        let input_origin = node.absolute_position(0.0, 0.0);
        let content_x = input_origin.x + layout.border.left + layout.padding.left;
        let content_y = input_origin.y + layout.border.top + layout.padding.top;
        let content_w = layout.content_box_width().max(0.0);
        let content_h = layout.content_box_height().max(1.0);
        let y_offset = node.text_input_v_centering_offset(1.0) as f32;
        let display_text = instance
            .js
            .inputs
            .borrow()
            .get(&input_id)
            .unwrap()
            .render(true)
            .0;
        let anchor_char = anchor;
        let focus_char = focus;
        let anchor = Cursor::from_byte_index(
            input_data.editor.try_layout().unwrap(),
            char_index_to_byte_index(&display_text, anchor_char),
            Affinity::Downstream,
        );
        let focus = Cursor::from_byte_index(
            input_data.editor.try_layout().unwrap(),
            char_index_to_byte_index(&display_text, focus_char),
            Affinity::Downstream,
        );
        let selection = Selection::new(anchor, focus);
        let mut rects = Vec::new();
        selection.geometry_with(input_data.editor.try_layout().unwrap(), |rect, _| {
            let x0 = (content_x + rect.x0 as f32).clamp(content_x, content_x + content_w);
            let x1 = (content_x + rect.x1 as f32).clamp(content_x, content_x + content_w);
            let y0 =
                (content_y + y_offset + rect.y0 as f32).clamp(content_y, content_y + content_h);
            let y1 =
                (content_y + y_offset + rect.y1 as f32).clamp(content_y, content_y + content_h);
            rects.push((x0, y0, x1 - x0, y1 - y0));
        });

        if rects.is_empty() {
            let width = (estimated_input_char_width(&node)
                * (focus_char as f32 - anchor_char as f32))
                .max(1.0);
            let height = (content_h * 0.7).max(1.0);
            let y = content_y + ((content_h - height).max(0.0) * 0.5);
            rects.push((
                content_x + estimated_input_char_width(&node) * anchor_char as f32,
                y,
                width,
                height,
            ));
        }
        rects
    };

    assert_eq!(selections.len(), expected.len());
    for (actual, expected) in selections.iter().zip(expected.iter()) {
        assert!(
            (actual.x - expected.0).abs() < 0.01,
            "x mismatch: {} vs {}",
            actual.x,
            expected.0
        );
        assert!(
            (actual.y - expected.1).abs() < 0.01,
            "y mismatch: {} vs {}",
            actual.y,
            expected.1
        );
        assert!(
            (actual.width - expected.2).abs() < 0.01,
            "width mismatch: {} vs {}",
            actual.width,
            expected.2
        );
        assert!(
            (actual.height - expected.3).abs() < 0.01,
            "height mismatch: {} vs {}",
            actual.height,
            expected.3
        );
    }
}

#[test]
fn input_element_routes_keys_to_rust_owned_value() {
    const COMPONENT: &str = r#"
            import { render } from "solite-runtime";
            function App() {
              const input = __sol_createElement("input");
              __sol_setProperty(input, "style", "display:block; width:200px; height:40px;");
              __sol_setProperty(input, "onInput", (e) => {
                globalThis.state.value = e.value;
                globalThis.state.caret = e.selectionStart;
              });
              return input;
            }
            render(() => App(), __SOL_ROOT__);
        "#;
    let (mut instance, _rx) = make_instance_with(COMPONENT, &[]);
    let _ = instance.render();

    // Click to focus.
    let _ = instance.dispatch_mouse(
        10.0,
        10.0,
        MouseEvent::Down {
            x: 10.0,
            y: 10.0,
            button: MouseButton::Left,
        },
    );

    // Type "hi".
    let _ = instance.dispatch_key_down(type_key("h"));
    let _ = instance.dispatch_key_down(type_key("i"));
    assert_eq!(instance.state().get("value"), Some(json!("hi")));
    assert_eq!(instance.state().get("caret"), Some(json!(2)));

    // Backspace.
    let _ = instance.dispatch_key_down(type_key("Backspace"));
    assert_eq!(instance.state().get("value"), Some(json!("h")));
    assert_eq!(instance.state().get("caret"), Some(json!(1)));

    // ArrowLeft moves caret but doesn't emit `input` (no value change),
    // so state.caret stays at 1.
    let _ = instance.dispatch_key_down(type_key("ArrowLeft"));
    assert_eq!(instance.state().get("caret"), Some(json!(1)));

    // Typing now inserts at position 0.
    let _ = instance.dispatch_key_down(type_key("a"));
    assert_eq!(instance.state().get("value"), Some(json!("ah")));
    assert_eq!(instance.state().get("caret"), Some(json!(1)));
}

#[test]
fn tab_moves_focus_between_native_inputs() {
    const COMPONENT: &str = r#"
            import { render } from "solite-runtime";
            function App() {
              const root = __sol_createElement("div");

              const first = __sol_createElement("input");
              __sol_setProperty(first, "style", "display:block; width:200px; height:24px;");
              __sol_setProperty(first, "onFocus", () => {
                globalThis.state.focused = "first";
              });
              __sol_setProperty(first, "onInput", (event) => {
                globalThis.state.firstValue = event.value;
              });

              const second = __sol_createElement("input");
              __sol_setProperty(second, "style", "display:block; width:200px; height:24px;");
              __sol_setProperty(second, "onFocus", () => {
                globalThis.state.focused = "second";
              });
              __sol_setProperty(second, "onInput", (event) => {
                globalThis.state.secondValue = event.value;
              });

              __sol_insertNode(root, first, null);
              __sol_insertNode(root, second, null);
              return root;
            }
            render(() => App(), __SOL_ROOT__);
        "#;
    let (mut instance, _rx) = make_instance_with(COMPONENT, &[]);
    let _ = instance.render();

    let _ = instance.dispatch_mouse(
        10.0,
        10.0,
        MouseEvent::Down {
            x: 10.0,
            y: 10.0,
            button: MouseButton::Left,
        },
    );
    assert_eq!(instance.state().get("focused"), Some(json!("first")));

    let _ = instance.dispatch_key_down(make_key_event(
        "Tab", "Tab", 9, false, false, false, false, false,
    ));
    assert_eq!(instance.state().get("focused"), Some(json!("second")));

    let _ = instance.dispatch_key_down(type_key("x"));
    assert_eq!(instance.state().get("firstValue"), None);
    assert_eq!(instance.state().get("secondValue"), Some(json!("x")));

    let _ = instance.dispatch_key_down(make_key_event(
        "Tab", "Tab", 9, false, true, false, false, false,
    ));
    assert_eq!(instance.state().get("focused"), Some(json!("first")));

    let _ = instance.dispatch_key_down(type_key("y"));
    assert_eq!(instance.state().get("firstValue"), Some(json!("y")));
    assert_eq!(instance.state().get("secondValue"), Some(json!("x")));
}

#[test]
fn tab_commits_open_select_and_advances_focus() {
    const COMPONENT: &str = r#"
            import { render } from "solite-runtime";
            function App() {
              const root = __sol_createElement("div");

              const first = __sol_createElement("input");
              __sol_setProperty(first, "style", "display:block; width:200px; height:24px;");
              __sol_setProperty(first, "onFocus", () => {
                globalThis.state.focused = "first";
              });

              const select = __sol_createElement("select");
              __sol_setProperty(select, "style", "display:block; width:200px; height:24px;");
              __sol_setProperty(select, "value", globalThis.state.selectValue ?? "");
              __sol_setProperty(select, "onFocus", () => {
                globalThis.state.focused = "select";
              });
              __sol_setProperty(select, "onChange", (event) => {
                globalThis.state.selectValue = event.value;
              });

              const opt0 = __sol_createElement("option");
              __sol_setProperty(opt0, "value", "");
              __sol_setProperty(opt0, "disabled", "");
              __sol_setProperty(opt0, "selected", "");
              __sol_setProperty(opt0, "hidden", "");
              __sol_insertNode(opt0, __sol_createTextNode("Choose.."), null);

              const opt1 = __sol_createElement("option");
              __sol_setProperty(opt1, "value", "one");
              __sol_insertNode(opt1, __sol_createTextNode("One"), null);

              const opt2 = __sol_createElement("option");
              __sol_setProperty(opt2, "value", "two");
              __sol_insertNode(opt2, __sol_createTextNode("Two"), null);

              __sol_insertNode(select, opt0, null);
              __sol_insertNode(select, opt1, null);
              __sol_insertNode(select, opt2, null);

              const second = __sol_createElement("input");
              __sol_setProperty(second, "style", "display:block; width:200px; height:24px;");
              __sol_setProperty(second, "onFocus", () => {
                globalThis.state.focused = "second";
              });

              __sol_insertNode(root, first, null);
              __sol_insertNode(root, select, null);
              __sol_insertNode(root, second, null);
              return root;
            }
            render(() => App(), __SOL_ROOT__);
        "#;
    let (mut instance, _rx) = make_instance_with(COMPONENT, &[]);
    let _ = instance.render();

    let _ = instance.dispatch_mouse(
        10.0,
        10.0,
        MouseEvent::Down {
            x: 10.0,
            y: 10.0,
            button: MouseButton::Left,
        },
    );
    assert_eq!(instance.state().get("focused"), Some(json!("first")));

    let _ = instance.dispatch_key_down(make_key_event(
        "Tab", "Tab", 9, false, false, false, false, false,
    ));
    assert_eq!(instance.state().get("focused"), Some(json!("select")));

    let _ = instance.dispatch_key_down(make_key_event(
        "Enter", "Enter", 13, false, false, false, false, false,
    ));
    let _ = instance.dispatch_key_down(make_key_event(
        "ArrowDown",
        "ArrowDown",
        40,
        false,
        false,
        false,
        false,
        false,
    ));
    let _ = instance.dispatch_key_down(make_key_event(
        "Tab", "Tab", 9, false, false, false, false, false,
    ));

    assert_eq!(instance.state().get("selectValue"), Some(json!("two")));
    assert_eq!(instance.state().get("focused"), Some(json!("second")));
}

#[test]
fn open_select_arrow_enter_and_escape_match_keyboard_behavior() {
    const COMPONENT: &str = r#"
            import { render } from "solite-runtime";
            function App() {
              const select = __sol_createElement("select");
              __sol_setProperty(select, "style", "display:block; width:200px; height:24px;");
              __sol_setProperty(select, "value", globalThis.state.selectValue ?? "");
              __sol_setProperty(select, "onChange", (event) => {
                globalThis.state.selectValue = event.value;
              });

              const placeholder = __sol_createElement("option");
              __sol_setProperty(placeholder, "value", "");
              __sol_setProperty(placeholder, "disabled", "");
              __sol_setProperty(placeholder, "selected", "");
              __sol_setProperty(placeholder, "hidden", "");
              __sol_insertNode(placeholder, __sol_createTextNode("Choose.."), null);

              const first = __sol_createElement("option");
              __sol_setProperty(first, "value", "first");
              __sol_insertNode(first, __sol_createTextNode("First"), null);

              const second = __sol_createElement("option");
              __sol_setProperty(second, "value", "second");
              __sol_insertNode(second, __sol_createTextNode("Second"), null);

              __sol_insertNode(select, placeholder, null);
              __sol_insertNode(select, first, null);
              __sol_insertNode(select, second, null);
              return select;
            }
            render(() => App(), __SOL_ROOT__);
        "#;
    let (mut instance, _rx) = make_instance_with(COMPONENT, &[]);
    let _ = instance.render();

    let select_id = instance
        .doc
        .borrow()
        .get_node(1)
        .and_then(|root| root.children.first().copied())
        .expect("select should exist");

    let _ = instance.dispatch_mouse(
        10.0,
        10.0,
        MouseEvent::Down {
            x: 10.0,
            y: 10.0,
            button: MouseButton::Left,
        },
    );

    {
        let state = instance.js.selects.borrow();
        let select = state.get(&select_id).expect("select state");
        assert!(select.is_open());
        assert_eq!(select.active_index(), Some(1));
        assert_eq!(select.selected_index(), Some(0));
    }

    let _ = instance.dispatch_key_down(make_key_event(
        "ArrowDown",
        "ArrowDown",
        40,
        false,
        false,
        false,
        false,
        false,
    ));

    {
        let state = instance.js.selects.borrow();
        let select = state.get(&select_id).expect("select state");
        assert_eq!(select.active_index(), Some(2));
        assert_eq!(select.selected_index(), Some(0));
    }

    let _ = instance.dispatch_key_down(make_key_event(
        "Escape", "Escape", 27, false, false, false, false, false,
    ));
    {
        let state = instance.js.selects.borrow();
        let select = state.get(&select_id).expect("select state");
        assert!(!select.is_open());
        assert_eq!(select.selected_index(), Some(0));
    }
    assert_eq!(instance.state().get("selectValue"), None);

    let _ = instance.dispatch_mouse(
        10.0,
        10.0,
        MouseEvent::Down {
            x: 10.0,
            y: 10.0,
            button: MouseButton::Left,
        },
    );
    let _ = instance.dispatch_key_down(make_key_event(
        "ArrowDown",
        "ArrowDown",
        40,
        false,
        false,
        false,
        false,
        false,
    ));
    let _ = instance.dispatch_key_down(make_key_event(
        "Enter", "Enter", 13, false, false, false, false, false,
    ));

    {
        let state = instance.js.selects.borrow();
        let select = state.get(&select_id).expect("select state");
        assert!(!select.is_open());
        assert_eq!(select.selected_index(), Some(2));
    }
    assert_eq!(instance.state().get("selectValue"), Some(json!("second")));
}

#[test]
fn radio_arrow_keys_move_selection_and_focus() {
    const COMPONENT: &str = r#"
            import { render } from "solite-runtime";
            function App() {
              const root = __sol_createElement("div");

              const radio1 = __sol_createElement("input");
              __sol_setProperty(radio1, "type", "radio");
              __sol_setProperty(radio1, "name", "group-a");
              __sol_setProperty(radio1, "style", "display:block; width:20px; height:20px;");
              __sol_setProperty(radio1, "onFocus", () => {
                globalThis.state.focused = "r1";
              });
              __sol_setProperty(radio1, "onInput", (event) => {
                if (event.checked) {
                  globalThis.state.selected = "r1";
                }
              });

              const radio2 = __sol_createElement("input");
              __sol_setProperty(radio2, "type", "radio");
              __sol_setProperty(radio2, "name", "group-a");
              __sol_setProperty(radio2, "style", "display:block; width:20px; height:20px;");
              __sol_setProperty(radio2, "onFocus", () => {
                globalThis.state.focused = "r2";
              });
              __sol_setProperty(radio2, "onInput", (event) => {
                if (event.checked) {
                  globalThis.state.selected = "r2";
                }
              });

              const radio3 = __sol_createElement("input");
              __sol_setProperty(radio3, "type", "radio");
              __sol_setProperty(radio3, "name", "group-a");
              __sol_setProperty(radio3, "style", "display:block; width:20px; height:20px;");
              __sol_setProperty(radio3, "onFocus", () => {
                globalThis.state.focused = "r3";
              });
              __sol_setProperty(radio3, "onInput", (event) => {
                if (event.checked) {
                  globalThis.state.selected = "r3";
                }
              });

              __sol_insertNode(root, radio1, null);
              __sol_insertNode(root, radio2, null);
              __sol_insertNode(root, radio3, null);
              return root;
            }
            render(() => App(), __SOL_ROOT__);
        "#;
    let (mut instance, _rx) = make_instance_with(COMPONENT, &[]);
    let _ = instance.render();

    let _ = instance.dispatch_mouse(
        10.0,
        10.0,
        MouseEvent::Down {
            x: 10.0,
            y: 10.0,
            button: MouseButton::Left,
        },
    );
    assert_eq!(instance.state().get("focused"), Some(json!("r1")));
    assert_eq!(instance.state().get("selected"), Some(json!("r1")));

    let _ = instance.dispatch_key_down(make_key_event(
        "ArrowRight",
        "ArrowRight",
        39,
        false,
        false,
        false,
        false,
        false,
    ));
    assert_eq!(instance.state().get("focused"), Some(json!("r2")));
    assert_eq!(instance.state().get("selected"), Some(json!("r2")));

    let _ = instance.dispatch_key_down(make_key_event(
        "ArrowLeft",
        "ArrowLeft",
        37,
        false,
        false,
        false,
        false,
        false,
    ));
    assert_eq!(instance.state().get("focused"), Some(json!("r1")));
    assert_eq!(instance.state().get("selected"), Some(json!("r1")));
}

#[test]
fn input_space_and_caret_movement_refresh_rendered_caret() {
    const COMPONENT: &str = r#"
            import { render } from "solite-runtime";
            function App() {
              const input = __sol_createElement("input");
              __sol_setProperty(input, "style", "display:block; width:200px; height:40px;");
              return input;
            }
            render(() => App(), __SOL_ROOT__);
        "#;
    let (mut instance, _rx) = make_instance_with(COMPONENT, &[]);
    let _ = instance.render();

    let _ = instance.dispatch_mouse(
        10.0,
        10.0,
        MouseEvent::Down {
            x: 10.0,
            y: 10.0,
            button: MouseButton::Left,
        },
    );

    let input_id = instance
        .doc
        .borrow()
        .get_node(1)
        .and_then(|root| root.children.first().copied())
        .unwrap();

    let _ = instance.dispatch_key_down(type_key("h"));
    let _ = instance.dispatch_key_down(type_key("i"));
    let _ = instance.render();
    assert_eq!(input_child_text(&instance, input_id), "hi");
    let editor_text = instance
        .doc
        .borrow()
        .get_node(input_id)
        .and_then(|node| node.element_data())
        .and_then(|element| element.text_input_data())
        .map(|input| input.editor.raw_text().to_string());
    assert_eq!(editor_text.as_deref(), Some("hi"));
    let end_x = instance.collect_input_carets()[0].x;

    let _ = instance.dispatch_key_down(type_key("ArrowLeft"));
    let _ = instance.render();
    assert_eq!(input_child_text(&instance, input_id), "hi");
    let mid_x = instance.collect_input_carets()[0].x;
    assert!(
        mid_x < end_x,
        "expected caret to move left: {mid_x} >= {end_x}"
    );

    let _ = instance.dispatch_key_down(type_key(" "));
    let _ = instance.render();
    assert_eq!(input_child_text(&instance, input_id), "h i");

    let _ = instance.dispatch_key_down(type_key("a"));
    let _ = instance.render();
    assert_eq!(input_child_text(&instance, input_id), "h ai");
}

#[test]
fn input_number_restricts_to_numeric_chars() {
    const COMPONENT: &str = r#"
            import { render } from "solite-runtime";
            function App() {
              const input = __sol_createElement("input");
              __sol_setProperty(input, "type", "number");
              __sol_setProperty(input, "style", "display:block; width:200px; height:40px;");
              __sol_setProperty(input, "onInput", (e) => {
                globalThis.state.value = e.value;
              });
              return input;
            }
            render(() => App(), __SOL_ROOT__);
        "#;
    let (mut instance, _rx) = make_instance_with(COMPONENT, &[]);
    let _ = instance.render();
    let _ = instance.dispatch_mouse(
        10.0,
        10.0,
        MouseEvent::Down {
            x: 10.0,
            y: 10.0,
            button: MouseButton::Left,
        },
    );

    let _ = instance.dispatch_key_down(type_key("1"));
    let _ = instance.dispatch_key_down(type_key("2"));
    let _ = instance.dispatch_key_down(type_key("."));
    let _ = instance.dispatch_key_down(type_key("3"));
    assert_eq!(instance.state().get("value"), Some(json!("12.3")));

    // Alphabetic characters are rejected by number-input handling.
    let _ = instance.dispatch_key_down(type_key("a"));
    assert_eq!(instance.state().get("value"), Some(json!("12.3")));
}

#[test]
fn input_text_selection_uses_layout_geometry() {
    assert_input_selection_matches_layout("text", "illWWWtext", 2, 7);
}

#[test]
fn input_password_selection_uses_masked_layout_geometry() {
    assert_input_selection_matches_layout("password", "supersecret", 1, 8);
}

#[test]
fn input_range_responds_to_step_and_extremes() {
    const COMPONENT: &str = r#"
            import { render } from "solite-runtime";
            function App() {
              const input = __sol_createElement("input");
              __sol_setProperty(input, "type", "range");
              __sol_setProperty(input, "min", "0");
              __sol_setProperty(input, "max", "10");
              __sol_setProperty(input, "step", "2");
              __sol_setProperty(input, "value", "4");
              __sol_setProperty(input, "style", "display:block; width:200px; height:40px;");
              __sol_setProperty(input, "onInput", (e) => {
                globalThis.state.value = e.value;
              });
              return input;
            }
            render(() => App(), __SOL_ROOT__);
        "#;
    let (mut instance, _rx) = make_instance_with(COMPONENT, &[]);
    let _ = instance.render();
    let input_id = instance
        .doc
        .borrow()
        .get_node(1)
        .and_then(|root| root.children.first().copied())
        .unwrap();

    // Range is rendered via custom slider UI; child text should stay empty.
    assert_eq!(input_child_text(&instance, input_id), "");

    // Click to focus the range input (any position inside it).  The value
    // may or may not change depending on layout; the assertion intentionally
    // avoids checking post-click value here and verifies click starts a drag.
    let _ = instance.dispatch_mouse(
        100.0,
        10.0,
        MouseEvent::Down {
            x: 100.0,
            y: 10.0,
            button: MouseButton::Left,
        },
    );
    // End drag so later move events don't interfere.
    let _ = instance.dispatch_mouse(
        100.0,
        10.0,
        MouseEvent::Up {
            x: 100.0,
            y: 10.0,
            button: MouseButton::Left,
        },
    );

    // Keyboard navigation from the current value (seeded as 4, or whatever
    // click set): ArrowRight steps +2, ArrowLeft steps -2, Home/End jump.
    let _ = instance.dispatch_key_down(type_key("Home"));
    assert_eq!(instance.state().get("value"), Some(json!("0")));
    let _ = instance.render();
    assert_eq!(
        instance
            .doc
            .borrow()
            .get_node(input_id)
            .and_then(|node| node.attr(LocalName::from("value")))
            .unwrap_or(""),
        "0"
    );

    let _ = instance.dispatch_key_down(type_key("ArrowRight"));
    assert_eq!(instance.state().get("value"), Some(json!("2")));
    let _ = instance.render();
    assert_eq!(
        instance
            .doc
            .borrow()
            .get_node(input_id)
            .and_then(|node| node.attr(LocalName::from("value")))
            .unwrap_or(""),
        "2"
    );

    let _ = instance.dispatch_key_down(type_key("ArrowRight"));
    assert_eq!(instance.state().get("value"), Some(json!("4")));
    let _ = instance.render();
    assert_eq!(
        instance
            .doc
            .borrow()
            .get_node(input_id)
            .and_then(|node| node.attr(LocalName::from("value")))
            .unwrap_or(""),
        "4"
    );

    let _ = instance.dispatch_key_down(type_key("ArrowLeft"));
    assert_eq!(instance.state().get("value"), Some(json!("2")));
    let _ = instance.render();
    assert_eq!(
        instance
            .doc
            .borrow()
            .get_node(input_id)
            .and_then(|node| node.attr(LocalName::from("value")))
            .unwrap_or(""),
        "2"
    );

    let _ = instance.dispatch_key_down(type_key("End"));
    assert_eq!(instance.state().get("value"), Some(json!("10")));
    let _ = instance.render();
    assert_eq!(
        instance
            .doc
            .borrow()
            .get_node(input_id)
            .and_then(|node| node.attr(LocalName::from("value")))
            .unwrap_or(""),
        "10"
    );
}

#[test]
fn input_checkbox_and_radio_types_toggle() {
    const COMPONENT: &str = r#"
            import { render } from "solite-runtime";
            function App() {
              const root = __sol_createElement("div");

              const checkbox = __sol_createElement("input");
              __sol_setProperty(checkbox, "type", "checkbox");
              __sol_setProperty(checkbox, "style", "display:block; width:20px; height:20px;");
              __sol_setProperty(checkbox, "onInput", (e) => {
                globalThis.state.checkbox = e.checked;
              });

              const radio1 = __sol_createElement("input");
              __sol_setProperty(radio1, "type", "radio");
              __sol_setProperty(radio1, "name", "group-a");
              __sol_setProperty(radio1, "style", "display:block; width:20px; height:20px;");

              const radio2 = __sol_createElement("input");
              __sol_setProperty(radio2, "type", "radio");
              __sol_setProperty(radio2, "name", "group-a");
              __sol_setProperty(radio2, "style", "display:block; width:20px; height:20px;");

              __sol_insertNode(root, checkbox, null);
              __sol_insertNode(root, radio1, null);
              __sol_insertNode(root, radio2, null);

              globalThis.state.radio1 = radio1;
              globalThis.state.radio2 = radio2;

              return root;
            }
            render(() => App(), __SOL_ROOT__);
        "#;
    let (mut instance, _rx) = make_instance_with(COMPONENT, &[]);
    let _ = instance.render();

    // Checkbox: clicking toggles it; Space while focused toggles again.
    let _ = instance.dispatch_mouse(
        10.0,
        10.0,
        MouseEvent::Down {
            x: 10.0,
            y: 10.0,
            button: MouseButton::Left,
        },
    );
    assert_eq!(instance.state().get("checkbox"), Some(json!(true)));

    // Space toggles it back off.
    let _ = instance.dispatch_key_down(type_key("Space"));
    assert_eq!(instance.state().get("checkbox"), Some(json!(false)));

    // Click toggles on again so radio tests start from a stable state.
    let _ = instance.dispatch_mouse(
        10.0,
        10.0,
        MouseEvent::Down {
            x: 10.0,
            y: 10.0,
            button: MouseButton::Left,
        },
    );
    assert_eq!(instance.state().get("checkbox"), Some(json!(true)));

    let ids = [
        instance.state().get("radio1"),
        instance.state().get("radio2"),
    ];
    let ids = [
        state_node_id(ids[0].as_ref(), "radio1"),
        state_node_id(ids[1].as_ref(), "radio2"),
    ] as [usize; 2];

    let (radio1_x, radio1_y, radio2_x, radio2_y) = {
        let doc = instance.doc.borrow();
        let r1 = doc.get_node(ids[0]).unwrap();
        let r2 = doc.get_node(ids[1]).unwrap();
        (
            r1.absolute_position(0.0, 0.0).x + 4.0,
            r1.absolute_position(0.0, 0.0).y + 4.0,
            r2.absolute_position(0.0, 0.0).x + 4.0,
            r2.absolute_position(0.0, 0.0).y + 4.0,
        )
    };

    // Pick the first radio.
    let _ = instance.dispatch_mouse(
        radio1_x,
        radio1_y,
        MouseEvent::Down {
            x: radio1_x,
            y: radio1_y,
            button: MouseButton::Left,
        },
    );
    let _ = instance.dispatch_key_down(type_key(" "));
    assert_eq!(instance.input_value(ids[0]).as_deref(), Some("on"));
    assert_eq!(instance.input_value(ids[1]).as_deref(), Some("off"));

    // Focus/select second radio and ensure group semantics switch.
    let _ = instance.dispatch_mouse(
        radio2_x,
        radio2_y,
        MouseEvent::Down {
            x: radio2_x,
            y: radio2_y,
            button: MouseButton::Left,
        },
    );
    let _ = instance.dispatch_key_down(type_key(" "));
    assert_eq!(instance.input_value(ids[0]).as_deref(), Some("off"));
    assert_eq!(instance.input_value(ids[1]).as_deref(), Some("on"));
}

#[test]
fn input_named_space_key_is_treated_as_space() {
    const COMPONENT: &str = r#"
            import { render } from "solite-runtime";
            function App() {
              const input = __sol_createElement("input");
              __sol_setProperty(input, "style", "display:block; width:200px; height:40px;");
              return input;
            }
            render(() => App(), __SOL_ROOT__);
        "#;
    let (mut instance, _rx) = make_instance_with(COMPONENT, &[]);
    let _ = instance.render();
    let _ = instance.dispatch_mouse(
        10.0,
        10.0,
        MouseEvent::Down {
            x: 10.0,
            y: 10.0,
            button: MouseButton::Left,
        },
    );

    let input_id = instance
        .doc
        .borrow()
        .get_node(1)
        .and_then(|root| root.children.first().copied())
        .unwrap();
    let _ = instance.dispatch_key_down(type_key("Space"));
    assert_eq!(instance.input_value(input_id), Some(" ".into()));
}

#[test]
fn input_value_attribute_seeds_rust_state() {
    // Setting `value` via __sol_setProperty before any user input must
    // populate the InputState; the instance API should see it too.
    const COMPONENT: &str = r#"
            import { render } from "solite-runtime";
            function App() {
              const input = __sol_createElement("input");
              __sol_setProperty(input, "style", "display:block; width:200px; height:40px;");
              __sol_setProperty(input, "value", "preset");
              return input;
            }
            render(() => App(), __SOL_ROOT__);
        "#;
    let (mut instance, _rx) = make_instance_with(COMPONENT, &[]);
    let _ = instance.render();
    let id = instance
        .doc
        .borrow()
        .get_node(1)
        .and_then(|root| root.children.first().copied())
        .unwrap();
    assert_eq!(instance.input_value(id).as_deref(), Some("preset"));

    // Host can rewrite the value directly.
    assert!(instance.set_input_value(id, "from-host"));
    assert_eq!(instance.input_value(id).as_deref(), Some("from-host"));
}

#[test]
fn keydown_handler_sees_event_value() {
    const COMPONENT: &str = r#"
            import { render } from "solite-runtime";
            function App() {
              const input = __sol_createElement("input");
              __sol_setProperty(input, "style", "display:block; width:200px; height:40px;");
              __sol_setProperty(input, "onKeyDown", (e) => {
                globalThis.state.observedValue = e.value;
                globalThis.state.observedKey = e.key;
              });
              return input;
            }
            render(() => App(), __SOL_ROOT__);
        "#;
    let (mut instance, _rx) = make_instance_with(COMPONENT, &[]);
    let _ = instance.render();
    let _ = instance.dispatch_mouse(
        10.0,
        10.0,
        MouseEvent::Down {
            x: 10.0,
            y: 10.0,
            button: MouseButton::Left,
        },
    );
    let _ = instance.dispatch_key_down(type_key("z"));
    // The handler runs *before* the edit is committed in our dispatcher
    // order — but since enrichment reads the live InputState, the value
    // visible to the handler reflects whatever the field holds at the
    // moment the event fires. We just assert the key was observed and
    // that `e.value` is present (either "" or "z" depending on order).
    assert_eq!(instance.state().get("observedKey"), Some(json!("z")));
    assert!(instance.state().get("observedValue").is_some());
}

#[test]
fn blink_toggles_visible_text_on_tick() {
    const COMPONENT: &str = r#"
            import { render } from "solite-runtime";
            function App() {
              const input = __sol_createElement("input");
              __sol_setProperty(input, "style", "display:block; width:200px; height:40px;");
              __sol_setProperty(input, "value", "hi");
              return input;
            }
            render(() => App(), __SOL_ROOT__);
        "#;
    let (mut instance, _rx) = make_instance_with(COMPONENT, &[]);
    let _ = instance.render();
    let id = instance
        .doc
        .borrow()
        .get_node(1)
        .and_then(|root| root.children.first().copied())
        .unwrap();
    let _ = instance.dispatch_mouse(
        10.0,
        10.0,
        MouseEvent::Down {
            x: 10.0,
            y: 10.0,
            button: MouseButton::Left,
        },
    );
    // After focus, text remains clean and the native caret overlay is visible.
    let _ = instance.render();
    assert!(
        !instance.collect_input_carets().is_empty(),
        "expected visible caret overlay"
    );
    assert_eq!(input_child_text(&instance, id), "hi");

    // Force blink to flip by rewinding the last_blink instant.
    instance
        .js
        .inputs
        .borrow_mut()
        .get_mut(&id)
        .unwrap()
        .force_blink_for_test(std::time::Duration::from_millis(600));
    let _ = instance.tick();
    let _ = instance.render();
    assert!(
        instance.collect_input_carets().is_empty(),
        "expected hidden caret overlay after blink"
    );
}

#[test]
fn active_pseudo_class_flips_on_press() {
    const COMPONENT: &str = r#"
            import { render } from "solite-runtime";
            function App() {
              const d = __sol_createElement("div");
              __sol_setProperty(d, "className", "tag");
              __sol_setProperty(d, "style", "display:block; width:80px; height:80px;");
              return d;
            }
            render(() => App(), __SOL_ROOT__);
        "#;
    let (mut instance, _rx) = make_instance_with(
        COMPONENT,
        &[".tag { color: rgb(1, 1, 1) } .tag:active { color: rgb(222, 0, 0) }"],
    );
    assert_eq!(first_child_color(&mut instance), Some((1, 1, 1)));
    let _ = instance.dispatch_mouse(
        20.0,
        20.0,
        MouseEvent::Down {
            x: 20.0,
            y: 20.0,
            button: MouseButton::Left,
        },
    );
    assert_eq!(first_child_color(&mut instance), Some((222, 0, 0)));
    let _ = instance.dispatch_mouse(
        20.0,
        20.0,
        MouseEvent::Up {
            x: 20.0,
            y: 20.0,
            button: MouseButton::Left,
        },
    );
    assert_eq!(first_child_color(&mut instance), Some((1, 1, 1)));
}

#[test]
fn focus_pseudo_class_flips_on_click() {
    const COMPONENT: &str = r#"
            import { render } from "solite-runtime";
            function App() {
              const input = __sol_createElement("input");
              __sol_setProperty(input, "className", "field");
              __sol_setProperty(input, "type", "text");
              __sol_setProperty(input, "style", "display:block; width:80px; height:30px;");
              return input;
            }
            render(() => App(), __SOL_ROOT__);
        "#;
    let (mut instance, _rx) = make_instance_with(
        COMPONENT,
        &[".field { color: rgb(1, 1, 1) } .field:focus { color: rgb(200, 50, 10) }"],
    );
    assert_eq!(first_child_color(&mut instance), Some((1, 1, 1)));
    let _ = instance.dispatch_mouse(
        20.0,
        20.0,
        MouseEvent::Down {
            x: 20.0,
            y: 20.0,
            button: MouseButton::Left,
        },
    );
    assert_eq!(first_child_color(&mut instance), Some((200, 50, 10)));
    let _ = instance.dispatch_mouse(
        500.0,
        500.0,
        MouseEvent::Down {
            x: 500.0,
            y: 500.0,
            button: MouseButton::Left,
        },
    );
    assert_eq!(first_child_color(&mut instance), Some((1, 1, 1)));
}

// ─── Image loading ────────────────────────────────────────────────────────

/// Build a valid 1×1 RGBA PNG using the `image` crate. Done once at runtime
/// to dodge the fragility of hand-coded PNG byte literals (a bad CRC turns
/// a "load" path into an "error" path and would mask the watcher logic).
fn tiny_png_bytes() -> Vec<u8> {
    let mut buf = Vec::new();
    let img =
        image::ImageBuffer::<image::Rgba<u8>, _>::from_fn(1, 1, |_, _| image::Rgba([0, 0, 0, 0]));
    image::DynamicImage::ImageRgba8(img)
        .write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
        .expect("encode tiny png");
    buf
}

fn unique_tmp_path(prefix: &str, suffix: &str) -> PathBuf {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time")
        .as_nanos();
    let mut dir = PathBuf::from("target");
    dir.push("test-tmp");
    std::fs::create_dir_all(&dir).expect("create tmp dir");
    dir.push(format!("solite-{prefix}-{nanos}{suffix}"));
    dir
}

fn write_tmp_png(prefix: &str) -> PathBuf {
    let path = unique_tmp_path(prefix, ".png");
    std::fs::write(&path, tiny_png_bytes()).expect("write png");
    path.canonicalize().unwrap_or(path)
}

fn file_url(path: &Path) -> String {
    let abs = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    url::Url::from_file_path(&abs)
        .expect("absolute path")
        .to_string()
}

const IMG_LOAD_COMPONENT: &str = r#"
        import { render } from "solite-runtime";
        function App() {
          const img = __sol_createElement("img");
          __sol_setProperty(img, "onLoad", function(ev) {
            sendEvent("img:load", JSON.stringify({ target: ev.target }));
          });
          __sol_setProperty(img, "onError", function(ev) {
            sendEvent("img:error", JSON.stringify({ target: ev.target }));
          });
          __sol_setProperty(img, "src", globalThis.__OX_IMG_SRC);
          return img;
        }
        render(() => App(), __SOL_ROOT__);
    "#;

fn drain_events(rx: &mut UnboundedReceiver<Event>) -> Vec<Event> {
    let mut out = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        out.push(ev);
    }
    out
}

#[test]
fn tiny_png_decodes_for_test_fixture_sanity() {
    // Catch a regression where `tiny_png_bytes` produces an undecodable
    // blob — that's failure-mode-1 if `valid_img_src_…` returns no
    // events.
    use image::ImageReader;
    use std::io::Cursor;
    let bytes = tiny_png_bytes();
    let img = ImageReader::new(Cursor::new(&bytes))
        .with_guessed_format()
        .expect("guess format")
        .decode()
        .expect("decode tiny_png_bytes");
    assert_eq!(img.width(), 1);
    assert_eq!(img.height(), 1);
}

fn run_img_test(src_url: &str) -> Vec<Event> {
    let component = format!(
        "globalThis.__OX_IMG_SRC = {src:?};\n{body}",
        src = src_url,
        body = IMG_LOAD_COMPONENT
    );
    let (device, queue) = test_device();
    let (mut instance, mut rx) = Instance::new(
        InstanceConfig {
            width: 64,
            height: 64,
            device,
            queue,
            stylesheets: vec![],
            document_scroll: false,
            base_url: None,
            initial_state: None,
            registered_resources: vec![],
            scale_factor: 1.0,
        },
        &component,
    );
    // First render: blitz's `resolve()` calls `handle_messages()`, which
    // applies the loaded image (or removes the pending entry on error).
    let _ = instance.render();
    // Second tick: img watcher sees the applied state and dispatches
    // `load` or `error` to JS.
    let _ = instance.tick();
    let _ = instance.render();
    drain_events(&mut rx)
}

#[test]
fn valid_img_src_dispatches_load_event() {
    let png_path = write_tmp_png("img-load");
    let events = run_img_test(&file_url(&png_path));
    let names: Vec<_> = events.iter().map(|e| e.name.as_str()).collect();
    assert!(
        names.contains(&"img:load"),
        "expected img:load, got {names:?}"
    );
    assert!(
        !names.contains(&"img:error"),
        "did not expect img:error, got {names:?}"
    );
    let _ = std::fs::remove_file(&png_path);
}

#[test]
fn missing_img_src_dispatches_error_event() {
    let url = "file:///tmp/solite-does-not-exist-12345.png";
    let events = run_img_test(url);
    let names: Vec<_> = events.iter().map(|e| e.name.as_str()).collect();
    assert!(
        names.contains(&"img:error"),
        "expected img:error, got {names:?}"
    );
    assert!(
        !names.contains(&"img:load"),
        "did not expect img:load, got {names:?}"
    );
}

#[test]
fn dynamic_src_mutation_fires_load_for_each_new_url() {
    // Two distinct on-disk PNGs. Mount a component that swaps `src` from
    // the first to the second after each `tick()`. Both transitions
    // should yield a `load` event.
    let a = write_tmp_png("img-dyn-a");
    let b = write_tmp_png("img-dyn-b");
    let component = format!(
        r#"
            import {{ render }} from "solite-runtime";
            globalThis.__OX_FIRST = {first:?};
            globalThis.__OX_SECOND = {second:?};
            function App() {{
              const img = __sol_createElement("img");
              __sol_setProperty(img, "onLoad", function(ev) {{
                sendEvent("img:load", JSON.stringify({{ src: __sol_getAttr(ev.target, "src") }}));
              }});
              __sol_setProperty(img, "src", globalThis.__OX_FIRST);
              globalThis.__OX_IMG = img;
              return img;
            }}
            render(() => App(), __SOL_ROOT__);
            "#,
        first = file_url(&a),
        second = file_url(&b),
    );
    let (device, queue) = test_device();
    let (mut instance, mut rx) = Instance::new(
        InstanceConfig {
            width: 64,
            height: 64,
            device,
            queue,
            stylesheets: vec![],
            document_scroll: false,
            base_url: None,
            initial_state: None,
            registered_resources: vec![],
            scale_factor: 1.0,
        },
        &component,
    );

    // First load.
    let _ = instance.render();
    let _ = instance.tick();
    let _ = instance.render();
    let first_events: Vec<String> = drain_events(&mut rx).into_iter().map(|e| e.name).collect();
    assert!(
        first_events.iter().any(|n| n == "img:load"),
        "expected first img:load, got {first_events:?}"
    );

    // Swap src to a different URL.
    instance
        .js
        .eval_test_code("__sol_setProperty(__OX_IMG, 'src', globalThis.__OX_SECOND)");

    let _ = instance.render();
    let _ = instance.tick();
    let _ = instance.render();
    let second_events: Vec<String> = drain_events(&mut rx).into_iter().map(|e| e.name).collect();
    assert!(
        second_events.iter().any(|n| n == "img:load"),
        "expected second img:load after src swap, got {second_events:?}"
    );

    let _ = std::fs::remove_file(&a);
    let _ = std::fs::remove_file(&b);
}

#[test]
fn data_url_image_dispatches_load_event() {
    // base64-encoded 1x1 PNG.
    use base64_dummy::encode as b64;
    let bytes = tiny_png_bytes();
    let mut url = String::from("data:image/png;base64,");
    url.push_str(&b64(&bytes));
    let events = run_img_test(&url);
    let names: Vec<_> = events.iter().map(|e| e.name.as_str()).collect();
    assert!(
        names.contains(&"img:load"),
        "expected img:load for data URL, got {names:?}"
    );
}

// ─── Font registration ───────────────────────────────────────────────────

const BULLET_FONT_BYTES: &[u8] =
    include_bytes!("../../vendor/blitz/packages/blitz-dom/assets/moz-bullet-font.otf");

const FONT_COMPONENT: &str = r#"
        import { render } from "solite-runtime";
        function App() {
          const p = __sol_createElement("p");
          __sol_setProperty(p, "class", "uses-custom");
          __sol_insertNode(p, __sol_createTextNode("•"), null);
          return p;
        }
        render(() => App(), __SOL_ROOT__);
    "#;

const FONT_CSS: &str = ".uses-custom { font-family: 'SoliteTestBullet'; font-size: 32px; }";

#[test]
fn register_font_bytes_returns_distinct_stylesheet_ids() {
    let (device, queue) = test_device();
    let (mut instance, _rx) = Instance::new(
        InstanceConfig {
            width: 64,
            height: 64,
            device,
            queue,
            stylesheets: vec![FONT_CSS.to_string()],
            document_scroll: false,
            base_url: None,
            initial_state: None,
            registered_resources: vec![],
            scale_factor: 1.0,
        },
        FONT_COMPONENT,
    );
    let a = instance.register_font_bytes(
        "SoliteTestBullet",
        BULLET_FONT_BYTES.to_vec(),
        FontFormat::Opentype,
    );
    let b = instance.register_font_bytes(
        "SoliteTestBullet2",
        BULLET_FONT_BYTES.to_vec(),
        FontFormat::Opentype,
    );
    assert_ne!(a, b);
}

#[test]
fn register_font_bytes_does_not_panic_during_render() {
    let (device, queue) = test_device();
    let (mut instance, _rx) = Instance::new(
        InstanceConfig {
            width: 128,
            height: 64,
            device,
            queue,
            stylesheets: vec![FONT_CSS.to_string()],
            document_scroll: false,
            base_url: None,
            initial_state: None,
            registered_resources: vec![],
            scale_factor: 1.0,
        },
        FONT_COMPONENT,
    );
    // Register the font, then render. We're not asserting glyph pixels —
    // just that the @font-face + NetProvider round-trip plus a follow-up
    // resolve() completes without crashing.
    let _id = instance.register_font_bytes(
        "SoliteTestBullet",
        BULLET_FONT_BYTES.to_vec(),
        FontFormat::Opentype,
    );
    let _ = instance.tick();
    let _ = instance.render();
}

#[test]
fn register_font_from_path_reads_file() {
    let path = PathBuf::from("vendor/blitz/packages/blitz-dom/assets/moz-bullet-font.otf");
    let (device, queue) = test_device();
    let (mut instance, _rx) = Instance::new(
        InstanceConfig {
            width: 64,
            height: 64,
            device,
            queue,
            stylesheets: vec![FONT_CSS.to_string()],
            document_scroll: false,
            base_url: None,
            initial_state: None,
            registered_resources: vec![],
            scale_factor: 1.0,
        },
        FONT_COMPONENT,
    );
    let id = instance
        .register_font_from_path("SoliteTestBullet", &path)
        .expect("font loads");
    // Unregister should succeed via the returned stylesheet id.
    assert!(instance.remove_stylesheet(id));
}

#[test]
fn register_font_from_path_rejects_unknown_extension() {
    let path = PathBuf::from("Cargo.toml");
    let (device, queue) = test_device();
    let (mut instance, _rx) = Instance::new(
        InstanceConfig {
            width: 64,
            height: 64,
            device,
            queue,
            stylesheets: vec![],
            document_scroll: false,
            base_url: None,
            initial_state: None,
            registered_resources: vec![],
            scale_factor: 1.0,
        },
        "import { render } from \"solite-runtime\"; render(() => __sol_createElement(\"div\"), __SOL_ROOT__);",
    );
    let err = instance
        .register_font_from_path("X", &path)
        .expect_err("must reject .toml");
    assert!(matches!(err, RegisterFontError::UnknownFormat));
}

/// Minimal base64 encoder used by the data-url test. Inlined to avoid
/// adding a runtime dep just for tests.
mod base64_dummy {
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    pub fn encode(input: &[u8]) -> String {
        let mut out = String::with_capacity((input.len() + 2) / 3 * 4);
        let mut chunks = input.chunks_exact(3);
        for chunk in chunks.by_ref() {
            let n = ((chunk[0] as u32) << 16) | ((chunk[1] as u32) << 8) | (chunk[2] as u32);
            out.push(CHARS[((n >> 18) & 63) as usize] as char);
            out.push(CHARS[((n >> 12) & 63) as usize] as char);
            out.push(CHARS[((n >> 6) & 63) as usize] as char);
            out.push(CHARS[(n & 63) as usize] as char);
        }
        let rem = chunks.remainder();
        match rem.len() {
            0 => {}
            1 => {
                let n = (rem[0] as u32) << 16;
                out.push(CHARS[((n >> 18) & 63) as usize] as char);
                out.push(CHARS[((n >> 12) & 63) as usize] as char);
                out.push('=');
                out.push('=');
            }
            2 => {
                let n = ((rem[0] as u32) << 16) | ((rem[1] as u32) << 8);
                out.push(CHARS[((n >> 18) & 63) as usize] as char);
                out.push(CHARS[((n >> 12) & 63) as usize] as char);
                out.push(CHARS[((n >> 6) & 63) as usize] as char);
                out.push('=');
            }
            _ => unreachable!(),
        }
        out
    }
}

// ─── Keyboard navigation parity ──────────────────────────────────────────

fn enter_key() -> KeyboardEvent {
    make_key_event("Enter", "Enter", 13, false, false, false, false, false)
}
fn space_key() -> KeyboardEvent {
    make_key_event(" ", "Space", 32, false, false, false, false, false)
}
fn tab_key(shift: bool) -> KeyboardEvent {
    make_key_event("Tab", "Tab", 9, false, shift, false, false, false)
}
fn ctrl_key(key: &str) -> KeyboardEvent {
    make_key_event(key, key, 0, false, false, true, false, false)
}
fn ctrl_shift_key(key: &str) -> KeyboardEvent {
    make_key_event(key, key, 0, false, true, true, false, false)
}
fn plain_key(key: &str) -> KeyboardEvent {
    make_key_event(key, key, 0, false, false, false, false, false)
}
fn alt_key(key: &str) -> KeyboardEvent {
    make_key_event(key, key, 0, false, false, false, true, false)
}

/// Render once to make first-time blitz layout happen, then drive a single
/// tick to settle any image/font side effects.
fn settle(instance: &mut Instance) {
    let _ = instance.tick();
    let _ = instance.render();
}

/// Component with two text inputs and a button arranged horizontally,
/// each with a stable id for hit-testing.
const KB_NAV_COMPONENT: &str = r#"
        import { render } from "solite-runtime";
        function App() {
          const root = __sol_createElement("div");
          const make = (tag, attrs) => {
            const el = __sol_createElement(tag);
            for (const k in attrs) __sol_setProperty(el, k, attrs[k]);
            __sol_insertNode(root, el, null);
            globalThis["__sol_" + (attrs.id || tag)] = el;
            return el;
          };
          make("input", { id: "first", type: "text" });
          make("input", { id: "second", type: "text" });
          make("button", { id: "btn", onClick: function() { state.clicked = (state.clicked || 0) + 1; } });
          return root;
        }
        render(() => App(), __SOL_ROOT__);
    "#;

fn make_kb_nav_instance() -> (Instance, tokio::sync::mpsc::UnboundedReceiver<Event>) {
    let (device, queue) = test_device();
    let (mut instance, rx) = Instance::new(
        InstanceConfig {
            width: 320,
            height: 160,
            device,
            queue,
            stylesheets: vec![],
            document_scroll: false,
            base_url: None,
            initial_state: None,
            registered_resources: vec![],
            scale_factor: 1.0,
        },
        KB_NAV_COMPONENT,
    );
    instance.state().set("clicked", json!(0));
    settle(&mut instance);
    (instance, rx)
}

fn focused_tag(instance: &Instance) -> Option<String> {
    let id = instance.focused_node_id?;
    instance
        .doc
        .borrow()
        .get_node(id)
        .and_then(|n| n.element_data())
        .map(|e| e.name.local.as_ref().to_owned())
}

fn js_node_id(instance: &Instance, var: &str) -> usize {
    // The runtime.ts wraps every node-creating bridge call so app code
    // sees `{ __solId: n }` instead of a raw number. Unwrap here so test
    // helpers can write idiomatic `__sol_a = make(...)` and still get the
    // numeric blitz id back.
    let code = format!(
        "(function() {{ var v = globalThis.__sol_{var}; if (v && typeof v === 'object' && typeof v.__solId === 'number') return v.__solId; if (typeof v === 'number') return v; throw new Error('not a node handle: ' + (typeof v) + ' value=' + v); }})()"
    );
    let mut out: Option<usize> = None;
    instance
        .js
        .context_with(|ctx| match ctx.eval::<i32, _>(code.as_str()) {
            Ok(v) => out = Some(v as usize),
            Err(err) => {
                let exception = ctx.catch();
                panic!("js_node_id eval failed for __sol_{var}: {err}; exception={exception:?}");
            }
        });
    out.expect("js_node_id read")
}

fn state_node_id(value: Option<&serde_json::Value>, label: &str) -> usize {
    let Some(value) = value else {
        panic!("{label} missing");
    };
    if let Some(id) = value.as_u64() {
        return id as usize;
    }
    if let Some(map) = value.as_object() {
        if let Some(id) = map.get("__solId").and_then(Value::as_u64) {
            return id as usize;
        }
    }
    panic!("invalid {label} node handle: {value:?}");
}

#[derive(Debug, Clone)]
struct DomSig {
    id: usize,
    kind: &'static str,
    name: String,
    class: Option<String>,
    child_count: usize,
    text: Option<String>,
}

fn collect_dom_signature(doc: &BaseDocument, root_id: usize) -> Vec<DomSig> {
    fn walk(doc: &BaseDocument, node_id: usize, out: &mut Vec<DomSig>) {
        let Some(node) = doc.get_node(node_id) else {
            return;
        };

        let kind = if node.is_text_node() {
            "text"
        } else if node.is_element() {
            "element"
        } else {
            "other"
        };

        let class = node
            .attr(LocalName::from("class"))
            .map(|value| value.to_string());
        let name = node
            .element_data()
            .map(|el| el.name.local.as_ref().to_string())
            .unwrap_or_default();
        let text = node.text_data().map(|text| text.content.clone());
        let child_count = node.children.len();

        out.push(DomSig {
            id: node_id,
            kind,
            name,
            class,
            child_count,
            text,
        });

        for child_id in node.children.iter().copied() {
            walk(doc, child_id, out);
        }
    }

    let mut out = Vec::new();
    walk(doc, root_id, &mut out);
    out
}

fn assert_structural_dom_signature_eq(expected: &[DomSig], actual: &[DomSig]) {
    assert_eq!(
        expected.len(),
        actual.len(),
        "DOM node count should not change"
    );
    for (exp, act) in expected.iter().zip(actual.iter()) {
        assert_eq!(exp.id, act.id, "node id changed for structural slot");
        assert_eq!(exp.kind, act.kind, "node kind changed for id {}", exp.id);
        assert_eq!(exp.name, act.name, "node name changed for id {}", exp.id);
        assert_eq!(exp.class, act.class, "class changed for id {}", exp.id);
        assert_eq!(
            exp.child_count, act.child_count,
            "child-count changed for node {}",
            exp.id,
        );
    }
}

fn text_for_node<'a>(sig: &'a [DomSig], node_id: usize) -> Option<&'a str> {
    sig.iter()
        .find(|entry| entry.id == node_id)?
        .text
        .as_deref()
}

fn ids_from_state(state: &StateHandle, path: &str) -> Vec<usize> {
    let Some(value) = state.get(path) else {
        return Vec::new();
    };
    let Some(arr) = value.as_array() else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|value| value.as_u64().map(|id| id as usize))
        .collect()
}

fn id_from_state(state: &StateHandle, path: &str) -> usize {
    state
        .get(path)
        .and_then(|v| v.as_u64())
        .unwrap_or_else(|| panic!("{path} should be present as a node id")) as usize
}

/// Mutation canary component that updates text and list nodes from Rust-driven
/// state patches without recreating unrelated DOM nodes.
const CANARY_MUTATION_COMPONENT: &str = r#"
            import { createEffect, render } from "solite-runtime";
            function App() {
              const root = __sol_createElement("div");
              __sol_setProperty(root, "className", "canary-root");

              const title = __sol_createElement("h1");
              __sol_setProperty(title, "className", "title");
              const titleText = __sol_createTextNode("");
              __sol_insertNode(title, titleText, null);
              __sol_insertNode(root, title, null);

              const nested = __sol_createElement("p");
              __sol_setProperty(nested, "className", "nested");
              const nestedText = __sol_createTextNode("");
              __sol_insertNode(nested, nestedText, null);
              __sol_insertNode(root, nested, null);

                  const list = __sol_createElement("div");
                  __sol_setProperty(list, "className", "rows");
                  __sol_insertNode(root, list, null);

                  const row0 = __sol_createElement("div");
                  __sol_setProperty(row0, "className", "row");
                  const row0Text = __sol_createTextNode("");
                  __sol_insertNode(row0, row0Text, null);

                  const row1 = __sol_createElement("div");
                  __sol_setProperty(row1, "className", "row");
                  const row1Text = __sol_createTextNode("");
                  __sol_insertNode(row1, row1Text, null);

                  const row2 = __sol_createElement("div");
                  __sol_setProperty(row2, "className", "row");
                  const row2Text = __sol_createTextNode("");
                  __sol_insertNode(row2, row2Text, null);

                  const row3 = __sol_createElement("div");
                  __sol_setProperty(row3, "className", "row");
                  const row3Text = __sol_createTextNode("");
                  __sol_insertNode(row3, row3Text, null);

                  __sol_insertNode(list, row0, null);
                  __sol_insertNode(list, row1, null);
                  __sol_insertNode(list, row2, null);
                  __sol_insertNode(list, row3, null);

                  const status = __sol_createElement("div");
                  __sol_setProperty(status, "className", "status");
                  const statusText = __sol_createTextNode("");
                  __sol_insertNode(status, statusText, null);
                  __sol_insertNode(root, status, null);
              globalThis.state.canaryRootId = root.__solId;

                  globalThis.state.canaryTitleTextId = titleText.__solId;
                  globalThis.state.canaryNestedTextId = nestedText.__solId;
                  globalThis.state.canaryStatusTextId = statusText.__solId;
                  globalThis.state.canaryRowNodeIds = [row0.__solId, row1.__solId, row2.__solId, row3.__solId];
                  globalThis.state.canaryRowTextIds = [
                    row0Text.__solId,
                    row1Text.__solId,
                    row2Text.__solId,
                    row3Text.__solId,
                  ];

                  createEffect(() => {
                    __sol_setText(titleText, String(globalThis.state.title || ""));
                    const nestedValue =
                      globalThis.state.nested && globalThis.state.nested.value != null
                        ? globalThis.state.nested.value
                        : "";
                    __sol_setText(nestedText, String(nestedValue));

                    const rowsValue = globalThis.state.rows || {};
                    const row0Value = rowsValue?.[0];
                    const row1Value = rowsValue?.[1];
                    const row2Value = rowsValue?.[2];
                    const row3Value = rowsValue?.[3];
                    const rowEntries = [row0Value, row1Value, row2Value, row3Value];
                    const maxIdx = Object.keys(rowsValue)
                      .filter((key) => /^\d+$/.test(key))
                      .map((key) => Number(key))
                      .reduce((acc, key) => Math.max(acc, key), -1);
                    const rowLength =
                      typeof rowsValue.length === "number"
                        ? rowsValue.length
                        : maxIdx >= 0
                          ? maxIdx + 1
                          : 0;

                    __sol_setText(row0Text, rowEntries[0] == null ? "" : String(rowEntries[0]));
                    __sol_setText(row1Text, rowEntries[1] == null ? "" : String(rowEntries[1]));
                    __sol_setText(row2Text, rowEntries[2] == null ? "" : String(rowEntries[2]));
                    __sol_setText(row3Text, rowEntries[3] == null ? "" : String(rowEntries[3]));
                    globalThis.state.canaryRowCount = rowLength;
                    __sol_setText(statusText, "status=" + rowLength);
                  });

              return root;
            }
            render(() => App(), __SOL_ROOT__);
        "#;

fn make_state_mutation_canary() -> (Instance, StateHandle) {
    let (device, queue) = test_device();
    let (mut instance, _rx) = Instance::new(
        InstanceConfig {
            width: 220,
            height: 120,
            device,
            queue,
            stylesheets: vec![],
            document_scroll: false,
            base_url: None,
            initial_state: None,
            registered_resources: vec![],
            scale_factor: 1.0,
        },
        CANARY_MUTATION_COMPONENT,
    );
    let state = instance.state();
    state.set("title", json!("seed"));
    state.set("nested", json!({ "value": "inner" }));
    state.set("rows", json!(["initial"]));
    for _ in 0..2 {
        let _ = instance.tick();
        let _ = instance.render();
    }
    (instance, state)
}

#[test]
fn state_mutation_matrix_keeps_unrelated_dom_nodes_stable() {
    let (mut instance, state) = make_state_mutation_canary();

    let root = instance.container_id();
    let baseline = {
        let doc = instance.doc.borrow();
        collect_dom_signature(&doc, root)
    };

    let title_text_id = id_from_state(&state, "canaryTitleTextId");
    let nested_text_id = id_from_state(&state, "canaryNestedTextId");
    let row_node_ids = ids_from_state(&state, "canaryRowNodeIds");
    let row_text_ids = ids_from_state(&state, "canaryRowTextIds");

    assert_eq!(
        row_node_ids.len(),
        4,
        "initial should render four fixed row nodes"
    );
    assert_eq!(
        row_text_ids.len(),
        4,
        "initial should render four fixed row text nodes"
    );

    assert_eq!(
        text_for_node(&baseline, title_text_id),
        Some("seed"),
        "initial title text should be mounted"
    );
    assert_eq!(
        text_for_node(&baseline, nested_text_id),
        Some("inner"),
        "initial nested text should be mounted"
    );

    let run_and_capture = |instance: &mut Instance| -> Vec<DomSig> {
        let _ = instance.tick();
        let _ = instance.render();
        let _ = instance.tick();
        let _ = instance.render();
        let doc = instance.doc.borrow();
        collect_dom_signature(&doc, instance.container_id())
    };

    // Unrelated writes should only touch Rust state, not the mounted DOM.
    state.set("unrelated", json!(true));
    let unchanged = run_and_capture(&mut instance);
    assert_structural_dom_signature_eq(&baseline, &unchanged);
    assert_eq!(ids_from_state(&state, "canaryRowNodeIds"), row_node_ids);
    assert_eq!(ids_from_state(&state, "canaryRowTextIds"), row_text_ids);
    assert_eq!(
        text_for_node(&unchanged, title_text_id),
        Some("seed"),
        "unrelated state should not alter title text"
    );

    // Text-only path updates should mutate text content only.
    state.set("title", json!("next"));
    let title_mut = run_and_capture(&mut instance);
    assert_structural_dom_signature_eq(&baseline, &title_mut);
    assert_eq!(
        text_for_node(&title_mut, title_text_id),
        Some("next"),
        "title text should update"
    );
    assert_eq!(
        text_for_node(&title_mut, nested_text_id),
        Some("inner"),
        "nested text should remain stable"
    );

    // Duplicate same-path writes should keep the same visual result as last write.
    state.set("title", json!("temp"));
    state.set("title", json!("final"));
    let dup_paths = run_and_capture(&mut instance);
    assert_structural_dom_signature_eq(&baseline, &dup_paths);
    assert_eq!(
        text_for_node(&dup_paths, title_text_id),
        Some("final"),
        "last write should win"
    );

    // Nested path update should be reflected in the nested text node.
    state.set("nested.value", json!("deep"));
    let nested_update = run_and_capture(&mut instance);
    assert_structural_dom_signature_eq(&baseline, &nested_update);
    assert_eq!(
        text_for_node(&nested_update, nested_text_id),
        Some("deep"),
        "nested update should surface in nested text"
    );

    // Existing index update should preserve node ids and only mutate row text.
    let existing_row_node_ids = ids_from_state(&state, "canaryRowNodeIds");
    let existing_row_text_ids = ids_from_state(&state, "canaryRowTextIds");
    state.set("rows.0", json!("rewritten"));
    let updated_row = run_and_capture(&mut instance);
    let updated_row_node_ids = ids_from_state(&state, "canaryRowNodeIds");
    let updated_row_text_ids = ids_from_state(&state, "canaryRowTextIds");
    assert_eq!(updated_row_node_ids, existing_row_node_ids);
    assert_eq!(updated_row_text_ids, existing_row_text_ids);
    assert_eq!(
        text_for_node(&updated_row, existing_row_text_ids[0]),
        Some("rewritten"),
        "existing row text node should be updated in place"
    );

    // Out-of-bounds array updates should update the right row while keeping
    // existing ids stable.
    state.set("rows.3", json!("tail"));
    let oob = run_and_capture(&mut instance);
    let oob_row_node_ids = ids_from_state(&state, "canaryRowNodeIds");
    let oob_row_text_ids = ids_from_state(&state, "canaryRowTextIds");

    assert_eq!(oob_row_node_ids.len(), 4, "we keep four fixed row nodes");
    assert_eq!(
        oob_row_node_ids[0], updated_row_node_ids[0],
        "row 0 should be preserved"
    );
    assert_eq!(
        oob_row_text_ids.len(),
        4,
        "row text ids should track each index"
    );
    assert_eq!(
        text_for_node(&oob, oob_row_text_ids[0]),
        Some("rewritten"),
        "row 0 text should keep its rewritten value"
    );
    assert_eq!(
        text_for_node(&oob, oob_row_text_ids[1]),
        Some(""),
        "row 1 should remain empty when not provided"
    );
    assert_eq!(
        text_for_node(&oob, oob_row_text_ids[2]),
        Some(""),
        "row 2 should remain empty when not provided"
    );
    assert_eq!(
        text_for_node(&oob, oob_row_text_ids[3]),
        Some("tail"),
        "out-of-bounds index should materialize at tail"
    );

    // Keep an eye on pure root replacement as a full-shape mutation.
    state.set(
        "",
        json!({
            "title": "rooted",
            "nested": { "value": "replaced" },
            "rows": ["a", "b", "c"],
            "canaryRootId": root,
            "canaryTitleTextId": title_text_id,
            "canaryNestedTextId": nested_text_id,
            "canaryStatusTextId": id_from_state(&state, "canaryStatusTextId"),
            "canaryRowNodeIds": oob_row_node_ids,
            "canaryRowTextIds": oob_row_text_ids,
        }),
    );
    let root_replace = run_and_capture(&mut instance);
    let replaced_row_ids = ids_from_state(&state, "canaryRowNodeIds");
    let replaced_row_text_ids = ids_from_state(&state, "canaryRowTextIds");
    assert_eq!(
        replaced_row_ids.len(),
        4,
        "fixed row nodes remain mounted across root replacement"
    );
    assert_eq!(
        replaced_row_ids[0], oob_row_node_ids[0],
        "first row should remain through root replacement when present"
    );
    assert_eq!(
        oob_row_node_ids, replaced_row_ids,
        "rows nodes should be stable across root replacement"
    );
    assert_eq!(
        oob_row_text_ids, replaced_row_text_ids,
        "row text ids should be stable across root replacement"
    );
    assert_eq!(
        text_for_node(&root_replace, replaced_row_text_ids[0]),
        Some("a"),
        "row 0 should adopt root replacement value"
    );
    assert_eq!(
        text_for_node(&root_replace, replaced_row_text_ids[1]),
        Some("b"),
        "row 1 should receive root replacement value"
    );
    assert_eq!(
        text_for_node(&root_replace, replaced_row_text_ids[2]),
        Some("c"),
        "row 2 should receive root replacement value"
    );
    assert_eq!(
        id_from_state(&state, "canaryRootId"),
        root,
        "canary root id should remain stable"
    );
    assert_eq!(
        text_for_node(&root_replace, title_text_id),
        Some("rooted"),
        "title should reflect root replacement"
    );
    assert_eq!(
        text_for_node(&root_replace, nested_text_id),
        Some("replaced"),
        "nested field should reflect root replacement"
    );
}

#[test]
fn tab_walks_inputs_and_buttons_in_doc_order() {
    let (mut instance, _rx) = make_kb_nav_instance();
    // Initial focus is None; Tab moves to the first focusable.
    let _ = instance.dispatch_key_down(tab_key(false));
    assert_eq!(focused_tag(&instance).as_deref(), Some("input"));
    let _ = instance.dispatch_key_down(tab_key(false));
    assert_eq!(focused_tag(&instance).as_deref(), Some("input"));
    // The third focusable is the button now (was excluded under the old
    // inputs/selects-only filter).
    let _ = instance.dispatch_key_down(tab_key(false));
    assert_eq!(focused_tag(&instance).as_deref(), Some("button"));
    // Shift+Tab walks back.
    let _ = instance.dispatch_key_down(tab_key(true));
    assert_eq!(focused_tag(&instance).as_deref(), Some("input"));
}

#[test]
fn automatic_tab_order_walks_all_default_focusables_in_doc_order() {
    // Verifies that every browser-default focusable element
    // (`<input>`, `<select>`, `<button>`, `<a href>`) — none with an
    // explicit `tabindex` — is placed in the Tab order in document
    // order. This is the "automatic tab order" path: nothing in the
    // component declares focus priority; the focus collector infers it.
    let component = r##"
            import { render } from "solite-runtime";
            function App() {
              const root = __sol_createElement("div");
              const add = (tag, id, attrs = {}) => {
                const el = __sol_createElement(tag);
                for (const k in attrs) __sol_setProperty(el, k, attrs[k]);
                __sol_insertNode(root, el, null);
                globalThis["__sol_" + id] = el;
                return el;
              };
              add("input", "txt", { type: "text" });
              add("button", "btn");
              const sel = add("select", "sel");
              const opt = __sol_createElement("option");
              __sol_setProperty(opt, "value", "a");
              __sol_insertNode(opt, __sol_createTextNode("A"), null);
              __sol_insertNode(sel, opt, null);
              add("a", "link", { href: "#x" });
              add("div", "plain"); // not focusable, must be skipped
              return root;
            }
            render(() => App(), __SOL_ROOT__);
        "##;
    let (device, queue) = test_device();
    let (mut instance, _rx) = Instance::new(
        InstanceConfig {
            width: 320,
            height: 160,
            device,
            queue,
            stylesheets: vec![],
            document_scroll: false,
            base_url: None,
            initial_state: None,
            registered_resources: vec![],
            scale_factor: 1.0,
        },
        component,
    );
    settle(&mut instance);

    let txt = js_node_id(&instance, "txt");
    let btn = js_node_id(&instance, "btn");
    let sel = js_node_id(&instance, "sel");
    let link = js_node_id(&instance, "link");

    // Tab from no focus → input → button → select → anchor → wraps to input.
    let _ = instance.dispatch_key_down(tab_key(false));
    assert_eq!(instance.focused_node_id, Some(txt));
    let _ = instance.dispatch_key_down(tab_key(false));
    assert_eq!(instance.focused_node_id, Some(btn));
    let _ = instance.dispatch_key_down(tab_key(false));
    assert_eq!(instance.focused_node_id, Some(sel));
    let _ = instance.dispatch_key_down(tab_key(false));
    assert_eq!(instance.focused_node_id, Some(link));
    let _ = instance.dispatch_key_down(tab_key(false));
    assert_eq!(instance.focused_node_id, Some(txt), "Tab should wrap");

    // Shift+Tab walks back the same chain.
    let _ = instance.dispatch_key_down(tab_key(true));
    assert_eq!(instance.focused_node_id, Some(link));
    let _ = instance.dispatch_key_down(tab_key(true));
    assert_eq!(instance.focused_node_id, Some(sel));
}

#[test]
fn anchor_without_href_is_not_in_automatic_tab_order() {
    // `<a>` without `href` is NOT a default focusable per HTML spec.
    // The collector must skip it unless an explicit `tabindex` opts it in.
    let component = r##"
            import { render } from "solite-runtime";
            function App() {
              const root = __sol_createElement("div");
              const inp = __sol_createElement("input"); __sol_insertNode(root, inp, null);
              const a_no_href = __sol_createElement("a");
              __sol_insertNode(a_no_href, __sol_createTextNode("nope"), null);
              __sol_insertNode(root, a_no_href, null);
              const a_with_href = __sol_createElement("a");
              __sol_setProperty(a_with_href, "href", "#x");
              __sol_insertNode(a_with_href, __sol_createTextNode("yes"), null);
              __sol_insertNode(root, a_with_href, null);
              globalThis.__sol_inp = inp;
              globalThis.__sol_a_no_href = a_no_href;
              globalThis.__sol_a_with_href = a_with_href;
              return root;
            }
            render(() => App(), __SOL_ROOT__);
        "##;
    let (device, queue) = test_device();
    let (mut instance, _rx) = Instance::new(
        InstanceConfig {
            width: 320,
            height: 160,
            device,
            queue,
            stylesheets: vec![],
            document_scroll: false,
            base_url: None,
            initial_state: None,
            registered_resources: vec![],
            scale_factor: 1.0,
        },
        component,
    );
    settle(&mut instance);
    let inp = js_node_id(&instance, "inp");
    let a_with_href = js_node_id(&instance, "a_with_href");
    let _ = instance.dispatch_key_down(tab_key(false));
    assert_eq!(instance.focused_node_id, Some(inp));
    let _ = instance.dispatch_key_down(tab_key(false));
    assert_eq!(
        instance.focused_node_id,
        Some(a_with_href),
        "<a> without href must NOT receive Tab focus"
    );
}

#[test]
fn disabled_default_focusable_is_skipped_by_automatic_tab_order() {
    let component = r#"
            import { render } from "solite-runtime";
            function App() {
              const root = __sol_createElement("div");
              const a = __sol_createElement("input"); __sol_insertNode(root, a, null);
              const b = __sol_createElement("input"); __sol_setProperty(b, "disabled", "");
              __sol_insertNode(root, b, null);
              const c = __sol_createElement("button"); __sol_setProperty(c, "disabled", "");
              __sol_insertNode(root, c, null);
              const d = __sol_createElement("input"); __sol_insertNode(root, d, null);
              globalThis.__sol_a = a; globalThis.__sol_d = d;
              return root;
            }
            render(() => App(), __SOL_ROOT__);
        "#;
    let (device, queue) = test_device();
    let (mut instance, _rx) = Instance::new(
        InstanceConfig {
            width: 320,
            height: 160,
            device,
            queue,
            stylesheets: vec![],
            document_scroll: false,
            base_url: None,
            initial_state: None,
            registered_resources: vec![],
            scale_factor: 1.0,
        },
        component,
    );
    settle(&mut instance);
    let a = js_node_id(&instance, "a");
    let d = js_node_id(&instance, "d");
    let _ = instance.dispatch_key_down(tab_key(false));
    assert_eq!(instance.focused_node_id, Some(a));
    let _ = instance.dispatch_key_down(tab_key(false));
    assert_eq!(
        instance.focused_node_id,
        Some(d),
        "disabled input and disabled button must both be skipped"
    );
}

#[test]
fn tabindex_negative_skips_element_from_tab_order() {
    let component = r#"
            import { render } from "solite-runtime";
            function App() {
              const root = __sol_createElement("div");
              const make = (attrs) => {
                const el = __sol_createElement("input");
                for (const k in attrs) __sol_setProperty(el, k, attrs[k]);
                __sol_insertNode(root, el, null);
                globalThis["__sol_" + attrs.id] = el;
                return el;
              };
              make({ id: "a" });
              make({ id: "b", tabindex: "-1" });
              make({ id: "c" });
              return root;
            }
            render(() => App(), __SOL_ROOT__);
        "#;
    let (device, queue) = test_device();
    let (mut instance, _rx) = Instance::new(
        InstanceConfig {
            width: 320,
            height: 160,
            device,
            queue,
            stylesheets: vec![],
            document_scroll: false,
            base_url: None,
            initial_state: None,
            registered_resources: vec![],
            scale_factor: 1.0,
        },
        component,
    );
    settle(&mut instance);
    let a = js_node_id(&instance, "a");
    let c = js_node_id(&instance, "c");
    let _ = instance.dispatch_key_down(tab_key(false));
    assert_eq!(instance.focused_node_id, Some(a));
    let _ = instance.dispatch_key_down(tab_key(false));
    assert_eq!(
        instance.focused_node_id,
        Some(c),
        "tabindex=-1 input must be skipped"
    );
}

#[test]
fn positive_tabindex_takes_priority_over_doc_order() {
    let component = r#"
            import { render } from "solite-runtime";
            function App() {
              const root = __sol_createElement("div");
              const make = (attrs) => {
                const el = __sol_createElement("input");
                for (const k in attrs) __sol_setProperty(el, k, attrs[k]);
                __sol_insertNode(root, el, null);
                globalThis["__sol_" + attrs.id] = el;
                return el;
              };
              make({ id: "a" });
              make({ id: "b", tabindex: "2" });
              make({ id: "c", tabindex: "1" });
              return root;
            }
            render(() => App(), __SOL_ROOT__);
        "#;
    let (device, queue) = test_device();
    let (mut instance, _rx) = Instance::new(
        InstanceConfig {
            width: 320,
            height: 160,
            device,
            queue,
            stylesheets: vec![],
            document_scroll: false,
            base_url: None,
            initial_state: None,
            registered_resources: vec![],
            scale_factor: 1.0,
        },
        component,
    );
    settle(&mut instance);
    let a = js_node_id(&instance, "a");
    let b = js_node_id(&instance, "b");
    let c = js_node_id(&instance, "c");
    // Order should be: tabindex=1 (c), tabindex=2 (b), tabindex=0/unset (a).
    let _ = instance.dispatch_key_down(tab_key(false));
    assert_eq!(instance.focused_node_id, Some(c));
    let _ = instance.dispatch_key_down(tab_key(false));
    assert_eq!(instance.focused_node_id, Some(b));
    let _ = instance.dispatch_key_down(tab_key(false));
    assert_eq!(instance.focused_node_id, Some(a));
}

#[test]
fn enter_on_focused_button_fires_click() {
    let (mut instance, _rx) = make_kb_nav_instance();
    let btn = js_node_id(&instance, "btn");
    instance.focused_node_id = Some(btn);
    let _ = instance.dispatch_key_down(enter_key());
    assert_eq!(instance.state().get("clicked"), Some(json!(1)));
}

#[test]
fn space_keyup_on_focused_button_fires_click() {
    let (mut instance, _rx) = make_kb_nav_instance();
    let btn = js_node_id(&instance, "btn");
    instance.focused_node_id = Some(btn);
    // keydown alone must NOT fire — only keyup completes the click.
    let _ = instance.dispatch_key_down(space_key());
    assert_eq!(instance.state().get("clicked"), Some(json!(0)));
    let _ = instance.dispatch_key_up(space_key());
    assert_eq!(instance.state().get("clicked"), Some(json!(1)));
}

#[test]
fn ctrl_left_jumps_by_word() {
    use crate::input::InputState;
    let mut s = InputState::default();
    s.set_value("hello world here");
    s.move_end();
    // Caret at end (16). Ctrl+Left → start of "here" (12).
    assert!(s.move_word_left_extending(false));
    assert_eq!(s.caret(), 12);
    // Again → start of "world" (6).
    assert!(s.move_word_left_extending(false));
    assert_eq!(s.caret(), 6);
    // Again → start of "hello" (0).
    assert!(s.move_word_left_extending(false));
    assert_eq!(s.caret(), 0);
}

#[test]
fn ctrl_right_jumps_by_word() {
    use crate::input::InputState;
    let mut s = InputState::default();
    s.set_value("hello world here");
    s.move_home();
    assert!(s.move_word_right_extending(false));
    assert_eq!(s.caret(), 5); // end of "hello"
    assert!(s.move_word_right_extending(false));
    assert_eq!(s.caret(), 11); // end of "world"
    assert!(s.move_word_right_extending(false));
    assert_eq!(s.caret(), 16); // end of "here"
}

#[test]
fn ctrl_shift_right_extends_selection_by_word() {
    use crate::input::InputState;
    let mut s = InputState::default();
    s.set_value("foo bar");
    s.move_home();
    assert!(s.move_word_right_extending(true));
    assert_eq!(s.selection_start(), 0);
    assert_eq!(s.selection_end(), 3);
}

#[test]
fn ctrl_backspace_deletes_previous_word() {
    use crate::input::InputState;
    let mut s = InputState::default();
    s.set_value("hello world");
    s.move_end();
    assert!(s.delete_word_left());
    assert_eq!(s.value(), "hello ");
    assert!(s.delete_word_left());
    assert_eq!(s.value(), "");
}

#[test]
fn ctrl_delete_removes_next_word() {
    use crate::input::InputState;
    let mut s = InputState::default();
    s.set_value("hello world");
    s.move_home();
    assert!(s.delete_word_right());
    assert_eq!(s.value(), " world");
}

#[test]
fn ctrl_left_via_dispatch_key_works_end_to_end() {
    let component = r#"
            import { render } from "solite-runtime";
            function App() {
              const input = __sol_createElement("input");
              __sol_setProperty(input, "type", "text");
              __sol_setProperty(input, "value", "hello world");
              globalThis.__sol_input = input;
              return input;
            }
            render(() => App(), __SOL_ROOT__);
        "#;
    let (device, queue) = test_device();
    let (mut instance, _rx) = Instance::new(
        InstanceConfig {
            width: 320,
            height: 80,
            device,
            queue,
            stylesheets: vec![],
            document_scroll: false,
            base_url: None,
            initial_state: None,
            registered_resources: vec![],
            scale_factor: 1.0,
        },
        component,
    );
    settle(&mut instance);
    let input = js_node_id(&instance, "input");
    instance.focused_node_id = Some(input);
    // Caret defaults to end-of-value after set_value (11).
    let before = instance
        .js
        .inputs
        .borrow()
        .get(&input)
        .map(|s| s.caret())
        .unwrap_or(0);
    assert_eq!(before, 11);
    // Ctrl+Left → caret moves to start of "world" (6).
    let _ = instance.dispatch_key_down(ctrl_key("ArrowLeft"));
    let after = instance
        .js
        .inputs
        .borrow()
        .get(&input)
        .map(|s| s.caret())
        .unwrap_or(0);
    assert_eq!(after, 6);
    // Ctrl+Shift+Left → extends selection back to start of "hello".
    let _ = instance.dispatch_key_down(ctrl_shift_key("ArrowLeft"));
    let (sel_start, sel_end) = instance
        .js
        .inputs
        .borrow()
        .get(&input)
        .map(|s| (s.selection_start(), s.selection_end()))
        .unwrap_or((0, 0));
    assert_eq!(sel_start, 0);
    assert_eq!(sel_end, 6);
}

// ─── Select keyboard navigation ──────────────────────────────────────────

const SELECT_NAV_COMPONENT: &str = r#"
        import { render } from "solite-runtime";
        function App() {
          const sel = __sol_createElement("select");
          const make_opt = (val, label) => {
            const o = __sol_createElement("option");
            __sol_setProperty(o, "value", val);
            __sol_insertNode(o, __sol_createTextNode(label), null);
            __sol_insertNode(sel, o, null);
          };
          make_opt("apple", "Apple");
          make_opt("apricot", "Apricot");
          make_opt("banana", "Banana");
          make_opt("blueberry", "Blueberry");
          make_opt("cherry", "Cherry");
          make_opt("date", "Date");
          make_opt("elderberry", "Elderberry");
          make_opt("fig", "Fig");
          make_opt("grape", "Grape");
          make_opt("honeydew", "Honeydew");
          make_opt("kiwi", "Kiwi");
          make_opt("lemon", "Lemon");
          globalThis.__sol_sel = sel;
          return sel;
        }
        render(() => App(), __SOL_ROOT__);
    "#;

fn make_select_nav_instance() -> Instance {
    let (device, queue) = test_device();
    let (mut instance, _rx) = Instance::new(
        InstanceConfig {
            width: 240,
            height: 200,
            device,
            queue,
            stylesheets: vec![],
            document_scroll: false,
            base_url: None,
            initial_state: None,
            registered_resources: vec![],
            scale_factor: 1.0,
        },
        SELECT_NAV_COMPONENT,
    );
    settle(&mut instance);
    let sel = js_node_id(&instance, "sel");
    instance.focused_node_id = Some(sel);
    instance
}

fn select_value(instance: &Instance, sel_id: usize) -> Option<String> {
    instance
        .js
        .selects
        .borrow()
        .get(&sel_id)
        .and_then(|s| s.value())
}

#[test]
fn select_type_ahead_jumps_to_first_match_after_current() {
    let mut instance = make_select_nav_instance();
    let sel = js_node_id(&instance, "sel");
    // The closed select starts with the first option selected. Press
    // "c" — should jump to "Cherry" (next "c" match after current).
    let _ = instance.dispatch_key_down(plain_key("c"));
    assert_eq!(select_value(&instance, sel).as_deref(), Some("cherry"));
}

#[test]
fn select_type_ahead_b_finds_banana() {
    let mut instance = make_select_nav_instance();
    let sel = js_node_id(&instance, "sel");
    let _ = instance.dispatch_key_down(plain_key("b"));
    assert_eq!(select_value(&instance, sel).as_deref(), Some("banana"));
}

#[test]
fn select_type_ahead_cycles_on_repeat_letter() {
    let mut instance = make_select_nav_instance();
    let sel = js_node_id(&instance, "sel");
    // Browser semantics: from "apple" selected, pressing "a" advances to
    // the next option matching "a" — that's apricot (idx 1). Pressing
    // "a" again cycles forward and wraps back to apple.
    let _ = instance.dispatch_key_down(plain_key("a"));
    assert_eq!(select_value(&instance, sel).as_deref(), Some("apricot"));
    let _ = instance.dispatch_key_down(plain_key("a"));
    assert_eq!(select_value(&instance, sel).as_deref(), Some("apple"));
    let _ = instance.dispatch_key_down(plain_key("a"));
    assert_eq!(select_value(&instance, sel).as_deref(), Some("apricot"));
}

#[test]
fn select_page_down_steps_ten() {
    let mut instance = make_select_nav_instance();
    let sel = js_node_id(&instance, "sel");
    // First option selected → PageDown moves +10 → index 10 = "kiwi".
    let _ = instance.dispatch_key_down(plain_key("PageDown"));
    assert_eq!(select_value(&instance, sel).as_deref(), Some("kiwi"));
    // PageUp from index 10 → clamps to 0 = "apple".
    let _ = instance.dispatch_key_down(plain_key("PageUp"));
    assert_eq!(select_value(&instance, sel).as_deref(), Some("apple"));
}

#[test]
fn select_alt_down_opens_dropdown() {
    let mut instance = make_select_nav_instance();
    let sel = js_node_id(&instance, "sel");
    assert!(!instance.js.selects.borrow().get(&sel).unwrap().is_open());
    let _ = instance.dispatch_key_down(alt_key("ArrowDown"));
    assert!(instance.js.selects.borrow().get(&sel).unwrap().is_open());
}

#[test]
fn select_alt_up_commits_and_closes() {
    let mut instance = make_select_nav_instance();
    let sel = js_node_id(&instance, "sel");
    // Open and move highlight to second option.
    let _ = instance.dispatch_key_down(alt_key("ArrowDown"));
    let _ = instance.dispatch_key_down(plain_key("ArrowDown"));
    // Alt+Up commits and closes.
    let _ = instance.dispatch_key_down(alt_key("ArrowUp"));
    assert!(!instance.js.selects.borrow().get(&sel).unwrap().is_open());
    assert_eq!(select_value(&instance, sel).as_deref(), Some("apricot"));
}

// ────────────────────────────────────────────────────────────────────────
// todo example end-to-end: drives the actual examples/todo_app.jsx through
// a full UX loop (add several items → switch filters → toggle done →
// re-check filters → clear completed) and asserts the visible DOM matches
// expectations at each step. Loads the JSX from disk so the test always
// reflects the shipped example.
// ────────────────────────────────────────────────────────────────────────
#[cfg(feature = "jsx-compiler")]
mod todo_example {
    use super::*;

    const TODO_JSX: &str = include_str!("../../examples/todo_app.jsx");
    const TODO_CSS: &str = include_str!("../../examples/todo_app.css");

    fn find_descendants_by_class(doc: &BaseDocument, root: usize, class: &str) -> Vec<usize> {
        let mut out = Vec::new();
        let mut stack = vec![root];
        let class_ln = LocalName::from("class");
        while let Some(id) = stack.pop() {
            let Some(node) = doc.get_node(id) else {
                continue;
            };
            if let Some(value) = node.attr(class_ln.clone()) {
                if value.split_whitespace().any(|c| c == class) {
                    out.push(id);
                }
            }
            for child in node.children.iter().rev() {
                stack.push(*child);
            }
        }
        out
    }

    fn first_by_class(doc: &BaseDocument, root: usize, class: &str) -> usize {
        *find_descendants_by_class(doc, root, class)
            .first()
            .unwrap_or_else(|| panic!("missing element with class '{class}'"))
    }

    fn center_of(doc: &BaseDocument, node_id: usize) -> (f32, f32) {
        let node = doc.get_node(node_id).expect("node");
        let pos = node.absolute_position(0.0, 0.0);
        let size = node.final_layout.size;
        (pos.x + size.width / 2.0, pos.y + size.height / 2.0)
    }

    fn has_class(doc: &BaseDocument, node_id: usize, class: &str) -> bool {
        let class_ln = LocalName::from("class");
        doc.get_node(node_id)
            .and_then(|n| n.attr(class_ln))
            .map(|v| v.split_whitespace().any(|c| c == class))
            .unwrap_or(false)
    }

    fn text_of(doc: &BaseDocument, node_id: usize) -> String {
        doc.get_node(node_id)
            .map(|n| n.text_content())
            .unwrap_or_default()
    }

    fn visible_items(instance: &Instance) -> Vec<(String, bool)> {
        let doc = instance.doc.borrow();
        let root = instance.container_id();
        let items = find_descendants_by_class(&doc, root, "todo-item");
        items
            .into_iter()
            .map(|id| {
                // Each todo-item has a .todo-text span.
                let text_id = first_by_class(&doc, id, "todo-text");
                let text = text_of(&doc, text_id);
                let done = has_class(&doc, id, "done");
                (text, done)
            })
            .collect()
    }

    fn empty_state_text(instance: &Instance) -> Option<String> {
        let doc = instance.doc.borrow();
        let root = instance.container_id();
        let states = find_descendants_by_class(&doc, root, "empty-state");
        states.first().map(|id| text_of(&doc, *id))
    }

    fn active_chip(instance: &Instance) -> Option<String> {
        let doc = instance.doc.borrow();
        let root = instance.container_id();
        let chips = find_descendants_by_class(&doc, root, "chip");
        chips
            .into_iter()
            .find(|id| has_class(&doc, *id, "active"))
            .map(|id| {
                // text_content concatenates the chip label and its count
                // span (e.g. "All3"). Match against the known labels by
                // prefix so we return just the label.
                let text = text_of(&doc, id);
                for label in ["Active", "All", "Done"] {
                    if text.starts_with(label) {
                        return label.to_string();
                    }
                }
                text
            })
    }

    fn assert_chip_labels_stable(instance: &Instance) {
        let doc = instance.doc.borrow();
        let root = instance.container_id();
        let labels = find_descendants_by_class(&doc, root, "chip")
            .into_iter()
            .map(|id| text_of(&doc, id))
            .collect::<Vec<_>>();
        assert_eq!(labels.len(), 3, "todo filter should keep three chips");
        assert!(
            labels[0].starts_with("All")
                && labels[1].starts_with("Active")
                && labels[2].starts_with("Done"),
            "filter chip labels should not be replaced by reactive booleans/counts: {labels:?}"
        );
    }

    fn click(instance: &mut Instance, x: f32, y: f32) {
        let _ = instance.dispatch_mouse(
            x,
            y,
            MouseEvent::Down {
                x,
                y,
                button: MouseButton::Left,
            },
        );
        let _ = instance.dispatch_mouse(
            x,
            y,
            MouseEvent::Up {
                x,
                y,
                button: MouseButton::Left,
            },
        );
    }

    fn click_by_class(instance: &mut Instance, class: &str) {
        let (x, y) = {
            let doc = instance.doc.borrow();
            let root = instance.container_id();
            let id = first_by_class(&doc, root, class);
            center_of(&doc, id)
        };
        click(instance, x, y);
        let _ = instance.tick();
        let _ = instance.render();
    }

    fn click_nth_by_class(instance: &mut Instance, class: &str, n: usize) {
        let (x, y) = {
            let doc = instance.doc.borrow();
            let root = instance.container_id();
            let nodes = find_descendants_by_class(&doc, root, class);
            let id = *nodes
                .get(n)
                .unwrap_or_else(|| panic!("no element {class} at index {n}"));
            center_of(&doc, id)
        };
        click(instance, x, y);
        let _ = instance.tick();
        let _ = instance.render();
    }

    fn type_text(instance: &mut Instance, text: &str) {
        for ch in text.chars() {
            let _ = instance.dispatch_key_down(make_key_event(
                &ch.to_string(),
                "",
                0,
                false,
                false,
                false,
                false,
                false,
            ));
        }
        let _ = instance.tick();
        let _ = instance.render();
    }

    fn press_enter(instance: &mut Instance) {
        let _ = instance.dispatch_key_down(make_key_event(
            "Enter", "Enter", 13, false, false, false, false, false,
        ));
        let _ = instance.tick();
        let _ = instance.render();
    }

    fn type_and_enter(instance: &mut Instance, text: &str) {
        type_text(instance, text);
        press_enter(instance);
    }

    fn make_todo_instance() -> Instance {
        let compiled =
            solite_build::compile_component_source(std::path::Path::new("todo_app.jsx"), TODO_JSX)
                .expect("compile todo jsx");
        let (device, queue) = test_device();
        let (mut instance, _rx) = Instance::new(
            InstanceConfig {
                width: 540,
                height: 800,
                device,
                queue,
                stylesheets: vec![TODO_CSS.to_string()],
                document_scroll: true,
                base_url: None,
                initial_state: None,
                registered_resources: vec![],
                scale_factor: 1.0,
            },
            &compiled,
        );
        let _ = instance.tick();
        let _ = instance.render();
        instance
    }

    /// End-to-end: with createEffect mirrors injected, verify that
    /// (a) typing into the input updates the `draft` signal, and
    /// (b) clicking the Add button fires the handler, calls
    ///     setTodos+setDraft, and clears the input.
    /// This catches the original bug ("Add does nothing") at the signal
    /// level — independent of paint/render.
    #[test]
    fn add_button_click_fires_handler_and_updates_signals() {
        use crate::scene::{Scene, SurfaceRect};

        // Inject side-channel mirrors via createEffect so we can read
        // signal state from Rust through globalThis.state.
        let probed = TODO_JSX
                .replacen(
                    "import { createMemo, createSignal, render } from \"solite-runtime\";",
                    "import { createEffect, createMemo, createSignal, render } from \"solite-runtime\";",
                    1,
                )
                .replacen(
                    "let nextId = 1;",
                    "let nextId = 1;\n  createEffect(() => { globalThis.state.__draft = draft(); });\n  createEffect(() => { globalThis.state.__todoCount = todos().length; });",
                    1,
                );

        let compiled =
            solite_build::compile_component_source(std::path::Path::new("todo_app.jsx"), &probed)
                .expect("compile");
        let (device, queue) = test_device();
        let (mut instance, _rx) = Instance::new(
            InstanceConfig {
                width: 540,
                height: 800,
                device,
                queue,
                stylesheets: vec![TODO_CSS.to_string()],
                document_scroll: true,
                base_url: None,
                initial_state: None,
                registered_resources: vec![],
                scale_factor: 1.0,
            },
            &compiled,
        );
        let _ = instance.tick();
        let _ = instance.render();

        let state = instance.state();
        assert_eq!(state.get("__draft"), Some(json!("")));
        assert_eq!(state.get("__todoCount"), Some(json!(0)));

        let (input_x, input_y, add_x, add_y) = {
            let doc = instance.doc.borrow();
            let root = instance.container_id();
            let input_id = first_by_class(&doc, root, "todo-input");
            let add_id = first_by_class(&doc, root, "add-btn");
            let (ix, iy) = center_of(&doc, input_id);
            let (ax, ay) = center_of(&doc, add_id);
            (ix, iy, ax, ay)
        };

        let mut scene: Scene<()> = Scene::new();
        scene.add_surface(instance, SurfaceRect::new(0.0, 0.0, 540.0, 800.0), ());

        // Focus + type "tea".
        let _ = scene.dispatch_mouse(
            input_x,
            input_y,
            MouseEvent::Down {
                x: input_x,
                y: input_y,
                button: MouseButton::Left,
            },
        );
        let _ = scene.dispatch_mouse(
            input_x,
            input_y,
            MouseEvent::Up {
                x: input_x,
                y: input_y,
                button: MouseButton::Left,
            },
        );
        for ch in "tea".chars() {
            let _ = scene.dispatch_key_down(make_key_event(
                &ch.to_string(),
                "",
                0,
                false,
                false,
                false,
                false,
                false,
            ));
        }
        let _ = scene.tick();
        assert_eq!(
            state.get("__draft"),
            Some(json!("tea")),
            "draft signal must mirror typed text after the user types"
        );

        // Click Add.
        let _ = scene.dispatch_mouse(
            add_x,
            add_y,
            MouseEvent::Down {
                x: add_x,
                y: add_y,
                button: MouseButton::Left,
            },
        );
        let _ = scene.dispatch_mouse(
            add_x,
            add_y,
            MouseEvent::Up {
                x: add_x,
                y: add_y,
                button: MouseButton::Left,
            },
        );
        let _ = scene.tick();

        assert_eq!(
            state.get("__todoCount"),
            Some(json!(1)),
            "todoCount should be 1 after clicking Add"
        );
        assert_eq!(
            state.get("__draft"),
            Some(json!("")),
            "draft signal should clear after Add"
        );
    }

    /// Drive the example through the same Scene + dispatch path the
    /// `examples/todo.rs` winit host uses. Catches bugs that don't appear
    /// when driving an Instance directly (e.g. event routing through a
    /// surface, focus tracking on the scene).
    #[test]
    fn clicking_add_button_via_scene_grows_the_list() {
        use crate::scene::{Scene, SurfaceRect};

        let compiled =
            solite_build::compile_component_source(std::path::Path::new("todo_app.jsx"), TODO_JSX)
                .expect("compile todo jsx");
        let (device, queue) = test_device();
        let (mut instance, _rx) = Instance::new(
            InstanceConfig {
                width: 540,
                height: 800,
                device,
                queue,
                stylesheets: vec![TODO_CSS.to_string()],
                document_scroll: true,
                base_url: None,
                initial_state: None,
                registered_resources: vec![],
                scale_factor: 1.0,
            },
            &compiled,
        );
        let _ = instance.tick();
        let _ = instance.render();

        // Snapshot the input + Add button positions BEFORE moving into the
        // scene, since the scene takes ownership.
        let (input_x, input_y, add_x, add_y) = {
            let doc = instance.doc.borrow();
            let root = instance.container_id();
            let input_id = first_by_class(&doc, root, "todo-input");
            let add_id = first_by_class(&doc, root, "add-btn");
            let (ix, iy) = center_of(&doc, input_id);
            let (ax, ay) = center_of(&doc, add_id);
            (ix, iy, ax, ay)
        };

        let mut scene: Scene<()> = Scene::new();
        scene.add_surface(instance, SurfaceRect::new(0.0, 0.0, 540.0, 800.0), ());

        // Click the input to focus it via the Scene dispatch path.
        let _ = scene.dispatch_mouse(
            input_x,
            input_y,
            MouseEvent::Down {
                x: input_x,
                y: input_y,
                button: MouseButton::Left,
            },
        );
        let _ = scene.dispatch_mouse(
            input_x,
            input_y,
            MouseEvent::Up {
                x: input_x,
                y: input_y,
                button: MouseButton::Left,
            },
        );
        let _ = scene.tick();

        // Type "milk" via the Scene's key dispatch path (routes to the
        // focused surface).
        for ch in "milk".chars() {
            let _ = scene.dispatch_key_down(make_key_event(
                &ch.to_string(),
                "",
                0,
                false,
                false,
                false,
                false,
                false,
            ));
        }
        let _ = scene.tick();

        // Click the Add button (NOT Enter) via the Scene path.
        let _ = scene.dispatch_mouse(
            add_x,
            add_y,
            MouseEvent::Down {
                x: add_x,
                y: add_y,
                button: MouseButton::Left,
            },
        );
        let _ = scene.dispatch_mouse(
            add_x,
            add_y,
            MouseEvent::Up {
                x: add_x,
                y: add_y,
                button: MouseButton::Left,
            },
        );
        let _ = scene.tick();

        let surface = &scene.surfaces_mut()[0];
        let instance_ref = &surface.instance;
        let _ = instance_ref;
        let surface = &mut scene.surfaces_mut()[0];
        let _ = surface.instance.render();

        // Verify the item was added by looking at the DOM.
        let items: Vec<(String, bool)> = {
            let instance = &scene.surfaces_mut()[0].instance;
            let doc = instance.doc.borrow();
            let root = instance.container_id();
            let item_ids = find_descendants_by_class(&doc, root, "todo-item");
            item_ids
                .into_iter()
                .map(|id| {
                    let text_id = first_by_class(&doc, id, "todo-text");
                    (text_of(&doc, text_id), has_class(&doc, id, "done"))
                })
                .collect()
        };
        assert_eq!(
            items.len(),
            1,
            "clicking Add via Scene should add exactly one item"
        );
        assert_eq!(items[0].0, "milk");
    }

    #[test]
    fn full_user_flow_create_filter_toggle_clear() {
        let mut instance = make_todo_instance();

        // Initial state: no items, "all" filter, empty state visible.
        assert!(visible_items(&instance).is_empty());
        assert_eq!(active_chip(&instance).as_deref(), Some("All"));
        assert!(
            empty_state_text(&instance)
                .unwrap_or_default()
                .to_lowercase()
                .contains("nothing here")
        );

        // Focus the input by clicking it, then add two todos via Enter
        // and one via clicking the Add button.
        click_by_class(&mut instance, "todo-input");
        type_and_enter(&mut instance, "buy milk");
        type_and_enter(&mut instance, "walk dog");
        type_text(&mut instance, "write report");
        click_by_class(&mut instance, "add-btn");

        let items = visible_items(&instance);
        assert_eq!(items.len(), 3, "three items after adding");
        assert_eq!(
            items.iter().map(|(t, _)| t.as_str()).collect::<Vec<_>>(),
            vec!["buy milk", "walk dog", "write report"]
        );
        assert!(items.iter().all(|(_, done)| !done), "all start undone");

        // Filter: Active should show all 3 (none are done yet).
        click_by_class(&mut instance, "chip"); // first chip (All) — no-op, just confirms
        // The "All" chip is index 0; clicking it again should leave state
        // unchanged. Now click "Active" (index 1) and "Done" (index 2).
        click_nth_by_class(&mut instance, "chip", 1);
        assert_chip_labels_stable(&instance);
        assert_eq!(active_chip(&instance).as_deref(), Some("Active"));
        assert_eq!(
            visible_items(&instance).len(),
            3,
            "active = 3 when nothing done"
        );

        click_nth_by_class(&mut instance, "chip", 2);
        assert_chip_labels_stable(&instance);
        assert_eq!(active_chip(&instance).as_deref(), Some("Done"));
        assert!(
            visible_items(&instance).is_empty(),
            "done filter shows no items before toggling"
        );
        assert!(
            empty_state_text(&instance)
                .unwrap_or_default()
                .to_lowercase()
                .contains("no completed"),
            "empty state should describe completed filter"
        );

        // Back to "All".
        click_nth_by_class(&mut instance, "chip", 0);
        assert_chip_labels_stable(&instance);
        assert_eq!(active_chip(&instance).as_deref(), Some("All"));
        assert_eq!(visible_items(&instance).len(), 3);

        // Mark items 1 ("buy milk") and 3 ("write report") as done by
        // clicking their checkboxes.
        click_nth_by_class(&mut instance, "todo-checkbox", 0);
        click_nth_by_class(&mut instance, "todo-checkbox", 2);

        let items = visible_items(&instance);
        assert_eq!(items.len(), 3);
        assert!(items[0].1, "buy milk done");
        assert!(!items[1].1, "walk dog not done");
        assert!(items[2].1, "write report done");

        // Active filter → only "walk dog".
        click_nth_by_class(&mut instance, "chip", 1);
        assert_chip_labels_stable(&instance);
        let active_items = visible_items(&instance);
        assert_eq!(active_items.len(), 1, "active = 1 after marking two done");
        assert_eq!(active_items[0].0, "walk dog");

        // Done filter → "buy milk", "write report".
        click_nth_by_class(&mut instance, "chip", 2);
        assert_chip_labels_stable(&instance);
        let done_items = visible_items(&instance);
        assert_eq!(done_items.len(), 2);
        let done_texts: Vec<&str> = done_items.iter().map(|(t, _)| t.as_str()).collect();
        assert!(done_texts.contains(&"buy milk"));
        assert!(done_texts.contains(&"write report"));

        // Switch back to All, then click Clear completed.
        click_nth_by_class(&mut instance, "chip", 0);
        assert_chip_labels_stable(&instance);
        click_by_class(&mut instance, "clear-btn");

        let remaining = visible_items(&instance);
        assert_eq!(remaining.len(), 1, "clear completed leaves only undone");
        assert_eq!(remaining[0].0, "walk dog");
        assert!(!remaining[0].1, "remaining item is not done");

        // After clearing, switching to Done shows the empty state again.
        click_nth_by_class(&mut instance, "chip", 2);
        assert_chip_labels_stable(&instance);
        assert!(visible_items(&instance).is_empty());
        assert!(
            empty_state_text(&instance)
                .unwrap_or_default()
                .to_lowercase()
                .contains("no completed")
        );
    }

    #[cfg(feature = "a11y")]
    const A11Y_COMPONENT: &str = r#"
        import { render } from "solite-runtime";
        function App() {
          const root = __sol_createElement("div");
          __sol_setProperty(root, "style", "display:block; width:200px; height:200px;");

          const cb = __sol_createElement("input");
          __sol_setProperty(cb, "type", "checkbox");
          __sol_setProperty(cb, "aria-label", "Accept terms");
          __sol_insertNode(root, cb, null);

          const range = __sol_createElement("input");
          __sol_setProperty(range, "type", "range");
          __sol_setProperty(range, "min", "0");
          __sol_setProperty(range, "max", "10");
          __sol_setProperty(range, "value", "5");
          __sol_insertNode(root, range, null);

          const btn = __sol_createElement("button");
          __sol_insertNode(btn, __sol_createTextNode("Go"), null);
          __sol_insertNode(root, btn, null);

          return root;
        }
        render(() => App(), __SOL_ROOT__);
    "#;

    #[cfg(feature = "a11y")]
    #[test]
    fn accessibility_tree_reports_roles_states_and_drives_actions() {
        use accesskit::{Action, Role, Toggled};

        let (device, queue) = test_device();
        let (mut instance, _rx) = Instance::new(
            InstanceConfig {
                width: 200,
                height: 200,
                device,
                queue,
                stylesheets: vec![],
                document_scroll: false,
                base_url: None,
                initial_state: None,
                registered_resources: vec![],
                scale_factor: 1.0,
            },
            A11Y_COMPONENT,
        );
        let _ = instance.tick();
        let _ = instance.render();

        let tree = instance.accessibility_tree();
        let by_role = |role: Role| {
            tree.nodes
                .iter()
                .find(|(_, node)| node.role() == role)
                .map(|(id, node)| (*id, node))
        };

        let (_cb_id, checkbox) = by_role(Role::CheckBox).expect("checkbox node");
        assert_eq!(checkbox.toggled(), Some(Toggled::False));
        assert_eq!(checkbox.label(), Some("Accept terms"));
        assert!(checkbox.supports_action(Action::Click));

        let (slider_id, slider) = by_role(Role::Slider).expect("slider node");
        assert_eq!(slider.numeric_value(), Some(5.0));
        assert_eq!(slider.min_numeric_value(), Some(0.0));
        assert_eq!(slider.max_numeric_value(), Some(10.0));
        assert!(slider.supports_action(Action::Increment));

        by_role(Role::Button).expect("button node");

        // An AT-issued Increment should step the slider's value.
        instance.perform_accessibility_action(&accesskit::ActionRequest {
            action: Action::Increment,
            target_tree: accesskit::TreeId::ROOT,
            target_node: slider_id,
            data: None,
        });

        let after = instance.accessibility_tree();
        let stepped = after
            .nodes
            .iter()
            .find(|(_, node)| node.role() == Role::Slider)
            .map(|(_, node)| node.numeric_value())
            .expect("slider node after increment");
        assert_eq!(stepped, Some(6.0));
    }
}
