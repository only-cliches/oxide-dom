export {};

type Accessor<T> = () => T;
type NoInfer<T extends any> = [T][T extends any ? 0 : never];
type DistributeOverride<T, F> = T extends undefined ? F : T;
type Signal<T> = [Accessor<T>, Setter<T>];
type Setter<in out T> = {
  <U extends T>(...args: undefined extends T ? [] : [value: Exclude<U, Function> | ((prev: T) => U)]): undefined extends T ? undefined : U;
  <U extends T>(value: (prev: T) => U): U;
  <U extends T>(value: Exclude<U, Function>): U;
  <U extends T>(value: Exclude<U, Function> | ((prev: T) => U)): U;
};
type Component<P = {}> = (props: P) => JSX.Element;
type EffectFunction<Prev, Next extends Prev = Prev> = (v: Prev) => Next;
type BaseOptions = {
  name?: string;
};
interface EffectOptions extends BaseOptions {}
interface MemoOptions<T> extends EffectOptions {
  equals?: false | ((prev: T, next: T) => boolean);
}
type SignalOptions<T> = MemoOptions<T> & {
  internal?: boolean;
};
interface EffectRunner {
  <Next>(fn: EffectFunction<undefined | NoInfer<Next>, Next>): void;
  <Next, Init = Next>(fn: EffectFunction<Init | Next, Next>, value: Init, options?: EffectOptions & {
    render?: boolean;
  }): void;
}
interface MemoRunner {
  <Next extends Prev, Prev = Next>(fn: EffectFunction<undefined | NoInfer<Prev>, Next>): Accessor<Next>;
  <Next extends Prev, Init = Next, Prev = Next>(fn: EffectFunction<Init | Prev, Next>, value: Init, options?: MemoOptions<Next>): Accessor<Next>;
}
interface RuntimeRenderer<NodeType> {
  render(code: () => NodeType, node: NodeType): () => void;
  effect<T>(fn: (prev?: T) => T, init?: T): void;
  memo<T>(fn: () => T, equal: boolean): () => T;
  createComponent<T>(Comp: (props: T) => NodeType, props: T): NodeType;
  createElement(tag: string): NodeType;
  createTextNode(value: string): NodeType;
  insertNode(parent: NodeType, node: NodeType, anchor?: NodeType): void;
  insert<T>(parent: any, accessor: (() => T) | T, marker?: any | null, initial?: any): NodeType;
  spread<T>(node: any, accessor: (() => T) | T, skipChildren?: boolean): void;
  setProp<T>(node: NodeType, name: string, value: T, prev?: T): T;
  mergeProps(...sources: unknown[]): unknown;
  use<A, T>(fn: (element: NodeType, arg: A) => T, element: NodeType, arg: A): T;
}
type MergeOverride<T, U> = T extends any ? U extends any ? {
  [K in keyof T]: K extends keyof U ? DistributeOverride<U[K], T[K]> : T[K];
} & {
  [K in keyof U]: K extends keyof T ? DistributeOverride<U[K], T[K]> : U[K];
} : T & U : T & U;
type MergePropsList<T, Curr = {}> = T extends [
  infer Next | (() => infer Next),
  ...infer Rest
] ? MergePropsList<Rest, MergeOverride<Curr, Next>> : T extends [...infer Rest, infer Next | (() => infer Next)] ? MergePropsList<Rest, MergeOverride<Curr, Next>> : T extends [] ? Curr : T extends (infer I | (() => infer I))[] ? MergeOverrideSpread<Curr, I> : Curr;
type MergeOverrideSpread<T, U> = T extends any ? {
  [P in keyof ({
    [K in keyof T]: any;
  } & {
    [K in keyof U]?: any;
  } & {
    [K in U extends any ? keyof U : keyof U]?: any;
  })]: P extends keyof T
    ? Exclude<U extends any ? U[P & keyof U] : never, undefined> | T[P]
    : U extends any ? U[P & keyof U] : never;
} : T;
type MergeProps<T extends unknown[]> = Simplify<MergePropsList<T>>;
type Simplify<T> = T extends any ? {
  [K in keyof T]: T[K];
} : T;

type RuntimeRendererNode = RuntimeRenderer<NodeHandle>;

declare global {
  var state: Record<string, unknown>;
  var sendEvent: (type: string, payloadJson: string) => void;
  var __SOL_ROOT__: number;

  interface SoliteRuntimeEvent {
    type: string;
    detail: unknown;
    payload: unknown;
    defaultPrevented: boolean;
    preventDefault: () => void;
  }

  interface Window {
    __sol_createElement: (tag: string) => NodeHandle;
    __sol_createTextNode: (text: string) => NodeHandle;
    __sol_setProperty: (
      node: NodeHandle | number,
      key: string,
      value: unknown,
    ) => void;
    __sol_insertNode: (
      parent: NodeHandle | number,
      node: NodeHandle | number,
      anchor: NodeHandle | number | null,
    ) => void;
    __sol_removeNode: (
      parent: NodeHandle | number,
      node: NodeHandle | number,
    ) => void;
    __sol_setText: (node: NodeHandle | number, value: string) => void;
    __sol_isTextNode: (node: NodeHandle | number) => boolean;
    __sol_getFirstChild: (node: NodeHandle | number) => number | null;
    __sol_getNextSibling: (node: NodeHandle | number) => number | null;
    __sol_getParentNode: (node: NodeHandle | number) => number | null;
    __sol_state_set: (path: string, valueJson: string) => void;
    __sol_state: { __init: (snapshot: unknown) => void };
    __sol_apply_state_patch?: (path: string, valueJson: string) => void;
    __sol_addEventListener: (
      type: string,
      listener: (event: SoliteRuntimeEvent) => void,
    ) => void;
    __sol_removeEventListener: (
      type: string,
      listener: (event: SoliteRuntimeEvent) => void,
    ) => void;
    __sol_dispatch_runtime_event: (type: string, payloadJson: string) => number;
    __sol_last_runtime_event_error: string;
  }
}

interface NodeHandle {
  readonly __solId: number;
}

declare module "solite-runtime" {
  export interface ForProps<T> {
    each?: readonly T[] | T[] | false | null;
    children: (item: T, index: Accessor<number>) => JSX.Element;
    fallback?: JSX.Element;
  }

  export type Node = NodeHandle;

  export function createElement(
    tag: string,
    props?: Record<string, unknown> | null,
    ...children: readonly unknown[]
  ): Node;
  export function createElement<P extends Record<string, unknown>>(
    tag: (props: P) => Node,
    props: P,
    ...children: readonly unknown[]
  ): Node;

  export function createTextNode(text: string): NodeHandle;
  export function createComponent<T>(
    component: Component<T>,
    props: T,
  ): Node;
  export function render(
    code: () => Node,
    root: number | Node | null,
  ): () => void;

  export const createEffect: EffectRunner;
  export const createMemo: MemoRunner;
  export const createSignal: {
    <T>(): Signal<T | undefined>;
    <T>(value: T, options?: SignalOptions<T>): Signal<T>;
  };
  export const onCleanup: <T extends () => any>(fn: T) => T;
  export const untrack: <T>(fn: Accessor<T>) => T;

  export const spread: RuntimeRendererNode["spread"];
  export const effect: RuntimeRendererNode["effect"];
  export const insert: RuntimeRendererNode["insert"];
  export const insertNode: RuntimeRendererNode["insertNode"];
  export const memo: RuntimeRendererNode["memo"];
  export const use: RuntimeRendererNode["use"];
  export const mergeProps: <T extends unknown[]>(...sources: T) => MergeProps<T>;
  export const setProp: RuntimeRendererNode["setProp"];
  export const For: <T>(props: ForProps<T>) => JSX.Element;
}

declare namespace JSX {
  interface Element {
    __elementType?: unknown;
  }

  interface IntrinsicElements {
    [tag: string]: Record<string, unknown>;
  }
}
