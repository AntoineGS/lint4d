# dcc warning-switch tables

These checked-in tables record the warning and hint switches observed from
Delphi Win32 `dcc32` compilers 21.0, 24.0, and 37.0. The switch identifiers
and defaults are compiler-derived, not inferred from diagnostic names.

## Regenerating

The developer-only script invokes the installed compilers through Wine:

```bash
scripts/dcc-switch-derive.sh 37.0
scripts/dcc-switch-derive.sh 21.0
scripts/dcc-switch-derive.sh 24.0
```

Run the isolated probe-classifier regression tests with
`python3 scripts/tests/test_dcc_switch_probe.py`.

It expects installs beneath
`~/.local/share/Headless-Delphi/files` and the existing Wine prefix at
`~/.local/share/Headless-Delphi/state/wine`. Set `DCC_ORACLE_WINEPREFIX` to a
different existing prefix. Like the other dcc oracle scripts, it refuses to
create a prefix when `drive_c` is missing.

For each code, the script compiles its trigger without warning directives to
record the default, then uses `{$WARNINGS ON}` / `{$HINTS ON}` followed by each
global OFF directive to identify the governing global. Triggers cover
deprecated, library, platform, and experimental symbols and units, the tier-0
W1009/W1010 fixtures, the flow H2077/H2135/H2164 and W1035/W1036/W1037
fixtures, the unused-private-field H2219 fixture, and the inline-use H2443
fixture.

Specific-ID candidates come from uppercase attributes on the `DCC` task in
`bin/CodeGear.Delphi.Targets` and uppercase strings in the compiler binaries.
The script preflights those tokens through dcc, because many binary strings are
not valid warning identifiers. It tests accepted IDs in batches of 40 and
bisects a batch only when the trigger diagnostic disappears. Unknown IDs and
invalid compiler-directive tokens are not counted as suppressors. `ALL` is the
only umbrella suppressor recognized by the derivation script; it is recorded
in the derivation log but is not a per-code switch or a `DCC_<ID>` task
property, so each table records the specific controlling ID.

Oracle recordings show that Delphi 21.0 and 24.0 reject `{$WARN ALL ...}` with
E1030 and leave warning and hint switches unchanged, while Delphi 37.0 accepts
it. In 37.0, `ALL OFF`, `ON`, `ERROR`, and `DEFAULT` update every table row
with a `$WARN` identifier; rows without an identifier are unchanged. In
particular, H2219 remains governed by `{$HINTS}`. No other umbrella form was
recognized by the derivation script.

For an identified warning ID, the script compiles once with
`{$WARN <ID> ERROR}` and stores whether dcc promotes that diagnostic to an
error. It accepts a promoted result only when the matching `Error: E####`
diagnostic accompanies dcc's normal error exit status (`1`), and accepts a
non-promoted result only when the matching `Warning: W####` accompanies a
successful exit (`0`). Missing or conflicting diagnostics, unexpected compiler
errors, timeouts, and other exit statuses abort derivation before the table is
replaced. The compiler banner must also match the requested version.

In all three observed versions, promotion changes the printed code from
`W####` to `E####` while preserving the diagnostic text; for example:

```text
W1000 Symbol 'DepMsg' is deprecated: 'use Other'
E1000 Symbol 'DepMsg' is deprecated: 'use Other'
```

H2219 has no per-code `$WARN` identifier in these tables; it is governed by
`{$HINTS}`. `error-capable` is therefore `no` for H2219 and `yes` for the
warning codes.

The script records every task-attribute mapping of the form
`ATTRIBUTE="$(DCC_property)"` as `DCC_property ATTRIBUTE`. For example,
`DCC_SYMBOL_PLATFORM SYMBOL_PLATFORM`, `DCC_Hints Hints`, and
`DCC_Warnings Warnings`.

The runner compiles from a fresh temporary working directory. Compiler-specific
configuration that dcc loads remains part of the environment: the installed
`.cfg` files inspected for these derivations contained alias and unit-search
settings, but no warning or hint switches. Inspect the selected installation's
configuration when deriving from a different setup.

## Platform scope

The tables are derived with Win32 `dcc32`. The warning identifiers are assumed
to be compiler-version-wide and therefore identical for Win64; `dcc64` has not
been separately verified for this task. The tables do not contain Delphi
source or compiler binary contents.

## Format and loader

Each UTF-8 table has a compiler banner, script version, a sorted `[codes]`
section, and a sorted `[properties]` section:

```text
# dcc-switches v1
# banner: Embarcadero Delphi for Win32 compiler version 37.0
# script-version: 1
[codes]
H2219 ON HINTS - no
W1000 ON WARNINGS SYMBOL_DEPRECATED yes
[properties]
DCC_Hints Hints
DCC_SYMBOL_DEPRECATED SYMBOL_DEPRECATED
DCC_Warnings Warnings
```

The code row fields are `<code> <default ON|OFF> <WARNINGS|HINTS>
<ID|-> <error-capable yes|no>`. `SwitchTable::parse` rejects malformed,
duplicate, or unsorted rows with a source line number. `switch_table(major,
minor)` lazily parses the checked-in files and returns `None` for unsupported
versions or invalid table data, so callers fail closed.

When the compiler version is absent or has no checked-in table, warning and
hint analysis uses a consensus table across all valid checked-in versions. A
code is included only when every table agrees on its default, governing global,
warning ID, and error-promotion capability; otherwise that code's state is
unknown and its warning or hint finding is withheld. Property mappings are
likewise retained only when all tables agree. For example, all checked-in
tables agree that W1009 defaults to ON under WARNINGS and is controlled by
`HIDING_MEMBER`, so that default remains usable without a known compiler
version.
