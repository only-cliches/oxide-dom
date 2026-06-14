// Solid universal renderer + Reactivity bridge for oxide-dom.
// Build with:
//
//   cd js && npx esbuild runtime.ts --bundle --format=esm --outfile=dist/runtime.js
//
// 1) DOM ops:
//    - Bridge node IDs are wrapped in opaque objects so Solid never treats them as text.
// 2) State ops:
//    - `state` is a JS proxy backed by a Solid store.
//    - Rust calls `__ox_apply_state_patch(path, value)` each `tick()`.
//    - JS writes to `state` call `__ox_state_set(path, value_json)` to mirror to Rust.

import { createEffect, createMemo, createSignal } from "solid-js";
import { createRenderer } from "solid-js/universal";
import { createStore } from "solid-js/store";

// Opaque wrapper for a Rust-side blitz-dom node ID.
export interface NodeHandle {
  readonly __oxId: number;
}

type PathPart = string | number;

const wrap = (id: number): NodeHandle => ({ __oxId: id });
const unwrap = (n: NodeHandle | number | null | undefined): number =>
  typeof n === "number" ? n : n?.__oxId ?? 0;
const unwrapOpt = (n: NodeHandle | number | null | undefined): number | null =>
  n == null ? null : typeof n === "number" ? n : n.__oxId;

const toPathParts = (path: string): PathPart[] =>
  path === ""
    ? []
    : path.split(".").map((part) => (/^\d+$/.test(part) ? Number(part) : part));

const normalizeStateValue = (value: unknown) =>
  value && typeof value === "object" ? value : {};

// Low-level bridge ops provided by Rust on the global scope.
declare const __ox_createElement: (tag: string) => number;
declare const __ox_createTextNode: (text: string) => number;
declare const __ox_setProperty: (
  node: number | NodeHandle,
  key: string,
  value: unknown,
) => void;
declare const __ox_insertNode: (
  parent: number | NodeHandle,
  node: number | NodeHandle,
  anchor: number | NodeHandle | null,
) => void;
declare const __ox_removeNode: (
  parent: number | NodeHandle,
  node: number | NodeHandle,
) => void;
declare const __ox_setText: (node: number | NodeHandle, value: string) => void;
declare const __ox_getFirstChild: (node: number | NodeHandle) => number | null;
declare const __ox_getNextSibling: (node: number | NodeHandle) => number | null;
declare const __ox_getParentNode: (node: number | NodeHandle) => number | null;
declare const __ox_state_set: (path: string, valueJson: string) => void;

declare global {
  // Exported for host + app code.
  var __ox_apply_state_patch: ((path: string, valueJson: string) => void) | undefined;
}

const stateStore = createStore<Record<string, unknown>>(normalizeStateValue({}));
let stateMap: Record<string, unknown> = stateStore[0] as unknown as Record<string, unknown>;
let setStateForPath = stateStore[1];
const proxyCache = new Map<string, any>();

const getStateValue = (parts: PathPart[]): unknown => {
  let current: unknown = stateMap;
  for (const p of parts) {
    if (current == null || (typeof current !== "object" && !Array.isArray(current))) {
      return undefined;
    }
    current = (current as Record<string | number, unknown>)[p];
  }
  return current;
};

const setStateForParts = (parts: PathPart[], value: unknown): void => {
  if (parts.length === 0) {
    const [nextState, nextSetter] = createStore(normalizeStateValue(value));
    stateMap = nextState as unknown as Record<string, unknown>;
    setStateForPath = nextSetter;
    proxyCache.clear();
    return;
  }
  (setStateForPath as (...args: any[]) => void)(...parts, value);
};

const propToPart = (raw: string | symbol): PathPart | undefined => {
  if (typeof raw !== "string") {
    return undefined;
  }
  return /^\d+$/.test(raw) ? Number(raw) : raw;
};

const makeStateProxy = (path: PathPart[]): any => {
  const key = path.join(".");
  if (proxyCache.has(key)) {
    return proxyCache.get(key);
  }

  const proxy = new Proxy(
    {},
    {
      get: (_target, rawProp) => {
        if (typeof rawProp === "symbol") {
          return (stateMap as any)[rawProp];
        }
        const part = propToPart(rawProp);
        if (part === undefined) {
          return (stateMap as any)[rawProp];
        }

        const nextParts = [...path, part];
        const value = getStateValue(nextParts);
        if (value === null || value === undefined) {
          return value;
        }
        return typeof value === "object" ? makeStateProxy(nextParts) : value;
      },
      set: (_target, rawProp, value) => {
        if (typeof rawProp === "symbol") {
          return false;
        }
        const part = propToPart(rawProp);
        if (part === undefined) {
          return false;
        }

        const nextParts = [...path, part];
        setStateForParts([...nextParts], value);
        __ox_state_set(nextParts.join("."), JSON.stringify(value));
        return true;
      },
      ownKeys: () => {
        const value = getStateValue(path);
        return value && typeof value === "object"
          ? Reflect.ownKeys(value as object)
          : [];
      },
      getOwnPropertyDescriptor: (_target, rawProp) => {
        if (typeof rawProp === "symbol") {
          return {
            configurable: true,
            enumerable: true,
            value: (stateMap as any)[rawProp],
            writable: true,
          };
        }
        const part = propToPart(rawProp);
        if (part === undefined) {
          return undefined;
        }
        const next = getStateValue([...path, part]);
        if (next === undefined) {
          return undefined;
        }
        return {
          configurable: true,
          enumerable: true,
          value: next,
          writable: true,
        };
      },
    },
  );

  proxyCache.set(key, proxy);
  return proxy;
};

const stateProxyObj = makeStateProxy([]);

// Keep Rust-facing calls numeric, but expose wrapped NodeHandle values to app/renderer
// code so node references remain stable object references for reconciler identity.
const rawCreateElement = __ox_createElement;
const rawCreateTextNode = __ox_createTextNode;
const rawSetProperty = __ox_setProperty;
const rawInsertNode = __ox_insertNode;
const rawRemoveNode = __ox_removeNode;
const rawSetText = __ox_setText;
const rawGetFirstChild = __ox_getFirstChild;
const rawGetNextSibling = __ox_getNextSibling;
const rawGetParentNode = __ox_getParentNode;

(globalThis as any).__ox_createElement = (tag: string) => wrap(rawCreateElement(tag));
(globalThis as any).__ox_createTextNode = (text: string) =>
  wrap(rawCreateTextNode(text));
(globalThis as any).__ox_setProperty = (node: NodeHandle | number, key: string, value: unknown) =>
  rawSetProperty(unwrap(node), key, value);
(globalThis as any).__ox_insertNode = (
  parent: NodeHandle | number,
  node: NodeHandle | number,
  anchor: NodeHandle | number | null,
) => rawInsertNode(unwrap(parent), unwrap(node), unwrapOpt(anchor));
(globalThis as any).__ox_removeNode = (parent: NodeHandle | number, node: NodeHandle | number) =>
  rawRemoveNode(unwrap(parent), unwrap(node));
(globalThis as any).__ox_setText = (node: NodeHandle | number, value: string) =>
  rawSetText(unwrap(node), value);
(globalThis as any).__ox_getFirstChild = (node: NodeHandle | number) =>
  rawGetFirstChild(unwrap(node));
(globalThis as any).__ox_getNextSibling = (node: NodeHandle | number) =>
  rawGetNextSibling(unwrap(node));
(globalThis as any).__ox_getParentNode = (node: NodeHandle | number) =>
  rawGetParentNode(unwrap(node));

const applyStatePatch = (path: string, value: unknown): void => {
  const parts = toPathParts(path);
  if (parts.length === 0) {
    const [nextState, nextSetter] = createStore(normalizeStateValue(value));
    stateMap = nextState as unknown as Record<string, unknown>;
    setStateForPath = nextSetter;
    proxyCache.clear();
    return;
  }
  (setStateForPath as (...args: any[]) => void)(...parts, value);
};

// Host can initialize and inspect this object directly in JS code:
//   state.count = 1
//   state.user.name = "x"
globalThis.state = stateProxyObj as any;

const runtimeState = {
  // Merge `snapshot` into the existing store rather than replacing it.
  //
  // Replacing the store would invalidate every effect already registered
  // against the previous store handle (its tracked signal dependencies
  // would point at a dead store), so post-mount state syncs would silently
  // stop firing reactive updates. Per-key writes go through the existing
  // setter, which preserves reactivity.
  __init(snapshot: unknown) {
    const next = normalizeStateValue(snapshot);
    if (!next || typeof next !== "object") return;
    for (const [key, value] of Object.entries(next as Record<string, unknown>)) {
      setStateForParts([key], value);
    }
  },
};
(globalThis as any).__ox_state = runtimeState;

// Rust side applies Rust-origin state patches here (no feedback through __ox_state_set).
globalThis.__ox_apply_state_patch = (path: string, value_json: string) => {
  let value: unknown = null;
  try {
    value = JSON.parse(value_json);
  } catch (_err) {
    value = value_json;
  }
  applyStatePatch(path, value);
};

// Re-export renderer primitives used by app components.
const renderer = createRenderer<NodeHandle>({
  createElement: (tag) => (globalThis as any).__ox_createElement(tag),
  createTextNode: (text) => (globalThis as any).__ox_createTextNode(text),
  replaceText: (node, text) => __ox_setText(unwrap(node), text),
  setProperty: (node, name, value) => __ox_setProperty(unwrap(node), name, value),
  insertNode: (parent, node, anchor) =>
    __ox_insertNode(unwrap(parent), unwrap(node), unwrapOpt(anchor)),
  isTextNode: (_node) => false,
  removeNode: (parent, node) => __ox_removeNode(unwrap(parent), unwrap(node)),
  getParentNode: (node) => {
    const id = __ox_getParentNode(unwrap(node));
    return id != null ? wrap(id) : null;
  },
  getFirstChild: (node) => {
    const id = __ox_getFirstChild(unwrap(node));
    return id != null ? wrap(id) : null;
  },
  getNextSibling: (node) => {
    const id = __ox_getNextSibling(unwrap(node));
    return id != null ? wrap(id) : null;
  },
});

const render = (code: unknown, root: number | NodeHandle | null) =>
  renderer.render(code, typeof root === "number" ? wrap(root) : root);

// JSX-compatible createElement: handles `createElement(tag, props, ...children)`
// produced by esbuild's `--jsx=transform --jsx-factory=createElement`.
// Single-arg calls (Solid-internal) still work.
//
// Function-valued children/props are wired into reactive effects so writers
// can opt into Solid-style reactivity by wrapping the dynamic expression in
// an arrow function: `<div>{() => state.value}</div>`.
const jsxCreateElement = (
  tag: any,
  props?: Record<string, any> | null,
  ...children: any[]
): NodeHandle => {
  if (typeof tag === "function") {
    return tag(
      Object.assign({}, props || {}, {
        children: children.length <= 1 ? children[0] : children,
      }),
    );
  }
  const node = renderer.createElement(tag);
  const id = unwrap(node);
  if (props && typeof props === "object") {
    for (const key of Object.keys(props)) {
      if (key === "children") continue;
      const value = (props as Record<string, any>)[key];
      // Functions whose names start with `on` are event handlers — pass
      // through verbatim. Other functions are reactive value getters.
      if (typeof value === "function" && !/^on[A-Z]/.test(key)) {
        createEffect(() => __ox_setProperty(id, key, value()));
      } else {
        __ox_setProperty(id, key, value);
      }
    }
  }
  const appendReactive = (getter: () => unknown): void => {
    // Track every node we inserted for this reactive slot so the next
    // re-run of the effect can remove them before inserting the new value.
    // Children may be element handles, raw IDs, arrays, or strings.
    //
    // Stability optimisation: when both the previous and the current value
    // are a single simple text-like value, mutate the existing text node
    // via __ox_setText instead of remove+create+insert. Keeping the same
    // node ID across re-renders matters because hover and focus state on
    // the Rust side index nodes by id — replacing the node would leave
    // `focused_node_id` pointing at a detached node.
    let prevInsertedIds: number[] = [];
    let prevWasSingleText = false;
    const isSimpleText = (v: unknown): boolean =>
      v != null &&
      v !== false &&
      v !== true &&
      !Array.isArray(v) &&
      typeof v !== "object";

    createEffect(() => {
      const value = getter();
      if (
        prevWasSingleText &&
        prevInsertedIds.length === 1 &&
        isSimpleText(value)
      ) {
        (globalThis as any).__ox_setText(prevInsertedIds[0], String(value));
        return;
      }

      for (const childId of prevInsertedIds) {
        try {
          __ox_removeNode(id, childId);
        } catch (_) {
          // ignore if already detached
        }
      }
      prevInsertedIds = [];
      prevWasSingleText = false;

      const insertOne = (child: any): void => {
        if (child == null || child === false || child === true) return;
        if (Array.isArray(child)) {
          for (const c of child) insertOne(c);
          return;
        }
        let childId: number;
        if (typeof child === "object" && typeof (child as any).__oxId === "number") {
          childId = (child as NodeHandle).__oxId;
        } else if (typeof child === "number" && Number.isInteger(child)) {
          childId = child;
        } else {
          childId = (globalThis as any).__ox_createTextNode(String(child));
        }
        __ox_insertNode(id, childId, null);
        prevInsertedIds.push(childId);
      };

      insertOne(value);

      if (prevInsertedIds.length === 1 && isSimpleText(value)) {
        prevWasSingleText = true;
      }
    });
  };
  const append = (child: any): void => {
    if (child == null || child === false || child === true) return;
    if (Array.isArray(child)) {
      for (const c of child) append(c);
      return;
    }
    if (typeof child === "object" && typeof (child as any).__oxId === "number") {
      __ox_insertNode(id, (child as NodeHandle).__oxId, null);
      return;
    }
    if (typeof child === "number" && Number.isInteger(child)) {
      __ox_insertNode(id, child, null);
      return;
    }
    if (typeof child === "function") {
      appendReactive(child as () => unknown);
      return;
    }
    const textId = (globalThis as any).__ox_createTextNode(String(child));
    __ox_insertNode(id, textId, null);
  };
  for (const child of children) append(child);
  return node;
};

export const createComponent = renderer.createComponent;
export const createElement = jsxCreateElement;
export const createTextNode = renderer.createTextNode;
export { render };
export const effect = renderer.effect;
export const insertNode = renderer.insertNode;
export const insert = renderer.insert;
export const memo = renderer.memo;
export const spread = renderer.spread;
export const setProp = renderer.setProp;
export const mergeProps = renderer.mergeProps;
// The JSX compiler always emits a prelude that imports `use` and `For` from
// the runtime, even when the component doesn't reference them. Re-export them
// here so the import resolves; `For` provides Solid's standard <For> helper
// for list rendering, and `use` exposes the `use:` directive primitive.
export const use = renderer.use;
const _For = (props: { each?: any[]; children: (item: any, index: () => number) => any; fallback?: any }) => {
  const each = props.each;
  if (!each || !each.length) {
    return typeof props.fallback === "function" ? props.fallback() : props.fallback;
  }
  return each.map((item, index) => props.children(item, () => index));
};
export { _For as For };
export { createEffect, createMemo, createSignal };
