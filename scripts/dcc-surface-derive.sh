#!/usr/bin/env bash
# Derive the checked-in dcc surface table for one compiler/platform (dev-only).
set -Eeuo pipefail
export LC_ALL=C

usage() {
  cat >&2 <<'EOF'
Usage: scripts/dcc-surface-derive.sh <version> <platform>
  version: 21.0 | 24.0 | 37.0
  platform: Win32 | Win64
EOF
  exit "${1:-2}"
}

fail() {
  printf 'dcc-surface-derive: %s\n' "$*" >&2
  exit 2
}

[[ $# -eq 2 ]] || usage 2
version=$1
platform=$2

root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)"
files_root="$HOME/.local/share/Headless-Delphi/files"
wine_prefix="${DCC_ORACLE_WINEPREFIX:-$HOME/.local/share/Headless-Delphi/state/wine}"
namespace_args=()

case "$version/$platform" in
  21.0/Win32)
    install="$files_root/RAD Studio/7.0"
    compiler="$install/bin/dcc32.exe"
    library="$install/lib"
    rtl_root="$install/source/Win32"
    ;;
  24.0/Win32)
    install="$files_root/RAD Studio/10.0"
    compiler="$install/bin/dcc32.exe"
    library="$install/lib/win32/release"
    rtl_root="$install/source"
    namespace_args=(-NSSystem)
    ;;
  24.0/Win64)
    install="$files_root/RAD Studio/10.0"
    compiler="$install/bin/dcc64.exe"
    library="$install/lib/win64/release"
    rtl_root="$install/source"
    namespace_args=(-NSSystem)
    ;;
  37.0/Win32)
    install="$files_root/Studio/37.0"
    compiler="$install/bin/dcc32.EXE"
    [[ -f $compiler ]] || compiler="$install/bin/dcc32.exe"
    library="$install/lib/win32/release"
    rtl_root="$install/source"
    namespace_args=(-NSSystem)
    ;;
  37.0/Win64)
    install="$files_root/Studio/37.0"
    compiler="$install/bin/dcc64.exe"
    library="$install/lib/win64/release"
    rtl_root="$install/source"
    namespace_args=(-NSSystem)
    ;;
  *)
    fail "unsupported compiler/platform combination: $version $platform"
    ;;
esac

[[ -d "$wine_prefix/drive_c" ]] || fail "Wine prefix not found: $wine_prefix"
[[ -f "$compiler" ]] || fail "compiler not found: $compiler"
[[ -d "$library" ]] || fail "compiler library directory not found: $library"
[[ -d "$install/bin" ]] || fail "compiler bin directory not found: $install/bin"
[[ -f "$rtl_root/rtl/sys/System.pas" ]] || fail "System.pas not found under $rtl_root/rtl/sys"
[[ -f "$rtl_root/rtl/sys/SysInit.pas" ]] || fail "SysInit.pas not found under $rtl_root/rtl/sys"
[[ -f "$root/crates/pascal-project/src/compiler_defines.rs" ]] || fail "compiler_defines.rs not found under $root"
[[ -f "$root/crates/pascal-lsp/examples/dcc_surface_table.rs" ]] || fail "dcc_surface_table example not found under $root"

for dependency in awk cargo cat cmp cp dirname find grep mkdir mktemp rm sed sort strings timeout tr wine wc; do
  command -v "$dependency" >/dev/null 2>&1 || fail "required command not found: $dependency"
done

winpath() {
  printf 'Z:%s' "$(printf '%s' "$1" | tr '/' '\134')"
}

system_file="$rtl_root/rtl/sys/System.pas"
sysinit_file="$rtl_root/rtl/sys/SysInit.pas"
library_win="$(winpath "$library")"
table_path="$root/crates/pascal-lsp/src/navigation/dcc_surface/$version-$platform.txt"
table_dir="$(dirname -- "$table_path")"
[[ -d "$table_dir" ]] || fail "table output directory not found: $table_dir"

tmpdir="$(mktemp -d)"
trap 'rm -rf -- "$tmpdir"' EXIT
mkdir -- "$tmpdir/work"

system_identifiers_raw="$tmpdir/system-identifiers.raw"
rtl_identifiers_raw="$tmpdir/rtl-identifiers.raw"
binary_identifiers_raw="$tmpdir/binary-identifiers.raw"
generic_identifiers_raw="$tmpdir/system-generic-identifiers.raw"
define_literals_raw="$tmpdir/define-literals.raw"
candidate_file="$tmpdir/candidates.txt"
system_candidate_file="$tmpdir/system-candidates.txt"
generic_candidate_file="$tmpdir/generic-candidates.txt"
grep_matches="$tmpdir/grep-matches.out"
compiler_files_raw="$tmpdir/compiler-files.raw"
compiler_files_sorted="$tmpdir/compiler-files.sorted"
source_files_raw="$tmpdir/source-files.raw"
source_files_sorted="$tmpdir/source-files.sorted"

: > "$system_identifiers_raw"
: > "$rtl_identifiers_raw"
: > "$binary_identifiers_raw"
: > "$generic_identifiers_raw"
: > "$define_literals_raw"
: > "$grep_matches"
: > "$compiler_files_raw"
: > "$source_files_raw"

append_grep_matches() {
  local pattern=$1
  local input=$2
  local output=$3
  local description=$4
  local grep_status

  if grep -oE "$pattern" "$input" > "$grep_matches"; then
    cat "$grep_matches" >> "$output" || fail "cannot append extracted tokens from $description"
  else
    grep_status=$?
    ((grep_status == 1)) && return 0
    fail "cannot extract tokens from $description (grep status $grep_status)"
  fi
}

append_generic_identifiers() {
  local input=$1
  local grep_status

  if grep -oP '\b([A-Za-z_][A-Za-z0-9_]*)\s*<' "$input" > "$grep_matches"; then
    sed -E 's/[[:space:]]*<$//' "$grep_matches" >> "$generic_identifiers_raw" \
      || fail "cannot append generic identifiers from $input"
  else
    grep_status=$?
    ((grep_status == 1)) && return 0
    fail "cannot find generic identifiers in $input (grep status $grep_status)"
  fi
}

append_grep_matches '[A-Za-z_][A-Za-z0-9_]{0,62}' "$system_file" "$system_identifiers_raw" "$system_file"
append_generic_identifiers "$system_file"
append_generic_identifiers "$sysinit_file"

for source_tree in rtl vcl; do
  source_tree_path="$rtl_root/$source_tree"
  [[ -d "$source_tree_path" ]] || fail "RTL source subtree not found: $source_tree_path"
  find "$source_tree_path" -type f \( -iname '*.pas' -o -iname '*.inc' \) -print0 \
    >> "$source_files_raw" \
    || fail "cannot enumerate Pascal source files under $source_tree_path"
done
sort -z "$source_files_raw" > "$source_files_sorted" \
  || fail "cannot sort Pascal source file list under $rtl_root"
mapfile -d '' -t source_files < "$source_files_sorted"
[[ ${#source_files[@]} -gt 0 ]] || fail "no .pas or .inc files found under $rtl_root/rtl and $rtl_root/vcl"
for source_file in "${source_files[@]}"; do
  [[ -f "$source_file" && -r "$source_file" ]] || fail "Pascal source file is not readable: $source_file"
  append_grep_matches '[A-Za-z_][A-Za-z0-9_]{0,62}' "$source_file" "$rtl_identifiers_raw" "$source_file"
done

find "$install/bin" -maxdepth 1 -type f \( -iname 'dcc*.exe' -o -iname 'dcc*.dll' \) -print0 \
  > "$compiler_files_raw" \
  || fail "cannot enumerate dcc*.exe and dcc*.dll files under $install/bin"
sort -z "$compiler_files_raw" > "$compiler_files_sorted" \
  || fail "cannot sort compiler binary list under $install/bin"
mapfile -d '' -t compiler_files < "$compiler_files_sorted"
[[ ${#compiler_files[@]} -gt 0 ]] || fail "no dcc*.exe or dcc*.dll files found under $install/bin"
for compiler_file in "${compiler_files[@]}"; do
  [[ -f "$compiler_file" && -r "$compiler_file" ]] || fail "compiler binary is not readable: $compiler_file"
  for encoding in ascii utf16le; do
    strings_output="$tmpdir/strings.out"
    if [[ $encoding == ascii ]]; then
      strings -a -n 2 "$compiler_file" > "$strings_output" \
        || fail "strings failed for $compiler_file"
    else
      strings -a -el -n 2 "$compiler_file" > "$strings_output" \
        || fail "wide strings failed for $compiler_file"
    fi
    append_grep_matches '[A-Za-z_][A-Za-z0-9_]{0,62}' "$strings_output" "$binary_identifiers_raw" "$encoding strings in $compiler_file"
  done
done

define_literals_quoted="$tmpdir/define-literals-quoted.raw"
: > "$define_literals_quoted"
append_grep_matches '"[A-Z0-9_]+"' "$root/crates/pascal-project/src/compiler_defines.rs" \
  "$define_literals_quoted" "$root/crates/pascal-project/src/compiler_defines.rs"
sed -E 's/^"//; s/"$//' "$define_literals_quoted" > "$define_literals_raw" \
  || fail "cannot normalize define literals from compiler_defines.rs"
cat "$rtl_identifiers_raw" "$binary_identifiers_raw" "$define_literals_raw" \
  > "$tmpdir/all-candidates.raw"
cat "$system_identifiers_raw" > "$tmpdir/system-candidates-source.raw"

reserved_words='and array as asm begin case class const constructor destructor dispinterface div do downto else end except exports file finalization finally for function goto if implementation in inherited initialization inline interface is label library mod nil not object of or out packed procedure program property raise record repeat resourcestring set shl shr string then threadvar to try type unit until uses var while with xor'
sanitize_candidates() {
  local input=$1
  local output=$2
  awk -v words="$reserved_words" '
    BEGIN {
      count = split(words, reserved_list, " ")
      for (i = 1; i <= count; i++) reserved[tolower(reserved_list[i])] = 1
      own["l4dsurfaceprobe"] = 1
      own["l4dprobeproc"] = 1
      own["tl4dprobe"] = 1
      own["l4dprobemethod"] = 1
      own["l4dv"] = 1
    }
    /^[A-Za-z_][A-Za-z0-9_]*$/ && length($0) <= 63 {
      key = tolower($0)
      if (!(key in reserved) && !(key in own) && !(key in seen)) {
        seen[key] = 1
        print $0
      }
    }
  ' "$input" > "$output"
}
sanitize_candidates "$tmpdir/all-candidates.raw" "$candidate_file"
sanitize_candidates "$tmpdir/system-candidates-source.raw" "$system_candidate_file"
sanitize_candidates "$generic_identifiers_raw" "$generic_candidate_file"

mapfile -t candidate_names < "$candidate_file"
mapfile -t system_names < "$system_candidate_file"
mapfile -t generic_source_names < "$generic_candidate_file"
(( ${#candidate_names[@]} >= 5000 )) \
  || fail "candidate set is too small (${#candidate_names[@]}; require at least 5000)"

probe_name=L4dSurfaceProbe.pas
compiler_output=''
compiler_status=0
compiler_banner=''
compiler_invocations=0
surface_absent="$tmpdir/surface-absent.txt"
defines_file="$tmpdir/defines.txt"
surface_file="$tmpdir/surface.txt"
: > "$surface_absent"
: > "$defines_file"

run_compiler() {
  local source=$1
  if compiler_output=$(
    cd -- "$tmpdir/work" && \
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
    compiler_banner=$(grep -m1 '^Embarcadero Delphi' <<< "$compiler_output" || true)
  fi
}

compiler_needs_split() {
  if [[ $compiler_status -ne 0 && $compiler_status -ne 1 ]] \
    || grep -qE 'E2226|Fatal:' <<< "$compiler_output"; then
    return 0
  fi
  return 1
}

line_has_e2003() {
  local line=$1
  local candidate=$2
  grep -iFq "L4dSurfaceProbe.pas($line) Error: E2003 Undeclared identifier: '$candidate'" \
    <<< "$compiler_output"
}

write_surface_probe() {
  local first=$1
  local count=$2
  local offset
  {
    printf 'unit L4dSurfaceProbe;\ninterface\nimplementation\nprocedure L4dProbeProc;\nbegin\n'
    for ((offset = 0; offset < count; offset++)); do
      printf '  %s;\n' "${candidate_names[first + offset]}"
    done
    printf 'end;\nend.\n'
  } > "$tmpdir/work/$probe_name"
}

surface_splits=0
surface_unresolved=0
probe_surface_range() {
  local first=$1
  local count=$2
  local offset left line candidate
  write_surface_probe "$first" "$count"
  run_compiler "$probe_name"
  if compiler_needs_split; then
    if ((count == 1)); then
      surface_unresolved=$((surface_unresolved + 1))
      printf 'dcc-surface-derive: surface probe unresolved for %s (status %s); keeping present\n' \
        "${candidate_names[first]}" "$compiler_status" >&2
      return 0
    fi
    surface_splits=$((surface_splits + 1))
    left=$((count / 2))
    probe_surface_range "$first" "$left"
    probe_surface_range "$((first + left))" "$((count - left))"
    return 0
  fi

  for ((offset = 0; offset < count; offset++)); do
    candidate=${candidate_names[first + offset]}
    line=$((6 + offset))
    if line_has_e2003 "$line" "$candidate"; then
      printf '%s\n' "$candidate" >> "$surface_absent"
    fi
  done
}

SECONDS=0
printf 'Deriving %s %s with %s\n' "$version" "$platform" "$compiler"
printf 'Candidates: %s total; %s System.pas identifiers for member probes\n' \
  "${#candidate_names[@]}" "${#system_names[@]}"
surface_batch_count=$((( ${#candidate_names[@]} + 59 ) / 60))
for ((first = 0; first < ${#candidate_names[@]}; first += 60)); do
  count=60
  ((first + count <= ${#candidate_names[@]})) || count=$((${#candidate_names[@]} - first))
  probe_surface_range "$first" "$count"
  completed=$((first / 60 + 1))
  if ((completed == 1)); then
    [[ -n $compiler_banner ]] || {
      printf '%s\n' "$compiler_output" >&2
      fail "dcc did not print an Embarcadero Delphi compiler banner"
    }
  fi
  if ((completed % 25 == 0 || completed == surface_batch_count)); then
    printf 'Surface probes: %s/%s batches\n' "$completed" "$surface_batch_count"
  fi
done

generic_probe_candidates_file="$tmpdir/generic-probe-candidates.txt"
generic_present_file="$tmpdir/generic-present.txt"
: > "$generic_present_file"
awk '
  FILENAME == ARGV[1] { absent[tolower($0)] = 1; next }
  FILENAME == ARGV[2] { generic[tolower($0)] = 1; next }
  tolower($0) in absent && tolower($0) in generic
' "$surface_absent" "$generic_candidate_file" "$candidate_file" \
  > "$generic_probe_candidates_file"
mapfile -t generic_probe_names < "$generic_probe_candidates_file"

generic_splits=0
generic_unresolved=0
line_has_generic_e2003() {
  local line=$1
  local candidate=$2
  line_has_e2003 "$line" "$candidate" \
    || grep -iFq "L4dSurfaceProbe.pas($line) Error: E2003 Undeclared identifier: '$candidate<" \
      <<< "$compiler_output"
}

write_generic_probe() {
  local first=$1
  local count=$2
  local offset candidate
  {
    printf 'unit L4dSurfaceProbe;\ninterface\nimplementation\nprocedure L4dProbeProc;\nvar\n'
    for ((offset = 0; offset < count; offset++)); do
      candidate=${generic_probe_names[first + offset]}
      printf '  L4dG%d: %s<Integer>;\n' "$((offset * 3))" "$candidate"
      printf '  L4dG%d: %s<Integer, Integer>;\n' "$((offset * 3 + 1))" "$candidate"
      printf '  L4dG%d: %s<Integer, Integer, Integer>;\n' "$((offset * 3 + 2))" "$candidate"
    done
    printf 'begin\nend;\nend.\n'
  } > "$tmpdir/work/$probe_name"
}

probe_generic_range() {
  local first=$1
  local count=$2
  local offset left candidate first_line second_line third_line
  write_generic_probe "$first" "$count"
  run_compiler "$probe_name"
  if compiler_needs_split; then
    if ((count == 1)); then
      generic_unresolved=$((generic_unresolved + 1))
      printf '%s\n' "${generic_probe_names[first]}" >> "$generic_present_file"
      printf 'dcc-surface-derive: generic probe unresolved for %s (status %s); keeping present\n' \
        "${generic_probe_names[first]}" "$compiler_status" >&2
      return 0
    fi
    generic_splits=$((generic_splits + 1))
    left=$((count / 2))
    probe_generic_range "$first" "$left"
    probe_generic_range "$((first + left))" "$((count - left))"
    return 0
  fi

  for ((offset = 0; offset < count; offset++)); do
    candidate=${generic_probe_names[first + offset]}
    first_line=$((6 + 3 * offset))
    second_line=$((first_line + 1))
    third_line=$((first_line + 2))
    if ! line_has_generic_e2003 "$first_line" "$candidate" \
      || ! line_has_generic_e2003 "$second_line" "$candidate" \
      || ! line_has_generic_e2003 "$third_line" "$candidate"; then
      printf '%s\n' "$candidate" >> "$generic_present_file"
    fi
  done
}

generic_batch_count=$((( ${#generic_probe_names[@]} + 19 ) / 20))
printf 'Generic type probes: source=%s System.pas/SysInit.pas names; bare-absent=%s candidates; batches=%s (max 60 declarations)\n' \
  "${#generic_source_names[@]}" "${#generic_probe_names[@]}" "$generic_batch_count"
for ((first = 0; first < ${#generic_probe_names[@]}; first += 20)); do
  count=20
  ((first + count <= ${#generic_probe_names[@]})) || count=$((${#generic_probe_names[@]} - first))
  probe_generic_range "$first" "$count"
  completed=$((first / 20 + 1))
  if ((completed % 25 == 0 || completed == generic_batch_count)); then
    printf 'Generic type probes: %s/%s batches\n' "$completed" "$generic_batch_count"
  fi
done

awk '
  FILENAME == ARGV[1] { absent[tolower($0)] = 1; next }
  FILENAME == ARGV[2] { generic_present[tolower($0)] = 1; next }
  !(tolower($0) in absent) || (tolower($0) in generic_present)
' "$surface_absent" "$generic_present_file" "$candidate_file" > "$surface_file"

defines_splits=0
write_defines_probe() {
  local first=$1
  local count=$2
  local offset candidate
  {
    printf 'unit L4dSurfaceProbe;\ninterface\nimplementation\n'
    for ((offset = 0; offset < count; offset++)); do
      candidate=${candidate_names[first + offset]}
      printf "{\$IFDEF %s}{\$MESSAGE HINT 'l4d-def:%s'}{\$ENDIF}\n" "$candidate" "$candidate"
    done
    printf 'end.\n'
  } > "$tmpdir/work/$probe_name"
}

probe_defines_range() {
  local first=$1
  local count=$2
  local offset left candidate has_error
  write_defines_probe "$first" "$count"
  run_compiler "$probe_name"
  has_error=0
  if [[ $compiler_status -ne 0 ]] || grep -qE '(Error|Fatal):' <<< "$compiler_output"; then
    has_error=1
  fi
  if ((has_error)); then
    if ((count == 1)); then
      printf '%s\n' "$compiler_output" >&2
      fail "define probe still failed for ${candidate_names[first]} after splitting"
    fi
    defines_splits=$((defines_splits + 1))
    left=$((count / 2))
    probe_defines_range "$first" "$left"
    probe_defines_range "$((first + left))" "$((count - left))"
    return 0
  fi

  for ((offset = 0; offset < count; offset++)); do
    candidate=${candidate_names[first + offset]}
    if grep -Eiq "l4d-def:${candidate}([^[:alnum:]_]|$)" <<< "$compiler_output"; then
      printf '%s\n' "$candidate" >> "$defines_file"
    fi
  done
}

printf 'Probing predefined defines\n'
for ((first = 0; first < ${#candidate_names[@]}; first += 60)); do
  count=60
  ((first + count <= ${#candidate_names[@]})) || count=$((${#candidate_names[@]} - first))
  probe_defines_range "$first" "$count"
done

member_file() {
  printf '%s/members-%s.txt' "$tmpdir" "$1"
}

write_member_probe() {
  local target=$1
  local first=$2
  local count=$3
  local offset candidate
  local output="$tmpdir/work/$probe_name"
  if [[ $target == IInterface || $target == IUnknown ]]; then
    {
      printf 'unit L4dSurfaceProbe;\ninterface\nimplementation\nprocedure L4dProbeProc;\nvar\n  L4dV: %s;\nbegin\n' "$target"
      for ((offset = 0; offset < count; offset++)); do
        candidate=${system_names[first + offset]}
        printf '  L4dV.%s;\n' "$candidate"
      done
      printf 'end;\nend.\n'
    } > "$output"
  else
    {
      printf 'unit L4dSurfaceProbe;\ninterface\ntype\n  TL4dProbe = class(%s)\n    procedure L4dProbeMethod;\n  end;\nimplementation\nprocedure TL4dProbe.L4dProbeMethod;\nbegin\n' "$target"
      for ((offset = 0; offset < count; offset++)); do
        candidate=${system_names[first + offset]}
        printf '  %s.%s;\n  Self.%s;\n' "$target" "$candidate" "$candidate"
      done
      printf 'end;\nend.\n'
    } > "$output"
  fi
}

member_splits=0
member_unresolved=0
probe_member_range() {
  local target=$1
  local first=$2
  local count=$3
  local offset left candidate first_line second_line
  write_member_probe "$target" "$first" "$count"
  run_compiler "$probe_name"
  if compiler_needs_split; then
    if ((count == 1)); then
      member_unresolved=$((member_unresolved + 1))
      printf 'dcc-surface-derive: %s member probe unresolved for %s (status %s); keeping present\n' \
        "$target" "${system_names[first]}" "$compiler_status" >&2
      return 0
    fi
    member_splits=$((member_splits + 1))
    left=$((count / 2))
    probe_member_range "$target" "$first" "$left"
    probe_member_range "$target" "$((first + left))" "$((count - left))"
    return 0
  fi

  for ((offset = 0; offset < count; offset++)); do
    candidate=${system_names[first + offset]}
    if [[ $target == IInterface || $target == IUnknown ]]; then
      first_line=$((8 + offset))
      if ! line_has_e2003 "$first_line" "$candidate"; then
        printf '%s\n' "$candidate" >> "$(member_file "$target")"
      fi
    else
      first_line=$((10 + 2 * offset))
      second_line=$((first_line + 1))
      if ! line_has_e2003 "$first_line" "$candidate" \
        || ! line_has_e2003 "$second_line" "$candidate"; then
        printf '%s\n' "$candidate" >> "$(member_file "$target")"
      fi
    fi
  done
}

printf 'Probing root-type members\n'
for target in TObject IInterface IUnknown TInterfacedObject; do
  member_output=$(member_file "$target")
  : > "$member_output"
  for ((first = 0; first < ${#system_names[@]}; first += 30)); do
    count=30
    ((first + count <= ${#system_names[@]})) || count=$((${#system_names[@]} - first))
    probe_member_range "$target" "$first" "$count"
  done
done

candidate_count=$(wc -l < "$candidate_file" | tr -d '[:space:]')
surface_count=$(wc -l < "$surface_file" | tr -d '[:space:]')
define_count=$(wc -l < "$defines_file" | tr -d '[:space:]')
generic_present_count=$(wc -l < "$generic_present_file" | tr -d '[:space:]')
object_member_count=$(wc -l < "$(member_file TObject)" | tr -d '[:space:]')
interface_member_count=$(wc -l < "$(member_file IInterface)" | tr -d '[:space:]')
unknown_member_count=$(wc -l < "$(member_file IUnknown)" | tr -d '[:space:]')
interfaced_object_member_count=$(wc -l < "$(member_file TInterfacedObject)" | tr -d '[:space:]')
printf 'Writing and validating %s\n' "$table_path"
(cd -- "$root" && cargo run --locked -q -p pascal-lsp --example dcc_surface_table -- \
  --banner "$compiler_banner" \
  --system "$system_file" \
  --sysinit "$sysinit_file" \
  --candidates "$candidate_file" \
  --surface "$surface_file" \
  --defines "$defines_file" \
  --members "TObject=$(member_file TObject)" \
  --members "IInterface=$(member_file IInterface)" \
  --members "IUnknown=$(member_file IUnknown)" \
  --members "TInterfacedObject=$(member_file TInterfacedObject)" \
  --out "$tmpdir/generated-table.txt")
cp -- "$tmpdir/generated-table.txt" "$table_path"
cmp -- "$tmpdir/generated-table.txt" "$table_path" \
  || fail "written table differs from the re-parsed generated table"

printf 'Done %s %s: candidates=%s surface=%s defines=%s generic-present=%s members[TObject]=%s members[IInterface]=%s members[IUnknown]=%s members[TInterfacedObject]=%s runtime=%ss splits[surface/defines/members/generic]=%s/%s/%s/%s unresolved[surface/members/generic]=%s/%s/%s\n' \
  "$version" "$platform" \
  "$candidate_count" "$surface_count" "$define_count" "$generic_present_count" \
  "$object_member_count" "$interface_member_count" "$unknown_member_count" \
  "$interfaced_object_member_count" \
  "$SECONDS" "$surface_splits" "$defines_splits" "$member_splits" "$generic_splits" \
  "$surface_unresolved" "$member_unresolved" "$generic_unresolved"
