# dcc surface table format

Each checked-in table describes facts probed from one Delphi compiler and
target. The UTF-8 text format is deliberately small and contains names and
source hashes only; it does not contain Embarcadero source text.

## Deriving the checked-in tables

The developer-only script `scripts/dcc-surface-derive.sh <version> <platform>`
uses the installed Delphi compilers through Wine. It supports `21.0 Win32`,
`24.0 Win32`, `24.0 Win64`, `37.0 Win32`, and `37.0 Win64`; there is no 21.0
Win64 table. It expects the compiler installs under
`~/.local/share/Headless-Delphi/files` and the Wine prefix at
`~/.local/share/Headless-Delphi/state/wine`. Like
`scripts/dcc-oracle-record.sh`, it refuses to run if the Wine prefix's
`drive_c` directory is missing rather than letting Wine create a new prefix.
Set `DCC_ORACLE_WINEPREFIX` to select a different existing prefix.

| Version | Install | Win32 compiler | Win64 compiler | Library path | RTL source root |
|---|---|---|---|---|---|
| 21.0 | `RAD Studio/7.0` | `bin/dcc32.exe` | — | `lib` | `source/Win32` |
| 24.0 | `RAD Studio/10.0` | `bin/dcc32.exe` | `bin/dcc64.exe` | `lib/<platform>/release` | `source` |
| 37.0 | `Studio/37.0` | `bin/dcc32.EXE` | `bin/dcc64.exe` | `lib/<platform>/release` | `source` |

`<platform>` in the library path is `win32` or `win64`. The RTL files are
`<RTL source root>/rtl/sys/System.pas` and `SysInit.pas`. The script adds
`-NSSystem` for 24.0 and 37.0.

For example, regenerate 37.0 Win32 with:

```bash
scripts/dcc-surface-derive.sh 37.0 Win32
```

Run it once for each supported version/platform pair to regenerate all five
tables. To verify reproducibility, save the existing file, regenerate that
key, then compare the bytes:

```bash
cp crates/pascal-lsp/src/navigation/dcc_surface/37.0-Win32.txt /tmp/opencode/37.0-Win32.txt
scripts/dcc-surface-derive.sh 37.0 Win32
cmp /tmp/opencode/37.0-Win32.txt crates/pascal-lsp/src/navigation/dcc_surface/37.0-Win32.txt
```

The candidate set combines identifier tokens from `System.pas` and
`SysInit.pas`, ASCII and UTF-16 strings from the install's `bin/dcc*.exe` and
`bin/dcc*.dll` files, and uppercase quoted literals in
`crates/pascal-project/src/compiler_defines.rs`. Candidates are deduplicated
case-insensitively; Delphi reserved words and the probe's own identifiers are
excluded. The script invokes the `dcc_surface_table` example to write the
sorted output and re-parse it before accepting the table.

Surface candidates are compiled as one statement per line in batches of at
most 60. A name is **absent** only when dcc emits the exact E2003 diagnostic
for that name on its corresponding line. If a batch reports E2226, any fatal
error, or an exit status other than 0 or 1, it is split and retried; a
single-candidate batch that still fails is kept as present. Every other
outcome—including a different diagnostic or a line dcc did not reach—also
counts as present. This present-by-default rule prevents uncertain probes
from introducing false undeclared-identifier findings. Define probes emit one
`{$IFDEF}` hint per candidate; only names with an observed hint are recorded.
If a define-probe batch errors, it is split, and generation stops if a
single-candidate probe still errors.

Root-type member probes use `System.pas` identifiers only. `TObject` and
`TInterfacedObject` are tested through both the type name and `Self` inside a
descendant method; `IInterface` and `IUnknown` are tested through a local
interface variable. These probes use no more than 30 candidates (60 reference
lines) per compile. A member is recorded if any reference form lacks an exact
line-specific E2003.

## Table contents

The tables contain identifier and conditional-symbol names only, plus the
compiler banner, derivation script version, candidate count, and BLAKE3 hashes
of the exact `System.pas` and `SysInit.pas` inputs. The sections describe the
implicit System/SysInit/compiler surface, dcc-defined conditionals, and
observed members of `TObject`, `IInterface`, `IUnknown`, and
`TInterfacedObject`. No RTL source text or compiler binary content is copied
into the tables.

```text
# dcc-surface v1
# banner: Embarcadero Delphi for Win32 compiler version 37.0
# script-version: 1
# system-blake3: <64 lowercase hexadecimal characters>
# sysinit-blake3: <64 lowercase hexadecimal characters>
# candidates: <non-negative integer>
[surface]
<name>
[defines]
<symbol>
[members TObject]
<member>
[members IInterface]
<member>
[members IUnknown]
<member>
[members TInterfacedObject]
<member>
```

The six header lines and six sections must appear exactly once and in this
order. A section may be empty. Each entry is one Pascal identifier matching
`[A-Za-z_][A-Za-z0-9_]{0,62}`. Entries are sorted by ASCII-lowercase spelling;
case-insensitive duplicates are invalid. The original spelling is retained in
the file. Hashes are lowercase BLAKE3 hex digests of the exact `System.pas` and
`SysInit.pas` bytes used to derive the table.

The registry is populated with `include_str!` entries by the derivation step.
An absent or invalid table is unavailable at runtime, so consumers must remain
fail-closed for that compiler version and platform.
