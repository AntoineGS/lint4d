#!/usr/bin/env bash
# Compile manifest projects with the selected Delphi compiler under Wine.
# Usage: scripts/dcc-corpus.sh [name ...]
set -Eeuo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
manifest="${LINT4D_CORPUS_MANIFEST:-$HOME/.config/lint4d/corpus.toml}"
out="${LINT4D_CORPUS_OUT:-$HOME/.cache/lint4d-corpus}"
prefix="${DCC_CORPUS_WINEPREFIX:-$HOME/.local/share/Headless-Delphi/state/wine}"
timeout_seconds="${DCC_CORPUS_TIMEOUT:-600}"
lsp_timeout_seconds="${LINT4D_LSP_TIMEOUT:-600}"
cargo_bin="${CARGO:-cargo}"
python_bin="${PYTHON:-python3}"
requested_names=()
lsp_binary=""
run_compare=0
status=0
selected_count=0
dcc_runs=0
skipped_projects=()

usage() {
  printf 'Usage: %s [--lsp <pascal-lsp-binary>] [--compare] [name ...]\n' "${BASH_SOURCE[0]}"
}

while (($#)); do
  case $1 in
    -h|--help)
      usage
      exit 0
      ;;
    --lsp)
      (($# >= 2)) || { echo "--lsp requires a pascal-lsp binary" >&2; exit 2; }
      [[ -z $lsp_binary ]] || { echo "--lsp may be specified only once" >&2; exit 2; }
      lsp_binary=$2
      shift 2
      ;;
    --compare)
      run_compare=1
      shift
      ;;
    --)
      shift
      requested_names+=("$@")
      break
      ;;
    *)
      requested_names+=("$1")
      shift
      ;;
  esac
done

command -v "$cargo_bin" >/dev/null || { echo "cargo not found: $cargo_bin" >&2; exit 2; }
command -v "$python_bin" >/dev/null || { echo "python3 is required to read command JSON" >&2; exit 2; }
command -v wine >/dev/null || { echo "wine not found" >&2; exit 2; }
command -v timeout >/dev/null || { echo "timeout not found" >&2; exit 2; }
[[ -d "$prefix/drive_c" ]] || { echo "Wine prefix not found: $prefix" >&2; exit 2; }
[[ -f "$manifest" ]] || { echo "corpus manifest not found: $manifest" >&2; exit 2; }
if [[ -n $lsp_binary ]]; then
  if [[ $lsp_binary != /* ]]; then
    lsp_binary="$PWD/$lsp_binary"
  fi
  [[ -x $lsp_binary ]] || { echo "pascal-lsp binary is not executable: $lsp_binary" >&2; exit 2; }
fi

manifest="$(cd "$(dirname "$manifest")" && pwd)/$(basename "$manifest")"
manifest_dir="$(dirname "$manifest")"
mkdir -p "$out"
out="$(cd "$out" && pwd)"

if ! records=$(
  cd "$root"
  "$cargo_bin" run -q -p pascal-lsp --example dcc_corpus_command -- --list "$manifest"
); then
  echo "could not list corpus manifest: $manifest" >&2
  exit 2
fi

if [[ -z $records ]]; then
  echo "corpus manifest contains no projects: $manifest" >&2
  exit 2
fi

linux_to_wine() {
  local path=$1
  printf 'Z:%s' "${path//\//\\}"
}

join_semicolon() {
  local -n values=$1
  local IFS=';'
  printf '%s' "${values[*]}"
}

append_path_argument() {
  local option=$1
  local -n paths=$2
  local -a wine_paths=()
  local path
  for path in "${paths[@]}"; do
    [[ -n $path ]] && wine_paths+=("$(linux_to_wine "$path")")
  done
  if ((${#wine_paths[@]})); then
    dcc_args+=("-$option$(join_semicolon wine_paths)")
  fi
}

matches_requested_name() {
  local candidate=$1
  local requested
  for requested in "${requested_names[@]}"; do
    if [[ $requested == all || $requested == "$candidate" ]]; then
      return 0
    fi
  done
  return 1
}

read_command_fields() {
  local command_json=$1
  mapfile -d '' -t json_values < <(
    "$python_bin" - "$command_json" <<'PY'
import json
import sys

with open(sys.argv[1], encoding="utf-8") as source:
    command = json.load(source)

for key in ("compiler", "platform", "dcc", "main_source"):
    value = command[key]
    if not isinstance(value, str):
        raise SystemExit(f"{key} must be a string")
    sys.stdout.write(value + "\0")

for key in ("unit_paths", "include_paths", "resource_paths", "object_paths",
            "namespaces", "defines", "files", "warnings"):
    values = command.get(key, [])
    if not isinstance(values, list) or not all(isinstance(value, str) for value in values):
        raise SystemExit(f"{key} must be an array of strings")
    sys.stdout.write(str(len(values)) + "\0")
    for value in values:
        sys.stdout.write(value + "\0")
PY
  )
}

read_json_array() {
  local -n destination=$1
  local count=${json_values[json_index]}
  json_index=$((json_index + 1))
  local item
  for ((item = 0; item < count; item += 1)); do
    destination+=("${json_values[json_index]}")
    json_index=$((json_index + 1))
  done
}

while IFS= read -r record; do
  [[ -n $record ]] || continue
  name="$(cut -f1 <<<"$record")"
  dproj="$(cut -f2 <<<"$record")"
  config="$(cut -f3 <<<"$record")"
  platform="$(cut -f4 <<<"$record")"

  if [[ ! $name =~ ^[[:alnum:]_.-]+$ || $name == "." || $name == ".." ]]; then
    echo "unsafe corpus project name: $name" >&2
    status=1
    continue
  fi

  if ((${#requested_names[@]})) && ! matches_requested_name "$name"; then
    continue
  fi
  selected_count=$((selected_count + 1))

  if [[ $dproj != /* ]]; then
    dproj="$manifest_dir/$dproj"
  fi
  project_out="$out/$name"
  mkdir -p "$project_out"
  command_json="$project_out/command.json"
  command_args=("$dproj")
  [[ -n $config ]] && command_args+=(--config "$config")
  [[ -n $platform ]] && command_args+=(--platform "$platform")

  echo "$name: resolving project command"
  if ! (
    cd "$root"
    "$cargo_bin" run -q -p pascal-lsp --example dcc_corpus_command -- "${command_args[@]}"
  ) >"$command_json"; then
    printf '2\n' >"$project_out/dcc.status"
    printf 'failed: command example exited unsuccessfully\n' >"$project_out/dcc.validation"
    echo "$name: command example failed" >&2
    status=1
    continue
  fi

  if ! "$python_bin" -m json.tool "$command_json" >/dev/null; then
    printf '2\n' >"$project_out/dcc.status"
    printf 'failed: command example did not produce JSON\n' >"$project_out/dcc.validation"
    echo "$name: command example returned invalid JSON" >&2
    status=1
    continue
  fi
  skip_reason=$("$python_bin" - "$command_json" <<'PY'
import json
import sys
with open(sys.argv[1], encoding="utf-8") as source:
    command = json.load(source)
if "skip" in command:
    print(command["skip"])
elif command.get("incomplete") is not False:
    print("project source closure is incomplete or unverified")
PY
)
  if [[ -n $skip_reason ]]; then
    printf 'skipped\n' >"$project_out/dcc.status"
    printf 'skipped: %s\n' "$skip_reason" >"$project_out/dcc.validation"
    skipped_projects+=("$name ($skip_reason)")
    echo "$name: skipped ($skip_reason)"
    continue
  fi

  json_values=()
  read_command_fields "$command_json"
  json_index=0
  compiler="${json_values[json_index]}"
  json_index=$((json_index + 1))
  selected_platform="${json_values[json_index]}"
  json_index=$((json_index + 1))
  dcc="${json_values[json_index]}"
  json_index=$((json_index + 1))
  main_source="${json_values[json_index]}"
  json_index=$((json_index + 1))
  # These list values are consumed through append_path_argument's nameref.
  # shellcheck disable=SC2034
  unit_paths=()
  # shellcheck disable=SC2034
  include_paths=()
  # shellcheck disable=SC2034
  resource_paths=()
  # shellcheck disable=SC2034
  object_paths=()
  namespaces=()
  defines=()
  files=()
  warnings=()
  read_json_array unit_paths
  read_json_array include_paths
  read_json_array resource_paths
  read_json_array object_paths
  read_json_array namespaces
  read_json_array defines
  read_json_array files
  read_json_array warnings
  echo "$name: ${#files[@]} project source file(s), ${#warnings[@]} context warning(s)"

  if [[ ! -f $dcc || ! -f $main_source ]]; then
    printf '2\n' >"$project_out/dcc.status"
    printf 'failed: dcc or main source does not exist\n' >"$project_out/dcc.validation"
    echo "$name: missing dcc executable or main source" >&2
    status=1
    continue
  fi

  source_dir="$(dirname "$main_source")"
  main_wine="$(linux_to_wine "$main_source")"
  compiler_usage=""
  if usage_output=$(
    cd "$source_dir"
    WINEPREFIX="$prefix" WINEDEBUG=-all wine "$dcc" 2>&1 | tr -d '\r'
  ); then
    compiler_usage=$usage_output
  else
    compiler_usage=$usage_output
  fi

  tmp="$(mktemp -d "${TMPDIR:-/tmp}/dcc-corpus.XXXXXXXX")"
  tmp_wine="$(linux_to_wine "$tmp")"
  dcc_args=(-B -Q -W+ -H+)
  append_path_argument U unit_paths
  append_path_argument I include_paths
  append_path_argument R resource_paths
  append_path_argument O object_paths
  if [[ $compiler != 21.0 && ${#namespaces[@]} -gt 0 ]]; then
    dcc_args+=("-NS$(join_semicolon namespaces)")
  fi
  if ((${#defines[@]})); then
    dcc_args+=("-D$(join_semicolon defines)")
  fi
  dcc_args+=("-N0$tmp_wine" "-E$tmp_wine" "-LE$tmp_wine" "-LN$tmp_wine")
  if grep -Eiq -- '-NU([[:space:]]|<|$)' <<<"$compiler_usage"; then
    dcc_args+=("-NU$tmp_wine")
  fi
  if grep -Eiq -- '-NO([[:space:]]|<|$)' <<<"$compiler_usage"; then
    dcc_args+=("-NO$tmp_wine")
  fi

  dcc_exit=0
  dcc_runs=$((dcc_runs + 1))
  if raw=$(
    cd "$source_dir"
    WINEPREFIX="$prefix" WINEDEBUG=-all timeout "$timeout_seconds" \
      wine "$dcc" "${dcc_args[@]}" "$main_wine" 2>&1 | tr -d '\r'
  ); then
    dcc_exit=0
  else
    dcc_exit=$?
  fi
  printf '%s\n' "$raw" >"$project_out/dcc.raw"
  printf '%s\n' "$dcc_exit" >"$project_out/dcc.status"
  rm -rf -- "$tmp"

  banner="$(grep -m1 '^Embarcadero Delphi' "$project_out/dcc.raw" || true)"
  message_count="$(grep -Ec '^[^ ].*\([0-9]+\) (Error|Warning|Hint|Fatal): [EWHF][0-9]{4} ' \
    "$project_out/dcc.raw" || true)"
  if [[ -z $banner ]]; then
    printf 'failed: compiler banner not found\n' >"$project_out/dcc.validation"
    echo "$name: failed (compiler banner missing); $message_count message(s), exit $dcc_exit" >&2
    status=1
    continue
  fi
  if [[ $banner != *"compiler version $compiler"* ]]; then
    printf 'failed: expected compiler %s, got %s\n' "$compiler" "$banner" \
      >"$project_out/dcc.validation"
    echo "$name: failed (expected compiler $compiler); $message_count message(s), exit $dcc_exit" >&2
    status=1
    continue
  fi
  if grep -qE '(Fatal|Error): F[0-9]{4}' "$project_out/dcc.raw"; then
    printf 'failed: fatal compiler diagnostic\n' >"$project_out/dcc.validation"
    echo "$name: failed (fatal compiler diagnostic); $message_count message(s), exit $dcc_exit" >&2
    status=1
    continue
  fi
  if [[ $dcc_exit -eq 1 ]] && ! grep -qE 'Error: E[0-9]{4} ' "$project_out/dcc.raw"; then
    printf 'failed: dcc exited 1 without an error diagnostic\n' >"$project_out/dcc.validation"
    echo "$name: failed (exit 1 without error); $message_count message(s)" >&2
    status=1
    continue
  fi
  if [[ $dcc_exit -ne 0 && $dcc_exit -ne 1 ]]; then
    printf 'failed: dcc exited %s\n' "$dcc_exit" >"$project_out/dcc.validation"
    echo "$name: failed (dcc exit $dcc_exit); $message_count message(s)" >&2
    status=1
    continue
  fi

  printf 'valid\n' >"$project_out/dcc.validation"
  echo "$name: Delphi $compiler $selected_platform, $message_count message(s), exit $dcc_exit"

  if [[ -n $lsp_binary ]]; then
    echo "$name: collecting pascal-lsp diagnostics"
    rm -f -- "$project_out/lsp.txt" "$project_out/lsp.meta.json" \
      "$project_out/lsp.status" "$project_out/lsp.validation" "$project_out/report.md"
    if (
      cd "$root"
      "$cargo_bin" run -q -p pascal-lsp --example lsp_corpus_diagnostics -- \
        "$lsp_binary" "$command_json" "$project_out/lsp.txt" \
        --timeout-secs "$lsp_timeout_seconds"
    ) >"$project_out/lsp-driver.log" 2>&1; then
      printf 'valid\n' >"$project_out/lsp.validation"
    else
      lsp_exit=$?
      printf '%s\n' "$lsp_exit" >"$project_out/lsp.status"
      printf 'failed: diagnostics driver exited %s\n' "$lsp_exit" >"$project_out/lsp.validation"
      rm -f -- "$project_out/lsp.txt" "$project_out/lsp.meta.json" "$project_out/report.md"
      echo "$name: LSP diagnostics driver failed (exit $lsp_exit)" >&2
      status=1
    fi
  fi

  if ((run_compare)); then
    lsp_ready=0
    if [[ -f $project_out/lsp.txt && -f $project_out/lsp.validation && \
        $(<"$project_out/lsp.validation") == "valid" ]]; then
      lsp_ready=1
    fi
    if ((lsp_ready)); then
      echo "$name: comparing DCC and pascal-lsp diagnostics"
      if ! (
        cd "$root"
        "$cargo_bin" run -q -p pascal-lsp --example corpus_compare -- \
          "$command_json" "$project_out/dcc.raw" "$project_out/lsp.txt" \
          "$project_out/report.md" --meta "$project_out/lsp.meta.json"
      ) >"$project_out/compare.log" 2>&1; then
        echo "$name: comparison report failed" >&2
        status=1
      fi
    else
      echo "$name: comparison skipped (no valid LSP diagnostics output)" >&2
      status=1
    fi
  fi
done <<<"$records"

if [[ $selected_count -eq 0 ]]; then
  echo "no manifest projects matched the requested name(s)" >&2
  exit 2
fi
printf 'summary: %s selected project(s), %s dcc run(s)\n' "$selected_count" "$dcc_runs"
if ((${#skipped_projects[@]})); then
  printf 'skipped projects: %s\n' "${skipped_projects[*]}"
fi
if [[ $dcc_runs -eq 0 ]]; then
  echo "no project was compiled; all selected projects were skipped or failed command extraction" >&2
  status=1
fi
exit "$status"
