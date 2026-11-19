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

unused_units=()
unit_scopes=()
read_unused_unit_check() {
  local case_file=$1
  local -a lines=()
  local -a entries=()
  mapfile -t lines < <(grep -E '^[[:space:]]*unused_unit_check[[:space:]]*=' "$case_file" || true)
  unused_units=()
  if ((${#lines[@]} == 0)); then
    return 0
  fi
  if ((${#lines[@]} != 1)) || \
    [[ ! ${lines[0]} =~ ^[[:space:]]*unused_unit_check[[:space:]]*=[[:space:]]*\[(.*)\][[:space:]]*$ ]]; then
    echo "$case_file: malformed unused_unit_check array" >&2
    return 1
  fi
  local values=${BASH_REMATCH[1]}
  [[ -n ${values//[[:space:]]/} ]] || {
    echo "$case_file: unused_unit_check must not be empty" >&2
    return 1
  }
  local value unit existing
  IFS=',' read -r -a entries <<< "$values"
  for value in "${entries[@]}"; do
    value=${value//[[:space:]]/}
    if [[ ! $value =~ ^\"([A-Za-z_][A-Za-z0-9_.]*)\"$ ]]; then
      echo "$case_file: invalid unused_unit_check entry: $value" >&2
      return 1
    fi
    unit=${BASH_REMATCH[1]}
    for existing in "${unused_units[@]}"; do
      if [[ ${existing,,} == "${unit,,}" ]]; then
        echo "$case_file: duplicate unused_unit_check entry: $unit" >&2
        return 1
      fi
    done
    unused_units+=("$unit")
  done
}

read_unit_scopes() {
  local case_file=$1
  local -a lines=()
  local -a entries=()
  unit_scopes=()
  mapfile -t lines < <(grep -E '^[[:space:]]*unit_scopes[[:space:]]*=' "$case_file" || true)
  if ((${#lines[@]} == 0)); then
    return 0
  fi
  if ((${#lines[@]} != 1)) || \
    [[ ! ${lines[0]} =~ ^[[:space:]]*unit_scopes[[:space:]]*=[[:space:]]*\[(.*)\][[:space:]]*$ ]]; then
    echo "$case_file: malformed unit_scopes array" >&2
    return 1
  fi
  local values=${BASH_REMATCH[1]}
  [[ -n ${values//[[:space:]]/} ]] || {
    echo "$case_file: unit_scopes must not be empty" >&2
    return 1
  }
  local value scope existing
  IFS=',' read -r -a entries <<< "$values"
  for value in "${entries[@]}"; do
    value=${value//[[:space:]]/}
    if [[ ! $value =~ ^\"([A-Za-z_][A-Za-z0-9_]*)\"$ ]]; then
      echo "$case_file: invalid unit_scopes entry: $value" >&2
      return 1
    fi
    scope=${BASH_REMATCH[1]}
    for existing in "${unit_scopes[@]}"; do
      if [[ ${existing,,} == "${scope,,}" ]]; then
        echo "$case_file: duplicate unit_scopes entry: $scope" >&2
        return 1
      fi
    done
    unit_scopes+=("$scope")
  done
}

compile_dcc() {
  local work_dir=$1
  local source_unit=$2
  local out_dir=$3
  local -a args=(-B -H+ -W+ -Q "-U$lib")
  args+=("${project_switch_flags[@]}")
  if [[ $use_unit_scopes == true ]]; then
    args+=(-NSSystem)
    for scope in "${unit_scopes[@]}"; do args+=("-NS$scope"); done
  fi
  args+=(-N0"$(winpath "$out_dir")" -E"$(winpath "$out_dir")" "$source_unit")
  (
    cd "$work_dir"
    WINEPREFIX="$prefix" WINEDEBUG=-all timeout 300 wine "$dcc" "${args[@]}" 2>&1
  ) | tr -d '\r'
}

validate_dcc_output() {
  local name=$1 raw=$2 compiler_status=$3
  local version
  case "$compiler_status" in
    0|1) ;;
    124|137)
      echo "$name: dcc timed out (exit $compiler_status), nothing recorded:" >&2
      printf '%s\n' "$raw" >&2
      return 1
      ;;
    *)
      echo "$name: dcc invocation failed (exit $compiler_status), nothing recorded:" >&2
      printf '%s\n' "$raw" >&2
      return 1
      ;;
  esac
  version=$(grep -m1 '^Embarcadero Delphi' <<<"$raw" || true)
  if [[ -z $version || $version != *"compiler version $compiler"* ]] || \
    grep -qE '(Fatal|Error): F[0-9]{4}' <<<"$raw"; then
    echo "$name: dcc did not run cleanly as Delphi $compiler, nothing recorded:" >&2
    printf '%s\n' "$raw" >&2
    return 1
  fi
  if [[ $compiler_status -eq 1 ]] && ! grep -qE 'Error: E[0-9]{4} ' <<<"$raw"; then
    echo "$name: dcc exited 1 without a compiler error, nothing recorded:" >&2
    printf '%s\n' "$raw" >&2
    return 1
  fi
}

write_normalized_dcc_record() {
  local destination=$1 raw=$2 unit=$3 removed_unit=${4:-}
  local version header
  version=$(grep -m1 '^Embarcadero Delphi' <<<"$raw")
  {
    printf '# %s\n' "$version"
    header="# dcc32 -B -H+ -W+ -Q -U<$lib_header>"
    for flag in "${project_switch_flags[@]}"; do header+=" $flag"; done
    if [[ $use_unit_scopes == true ]]; then
      header+=" -NSSystem"
      for scope in "${unit_scopes[@]}"; do header+=" -NS$scope"; done
    fi
    printf '%s %s\n' "$header" "$unit"
    if [[ -n $removed_unit ]]; then
      printf '# unused-unit check: removed %s\n' "$removed_unit"
    fi
    grep -E '^[^ ].*\([0-9]+\) (Error|Warning|Hint): [EWH][0-9]{4} ' <<<"$raw" \
      | grep -v ' H2675 ' \
      | sed -E 's/^(.*[\\/])?([^\\/(]+)\(([0-9]+)\) (Error|Warning|Hint): /\2(\3) /' || true
  } > "$destination"
}

normalize_dcc_record() {
  local path=$1 messages
  messages=$(grep -E '^[^ ].*\([0-9]+\) [EWH][0-9]{4} ' "$path" || true)
  [[ -n $messages ]] || return 0
  sed -E 's/^(.+)\([0-9]+\) /\1 /' <<<"$messages" | LC_ALL=C sort
}

first_message_difference() {
  local original=$1 edited=$2 difference
  difference=$(diff -u "$original" "$edited" \
    | awk '
      /^-[^-]/ { print "original only: " substr($0, 2); exit }
      /^\+[^+]/ { print "edited only: " substr($0, 2); exit }
    ' || true)
  printf '%s' "${difference:-diagnostic multiset changed}"
}

record_unused_unit_checks() {
  local name=$1 dir=$2 unit=$3 expected_output=$4 record tmp tmp_base check_unit
  local record_tmp edited_output edited_record
  ((${#unused_units[@]})) || return 0
  record="$dir/unused_unit_check.$compiler.txt"
  tmp_base="${TMPDIR:-/tmp/opencode}"
  mkdir -p "$tmp_base"
  tmp=$(mktemp -d "$tmp_base/dcc-oracle-unused.XXXXXX")
  normalize_dcc_record "$expected_output" > "$tmp/original.txt"
  record_tmp="$tmp/record.txt"
  : > "$record_tmp"
  for check_unit in "${unused_units[@]}"; do
    local case_copy="$tmp/edited/$check_unit"
    local output_dir="$tmp/output-$check_unit"
    mkdir -p "$case_copy" "$output_dir"
    if ! cp -a "$dir/." "$case_copy/"; then
      echo "$name: could not copy fixture for unused-unit check $check_unit" >&2
      rm -rf "$tmp"
      return 1
    fi
    local source="$case_copy/$unit"
    if [[ ! -f $source ]]; then
      echo "$name: checked unit source not found for unused-unit check $check_unit: $source" >&2
      rm -rf "$tmp"
      return 1
    fi
    if ! python3 "$root/scripts/dcc-oracle-remove-unit.py" --unit "$check_unit" \
      < "$source" > "$source.edited"; then
      echo "$name: could not remove $check_unit from $unit" >&2
      rm -rf "$tmp"
      return 1
    fi
    mv "$source.edited" "$source"

    local edited_raw edited_status
    if edited_raw=$(compile_dcc "$case_copy" "$unit" "$output_dir"); then
      edited_status=0
    else
      edited_status=$?
    fi
    if ! validate_dcc_output "$name without $check_unit" "$edited_raw" "$edited_status"; then
      rm -rf "$tmp"
      return 1
    fi
    edited_output="$tmp/edited-output.dcc"
    edited_record="$dir/unused_unit_check.$compiler.$check_unit.dcc"
    write_normalized_dcc_record "$edited_output" "$edited_raw" "$unit" "$check_unit"
    normalize_dcc_record "$edited_output" > "$tmp/edited.txt"
    if cmp -s "$tmp/original.txt" "$tmp/edited.txt"; then
      printf '%s identical\n' "$check_unit" >> "$record_tmp"
    else
      local difference
      difference=$(first_message_difference "$tmp/original.txt" "$tmp/edited.txt")
      printf '%s differs: %s\n' "$check_unit" "$difference" >> "$record_tmp"
    fi
    mv "$edited_output" "$edited_record"
  done
  mv "$record_tmp" "$record"
  rm -rf "$tmp"
  echo "$name: recorded unused-unit checks for Delphi $compiler"
}

status=0
for name in "${cases[@]}"; do
  dir="$cases_dir/$name"
  unit=$(sed -nE 's/^unit *= *"([^"]+)".*/\1/p' "$dir/case.toml")
  if [[ -z $unit ]]; then echo "$name: case.toml has no unit" >&2; status=1; continue; fi
  if ! read_unused_unit_check "$dir/case.toml"; then status=1; continue; fi
  if case_includes_compiler "$dir/case.toml"; then
    :
  else
    case_status=$?
    if [[ $case_status -eq 2 ]]; then status=1; continue; fi
    echo "$name: skipped (not configured for $compiler)"
    continue
  fi
  if ! read_unit_scopes "$dir/case.toml"; then status=1; continue; fi
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
    if [[ -z ${config//[[:space:]]/} ]]; then
      echo "$name: .dproj case.toml must set config" >&2
      status=1
      continue
    fi
    if [[ -z ${platform//[[:space:]]/} ]]; then
      echo "$name: .dproj case.toml must set platform" >&2
      status=1
      continue
    fi
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
  if raw=$(compile_dcc "$dir" "$unit" "$out"); then
    compiler_status=0
  else
    compiler_status=$?
  fi
  rm -rf "$out"
  if ! validate_dcc_output "$name" "$raw" "$compiler_status"; then
    status=1
    continue
  fi
  write_normalized_dcc_record "$output" "$raw" "$unit"
  if ((${#dproj_files[@]} == 1)); then
    {
      for flag in "${project_switch_flags[@]}"; do
        printf '%s\n' "$flag"
      done
    } > "$dir/dproj-switches.txt"
  fi
  echo "$name: recorded $(grep -vc '^#' "$output" || true) message(s) for Delphi $compiler"
  if ! record_unused_unit_checks "$name" "$dir" "$unit" "$output"; then status=1; fi
done
exit "$status"
