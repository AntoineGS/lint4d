# Pascal LSP for VS Code

Pascal LSP adds Delphi/Object Pascal language support to VS Code using the
native `pascal-lsp` server. It works on Linux and Windows. The server binary is
installed separately; the extension does not download or build it.

## Build and install

From `editors/vscode/`, package the extension:

```sh
npm ci
npm run package
```

This creates `pascal-lsp-0.1.0.vsix`. Install it locally with:

```sh
code --install-extension pascal-lsp-0.1.0.vsix
```

## Server binary

The extension resolves the server in this order:

1. `pascalLsp.serverPath` — supports `~` and `${workspaceFolder}`. This
setting has VS Code's `machine-overridable` scope.
2. The `PASCAL_LSP_BIN` environment variable.
3. `pascal-lsp` on `PATH` (`pascal-lsp.exe` on Windows).

For example, a Linux development build can be selected with:

```text
~/gits/lint4d/target/release/pascal-lsp
```

On Windows, set the path to the executable, for example:

```text
C:\path\to\pascal-lsp.exe
```

If the binary is not found, build it from the repository root with
`cargo build --release --locked -p pascal-lsp`, then set `pascalLsp.serverPath`
or add it to `PATH`.

## Settings

All settings use the `pascalLsp` prefix. Server settings with no explicit value
are omitted from the LSP configuration, so the server uses its own default.
Extension-only settings use the defaults shown below. Settings marked
**Startup only** take effect after restarting the server. The
[runtime configuration reference](https://github.com/AntoineGS/lint4d/blob/master/crates/pascal-lsp/README.md#runtime-configuration-lsp)
describes the server-side behavior.

| Setting | Description / unset behavior |
| --- | --- |
| `pascalLsp.serverPath` | Server executable path; unset uses `PASCAL_LSP_BIN`, then `PATH`. `machine-overridable`. |
| `pascalLsp.serverArgs` | Extra arguments after `--stdio`; default `[]`. `machine-overridable`. |
| `pascalLsp.serverEnv` | Extra environment variables for the server; default `{}`. `machine-overridable`. |
| `pascalLsp.trace.server` | LSP message tracing (`off`, `messages`, or `verbose`); default `off`. |
| `pascalLsp.statusBar.enabled` | Show the Pascal status bar item; default `true`. |
| `pascalLsp.projectFile` | Workspace-relative Delphi `.dproj` to use; unset uses the server default (nearest unambiguous project). |
| `pascalLsp.buildConfig` | Build configuration, for example `Debug`; unset uses the server default. |
| `pascalLsp.platform` | Target platform, for example `Win32`; unset uses the server default. |
| `pascalLsp.sourcePaths` | Additional source directories, such as Delphi RTL/VCL or third-party libraries; unset uses the server default. Relative paths are workspace-relative. |
| `pascalLsp.exclude` | Glob patterns excluded from the workspace; unset uses the server default. |
| `pascalLsp.compilerVersion` | Compiler version for conditional compilation, for example `24.0`; unset uses the server default. |
| `pascalLsp.compilerOptions` | Compiler switch states for conditionals; unset uses the server default. |
| `pascalLsp.conditionalDefines` | Additional conditional defines; unset uses the server default. |
| `pascalLsp.conditionalUndefines` | Conditional symbols known to be undefined; unset uses the server default. |
| `pascalLsp.conditionalConstants` | Constants available to `{$IF}` expressions; unset uses the server default. |
| `pascalLsp.maxFiles` | Workspace file limit; unset uses the server default. Values above the server cap are clamped. |
| `pascalLsp.maxFileBytes` | Per-file byte limit; unset uses the server default. |
| `pascalLsp.maxTotalBytes` | Total workspace byte limit; unset uses the server default. |
| `pascalLsp.maxCacheBytes` | Analysis memory budget in bytes; unset uses the server default. |
| `pascalLsp.maxWorkspaceUnits` | Maximum units in one workspace snapshot; unset uses the server default. |
| `pascalLsp.persistentCacheEnabled` | Enable the persistent analysis cache; unset uses the server default (enabled). |
| `pascalLsp.persistentCachePageBytes` | SQLite page-cache size in bytes; unset uses the server default. |
| `pascalLsp.persistentCacheScratchBytes` | Transient encode/decode buffer size in bytes; unset uses the server default. |
| `pascalLsp.persistentCacheWaitMs` | Longest cache lock wait per operation, in milliseconds; unset uses the server default. |
| `pascalLsp.persistentCacheDirectory` | Absolute cache directory; unset uses the platform directory. `machine-overridable`; **Startup only**. `~` is not expanded. |
| `pascalLsp.persistentCacheMaxNamespaces` | Number of build namespaces retained; unset uses the server default. **Startup only**. |

## Commands

Open the Command Palette and search for **Pascal**. Commands that depend on
server capabilities are available when the installed server advertises them.

| VS Code command | Neovim equivalent |
| --- | --- |
| Pascal: Restart Server | No direct equivalent in the Neovim helper |
| Pascal: Show Output | `:lua print(vim.lsp.log.get_filename())` locates the Neovim LSP log |
| Pascal: Show Server Status | No direct equivalent in the Neovim helper |
| Pascal: Refresh Server Status | No direct equivalent in the Neovim helper |
| Pascal: Select Project… | `:PascalProject` or `<leader>wp` |
| Pascal: Browse Repository Projects | Browse repository from the project picker |
| Pascal: Select Build Configuration | `:PascalBuildConfig` |
| Pascal: Select Platform | `:PascalPlatform` |
| Pascal: Select Delphi Installation | `:PascalDelphiVersion` |

## Status bar

The Pascal status item appears when a Pascal editor is active. Its namespace
icon is followed by the selected project and, when available, build
configuration/platform. A leading `$(error)` means the server is stopped or
failed; `$(sync~spin)` means it is starting or indexing; `$(warning)` indicates
an ambiguous/invalid selection or a bypassed cache. No leading severity icon
means no such state is currently reported. Click the item to open the project
menu.

## Compiled views

Definitions in compiled units can open in `lint4d-dcu:` documents. These views
are read-only and expose document symbols in VS Code's Outline view. A view
that is no longer current displays a stale-view message; navigate to it again
from the importing source to request a fresh view.

## Workspace trust

Untrusted workspaces are not supported. Workspace settings can override
`pascalLsp.serverPath` and launch a workspace-selected executable. The server
reads Delphi/MSBuild project metadata but does not execute project targets;
only open workspaces whose settings and files you trust.

## Troubleshooting

- Run **Pascal: Show Output** to inspect server startup errors and stderr.
- Set `pascalLsp.trace.server` to `messages` or `verbose` to inspect LSP
  traffic in the Pascal LSP output channel.
- Run **Pascal: Show Server Status** for the current project, index, cache,
  server binary, and any status-request errors.
- Run **Pascal: Restart Server** after changing a startup-only setting or
  replacing the server binary.
- If the server cannot start, verify `pascalLsp.serverPath`,
  `PASCAL_LSP_BIN`, or `PATH` resolves to a file for the current OS.

## Development

From `editors/vscode/`:

```sh
npm run watch
npm test
npm run test:grammar
xvfb-run -a npm run test:integration
```

The integration test launches VS Code and requires a built `pascal-lsp`
server. Build it from the repository root first, or set `PASCAL_LSP_BIN` to an
existing binary.
