#!/usr/bin/env bash
# Derive one compiler-version's dcc warning-switch table using Wine.
set -Eeuo pipefail
export LC_ALL=C

usage() {
  cat >&2 <<'EOF'
Usage: scripts/dcc-switch-derive.sh <version>
  version: 21.0 | 24.0 | 37.0
EOF
  exit "${1:-2}"
}

fail() {
  printf 'dcc-switch-derive: %s\n' "$*" >&2
  exit 2
}

[[ $# -eq 1 ]] || usage 2
version=$1

root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)"
source "$root/scripts/dcc-switch-probe.sh"
files_root="$HOME/.local/share/Headless-Delphi/files"
wine_prefix="${DCC_ORACLE_WINEPREFIX:-$HOME/.local/share/Headless-Delphi/state/wine}"
namespace_args=()

case "$version" in
  21.0)
    install="$files_root/RAD Studio/7.0"
    compiler="$install/bin/dcc32.exe"
    library="$install/lib"
    ;;
  24.0)
    install="$files_root/RAD Studio/10.0"
    compiler="$install/bin/dcc32.exe"
    library="$install/lib/win32/release"
    namespace_args=(-NSSystem)
    ;;
  37.0)
    install="$files_root/Studio/37.0"
    compiler="$install/bin/dcc32.EXE"
    [[ -f $compiler ]] || compiler="$install/bin/dcc32.exe"
    library="$install/lib/win32/release"
    namespace_args=(-NSSystem)
    ;;
  *)
    fail "unsupported compiler version: $version"
    ;;
esac

targets="$install/bin/CodeGear.Delphi.Targets"
[[ -d "$wine_prefix/drive_c" ]] || fail "Wine prefix not found: $wine_prefix"
[[ -f "$compiler" ]] || fail "compiler not found: $compiler"
[[ -d "$library" ]] || fail "compiler library directory not found: $library"
[[ -f "$targets" ]] || fail "CodeGear.Delphi.Targets not found: $targets"

for dependency in awk cat cmp cp find grep head mkdir mktemp mv rm sed sort strings timeout tr wc wine; do
  command -v "$dependency" >/dev/null 2>&1 || fail "required command not found: $dependency"
done

tmpdir="$(mktemp -d "${TMPDIR:-/tmp}/dcc-switch-derive.XXXXXX")"
trap 'rm -rf -- "$tmpdir"' EXIT
work_dir="$tmpdir/work"
mkdir -- "$work_dir"

winpath() {
  printf 'Z:%s' "$(printf '%s' "$1" | tr '/' '\134')"
}

library_win="$(winpath "$library")"
table_dir="$root/crates/pascal-lsp/src/navigation/dcc_switches"
table_path="$table_dir/$version.txt"
mkdir -p -- "$table_dir"

task_block="$tmpdir/dcc-task.xml"
sed -n '/<DCC DelphiCompile=/,/^[[:space:]]*\/>/p' "$targets" > "$task_block"
[[ -s "$task_block" ]] || fail "could not find DCC DelphiCompile task in $targets"

properties_raw="$tmpdir/properties.raw"
properties_file="$tmpdir/properties.txt"
if grep -oE '[A-Za-z_][A-Za-z0-9_]*[[:space:]]*=[[:space:]]*"\$\(DCC_[A-Za-z_][A-Za-z0-9_]*\)"' "$task_block" \
  > "$tmpdir/property-attributes.raw"; then
  sed -E \
    's/^([A-Za-z_][A-Za-z0-9_]*)[[:space:]]*=[[:space:]]*"\$\(DCC_([A-Za-z_][A-Za-z0-9_]*)\)"$/DCC_\2 \1/' \
    "$tmpdir/property-attributes.raw" > "$properties_raw"
else
  grep_status=$?
  ((grep_status == 1)) && fail "no DCC_<property> task mappings found in $targets"
  fail "cannot extract task property mappings from $targets (grep status $grep_status)"
fi
awk 'BEGIN { FS = "[[:space:]]+"; OFS = "\t" } { print tolower($1), $1, $2 }' \
  "$properties_raw" \
  | sort -t $'\t' -k1,1 -k2,2 \
  | awk -F '\t' '!seen[$1]++ { print $2 " " $3 }' > "$properties_file"
[[ -s "$properties_file" ]] || fail "no task property mappings extracted from $targets"

binary_files="$tmpdir/compiler-files.txt"
find "$install/bin" -maxdepth 1 -type f \( -iname 'dcc*.exe' -o -iname 'dcc*.dll' \) -print \
  | sort > "$binary_files"
[[ -s "$binary_files" ]] || fail "no dcc*.exe or dcc*.dll compiler binaries under $install/bin"

candidate_raw="$tmpdir/candidates.raw"
: > "$candidate_raw"
sed -E -n 's/^DCC_[A-Za-z_][A-Za-z0-9_]* ([A-Z][A-Z0-9_]{2,})$/\1/p' "$properties_file" \
  >> "$candidate_raw"

while IFS= read -r binary; do
  [[ -f "$binary" && -r "$binary" ]] || fail "compiler binary is not readable: $binary"
  for encoding in ascii utf16le; do
    strings_output="$tmpdir/strings.out"
    if [[ $encoding == ascii ]]; then
      strings -a -n 3 "$binary" > "$strings_output" \
        || fail "strings failed for $binary"
    else
      strings -a -el -n 3 "$binary" > "$strings_output" \
        || fail "wide strings failed for $binary"
    fi
    if grep -E '^[A-Z][A-Z0-9_]{2,}$' "$strings_output" >> "$candidate_raw"; then
      :
    else
      grep_status=$?
      ((grep_status == 1)) || fail "cannot extract identifiers from $binary (grep status $grep_status)"
    fi
  done
done < "$binary_files"

candidate_file="$tmpdir/candidates.txt"
grep -E '^[A-Z][A-Z0-9_]{2,}$' "$candidate_raw" | sort -u > "$candidate_file" \
  || fail "no candidate warning identifiers found"
mapfile -t candidate_ids < "$candidate_file"
candidate_ids_all=("${candidate_ids[@]}")
candidate_ids=()

codes=(H2219 W1000 W1001 W1002 W1003 W1004 W1005 W1006 W1007 W1009 W1010)
source_root="$root/crates/pascal-lsp/tests/dcc_oracle"
[[ -f "$source_root/w1009_property/W1009Property.pas" ]] || fail "W1009 trigger fixture is missing"
[[ -f "$source_root/w1010_hides_virtual/W1010HidesVirtual.pas" ]] || fail "W1010 trigger fixture is missing"
[[ -f "$source_root/h2219_unused_field/UnusedField.pas" ]] || fail "H2219 trigger fixture is missing"

compiler_output=''
compiler_status=0
compiler_banner=''
compiler_invocations=0
run_compiler() {
  local source=$1
  if compiler_output=$(
    cd -- "$work_dir" && \
      WINEPREFIX="$wine_prefix" WINEDEBUG=-all timeout 300 \
        wine "$compiler" -B -Q "-U$library_win" "${namespace_args[@]}" "$source" 2>&1 \
        | tr -d '\r'
  ); then
    compiler_status=0
  else
    compiler_status=$?
  fi
  compiler_invocations=$((compiler_invocations + 1))
  if ((compiler_invocations == 1)); then
    compiler_banner="$(grep -m1 '^Embarcadero Delphi' <<< "$compiler_output" || true)"
    if ! dcc_validate_compiler_banner "$version" "$compiler_banner"; then
      printf '%s\n' "$compiler_output" >&2
      fail "compiler banner does not match requested version $version"
    fi
  fi
}

validate_candidate_ids() {
  local batch_size=40
  local first count index identifier directives source
  local invalid_count=0
  local valid_file="$tmpdir/candidates.valid"
  : > "$valid_file"

  for ((first = 0; first < ${#candidate_ids_all[@]}; first += batch_size)); do
    count=$batch_size
    ((first + count <= ${#candidate_ids_all[@]})) \
      || count=$((${#candidate_ids_all[@]} - first))
    directives=$'{$WARNINGS ON}\n{$HINTS ON}\n'
    for ((index = 0; index < count; index++)); do
      identifier=${candidate_ids_all[first + index]}
      directives+="{\$WARN $identifier OFF}"$'\n'
    done
    source="$work_dir/L4dSwitchIdentifierProbe.pas"
    {
      printf 'program L4dSwitchIdentifierProbe;\n'
      printf '%s' "$directives"
      printf 'begin\nend.\n'
    } > "$source"
    run_compiler "L4dSwitchIdentifierProbe.pas"

    for ((index = 0; index < count; index++)); do
      identifier=${candidate_ids_all[first + index]}
      if grep -Fq "E1030 Invalid compiler directive: '$identifier'" \
        <<< "$compiler_output"; then
        invalid_count=$((invalid_count + 1))
      else
        printf '%s\n' "$identifier" >> "$valid_file"
      fi
    done

    completed=$((first + count))
    if ((completed % 1000 == 0 || completed == ${#candidate_ids_all[@]})); then
      printf 'Validated compiler identifiers: %s/%s\n' \
        "$completed" "${#candidate_ids_all[@]}"
    fi
  done

  mapfile -t candidate_ids < "$valid_file"
  printf 'Directive candidates accepted by dcc: %s (rejected invalid directive tokens: %s)\n' \
    "${#candidate_ids[@]}" "$invalid_count"
}

has_code() {
  local code=$1
  grep -Eq "(^|[^[:alnum:]_])${code}([^[:alnum:]_]|$)" <<< "$compiler_output"
}

write_program_trigger() {
  local code=$1
  local directives=$2
  local provider_flag=''
  local body=''
  local provider_declaration=''
  local provider_implementation=''

  case "$code" in
    W1000)
      provider_declaration="procedure DepMsg; deprecated 'use Other';"
      provider_implementation='procedure DepMsg; begin end;'
      body='DepMsg;'
      ;;
    W1001)
      provider_declaration='var LibVar: Integer library;'
      body='LibVar := 1;'
      ;;
    W1002)
      provider_declaration='procedure Plat; platform;'
      provider_implementation='procedure Plat; begin end;'
      body='Plat;'
      ;;
    W1003)
      provider_declaration='procedure Exper; experimental;'
      provider_implementation='procedure Exper; begin end;'
      body='Exper;'
      ;;
    W1004)
      provider_flag='library'
      ;;
    W1005)
      provider_flag='platform'
      ;;
    W1006)
      provider_flag='deprecated'
      ;;
    W1007)
      provider_flag='experimental'
      ;;
    *)
      fail "no program trigger for $code"
      ;;
  esac

  if [[ -n $provider_flag ]]; then
    {
      printf 'unit L4dSwitchProv %s;\ninterface\nimplementation\nend.\n' "$provider_flag"
    } > "$work_dir/L4dSwitchProv.pas"
  else
    {
      printf 'unit L4dSwitchProv;\ninterface\n'
      printf '%s\n' "$provider_declaration"
      printf 'implementation\n'
      [[ -z $provider_implementation ]] || printf '%s\n' "$provider_implementation"
      printf 'end.\n'
    } > "$work_dir/L4dSwitchProv.pas"
  fi

  {
    printf 'program L4dSwitchUse;\n'
    printf '%s' "$directives"
    printf 'uses L4dSwitchProv;\nbegin\n'
    [[ -z $body ]] || printf '  %s\n' "$body"
    printf 'end.\n'
  } > "$work_dir/L4dSwitchUse.pas"
  trigger_source='L4dSwitchUse.pas'
}

write_single_unit_trigger() {
  local code=$1
  local directives=$2
  local source_file=''
  local source_name=''

  case "$code" in
    H2219)
      source_file="$source_root/h2219_unused_field/UnusedField.pas"
      source_name='UnusedField.pas'
      ;;
    W1009)
      source_file="$source_root/w1009_property/W1009Property.pas"
      source_name='W1009Property.pas'
      ;;
    W1010)
      source_file="$source_root/w1010_hides_virtual/W1010HidesVirtual.pas"
      source_name='W1010HidesVirtual.pas'
      ;;
    *)
      fail "no single-unit trigger for $code"
      ;;
  esac

  {
    IFS= read -r first_line < "$source_file" || true
    printf '%s\n' "$first_line"
    printf '%s' "$directives"
    sed -n '2,$p' "$source_file"
  } > "$work_dir/$source_name"
  trigger_source=$source_name
}

trigger_source=''
write_trigger() {
  local code=$1
  local directives=$2
  case "$code" in
    W1000|W1001|W1002|W1003|W1004|W1005|W1006|W1007)
      write_program_trigger "$code" "$directives"
      ;;
    H2219|W1009|W1010)
      write_single_unit_trigger "$code" "$directives"
      ;;
    *)
      fail "no trigger for $code"
      ;;
  esac
}

validate_candidate_ids

global_on_directives=$'{$WARNINGS ON}\n{$HINTS ON}\n'
warn_off_directives="${global_on_directives}"$'{$WARNINGS OFF}\n'
hints_off_directives="${global_on_directives}"$'{$HINTS OFF}\n'

directives_for_id_range() {
  local mode=$1
  local first=$2
  local count=$3
  local index
  local result=''
  for ((index = 0; index < count; index++)); do
    result+="{\$WARN ${candidate_ids[first + index]} $mode}"$'\n'
  done
  printf '%s' "$result"
}

suppressors_file="$tmpdir/suppressors.txt"
id_splits=0
probe_id_range() {
  local code=$1
  local first=$2
  local count=$3
  local left
  local directives

  directives="$global_on_directives$(directives_for_id_range OFF "$first" "$count")"
  write_trigger "$code" "$directives"
  run_compiler "$trigger_source"
  if has_code "$code"; then
    return 0
  fi

  if ((count == 1)); then
    if grep -Fq "E1030 Invalid compiler directive: '${candidate_ids[first]}'" \
      <<< "$compiler_output"; then
      return 0
    fi
    if [[ $compiler_status -ne 0 ]] || grep -qE 'E2226|Fatal:' <<< "$compiler_output"; then
      printf '%s\n' "$compiler_output" >&2
      fail "$code identifier probe failed for ${candidate_ids[first]} (status $compiler_status)"
    fi
    if ! has_code "$code"; then
      printf '%s\n' "${candidate_ids[first]}" >> "$suppressors_file"
      return 0
    fi
  fi

  id_splits=$((id_splits + 1))
  left=$((count / 2))
  probe_id_range "$code" "$first" "$left"
  probe_id_range "$code" "$((first + left))" "$((count - left))"
}

declare -A defaults globals identifiers error_capabilities error_forms
error_capable=''
error_form=''
SECONDS=0
printf 'Deriving dcc %s Win32 with %s\n' "$version" "$compiler"
printf 'Candidates: %s warning IDs; %s task property mappings\n' \
  "${#candidate_ids[@]}" "$(wc -l < "$properties_file" | tr -d '[:space:]')"

for code in "${codes[@]}"; do
  printf 'Probe %s: ' "$code"
  write_trigger "$code" ''
  run_compiler "$trigger_source"
  [[ -n $compiler_banner ]] || {
    printf '%s\n' "$compiler_output" >&2
    fail "dcc did not print a compiler banner"
  }
  ((compiler_status == 0)) || {
    printf '%s\n' "$compiler_output" >&2
    fail "default trigger compile failed for $code (status $compiler_status)"
  }
  if has_code "$code"; then
    defaults[$code]=ON
  else
    defaults[$code]=OFF
  fi

  write_trigger "$code" "$global_on_directives"
  run_compiler "$trigger_source"
  ((compiler_status == 0)) || {
    printf '%s\n' "$compiler_output" >&2
    fail "global-on trigger compile failed for $code (status $compiler_status)"
  }
  has_code "$code" || {
    printf '%s\n' "$compiler_output" >&2
    fail "trigger did not produce $code with both global switches on"
  }

  write_trigger "$code" "$warn_off_directives"
  run_compiler "$trigger_source"
  ((compiler_status == 0)) || {
    printf '%s\n' "$compiler_output" >&2
    fail "WARNINGS-off trigger compile failed for $code (status $compiler_status)"
  }
  warn_removed=false
  has_code "$code" || warn_removed=true

  write_trigger "$code" "$hints_off_directives"
  run_compiler "$trigger_source"
  ((compiler_status == 0)) || {
    printf '%s\n' "$compiler_output" >&2
    fail "HINTS-off trigger compile failed for $code (status $compiler_status)"
  }
  hints_removed=false
  has_code "$code" || hints_removed=true

  if [[ $warn_removed == true && $hints_removed == false ]]; then
    globals[$code]=WARNINGS
  elif [[ $hints_removed == true && $warn_removed == false ]]; then
    globals[$code]=HINTS
  else
    fail "$code global switch was ambiguous (WARNINGS removed=$warn_removed, HINTS removed=$hints_removed)"
  fi

  : > "$suppressors_file"
  batch_size=40
  for ((first = 0; first < ${#candidate_ids[@]}; first += batch_size)); do
    count=$batch_size
    ((first + count <= ${#candidate_ids[@]})) || count=$((${#candidate_ids[@]} - first))
    probe_id_range "$code" "$first" "$count"
  done

  mapfile -t suppressors < "$suppressors_file"
  specific_suppressors=()
  property_suppressors=()
  umbrella_suppressors=()
  for suppressor in "${suppressors[@]}"; do
    if [[ $suppressor == ALL ]]; then
      umbrella_suppressors+=("$suppressor")
    elif grep -Fq "DCC_${suppressor} ${suppressor}" "$properties_file"; then
      property_suppressors+=("$suppressor")
    else
      specific_suppressors+=("$suppressor")
    fi
  done
  non_umbrella_suppressor_count=$((
    ${#property_suppressors[@]} + ${#specific_suppressors[@]}
  ))
  if ((non_umbrella_suppressor_count > 1)); then
    fail "$code has multiple non-umbrella suppressors: ${property_suppressors[*]} ${specific_suppressors[*]}"
  elif ((non_umbrella_suppressor_count == 0)) && [[ $code != H2219 ]]; then
    fail "$code has no non-umbrella suppressor"
  elif (( ${#property_suppressors[@]} == 1 )); then
    identifiers[$code]=${property_suppressors[0]}
  elif (( ${#specific_suppressors[@]} == 1 )); then
    identifiers[$code]=${specific_suppressors[0]}
  else
    identifiers[$code]='-'
  fi

  if [[ ${identifiers[$code]} != '-' ]]; then
    error_directives="${global_on_directives}{\$WARN ${identifiers[$code]} ERROR}"$'\n'
    write_trigger "$code" "$error_directives"
    run_compiler "$trigger_source"
    error_compile_output=$compiler_output
    if error_result=$(dcc_classify_error_probe \
      "$code" "$compiler_status" "$error_compile_output"); then
      IFS=$'\t' read -r error_capable error_form <<< "$error_result"
      error_capabilities[$code]=$error_capable
      error_forms[$code]=$error_form
    else
      printf '%s\n' "$error_compile_output" >&2
      fail "$code ERROR probe was inconclusive (status $compiler_status)"
    fi
  else
    error_capabilities[$code]=no
    error_forms[$code]='-'
  fi

  printf '%s default=%s global=%s id=%s error-capable=%s\n' \
    "$code" "${defaults[$code]}" "${globals[$code]}" \
    "${identifiers[$code]}" "${error_capabilities[$code]}"
  if (( ${#umbrella_suppressors[@]} > 0 )); then
    printf '  compiler-wide suppressor observed: %s (not recorded as a per-code ID)\n' \
      "${umbrella_suppressors[*]}"
  fi
  if [[ ${identifiers[$code]} != '-' ]]; then
    printf '  ERROR compile output: %s\n' "${error_compile_output//$'\n'/ | }"
  fi
  printf '  ERROR form: %s\n' "${error_forms[$code]}"
done

[[ -n $compiler_banner ]] || fail "compiler banner is missing"
generated_table="$tmpdir/$version.txt"
{
  printf '# dcc-switches v1\n# banner: %s\n# script-version: 1\n[codes]\n' "$compiler_banner"
  for code in "${codes[@]}"; do
    printf '%s %s %s %s %s\n' \
      "$code" "${defaults[$code]}" "${globals[$code]}" \
      "${identifiers[$code]}" "${error_capabilities[$code]}"
  done
  printf '[properties]\n'
  cat "$properties_file"
} > "$generated_table"

cp -- "$generated_table" "$table_path"
cmp -- "$generated_table" "$table_path" \
  || fail "written table differs from generated output"

printf 'Done dcc %s: %s compiler probes, %s ID-batch splits, runtime=%ss\n' \
  "$version" "$compiler_invocations" "$id_splits" "$SECONDS"
printf 'Wrote %s\n' "$table_path"
