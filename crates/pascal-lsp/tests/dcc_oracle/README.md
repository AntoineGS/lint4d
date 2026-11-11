# dcc-oracle fixtures

Each directory is a small Delphi case containing one or more `.pas` units, a
`case.toml`, and an `expected.dcc` recording. `case.toml` requires `unit =
"<file>.pas"` to name the unit analysed. It may also set `requires_rtl = true`
when compiling or analysing the case needs Delphi's RTL, and `reason = "..."`
to explain that dependency. Cases may set `recall = false` with a `reason` when
lint4d must conservatively stay silent because the semantic model cannot prove a
recorded dcc finding; this skips only the "dcc reported, lint4d missed"
comparison, while precision remains enforced. The recording has `#` header
lines followed by normalized messages in the form
`<File>.pas(<line>) <code> <message>`.

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
present in dcc's recording. Recall is also enforced unless `requires_rtl = true`
and `PASCAL_RTL` is unset, or `recall = false` is set with a reason. Cases
without an `expected.dcc` fail and must be recorded before they can participate
in the test.
