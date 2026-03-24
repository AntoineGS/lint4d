# lint4d

A fast, configurable static analysis tool (linter) for Delphi (Object Pascal), powered by tree-sitter.

## Features

- 12 built-in rules covering resource management, exception handling, naming conventions, and dangerous patterns
- DCU-aware analysis for type-level checks (field leaks, reassignment leaks)
- Auto-fix for naming violations (`--fix-fmt`)
- Parallel file processing via rayon
- Baseline suppression to silence pre-existing violations
- Inline suppression directives in source comments
- `.dproj` project file support
- BDS (RAD Studio) installation auto-discovery
- Output in text or JSON format
- Configurable severity thresholds and per-rule overrides

## Installation

```sh
cargo install --path .
```

Requires Rust 2021 edition or later.

## Quick Start

Lint a single file or directory:

```sh
lint4d src/
lint4d MyUnit.pas
```

Lint from a Delphi project file:

```sh
lint4d --project MyApp.dproj
```

List all available rules:

```sh
lint4d --list-rules
```

Explain a specific rule:

```sh
lint4d --explain resource-leak-unprotected
```

Output violations as JSON:

```sh
lint4d --format json src/
```

Fail the process only on errors (ignore warnings and hints):

```sh
lint4d --fail-on error src/
```

## Configuration

Generate a default configuration file:

```sh
lint4d --init
```

This creates `.lint4d.toml` in the current directory. Example configuration:

```toml
[rules]
# Override severity for a rule, or disable it entirely
bare-except = "error"
type-prefix = "off"

[dcu]
paths = ["build/dcu/Win64/Release"]

[output]
format = "text"
color = true
```

## Rules

| Rule ID | Category | Default Severity | Description |
|---|---|---|---|
| `resource-leak-unprotected` | Resource Management | error | Resources created without a `try..finally` block |
| `resource-leak-no-try` | Resource Management | warning | Resources assigned but not freed in `finally` |
| `field-not-freed` | Resource Management | warning | Class fields not freed in destructor [DCU] |
| `field-reassign-leak` | Resource Management | warning | Fields reassigned without freeing the prior value [DCU] |
| `empty-except` | Exception Handling | warning | Empty exception handlers that swallow errors |
| `bare-except` | Exception Handling | warning | Bare `except` blocks without re-raising |
| `type-prefix` | Naming Convention | hint | Types should use the `T` prefix |
| `interface-prefix` | Naming Convention | hint | Interfaces should use the `I` prefix |
| `constant-naming` | Naming Convention | hint | Constants should follow naming conventions |
| `local-variable-naming` | Naming Convention | hint | Local variables should follow naming conventions |
| `identifier-casing` | Naming Convention | hint | Identifier casing should be consistent |
| `with-statement` | Dangerous Pattern | warning | `with` statements are error-prone and should be avoided |

Rules marked **[DCU]** require DCU paths to be configured for type-aware analysis.

## Suppression Directives

Violations can be suppressed inline using compiler-style directives:

```pascal
// Suppress a specific rule on this line
Obj := TFoo.Create; {$LINT.OFF resource-leak-unprotected}

// Suppress with a reason
Obj := TFoo.Create; {$LINT.OFF resource-leak-unprotected - intentional singleton}

// Suppress all rules on this line
Obj := TFoo.Create; {$LINT.OFF *}
```

To suppress pre-existing violations across the entire codebase, generate a baseline file:

```sh
lint4d --generate-baseline src/
```

This creates `.lint4d-baseline.json`. Violations recorded in the baseline are silenced in subsequent runs, allowing teams to adopt lint4d incrementally without fixing all existing issues at once.

## Auto-Fix

The `--fix-fmt` flag automatically corrects naming violations in place:

```sh
lint4d --fix-fmt src/
```

Only naming convention rules (`type-prefix`, `interface-prefix`, `constant-naming`, `local-variable-naming`, `identifier-casing`) are auto-fixable. All other rule categories require manual remediation.

## DCU Support

For rules that require type information (`field-not-freed`, `field-reassign-leak`), provide the directory containing compiled DCU files:

```sh
lint4d --dcu-path build/dcu/Win64/Release src/
```

The `--dcu-path` flag can be repeated to specify multiple directories. When using `--project`, DCU paths are read from the `.dproj` file automatically. You can also override the target platform and build configuration:

```sh
lint4d --project MyApp.dproj --platform Win64 --build-config Release
```

If RAD Studio is installed, lint4d can locate the BDS root automatically. You can override this with `--bds-path`.

## License

MIT. See [LICENSE](LICENSE) for details.
