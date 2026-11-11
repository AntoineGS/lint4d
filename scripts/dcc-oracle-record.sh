#!/usr/bin/env bash
# Record dcc's diagnostics for the dcc-oracle fixtures (dev-only: needs Wine
# and Delphi 13). Usage: scripts/dcc-oracle-record.sh [case ...]
set -Eeuo pipefail
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cases_dir="$root/crates/pascal-lsp/tests/dcc_oracle"
prefix="${DCC_ORACLE_WINEPREFIX:-$HOME/.local/share/Headless-Delphi/state/wine}"
delphi="${DCC_ORACLE_DELPHI:-$HOME/.local/share/Headless-Delphi/files/Studio/37.0}"
# Wine silently creates a missing prefix; refuse instead.
[[ -d "$prefix/drive_c" ]] || { echo "Wine prefix not found: $prefix" >&2; exit 2; }
dcc="$delphi/bin/dcc32.EXE"
[[ -f $dcc ]] || dcc="$delphi/bin/dcc32.exe"
[[ -f $dcc ]] || { echo "dcc32 not found under $delphi/bin" >&2; exit 2; }
winpath() { printf 'Z:%s' "$(printf '%s' "$1" | tr / '\\')"; }
lib="$(winpath "$delphi/lib/win32/release")"
if (($#)); then
  cases=("$@")
else
  mapfile -t cases < <(find "$cases_dir" -mindepth 1 -maxdepth 1 -type d -printf '%f\n' | sort)
fi
status=0
for name in "${cases[@]}"; do
  dir="$cases_dir/$name"
  unit=$(sed -nE 's/^unit *= *"([^"]+)".*/\1/p' "$dir/case.toml")
  if [[ -z $unit ]]; then echo "$name: case.toml has no unit" >&2; status=1; continue; fi
  out=$(mktemp -d)
  raw=$(cd "$dir" && WINEPREFIX="$prefix" WINEDEBUG=-all timeout 300 \
    wine "$dcc" -B -H+ -W+ -Q -U"$lib" -NSSystem -N0"$(winpath "$out")" -E"$(winpath "$out")" "$unit" \
    2>&1 | tr -d '\r') || true
  rm -rf "$out"
  version=$(grep -m1 '^Embarcadero Delphi' <<<"$raw" || true)
  if [[ -z $version ]] || grep -qE '(Fatal|Error): F[0-9]{4}' <<<"$raw"; then
    echo "$name: dcc did not run cleanly, nothing recorded:" >&2
    printf '%s\n' "$raw" >&2
    status=1
    continue
  fi
  {
    echo "# $version"
    echo "# dcc32 -B -H+ -W+ -Q -U<lib\\win32\\release> -NSSystem $unit"
    grep -E '^[^ ].*\([0-9]+\) (Error|Warning|Hint): [EWH][0-9]{4} ' <<<"$raw" \
      | grep -v ' H2675 ' \
      | sed -E 's/^(.*[\\/])?([^\\/(]+)\(([0-9]+)\) (Error|Warning|Hint): /\2(\3) /' || true
  } > "$dir/expected.dcc"
  echo "$name: recorded $(grep -vc '^#' "$dir/expected.dcc" || true) message(s)"
done
exit "$status"
