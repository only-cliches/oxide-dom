# Solid universal parity checklist

The reference transform is `babel-plugin-jsx-dom-expressions` with `generate: "universal"`.

Vendored fixture directories:

- `attributeExpressions`
- `components`
- `conditionalExpressions`
- `fragments`
- `insertChildren`
- `simpleElements`
- `textInterpolation`

The ignored Rust test `tests/solid_universal_fixtures.rs` is the conformance harness entry point. Enable it while working fixture-by-fixture. Full parity means each fixture compiles and the output is either text-equivalent after normalization or behavior-equivalent against `oxide-runtime`.

Current oxide-dom compiler target:

- Runtime module: `oxide-runtime`
- Universal imports: `createElement`, `createTextNode`, `insertNode`, `insert`, `setProp`, `spread`, `effect`, `createComponent`, `mergeProps`, `use`
- Active implementation: `src/compiler.rs`
