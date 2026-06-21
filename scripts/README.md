`measure_binary_size.sh` builds three tiny release binaries:

- `baseline`: empty `fn main() {}`
- `solite-core`: `solite` with `default-features = false`
- `solite-default`: `solite` with crate defaults

It reports executable size and growth vs baseline without pulling in the optional
`winit` or `wgpu` integrations, so the numbers track the library itself rather
than a host windowing or GPU stack.
