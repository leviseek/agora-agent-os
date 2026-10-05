# @agentos/desktop - Tauri 2 shell

The desktop client loads the same UI as the browser (apps/web) and adds only four things:

* telling the UI where the runtime is (`runtime_config`),
* health probing (`runtime_health`),
* optionally starting or stopping the runtime process (`start_runtime` / `stop_runtime`),
* opening the operating system's folder picker (`pick_directory`, via `tauri-plugin-dialog`), which is
  what the console's "Explorer..." button on the workspaces view uses. A browser page cannot do this:
  the File System Access API hands it a handle, never a real path, so there would be nothing to send
  to the runtime. The folder still has to be one the node allows - `policy.extra_workspace_roots` is
  how an operator says "workspaces may also live under `D:\projects`".

Verify it without a human:

```bash
# the shell is a WebView2 window, so it is reachable over CDP like a browser
WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS=--remote-debugging-port=9353 pnpm desktop
node scripts/cdp-desktop-check.mjs http://127.0.0.1:9353
```

That checks the bridge and that the button is rendered. Choosing a folder is the operating system's
modal dialog: a human clicks it.

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