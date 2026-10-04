# @agentos/desktop - Tauri 2 shell

The desktop client loads the same UI as the browser (apps/web) and adds only three things:

* telling the UI where the runtime is (`runtime_config`),
* health probing (`runtime_health`),
* optionally starting or stopping the runtime process (`start_runtime` / `stop_runtime`).

No runtime logic is duplicated here: the window talks HTTP/WS to the runtime exactly as a browser
does, so a desktop user can point it at a remote node.

## Run

```bash
pnpm install
pnpm desktop            # tauri dev  (starts the web dev server first)
pnpm desktop:build      # tauri build
```

Prerequisites: Rust toolchain, Node 20+, and on Windows the WebView2 runtime (preinstalled on
Windows 11). Build the runtime binary first if you want the "start runtime" button to work:

```bash
cargo build -p agentos-server
```

## Icons

`src-tauri/icons/` ships a generated placeholder. Replace it with your own set:

```bash
pnpm --filter @agentos/desktop exec tauri icon path/to/logo.png
```