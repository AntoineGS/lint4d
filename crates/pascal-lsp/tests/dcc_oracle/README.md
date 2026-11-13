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
