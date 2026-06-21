// Solid universal renderer + Reactivity bridge for solite.
// Build with:
//
//   cd js && npx esbuild runtime.ts --bundle --format=esm --outfile=dist/runtime.js
//
// 1) DOM ops:
//    - Bridge node IDs are wrapped in opaque objects so Solid never treats them as text.
// 2) State ops:
//    - `state` is a JS proxy backed by a Solid store.
//    - Rust calls `__sol_apply_state_patch(path, value)` each `tick()`.
//    - JS writes to `state` call `__sol_state_set(path, value_json)` to mirror to Rust.

import {
  createEffect,
  createMemo,
  createSignal,
  onCleanup,
  untrack,
} from "solid-js";
import { createRenderer } from "solid-js/universal";
import { createStore } from "solid-js/store";

// Opaque wrapper for a Rust-side blitz-dom node ID.
export interface NodeHandle {
  readonly __solId: number;
}

type PathPart = string | number;

// Memoized so a given DOM id always returns the same wrapper object. Solid's
// universal renderer uses `===` to compare node handles (e.g. cleanChildren
// and reconcileArrays check `getParentNode(el) === parent` to decide what to
// remove); fresh wrappers per call would silently break list reconciliation
// when a reactive child swaps between an array of nodes and a single node.
const handleCache = new Map<number, NodeHandle>();
const wrap = (id: number): NodeHandle => {
  let handle = handleCache.get(id);
  if (!handle) {
    handle = { __solId: id };
    handleCache.set(id, handle);
  }
  return handle;
};
const unwrap = (n: NodeHandle | number | null | undefined): number =>
  typeof n === "number" ? n : n?.__solId ?? 0;
const unwrapOpt = (n: NodeHandle | number | null | undefined): number | null =>
  n == null ? null : typeof n === "number" ? n : n.__solId;

const toPathParts = (path: string): PathPart[] =>
  path === ""
    ? []
    : path.split(".").map((part) => (/^\d+$/.test(part) ? Number(part) : part));

const normalizeStateValue = (value: unknown) =>
  value && typeof value === "object" ? value : {};

// Low-level bridge ops provided by Rust on the global scope.
declare const __sol_createElement: (tag: string) => number;
declare const __sol_createTextNode: (text: string) => number;
declare const __sol_setProperty: (
  node: number | NodeHandle,
  key: string,
  value: unknown,
) => void;
declare const __sol_insertNode: (
  parent: number | NodeHandle,
  node: number | NodeHandle,
  anchor: number | NodeHandle | null,
) => void;
declare const __sol_removeNode: (
  parent: number | NodeHandle,
  node: number | NodeHandle,
) => void;
declare const __sol_setText: (node: number | NodeHandle, value: string) => void;
declare const __sol_isTextNode: (node: number | NodeHandle) => boolean;
declare const __sol_getFirstChild: (node: number | NodeHandle) => number | null;
declare const __sol_getNextSibling: (node: number | NodeHandle) => number | null;
declare const __sol_getParentNode: (node: number | NodeHandle) => number | null;
declare const __sol_state_set: (path: string, valueJson: string) => void;

declare global {
  // Exported for host + app code.
  var __sol_apply_state_patch: ((path: string, valueJson: string) => void) | undefined;
}

type RuntimeEventListener = (event: {
  type: string;
  detail: unknown;
  payload: unknown;
  defaultPrevented: boolean;
  preventDefault: () => void;
}) => void;

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
        __sol_state_set(nextParts.join("."), JSON.stringify(value));
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
const rawCreateElement = __sol_createElement;
const rawCreateTextNode = __sol_createTextNode;
const rawSetProperty = __sol_setProperty;
const rawInsertNode = __sol_insertNode;
const rawRemoveNode = __sol_removeNode;
const rawSetText = __sol_setText;
const rawIsTextNode = __sol_isTextNode;
const rawGetFirstChild = __sol_getFirstChild;
const rawGetNextSibling = __sol_getNextSibling;
const rawGetParentNode = __sol_getParentNode;

(globalThis as any).__sol_createElement = (tag: string) => wrap(rawCreateElement(tag));
(globalThis as any).__sol_createTextNode = (text: string) =>
  wrap(rawCreateTextNode(text));
(globalThis as any).__sol_setProperty = (node: NodeHandle | number, key: string, value: unknown) =>
  rawSetProperty(unwrap(node), key, value);
(globalThis as any).__sol_insertNode = (
  parent: NodeHandle | number,
  node: NodeHandle | number,
  anchor: NodeHandle | number | null,
) => rawInsertNode(unwrap(parent), unwrap(node), unwrapOpt(anchor));
(globalThis as any).__sol_removeNode = (parent: NodeHandle | number, node: NodeHandle | number) =>
  rawRemoveNode(unwrap(parent), unwrap(node));
(globalThis as any).__sol_setText = (node: NodeHandle | number, value: string) =>
  rawSetText(unwrap(node), value);
(globalThis as any).__sol_isTextNode = (node: NodeHandle | number) =>
  rawIsTextNode(unwrap(node));
(globalThis as any).__sol_getFirstChild = (node: NodeHandle | number) =>
  rawGetFirstChild(unwrap(node));
(globalThis as any).__sol_getNextSibling = (node: NodeHandle | number) =>
  rawGetNextSibling(unwrap(node));
(globalThis as any).__sol_getParentNode = (node: NodeHandle | number) =>
  rawGetParentNode(unwrap(node));

const applyStatePatch = (path: string, value: unknown): void => {
  const parts = toPathParts(path);
  if (parts.length === 0) {
    const next = normalizeStateValue(value) as Record<string, unknown>;
    const nextKeys = new Set(Object.keys(next));
    for (const key of Object.keys(stateMap)) {
      if (!nextKeys.has(key)) {
        setStateForParts([key], undefined);
      }
    }
    for (const [key, entryValue] of Object.entries(next)) {
      setStateForParts([key], entryValue);
    }
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
(globalThis as any).__SOL_INITIAL_STATE != null &&
  runtimeState.__init((globalThis as any).__SOL_INITIAL_STATE);
(globalThis as any).__sol_state = runtimeState;
try {
  // Keep bootstrap data transient so repeated mounts in the same page keep
  // behavior deterministic.
  delete (globalThis as any).__SOL_INITIAL_STATE;
} catch (_err) {
  (globalThis as any).__SOL_INITIAL_STATE = undefined;
}

// Rust side applies Rust-origin state patches here (no feedback through __sol_state_set).
globalThis.__sol_apply_state_patch = (path: string, value_json: string) => {
  let value: unknown = null;
  try {
    value = JSON.parse(value_json);
  } catch (_err) {
    value = value_json;
  }
  applyStatePatch(path, value);
};

const runtimeEventListeners = new Map<string, Set<RuntimeEventListener>>();

const addRuntimeEventListener = (
  type: string,
  listener: RuntimeEventListener,
): void => {
  if (typeof type !== "string" || typeof listener !== "function") {
    return;
  }
  let listeners = runtimeEventListeners.get(type);
  if (!listeners) {
    listeners = new Set();
    runtimeEventListeners.set(type, listeners);
  }
  listeners.add(listener);
};

const removeRuntimeEventListener = (
  type: string,
  listener: RuntimeEventListener,
): void => {
  runtimeEventListeners.get(type)?.delete(listener);
};

const dispatchRuntimeEvent = (type: string, payloadJson: string): number => {
  const listeners = runtimeEventListeners.get(type);
  if (!listeners || listeners.size === 0) {
    return 0;
  }

  let detail: unknown = null;
  try {
    detail = JSON.parse(payloadJson);
  } catch (_err) {
    detail = payloadJson;
  }

  let defaultPrevented = false;
  const event = {
    type,
    detail,
    payload: detail,
    get defaultPrevented() {
      return defaultPrevented;
    },
    preventDefault() {
      defaultPrevented = true;
    },
  };

  const snapshot = Array.from(listeners);
  for (const listener of snapshot) {
    try {
      listener(event);
    } catch (err) {
      (globalThis as any).__sol_last_runtime_event_error =
        err instanceof Error ? err.message : String(err);
    }
  }
  return snapshot.length;
};

(globalThis as any).__sol_addEventListener = addRuntimeEventListener;
(globalThis as any).__sol_removeEventListener = removeRuntimeEventListener;
(globalThis as any).__sol_dispatch_runtime_event = dispatchRuntimeEvent;
if (typeof (globalThis as any).addEventListener !== "function") {
  (globalThis as any).addEventListener = addRuntimeEventListener;
}
if (typeof (globalThis as any).removeEventListener !== "function") {
  (globalThis as any).removeEventListener = removeRuntimeEventListener;
}

const hyphenateStyleName = (name: string): string =>
  name.replace(/[A-Z]/g, (match) => `-${match.toLowerCase()}`);

const styleToString = (value: unknown): string => {
  if (value == null || value === false) return "";
  if (typeof value === "string") return value;
  if (typeof value !== "object") return String(value);
  return Object.entries(value as Record<string, unknown>)
    .filter(([, v]) => v != null && v !== false)
    .map(([k, v]) => `${hyphenateStyleName(k)}: ${String(v)}`)
    .join("; ");
};

const classListToString = (value: unknown): string => {
  if (value == null || value === false) return "";
  if (typeof value === "string") return value;
  if (Array.isArray(value)) return value.filter(Boolean).join(" ");
  if (typeof value !== "object") return String(value);
  return Object.entries(value as Record<string, unknown>)
    .filter(([, v]) => !!v)
    .map(([k]) => k)
    .join(" ");
};

// Track attributes we've already warned about so a list of N offending rows
// produces one message, not N. The branch fires at element creation, not per
// frame, so this only guards against repeated component instances.
const warnedAttributeFns = new Set<string>();

const warnUncalledAttributeFn = (name: string): void => {
  const warn = (globalThis as any).__sol_dev_warn;
  // `__sol_dev_warn` is only installed in debug builds; optional chaining keeps
  // this zero-cost (the template literal is not built) in release.
  if (typeof warn !== "function" || warnedAttributeFns.has(name)) {
    return;
  }
  warnedAttributeFns.add(name);
  const message =
    `attribute \`${name}\` was given a function instead of a value, so it is ` +
    `applied once and never updates. Call the getter in the attribute — e.g. ` +
    `\`${name}={fn()}\` not \`${name}={fn}\` — so it is wrapped in a reactive effect.`;
  warn(message);
  // Mirror into a global array for host/test introspection. Only reached in
  // dev builds (guarded by the host binding above), so release stays clean.
  const sink = ((globalThis as any).__sol_dev_warnings =
    (globalThis as any).__sol_dev_warnings || []);
  sink.push(message);
};

const applyRuntimeProperty = (
  node: NodeHandle | number,
  name: string,
  value: unknown,
  _prev?: unknown,
): unknown => {
  const id = unwrap(node);
  const event =
    typeof name === "string"
      ? (globalThis as any).__sol_extractEventName?.(name)
      : null;

  if (name === "ref") {
    if (typeof value === "function") {
      return untrack(() => (value as (node: NodeHandle | number) => unknown)(node));
    }
    return value;
  }

  if (typeof value === "function" && event == null) {
    // A function reaching a non-event, non-ref attribute means it was passed by
    // reference (`attr={fn}`) instead of called (`attr={fn()}`). The compiler
    // only wraps call/member expressions in a reactive effect, so the bare-
    // reference form is applied once and never updates. Warn (dev builds only),
    // then degrade gracefully by calling it once.
    warnUncalledAttributeFn(name);
    value = (value as () => unknown)();
  }

  if (name === "style") {
    value = styleToString(value);
  } else if (name === "classList") {
    name = "class";
    value = classListToString(value);
  }

  __sol_setProperty(id, name, value);
  return value;
};

// Re-export renderer primitives used by app components.
const renderer = createRenderer<NodeHandle>({
  createElement: (tag) => (globalThis as any).__sol_createElement(tag),
  createTextNode: (text) => (globalThis as any).__sol_createTextNode(text),
  replaceText: (node, text) => __sol_setText(unwrap(node), text),
  setProperty: (node, name, value, prev) =>
    applyRuntimeProperty(node, name, value, prev),
  insertNode: (parent, node, anchor) =>
    __sol_insertNode(unwrap(parent), unwrap(node), unwrapOpt(anchor)),
  isTextNode: (node) => __sol_isTextNode(unwrap(node)),
  removeNode: (parent, node) => __sol_removeNode(unwrap(parent), unwrap(node)),
  getParentNode: (node) => {
    const id = __sol_getParentNode(unwrap(node));
    return id != null ? wrap(id) : null;
  },
  getFirstChild: (node) => {
    const id = __sol_getFirstChild(unwrap(node));
    return id != null ? wrap(id) : null;
  },
  getNextSibling: (node) => {
    const id = __sol_getNextSibling(unwrap(node));
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
        createEffect(() => applyRuntimeProperty(id, key, value()));
      } else {
        applyRuntimeProperty(id, key, value);
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
    // via __sol_setText instead of remove+create+insert. Keeping the same
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
        (globalThis as any).__sol_setText(prevInsertedIds[0], String(value));
        return;
      }

      for (const childId of prevInsertedIds) {
        try {
          __sol_removeNode(id, childId);
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
        if (typeof child === "object" && typeof (child as any).__solId === "number") {
          childId = (child as NodeHandle).__solId;
        } else if (typeof child === "number" && Number.isInteger(child)) {
          childId = child;
        } else {
          childId = (globalThis as any).__sol_createTextNode(String(child));
        }
        __sol_insertNode(id, childId, null);
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
    if (typeof child === "object" && typeof (child as any).__solId === "number") {
      __sol_insertNode(id, (child as NodeHandle).__solId, null);
      return;
    }
    if (typeof child === "number" && Number.isInteger(child)) {
      __sol_insertNode(id, child, null);
      return;
    }
    if (typeof child === "function") {
      appendReactive(child as () => unknown);
      return;
    }
    const textId = (globalThis as any).__sol_createTextNode(String(child));
    __sol_insertNode(id, textId, null);
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
export { createEffect, createMemo, createSignal, onCleanup, untrack };
