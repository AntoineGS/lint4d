# dcc-oracle fixtures

Each directory is a small Delphi case containing one or more `.pas` units, any
matching text resources such as `.dfm`, a `case.toml`, and one or more dcc
recordings. `case.toml` requires `unit = "<file>.pas"` to name the unit
analysed. It may set `compilers = ["21.0", "24.0"]`; an omitted key means
`["37.0"]`. The selected compiler's Win32 closed conditional context is used
when indexing every Pascal unit in the case. The compiler's recording is
`expected.dcc` for 37.0 and `expected.<version>.dcc` for 21.0 and 24.0.

Cases may also set `requires_rtl = true` when compiling or analysing the case
needs Delphi's RTL, and `reason = "..."` to explain that dependency. Cases may
set `recall = false` with a `reason` when lint4d must conservatively stay silent
because the semantic model cannot prove a recorded dcc finding; this skips only
the "dcc reported, lint4d missed" comparison, while precision remains enforced.
A case may set `silent = ["E2065"]` to list codes lint4d must not report for
that case, even when dcc recorded the same code. Silent findings are removed
from dcc's expected side, but lint4d's actual findings are still checked, so any
such output fails precision. Use this only when an incomplete or unknown
semantic model makes even a matching dcc diagnostic unsafe. The recording has
`#` header lines followed by normalized messages in the form
`<File>.pas(<line>) <code> <message>`.

The recorder compiles the unit with sibling resources such as `.dfm`; the
semantic oracle indexes only `.pas` files. A DFM event binding is therefore not
counted as a Pascal identifier use by the H2219 scan, and its behavior must be
characterized by the recorded compiler output.

Regenerate compiler output with
`scripts/dcc-oracle-record.sh --compiler <version> [case ...]` from the
repository root. `--compiler` defaults to 37.0. With no case names it scans
every case but records only those configured for the selected compiler; cases
without `compilers` are configured for 37.0 only. The script uses Wine and
defaults to these Headless-Delphi installs:

| Version | Install | Compiler | Library path |
| --- | --- | --- | --- |
| 21.0 | `RAD Studio/7.0` | `bin/dcc32.exe` | `lib` |
| 24.0 | `RAD Studio/10.0` | `bin/dcc32.exe` | `lib/win32/release` |
| 37.0 | `Studio/37.0` | `bin/dcc32.exe` | `lib/win32/release` |

The recorder uses `-NSSystem` only for 24.0 and 37.0. It defaults to the
Headless-Delphi Wine prefix and accepts `DCC_ORACLE_WINEPREFIX` and
`DCC_ORACLE_DELPHI` overrides. It refuses to run when the Wine prefix is
missing because Wine would otherwise create an empty one. Never hand-edit an
`expected*.dcc` file: it is the compiler's recording, not a hand-authored
expectation.

The unit test in `src/navigation/dcc_oracle_tests.rs` compares only codes in
`ENABLED_CODES`; add a code there as the corresponding lint rule is implemented.
For enabled codes, precision is always enforced: every lint4d finding must be
present in the selected compiler's recording, and silent codes must have no
lint4d findings. Recall is also enforced unless `requires_rtl = true` and that
compiler's RTL variable is unset, or `recall = false` is set with a reason.
The test prints one line per enabled code with its total recorded `expected`,
exact `matched`, `invented`, enforced `missed`, and recall `waived` counts.
`waived` counts recorded findings whose recall was not enforced; it can overlap
`matched` when a waived finding happened to match. Cases without the selected
compiler's recording fail and must be recorded before they can participate in
the test.

To enforce recall for RTL-dependent fixtures, set the corresponding
`PASCAL_RTL_<major>_<minor>` to the Delphi source root (the directory
containing `rtl/sys/System.pas`). The 37.0 setting accepts `PASCAL_RTL` as an
alias. The 21.0 root is the `source/Win32` directory; 24.0 and 37.0 use their
`source` directories.

| Variable | RTL source root |
| --- | --- |
| `PASCAL_RTL_21_0` | `$HOME/.local/share/Headless-Delphi/files/RAD Studio/7.0/source/Win32` |
| `PASCAL_RTL_24_0` | `$HOME/.local/share/Headless-Delphi/files/RAD Studio/10.0/source` |
| `PASCAL_RTL_37_0` | `$HOME/.local/share/Headless-Delphi/files/Studio/37.0/source` |

```sh
PASCAL_RTL_37_0="$HOME/.local/share/Headless-Delphi/files/Studio/37.0/source" \
  cargo test -p pascal-lsp --lib dcc_oracle
```

When set, the oracle test loads that version's `rtl/sys/System.pas` into the
navigation index for each `requires_rtl = true` case and binds the case units
to it, allowing source-backed resolution of `TObject`, `TInterfacedObject`, and
`IInterface`.

## RTL/VCL E2003 corpus precision check

The ignored developer test indexes every `.pas` file under `rtl/sys`,
`rtl/common`, `rtl/win`, and `vcl`, binds imports by their exact `uses` unit
name (including dotted unit/file stems), and checks that the five supported
Windows keys produce no E2003 findings. Set all three version roots to run all
five keys; `PASCAL_CORPUS_<major>_<minor>` can add another dcc-clean tree for a
version.

```sh
PASCAL_RTL_21_0="$HOME/.local/share/Headless-Delphi/files/RAD Studio/7.0/source/Win32" \
PASCAL_RTL_24_0="$HOME/.local/share/Headless-Delphi/files/RAD Studio/10.0/source" \
PASCAL_RTL_37_0="$HOME/.local/share/Headless-Delphi/files/Studio/37.0/source" \
  cargo test -p pascal-lsp --lib corpus_reports_no_e2003_on_dcc_clean_rtl -- --ignored --nocapture
```

The report prints per-key file counts, skipped and lossily decoded files,
E2003 locations, resolved/proven-absent proof counts, and table-backed implicit
System decisions. `System.Internal.Unwinder` is reported as skipped on these
Windows keys because its unwinder library binding is only defined for Linux or
macOS.

## Real-project corpus

The developer-only corpus manifest defaults to
`~/.config/lint4d/corpus.toml` (override with `LINT4D_CORPUS_MANIFEST`). Outputs
are kept outside the checkout under `~/.cache/lint4d-corpus/` (override with
`LINT4D_CORPUS_OUT`). Each manifest entry names a project with `name` and
`dproj`; optional `config` and `platform` select the Delphi build configuration.
Never put private project names or source paths in repository files.
An optional `[lsp]` table can set `initialization_options_json` to a JSON string;
the corpus driver passes that value to the server, and uses the Neovim
`sourcePaths` defaults only when the manifest omits it.

Build the server in release mode, then run the DCC, LSP, and comparison steps:

```sh
cargo build --release -p pascal-lsp
scripts/dcc-corpus.sh --lsp "$PWD/target/release/pascal-lsp" --compare
```

The script resolves output and temporary paths through symlinks and refuses
destinations inside any project checkout or nearest LSP workspace root. It runs
DCC under the configured Headless-Delphi Wine prefix and writes temporary
compiler outputs and recorded results outside those roots. `--lsp <binary>`
starts the real server with `--stdio`, the manifest's initialization options
(or the Neovim `sourcePaths` defaults when none are present), and the nearest
`.lint4d.toml` or `.git` root. An optional `--init-options <json-file>` can
override those options for a direct driver invocation. `--compare` can be run
later against previously collected LSP findings, but refuses cached diagnostics
when their run fingerprint no longer matches the command, project file contents,
initialization options, or server binary path/mtime:

```sh
scripts/dcc-corpus.sh --compare <manifest-entry>
```

For one project, add its manifest `name` after the options. Set
`LINT4D_LSP_TIMEOUT` to adjust the default 600-second index-settle limit. The
script exits nonzero if indexing times out; the comparison report marks parity
counts unavailable and lists the project under the timeout exclusion. DCC runs
with compile errors or a nonzero exit are recorded as `excluded: dcc errors (N)`
and have no parity counts because DCC may stop before reporting later findings.
Files that the LSP driver could not analyse are listed but omitted from all
parity counts. The per-project `report.md` lists per-code match, invented, and missed counts,
precision and recall percentages (`—` when a denominator is zero), non-DCC
diagnostics, include-located DCC messages, and files the server could not
analyse; examples use only source basenames and line numbers.
