# dcc surface table format

Each checked-in table describes facts probed from one Delphi compiler and
target. The UTF-8 text format is deliberately small and contains names and
source hashes only; it does not contain Embarcadero source text.

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
