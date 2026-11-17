#!/usr/bin/env bash
# Record dcc's diagnostics for the dcc-oracle fixtures (dev-only: needs Wine
# and a supported Delphi install). Usage: scripts/dcc-oracle-record.sh [--compiler <version>] [case ...]
set -Eeuo pipefail
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cases_dir="$root/crates/pascal-lsp/tests/dcc_oracle"
prefix="${DCC_ORACLE_WINEPREFIX:-$HOME/.local/share/Headless-Delphi/state/wine}"
compiler=37.0
cases=()
while (($#)); do
  case "$1" in
    --compiler)
      (($# >= 2)) || { echo "--compiler requires a version" >&2; exit 2; }
      compiler="$2"
      shift 2
      ;;
    --compiler=*)
      compiler="${1#--compiler=}"
      shift
      ;;
    -h|--help)
      sed -n '2,3p' "${BASH_SOURCE[0]}"
      exit 0
      ;;
    --*)
      echo "unknown option: $1" >&2
      exit 2
      ;;
    *)
      cases+=("$1")
      shift
      ;;
  esac
done

case "$compiler" in
  21.0)
    default_delphi="$HOME/.local/share/Headless-Delphi/files/RAD Studio/7.0"
    lib_suffix=lib
    lib_header=lib
    use_unit_scopes=false
    ;;
  24.0)
    default_delphi="$HOME/.local/share/Headless-Delphi/files/RAD Studio/10.0"
    lib_suffix=lib/win32/release
    lib_header='lib\win32\release'
    use_unit_scopes=true
    ;;
  37.0)
    default_delphi="$HOME/.local/share/Headless-Delphi/files/Studio/37.0"
    lib_suffix=lib/win32/release
    lib_header='lib\win32\release'
    use_unit_scopes=true
    ;;
  *)
    echo "unsupported compiler version: $compiler (expected 21.0, 24.0, or 37.0)" >&2
    exit 2
    ;;
esac
delphi="${DCC_ORACLE_DELPHI:-$default_delphi}"
# Wine silently creates a missing prefix; refuse instead.
[[ -d "$prefix/drive_c" ]] || { echo "Wine prefix not found: $prefix" >&2; exit 2; }
dcc="$delphi/bin/dcc32.EXE"
[[ -f $dcc ]] || dcc="$delphi/bin/dcc32.exe"
[[ -f $dcc ]] || { echo "dcc32 not found under $delphi/bin" >&2; exit 2; }
winpath() { printf 'Z:%s' "${1//\//\\}"; }
lib="$(winpath "$delphi/$lib_suffix")"
if ((${#cases[@]})); then
  :
else
  mapfile -t cases < <(find "$cases_dir" -mindepth 1 -maxdepth 1 -type d -printf '%f\n' | sort)
fi

case_includes_compiler() {
  local case_file=$1 line compilers
  if ! grep -Eq '^[[:space:]]*compilers[[:space:]]*=' "$case_file"; then
    [[ $compiler == 37.0 ]]
    return
  fi
  line=$(grep -E '^[[:space:]]*compilers[[:space:]]*=' "$case_file" | head -n1)
  [[ $line == *'['*']'* ]] || {
    echo "$case_file: malformed compilers array" >&2
    return 2
  }
  compilers=$(sed -E 's/.*\[([^]]*)\].*/\1/' <<<"$line")
  compilers=${compilers//\"/}
  compilers=${compilers//[[:space:]]/}
  [[ ",$compilers," == *",$compiler,"* ]]
}

status=0
for name in "${cases[@]}"; do
  dir="$cases_dir/$name"
  unit=$(sed -nE 's/^unit *= *"([^"]+)".*/\1/p' "$dir/case.toml")
  if [[ -z $unit ]]; then echo "$name: case.toml has no unit" >&2; status=1; continue; fi
  if case_includes_compiler "$dir/case.toml"; then
    :
  else
    case_status=$?
    if [[ $case_status -eq 2 ]]; then status=1; continue; fi
    echo "$name: skipped (not configured for $compiler)"
    continue
  fi
  mapfile -d '' -t dproj_files < <(find "$dir" -mindepth 1 -maxdepth 1 -type f -iname '*.dproj' -print0)
  if ((${#dproj_files[@]} > 1)); then
    echo "$name: expected at most one .dproj, found ${#dproj_files[@]}" >&2
    status=1
    continue
  fi
  project_switch_flags=()
  if ((${#dproj_files[@]} == 1)); then
    switch_table="$root/crates/pascal-lsp/src/navigation/dcc_switches/$compiler.txt"
    config=$(sed -nE 's/^[[:space:]]*config[[:space:]]*=[[:space:]]*"([^"]*)".*/\1/p' "$dir/case.toml" | head -n1)
    platform=$(sed -nE 's/^[[:space:]]*platform[[:space:]]*=[[:space:]]*"([^"]*)".*/\1/p' "$dir/case.toml" | head -n1)
    if ! switch_output=$(python3 "$root/scripts/dcc-oracle-project-switches.py" \
      "${dproj_files[0]}" "$switch_table" "$config" "$platform"); then
      echo "$name: could not evaluate .dproj warning switches" >&2
      status=1
      continue
    fi
    while IFS= read -r flag; do
      [[ -n $flag ]] && project_switch_flags+=("$flag")
    done <<< "$switch_output"
  fi
  output="$dir/expected.dcc"
  if [[ $compiler != 37.0 ]]; then output="$dir/expected.$compiler.dcc"; fi
  out=$(mktemp -d)
  dcc_args=(-B -H+ -W+ -Q "-U$lib")
  dcc_args+=("${project_switch_flags[@]}")
  if [[ $use_unit_scopes == true ]]; then dcc_args+=(-NSSystem); fi
  dcc_args+=(-N0"$(winpath "$out")" -E"$(winpath "$out")" "$unit")
  if raw=$(cd "$dir" && WINEPREFIX="$prefix" WINEDEBUG=-all timeout 300 \
    wine "$dcc" "${dcc_args[@]}" 2>&1 | tr -d '\r'); then
    compiler_status=0
  else
    # With pipefail, command substitution preserves dcc's 0/1 exit status.
    compiler_status=$?
  fi
  rm -rf "$out"
  case "$compiler_status" in
    0|1) ;;
    124|137)
      echo "$name: dcc timed out (exit $compiler_status), nothing recorded:" >&2
      printf '%s\n' "$raw" >&2
      status=1
      continue
      ;;
    *)
      echo "$name: dcc invocation failed (exit $compiler_status), nothing recorded:" >&2
      printf '%s\n' "$raw" >&2
      status=1
      continue
      ;;
  esac
  version=$(grep -m1 '^Embarcadero Delphi' <<<"$raw" || true)
  if [[ -z $version || $version != *"compiler version $compiler"* ]] || \
    grep -qE '(Fatal|Error): F[0-9]{4}' <<<"$raw"; then
    echo "$name: dcc did not run cleanly as Delphi $compiler, nothing recorded:" >&2
    printf '%s\n' "$raw" >&2
    status=1
    continue
  fi
  if [[ $compiler_status -eq 1 ]] && ! grep -qE 'Error: E[0-9]{4} ' <<<"$raw"; then
    echo "$name: dcc exited 1 without a compiler error, nothing recorded:" >&2
    printf '%s\n' "$raw" >&2
    status=1
    continue
  fi
  {
    echo "# $version"
    header="# dcc32 -B -H+ -W+ -Q -U<$lib_header>"
    for flag in "${project_switch_flags[@]}"; do header+=" $flag"; done
    if [[ $use_unit_scopes == true ]]; then header+=" -NSSystem"; fi
    echo "$header $unit"
    grep -E '^[^ ].*\([0-9]+\) (Error|Warning|Hint): [EWH][0-9]{4} ' <<<"$raw" \
      | grep -v ' H2675 ' \
      | sed -E 's/^(.*[\\/])?([^\\/(]+)\(([0-9]+)\) (Error|Warning|Hint): /\2(\3) /' || true
  } > "$output"
  echo "$name: recorded $(grep -vc '^#' "$output" || true) message(s) for Delphi $compiler"
done
exit "$status"
