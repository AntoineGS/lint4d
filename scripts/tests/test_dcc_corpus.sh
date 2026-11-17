#!/usr/bin/env bash
set -Eeuo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
script="${DCC_CORPUS_SCRIPT:-$repo_root/scripts/dcc-corpus.sh}"
tmp="$(mktemp -d)"
trap 'rm -rf -- "$tmp"' EXIT

mkdir -p "$tmp/bin" "$tmp/prefix/drive_c"
: >"$tmp/corpus.toml"
: >"$tmp/project.dproj"

cat >"$tmp/bin/cargo" <<'SH'
#!/usr/bin/env bash
set -Eeuo pipefail
if [[ " $* " == *" --list "* ]]; then
  printf 'synthetic\t%s\t\t\n' "$DCC_FAKE_DPROJ"
  exit 0
fi
args=("$@")
example=""
separator=-1
for ((index = 0; index < ${#args[@]}; index += 1)); do
  if [[ ${args[index]} == --example ]]; then
    example=${args[index + 1]}
  elif [[ ${args[index]} == -- ]]; then
    separator=$index
    break
  fi
done

case "$example" in
  dcc_corpus_command)
    case "$DCC_FAKE_MODE" in
      skipped)
        printf '{"skip":"synthetic unavailable"}\n'
        ;;
      incomplete)
        printf '{"incomplete":true}\n'
        ;;
      failed-lsp|timeout-lsp)
        python3 - "$DCC_FAKE_ROOT" "$DCC_FAKE_DCC" "$DCC_FAKE_SOURCE" <<'PY'
import json
import os
import sys

root, dcc, source = sys.argv[1:]
print(json.dumps({
    "compiler": "37.0",
    "platform": "Win32",
    "dcc": dcc,
    "main_source": source,
    "project_file": os.path.join(root, "project.dproj"),
    "project_root": root,
    "workspace_root": root,
    "files": [source],
    "warnings": [],
    "unit_paths": [],
    "include_paths": [],
    "resource_paths": [],
    "object_paths": [],
    "namespaces": [],
    "defines": [],
    "incomplete": False,
}))
PY
        ;;
      *)
        echo "unexpected fake cargo mode" >&2
        exit 2
        ;;
    esac
    ;;
  lsp_corpus_diagnostics)
    output_index=$((separator + 3))
    printf 'stale diagnostics written before shutdown failure\n' >"${args[output_index]}"
    if [[ $DCC_FAKE_MODE == timeout-lsp ]]; then
      meta_path="${args[output_index]%.*}.meta.json"
      printf '{"index_timed_out":true,"unanalysed_files":[]}\n' >"$meta_path"
    fi
    exit 7
    ;;
  corpus_compare)
    if [[ $DCC_FAKE_MODE == timeout-lsp ]]; then
      report_index=$((separator + 4))
      cat >"${args[report_index]}" <<'MD'
# Corpus comparison

## Per-code parity

Parity counts unavailable: index timeout.

## Excluded

- index timeout
MD
    fi
    touch "$DCC_FAKE_COMPARE_MARKER"
    ;;
  *)
    echo "unexpected fake cargo example: $example" >&2
    exit 2
    ;;
esac
SH
cat >"$tmp/bin/wine" <<'SH'
#!/usr/bin/env bash
set -Eeuo pipefail
if (($# == 1)); then
  printf 'Fake compiler usage\n'
  exit 0
fi
source_wine="${DCC_FAKE_SOURCE//\//\\}"
printf 'Embarcadero Delphi for Win32 compiler version 37.0\n'
printf 'Z:%s(1) Warning: W1036 synthetic warning\n' "$source_wine"
SH
mkdir -p "$tmp/project"
touch "$tmp/project/Unit.pas" "$tmp/bin/dcc"
cat >"$tmp/bin/pascal-lsp" <<'SH'
#!/usr/bin/env bash
exit 0
SH
chmod +x "$tmp/bin/cargo" "$tmp/bin/wine" "$tmp/bin/pascal-lsp"

export PATH="$tmp/bin:$PATH"
export CARGO="$tmp/bin/cargo"
export PYTHON=python3
export LINT4D_CORPUS_MANIFEST="$tmp/corpus.toml"
export DCC_CORPUS_WINEPREFIX="$tmp/prefix"
export DCC_FAKE_DPROJ="$tmp/project.dproj"
export DCC_FAKE_ROOT="$tmp/project"
export DCC_FAKE_DCC="$tmp/bin/dcc"
export DCC_FAKE_SOURCE="$tmp/project/Unit.pas"
export DCC_FAKE_COMPARE_MARKER="$tmp/compare-called"

for mode in skipped incomplete; do
  export DCC_FAKE_MODE=$mode
  export LINT4D_CORPUS_OUT="$tmp/out-$mode"
  if "$script" synthetic >"$tmp/$mode.log" 2>&1; then
    echo "$mode-only corpus unexpectedly succeeded without a dcc run" >&2
    exit 1
  fi
  grep -q 'summary: 1 selected project(s), 0 dcc run(s)' "$tmp/$mode.log"
  grep -q 'skipped projects: synthetic (' "$tmp/$mode.log"
done

help_output="$("$script" --help)"
[[ $help_output == "Usage: "* ]] || {
  echo "--help did not print the usage line" >&2
  exit 1
}

export DCC_FAKE_MODE=failed-lsp
export LINT4D_CORPUS_OUT="$tmp/out-failed-lsp"
failed_project_out="$LINT4D_CORPUS_OUT/synthetic"
mkdir -p "$failed_project_out"
printf 'stale report\n' >"$failed_project_out/report.md"
printf 'stale diagnostics\n' >"$failed_project_out/lsp.txt"
if "$script" --lsp "$tmp/bin/pascal-lsp" --compare synthetic \
  >"$tmp/failed-lsp.log" 2>&1; then
  echo "failed LSP collection unexpectedly succeeded" >&2
  exit 1
fi
[[ ! -e $failed_project_out/lsp.txt ]] || {
  echo "failed LSP collection left diagnostics that can be compared" >&2
  exit 1
}
[[ ! -e $failed_project_out/report.md ]] || {
  echo "failed LSP collection left a stale comparison report" >&2
  exit 1
}

if "$script" --compare synthetic >"$tmp/compare-after-failure.log" 2>&1; then
  echo "comparison unexpectedly succeeded with failed cached LSP output" >&2
  exit 1
fi
[[ ! -e $DCC_FAKE_COMPARE_MARKER ]] || {
  echo "comparison ran against diagnostics from a failed LSP collection" >&2
  exit 1
}

export DCC_FAKE_MODE=timeout-lsp
export LINT4D_CORPUS_OUT="$tmp/out-timeout-lsp"
timeout_project_out="$LINT4D_CORPUS_OUT/synthetic"
rm -f -- "$DCC_FAKE_COMPARE_MARKER"
if "$script" --lsp "$tmp/bin/pascal-lsp" --compare synthetic \
  >"$tmp/timeout-lsp.log" 2>&1; then
  echo "index timeout unexpectedly produced a successful corpus status" >&2
  exit 1
fi
[[ $(<"$timeout_project_out/lsp.validation") == "index-timeout" ]] || {
  echo "index timeout was not distinguished from other LSP failures" >&2
  exit 1
}
grep -q '## Excluded' "$timeout_project_out/report.md"
grep -q 'index timeout' "$timeout_project_out/report.md"
[[ -e $DCC_FAKE_COMPARE_MARKER ]] || {
  echo "index-timeout exclusion report was not generated" >&2
  exit 1
}

for validation in failed missing; do
  export LINT4D_CORPUS_OUT="$tmp/out-cached-$validation"
  cached_project_out="$LINT4D_CORPUS_OUT/synthetic"
  mkdir -p "$cached_project_out"
  printf 'cached diagnostics\n' >"$cached_project_out/lsp.txt"
  if [[ $validation == failed ]]; then
    printf 'failed: previous driver error\n' >"$cached_project_out/lsp.validation"
  fi
  rm -f -- "$DCC_FAKE_COMPARE_MARKER"
  if "$script" --compare synthetic >"$tmp/cached-$validation.log" 2>&1; then
    echo "comparison unexpectedly accepted cached LSP output with $validation validation" >&2
    exit 1
  fi
  [[ ! -e $DCC_FAKE_COMPARE_MARKER ]] || {
    echo "comparison ran with cached LSP output and $validation validation" >&2
    exit 1
  }
done

export LINT4D_CORPUS_OUT="$tmp/out-cached-valid"
cached_project_out="$LINT4D_CORPUS_OUT/synthetic"
mkdir -p "$cached_project_out"
printf 'cached diagnostics\n' >"$cached_project_out/lsp.txt"
printf 'valid\n' >"$cached_project_out/lsp.validation"
rm -f -- "$DCC_FAKE_COMPARE_MARKER"
"$script" --compare synthetic >"$tmp/cached-valid.log" 2>&1
[[ -e $DCC_FAKE_COMPARE_MARKER ]] || {
  echo "comparison did not accept cached LSP output with valid validation" >&2
  exit 1
}
