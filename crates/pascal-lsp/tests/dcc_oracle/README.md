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
A case may instead set `recall_exempt = ["W1000"]` with a `reason` to waive
recall for only the listed codes; all other enabled codes in that case still
have recall enforced. `recall = false` remains a case-wide recall waiver.
A case may set `silent = ["E2065"]` to list codes lint4d must not report for
that case, even when dcc recorded the same code. Silent findings are removed
from dcc's expected side, but lint4d's actual findings are still checked, so any
such output fails precision. Use this only when an incomplete or unknown
semantic model makes even a matching dcc diagnostic unsafe. The recording has
`#` header lines followed by normalized messages in the form
`<File>.pas(<line>) <code> <message>`.

A case may set `unindexed_units = ["Provider.pas"]` to keep a sibling Pascal
source available to the dcc recorder while deliberately omitting it from the
navigation index. This models an import whose provider is not indexed.

A case may contain one sibling `.dproj`. Optional `config = "Debug"` and
`platform = "Win32"` entries in `case.toml` select its build configuration and
platform; when omitted, the project's evaluated defaults are used. The oracle
loads that project through `pascal-project`, retaining the selected compiler's
closed conditional context and taking project warning properties from the
selected `.dproj` configuration. The recorder applies those same configured
warning properties to its dcc invocation. It evaluates root-level
`PropertyGroup`s and property `Condition` comparisons with `Config` and
`Platform` plus `and`/`or`; unsupported conditions, nested property groups, and
unevaluable warning properties fail recording instead of silently guessing.
Imported MSBuild files are not evaluated by the recorder and therefore fail
recording. Cases without a `.dproj` keep the original projectless behavior.

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

Cases may set `unit_scopes = ["Vcl"]` to add compiler unit scopes alongside
`System` on 24.0 and 37.0; 21.0 keeps its unscoped VCL unit names.

Unused-unit fixtures set `unused_unit_check = ["UnitName", ...]`. For each
listed unit, the recorder compiles the original case and a temporary full copy
with that entry removed using the same conservative edit policy as the quick
fix. It compares the sorted multiset of `<file> <code> <message>` diagnostics,
ignoring shifted line numbers, and writes one result per unit to
`unused_unit_check.<version>.txt`: `<UnitName> identical` or
`<UnitName> differs: <first difference>`. It also persists the edited compiler
output as `unused_unit_check.<version>.<UnitName>.dcc`, in the same normalized
format as `expected*.dcc`, with a header naming the removed unit. Do not
hand-edit these files. The oracle harness compares each persisted edited output
to `expected*.dcc` and requires the `identical`/`differs` result to agree with
that comparison; it also rejects any lint4d finding when removal changes
compiler output, and requires an unused-unit finding for identical output
unless recall is waived with a reason. An `expect_silent_reason` inline table
names units that must remain silent despite identical compiler output, such as
units with initialization side effects; these still require `recall = false`
and a case-level `reason`.

Usage output from dcc32 21.0, 24.0, and 37.0 (run with no arguments) reports
`-W[+|-|^][warn_id]`; all three versions accept `-W-<ID>`, `-W+<ID>`, and
`-W^<ID>` for a warning property. The same usage output lists `-H` as the hint
output switch; `-H+` and `-H-` are accepted to enable/disable hints. The
recorder keeps the defaults `-H+ -W+`, then appends configured `.dproj`
switches so project values take precedence.

## Warning and hint switch fixtures

Tier 1B adds 11 switch cases recorded with Delphi 21.0, 24.0, and 37.0. The
recordings agree across all three versions: global warning regions and
per-ID `OFF` suppress only the governed findings, `DEFAULT` restores the
initial state, and `ERROR` reports warning IDs as `E1009`/`E1010` with Error
severity. The `.dproj` cases cover a configuration/platform-specific warning
property and `DCC_Hints=false`.

The paired H2219 case settles its governing position: dcc uses the hint state
at the end of the unit, so turning hints off before `end.` suppresses H2219
even when the private field precedes the directive. The dcc recording confirms
that a switch inside an expanded include affects following declarations.
The W1002 symbol-switch case records the switch at each symbol use, and the W1005
unit-switch case records the switch at each `uses` entry. Delphi accepts warning
directives between entries in a `uses` clause, so the OFF/ON pair suppresses the
first unit warning and retains the second.
The unexpanded-include case
records dcc's W1009 but sets `recall = false`: the navigation-only oracle
cannot see the include's switch, so lint4d must stay silent after that include
while precision remains enforced. Likewise, a switch in an unknown conditional
leaves the affected state unknown from that point; a direct switch-state test
covers this independently of whether semantic analysis produced a candidate
diagnostic.

Four umbrella-warning cases record `{$WARN ALL OFF}`, `ON` after `OFF`, `ERROR`,
and `DEFAULT` on Delphi 21.0, 24.0, and 37.0. The older compilers reject `ALL`
with E1030 and leave warning/hint states unchanged. Delphi 37.0 applies the
umbrella states to rows with `$WARN` identifiers: OFF suppresses W1000, W1009,
and W1010; ON after OFF restores them; ERROR promotes them to E1000, E1009, and
E1010; DEFAULT restores their initial state. H2219 is recorded in every mode,
so `ALL` does not affect hint rows without a `$WARN` identifier. The switch
derivation script recognizes no other umbrella form. For an unknown compiler
version, an `ALL` directive leaves those warning-ID states unknown and silent.

If the document has no compiler version or its version has no recognized
switch table, warning and hint analysis uses only the per-code defaults,
governing globals, and warning IDs shared by all checked-in tables. Findings
whose switch rows disagree across versions remain withheld. Error diagnostics
are not filtered by warning or hint switches.

The unit test in `src/navigation/dcc_oracle_tests.rs` compares only codes in
`ENABLED_CODES`; add a code there as the corresponding lint rule is implemented.
For enabled codes, precision is always enforced: every lint4d finding must be
present in the selected compiler's recording, and silent codes must have no
lint4d findings. Recall is also enforced unless `requires_rtl = true` and that
compiler's RTL variable is unset, `recall = false` is set with a reason, or the
code appears in `recall_exempt` with a reason.
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

For a fresh LSP run, `--verify-unused-units` recompiles each
`lint4d-unused-unit` finding after removing its `uses` entry from a temporary
source copy. The edited copy is first on DCC's unit search path, compiler outputs
stay in temporary directories, and the report compares the original and edited
diagnostic multisets while ignoring line shifts. Findings are marked `verified`
when the diagnostics are identical and `false-positive` with the first
difference otherwise; a false positive makes the command exit nonzero.
`--verify-unused-units` requires `--lsp <binary>` and can be combined with
`--compare`.

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
