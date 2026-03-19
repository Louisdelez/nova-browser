# NOVA Browser - Claude Project Config

## Project Overview
NOVA is a from-scratch web browser built in Rust with a micro-kernel + mods architecture.
The core is ultra-lightweight and all functionality (HTML, CSS, JS, codecs, PDF, etc.) is provided by dynamically-loaded mods.

## Architecture
- **Micro-kernel pattern**: the core knows nothing about web standards. It only loads mods, routes messages, manages processes, and composites GPU output.
- **Mods are the engine**: HTML parsing, CSS, JS execution, image decoding, etc. are all mods.
- **Mods never talk to each other directly** — all communication goes through the core's IPC bus.
- **Content Triggers**: the core auto-detects what mods are needed based on MIME types, magic bytes, HTML elements, JS API calls, etc.

## Tech Stack
- **Language**: Rust (edition 2024)
- **GPU**: Vello + wgpu (GPU-first rendering)
- **HTML parsing**: html5ever (via mod-html-parser)
- **CSS parsing**: cssparser (via mod-css-engine)
- **Layout**: Custom + Taffy for Flexbox/Grid (via mod-layout)
- **JS engine**: QuickJS (via mod-js, phase 1)
- **Networking**: hyper + quinn + rustls (via mod-network)
- **UI shell**: iced
- **Async runtime**: tokio
- **Parallelism**: rayon

## Workspace Structure
```
nova-browser/
├── Cargo.toml              # Workspace root
├── crates/
│   ├── nova-core/          # The micro-kernel
│   ├── nova-mod-api/       # Mod trait definitions + TypedData + CoreApi
│   ├── nova-ipc/           # IPC message bus
│   ├── nova-gpu/           # GPU compositor (Vello + wgpu)
│   ├── nova-registry/      # Capability registry + mod loader
│   ├── nova-security/      # Permissions, sandboxing, signatures
│   ├── nova-pipeline/      # Pipeline engine (URL → pixels)
│   ├── nova-shell/         # UI shell (iced) — address bar, tabs
│   └── mods/               # Built-in mods
│       ├── mod-network/
│       ├── mod-html-parser/
│       ├── mod-css-engine/
│       ├── mod-layout/
│       ├── mod-painter/
│       ├── mod-js/
│       └── mod-image/
```

## Coding Conventions
- All code in `/home/louisdelez/Documents/nova-browser/`
- Use `thiserror` for error types
- Use `tracing` for logging (not println!)
- Prefer `Arc<dyn Trait>` for shared mod references
- All public APIs must be documented with `///` doc comments
- Tests go in the same file (`#[cfg(test)] mod tests`)
- Mods implement traits from `nova-mod-api`
- No `unsafe` unless absolutely required and commented

## Build & Run
```sh
cargo build                  # build everything
cargo run -p nova-shell      # run the browser
cargo test                   # run all tests
```
