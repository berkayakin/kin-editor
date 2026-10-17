# Zed's extension API (WIT)

The interface Zed extensions' WebAssembly code (`extension.wasm`) is written against, from which
`src/extensions/wasm.rs` generates its host bindings with `wasmtime::component::bindgen!`.

- `since_v0.6.0/`: the `.wit` files of the `zed_extension_api` crate, version 0.7.0, unchanged
  (crates.io, `zed_extension_api-0.7.0.crate`, SHA-256
  `0729d50b4ca0a7e28e590bbe32e3ca0194d97ef654961451a424c661a366fca0`; its `wit/since_v0.6.0`).
  Extensions built with extension API 0.6.0 and 0.7.0 both use this interface.
- License: Apache-2.0 (`LICENSE-APACHE`), Copyright 2022 - 2025 Zed Industries, Inc.

Only these interface files come from Zed. Kuana's host side is its own: Zed's `extension_host`
crate is GPL-3.0 and none of it is used here.
