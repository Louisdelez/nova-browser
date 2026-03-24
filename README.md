# NOVA Browser

A from-scratch web browser built in Rust with a micro-kernel + mods architecture.

![NOVA rendering Google](https://img.shields.io/badge/Rust-2024_Edition-orange) ![Tests](https://img.shields.io/badge/tests-746_passing-green) ![Lines](https://img.shields.io/badge/code-57k_lines-blue)

## Screenshots

NOVA renders real websites — Google, Hacker News, Wikipedia, and more.

## Features

### Rendering Engine
- **HTML5 parser** (html5ever) with full DOM tree construction
- **CSS engine** with 170+ properties, cascade, specificity, `!important`, `initial`/`inherit`/`unset`
- **CSS variables** (`var(--name, fallback)`), `clamp()`, `min()`, `max()`, `calc()`, `env()`
- **Layout engine** (Taffy) with Flexbox, CSS Grid (`grid-template-areas`, `auto-fill`/`auto-fit`), table layout (colspan, rowspan, cellpadding, border-collapse)
- **Text rendering** with FreeType + rustybuzz (kerning, ligatures, shaping)
- **Font matching** — Arial, Helvetica, serif, monospace mapped to system fonts via fontconfig
- **woff2/woff** font decompression for web fonts
- **Margin collapsing** (sibling + parent-child) matching CSS block flow spec
- **`::before`/`::after`** pseudo-elements with `content` property
- **CSS transforms** (2D matrix, rotate, scale, skew, translate)
- **CSS transitions** and **@keyframes animations**
- **Gradients** (linear, radial, conic)
- **Background images** with `background-size`, `background-position`, `background-repeat`
- **Inline SVG** rasterization via resvg
- **`position: fixed/absolute/relative/sticky`**
- **CSS `float`** with `clear` property
- **`overflow: hidden/scroll/auto`** with `text-overflow: ellipsis`
- **`border-radius`**, dashed/dotted/double borders, `box-shadow` with blur
- **`object-fit`** (cover, contain, fill, none, scale-down)
- **`letter-spacing`**, `text-shadow`**, `text-indent`, `text-transform`
- **`vertical-align`** for inline elements
- **`white-space: pre/pre-wrap/pre-line/nowrap`**
- **`column-count`** multi-column layout
- **CSS `@media`** queries (width, height, orientation, hover, pointer, prefers-color-scheme, prefers-reduced-motion)
- **CSS `@supports`** and `@container` queries
- **`aspect-ratio`**, `order`, `flex-flow`, `place-items`/`place-content`
- **`<details>`/`<summary>`** with disclosure triangle
- **`<dialog>`** with modal backdrop
- **`<progress>`/`<meter>`** widget rendering
- **`<video>`/`<audio>`** element UI (play button, controls bar)
- **List markers** (disc, circle, square, decimal, alpha, roman)

### JavaScript Engine
- **QuickJS** runtime with full ES2020+ support
- **Complete DOM API** — querySelector (complex selectors), createElement, innerHTML, classList, dataset, closest, insertAdjacentHTML, getBoundingClientRect
- **Event system** — addEventListener with options (passive, once), dispatchEvent, DOMContentLoaded, hashchange, popstate
- **XHR** and **fetch** API
- **WebSocket** API
- **Canvas 2D** API (fillRect, paths, transforms, text, images, save/restore)
- **Shadow DOM** with slot distribution and style encapsulation
- **Custom Elements** (customElements.define, connectedCallback)
- **`history.pushState`/`replaceState`** for SPA routing
- **`window.location`** setter triggers navigation
- **Web API stubs** — IntersectionObserver, MutationObserver, ResizeObserver, matchMedia, crypto, URL/URLSearchParams, TextEncoder/TextDecoder, AbortController, Service Worker
- **Error resilient** — try/catch wrapping prevents single script failure from breaking the page

### Networking
- **HTTPS** with rustls (TLS 1.2/1.3)
- **Cookie jar** with persistence (`~/.nova/cookies.json`)
- **HTTP cache** with ETag, If-Modified-Since, LRU eviction (100MB)
- **WebSocket** support (tokio-tungstenite)
- **HSTS** store with persistence
- **CORS** preflight caching
- **CSP** header parsing
- **Gzip** decompression, chunked transfer encoding
- **Redirect** following (301/302/303/307/308)

### Browser Shell
- **Multi-tab** support (Ctrl+T, Ctrl+W, Ctrl+Tab, Ctrl+1-9)
- **Tab bar** with close buttons and new tab (+)
- **Navigation** — back/forward (Alt+Left/Right), reload (F5), anchor scrolling (#)
- **URL bar** with search query support (non-URL input → Google search)
- **Smooth scrolling** with linear interpolation
- **Interactive form fields** — keyboard input with cursor, Shift+arrow selection, double/triple-click
- **Clipboard** — Ctrl+C/V/X/A (via arboard)
- **File picker** for `<input type="file">` (via rfd)
- **Find in page** (Ctrl+F) with highlight
- **Zoom** (Ctrl+/-, Ctrl+0)
- **Bookmarks** (Ctrl+D) with persistence
- **History** with persistence
- **Downloads** manager
- **DevTools** — Console, Elements tree, Network tab (F12)
- **View source** (Ctrl+U)
- **Screenshot** (Ctrl+P → PNG)
- **Tooltips** on elements with `title` attribute
- **Page titles** in tab bar from `<title>` element
- **Favicon** URL extraction
- **`about:blank`** and **`about:version`** internal pages
- **Styled error pages** (DNS, TLS, timeout)
- **404 indicator** in URL bar
- **Notification toasts** for errors

### GPU Rendering
- **Vello** GPU backend (scene building, texture readback)
- **Software fallback** with FreeType rasterization
- **Glyph caching** for performance

### Security
- **Content Security Policy** (CSP) header parsing
- **CORS** with preflight
- **HSTS** enforcement
- **Secure cookie** flags
- **Mod sandboxing** with permissions

## Architecture

```
nova-browser/
├── crates/
│   ├── nova-core/          # Micro-kernel: loads mods, routes messages
│   ├── nova-mod-api/       # Mod trait definitions, TypedData, RenderOps
│   ├── nova-ipc/           # IPC message bus
│   ├── nova-gpu/           # GPU compositor (Vello + wgpu)
│   ├── nova-registry/      # Capability registry + mod loader
│   ├── nova-security/      # Permissions, CSP, sandboxing
│   ├── nova-pipeline/      # Pipeline engine (URL → pixels)
│   ├── nova-shell/         # UI shell (iced) — tabs, URL bar, DevTools
│   └── mods/
│       ├── mod-network/    # HTTP/HTTPS, cookies, cache, WebSocket, HSTS
│       ├── mod-html-parser/# HTML5 parsing (html5ever)
│       ├── mod-css-engine/ # CSS parsing, cascade, variables, animations
│       ├── mod-layout/     # Layout (Taffy flex/grid + table + IFC)
│       ├── mod-painter/    # Paint layout → RenderOps
│       ├── mod-js/         # JavaScript (QuickJS), DOM API, Canvas 2D
│       └── mod-image/      # Image decoding (PNG, JPEG, WebP, GIF, SVG)
```

The **micro-kernel** knows nothing about web standards. All functionality is provided by **mods** that communicate through the core's IPC bus.

## Build & Run

```bash
# Build
cargo build

# Run
cargo run -p nova-shell

# Run with a specific URL
cargo run -p nova-shell -- "https://www.google.com"

# Run tests
cargo test
```

### Requirements
- Rust (edition 2024)
- Linux (X11 or Wayland)
- System fonts (DejaVu, Liberation, or similar)
- GPU: optional (Vello uses wgpu; falls back to software renderer)

## Keyboard Shortcuts

| Shortcut | Action |
|----------|--------|
| Ctrl+T | New tab |
| Ctrl+W | Close tab |
| Ctrl+Tab | Next tab |
| Ctrl+1-9 | Switch to tab N |
| Ctrl+L / F6 | Focus URL bar |
| Alt+Left / Backspace | Back |
| Alt+Right | Forward |
| F5 / Ctrl+R | Reload |
| Ctrl+F | Find in page |
| Ctrl+D | Bookmark |
| Ctrl+H | History |
| F12 / Ctrl+Shift+I | DevTools |
| Ctrl+U | View source |
| Ctrl+P | Screenshot (PNG) |
| Ctrl++ / Ctrl+- | Zoom in/out |
| Ctrl+0 | Reset zoom |
| Ctrl+C/V/X/A | Copy/Paste/Cut/Select All |

## Tech Stack

| Component | Technology |
|-----------|-----------|
| Language | Rust (Edition 2024) |
| GPU | Vello + wgpu |
| HTML | html5ever |
| CSS | cssparser + custom engine |
| Layout | Taffy (Flexbox/Grid) + custom (table, IFC) |
| JavaScript | QuickJS (rquickjs) |
| Networking | rustls + tokio |
| UI Shell | iced |
| Text | FreeType + rustybuzz + fontdue |
| Images | image crate + resvg (SVG) |
| Fonts | woff2-patched + woff |

## Stats

- **57,000+** lines of Rust code
- **746** tests passing
- **170+** CSS properties supported
- **15** crates in workspace
- **0** unsafe code blocks

## License

See LICENSE file.
