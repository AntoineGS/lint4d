# dcc-oracle fixtures

Each directory is a small Delphi case containing one or more `.pas` units, any
matching text resources such as `.dfm`, a `case.toml`, and an `expected.dcc`
recording. `case.toml` requires `unit =
"<file>.pas"` to name the unit analysed. It may also set `requires_rtl = true`
when compiling or analysing the case needs Delphi's RTL, and `reason = "..."`
to explain that dependency. Cases may set `recall = false` with a `reason` when
lint4d must conservatively stay silent because the semantic model cannot prove a
recorded dcc finding; this skips only the "dcc reported, lint4d missed"
comparison, while precision remains enforced. A case may set `silent = ["E2065"]`
to list codes lint4d must not report for that case, even when dcc recorded the
same code. Silent findings are removed from dcc's expected side, but lint4d's
actual findings are still checked, so any such output fails precision. Use this
when an incomplete or unknown semantic model makes even a matching dcc
diagnostic unsafe. The recording has `#` header lines followed by normalized
messages in the form
`<File>.pas(<line>) <code> <message>`.

The recorder compiles the unit with sibling resources such as `.dfm`; the
semantic oracle indexes only `.pas` files. A DFM event binding is therefore not
counted as a Pascal identifier use by the H2219 scan, and its behavior must be
characterized by the recorded compiler output.

Regenerate compiler output with `scripts/dcc-oracle-record.sh [case ...]` from
the repository root. With no case names it records every case. The script uses
Wine and Delphi 13, defaults to the Headless-Delphi Wine prefix and Delphi root,
and accepts `DCC_ORACLE_WINEPREFIX` and `DCC_ORACLE_DELPHI` overrides. It
refuses to run when the Wine prefix is missing because Wine would otherwise
create an empty one. Never hand-edit `expected.dcc`: it is the compiler's
recording, not a hand-authored expectation.

The unit test in `src/navigation/dcc_oracle_tests.rs` compares only codes in
`ENABLED_CODES`; add a code there as the corresponding lint rule is implemented.
For enabled codes, precision is always enforced: every lint4d finding must be
present in dcc's recording, and silent codes must have no lint4d findings.
Recall is also enforced unless `requires_rtl = true` and `PASCAL_RTL` is unset,
or `recall = false` is set with a reason. Cases without an `expected.dcc` fail
and must be recorded before they can participate in the test.

To enforce recall for RTL-dependent fixtures, set `PASCAL_RTL` to the Delphi
source root (the directory containing `rtl/sys/System.pas`):

```sh
PASCAL_RTL="$HOME/.local/share/Headless-Delphi/files/Studio/37.0/source" \
  cargo test -p pascal-lsp --lib dcc_oracle
```

When set, the oracle test loads `rtl/sys/System.pas` into the navigation index
for each `requires_rtl = true` case and binds the case units to it, allowing
source-backed resolution of `TObject`, `TInterfacedObject`, and `IInterface`.
