#!/usr/bin/env bash
set -Eeuo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
script="${DCC_CORPUS_SCRIPT:-$repo_root/scripts/dcc-corpus.sh}"
tmp="$(mktemp -d)"
trap 'rm -rf -- "$tmp"' EXIT

mkdir -p "$tmp/bin" "$tmp/prefix/drive_c"
: >"$tmp/corpus.toml"

cat >"$tmp/bin/cargo" <<'SH'
#!/usr/bin/env bash
set -Eeuo pipefail
if [[ " $* " == *" --list "* ]]; then
  printf 'synthetic\t%s\t\t\t%s\n' "$DCC_FAKE_DPROJ" "${DCC_FAKE_INIT_OPTIONS:-}"
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

if [[ " ${args[*]} " == *" --fingerprint-only "* ]]; then
  printf '%s\n' "$DCC_FAKE_FINGERPRINT"
  exit 0
fi

case "$example" in
  dcc_corpus_command)
    case "$DCC_FAKE_MODE" in
      skipped)
        printf '{"skip":"synthetic unavailable"}\n'
        ;;
      incomplete)
        printf '{"incomplete":true}\n'
        ;;
      failed-lsp|timeout-lsp|valid|valid-lsp|dcc-errors|dcc-error-output|dcc-fatal|dcc-nonzero-no-error)
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
    output_file="${args[output_index]}"
    printf 'fresh diagnostics\n' >"$output_file"
    init_options=""
    timeout_seconds=NONE
    for ((index = separator + 4; index < ${#args[@]}; index += 1)); do
      case ${args[index]} in
        --init-options)
          init_options="$(<"${args[index + 1]}")"
          ;;
        --timeout-secs)
          timeout_seconds=${args[index + 1]}
          ;;
      esac
    done
    printf '%s\n' "${init_options:-NONE}" >"$DCC_FAKE_INIT_OPTIONS_RECORD"
    printf '%s\n' "$timeout_seconds" >"${DCC_FAKE_TIMEOUT_RECORD:-/dev/null}"
    meta_path="${output_file%.*}.meta.json"
    if [[ $DCC_FAKE_MODE == timeout-lsp ]]; then
      printf '{"index_timed_out":true,"unanalysed_files":[],"run_fingerprint":"%s","server_binary":"%s"}\n' \
        "$DCC_FAKE_FINGERPRINT" "$DCC_FAKE_SERVER_BINARY" >"$meta_path"
    else
      printf '{"index_timed_out":false,"unanalysed_files":[],"run_fingerprint":"%s","server_binary":"%s"}\n' \
        "$DCC_FAKE_FINGERPRINT" "$DCC_FAKE_SERVER_BINARY" >"$meta_path"
    fi
    if [[ $DCC_FAKE_MODE == failed-lsp || $DCC_FAKE_MODE == timeout-lsp ]]; then
      exit 7
    fi
    exit 0
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
    for ((index = separator + 1; index < ${#args[@]}; index += 1)); do
      if [[ ${args[index]} == --dcc-excluded ]]; then
        report_index=$((separator + 4))
        printf '# Corpus comparison\n\nexcluded: dcc errors (%s)\n' \
          "${args[index + 1]}" >"${args[report_index]}"
      fi
    done
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
if [[ $DCC_FAKE_MODE == dcc-errors || $DCC_FAKE_MODE == dcc-error-output ]]; then
  printf 'Z:%s(1) Error: E2003 Undeclared identifier\n' "$source_wine"
  [[ $DCC_FAKE_MODE != dcc-errors ]] || exit 1
  exit 0
fi
if [[ $DCC_FAKE_MODE == dcc-fatal ]]; then
  printf 'Fatal: compiler could not continue\n'
  exit 0
fi
if [[ $DCC_FAKE_MODE == dcc-nonzero-no-error ]]; then
  printf 'Z:%s(1) Warning: W1036 synthetic warning\n' "$source_wine"
  exit 1
fi
printf 'Z:%s(1) Warning: W1036 synthetic warning\n' "$source_wine"
SH
mkdir -p "$tmp/workspace/project"
: >"$tmp/workspace/.lint4d.toml"
: >"$tmp/workspace/project/project.dproj"
touch "$tmp/workspace/project/Unit.pas" "$tmp/bin/dcc"
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
export DCC_FAKE_DPROJ="$tmp/workspace/project/project.dproj"
export DCC_FAKE_ROOT="$tmp/workspace/project"
export DCC_FAKE_DCC="$tmp/bin/dcc"
export DCC_FAKE_SOURCE="$tmp/workspace/project/Unit.pas"
export DCC_FAKE_COMPARE_MARKER="$tmp/compare-called"
export DCC_FAKE_FINGERPRINT=fresh-fingerprint
export DCC_FAKE_SERVER_BINARY="$tmp/bin/pascal-lsp"
export DCC_FAKE_INIT_OPTIONS_RECORD="$tmp/init-options-record"

for mode in skipped incomplete; do
  export DCC_FAKE_MODE=$mode
  export LINT4D_CORPUS_OUT="$tmp/out-$mode"
  if "$script" synthetic >"$tmp/$mode.log" 2>&1; then
    echo "$mode-only corpus unexpectedly succeeded without a dcc run" >&2
    exit 1
  fi
  grep -q 'summary: 1 selected project(s), 0 dcc run(s)' "$tmp/$mode.log" || {
    cat "$tmp/$mode.log" >&2
    exit 1
  }
  grep -q 'skipped projects: synthetic (' "$tmp/$mode.log" || {
    cat "$tmp/$mode.log" >&2
    exit 1
  }
done

help_output="$("$script" --help)"
[[ $help_output == "Usage: "* ]] || {
  echo "--help did not print the usage line" >&2
  exit 1
}

export DCC_FAKE_MODE=valid
export LINT4D_CORPUS_OUT="$tmp/workspace/project/direct-output"
if "$script" synthetic >"$tmp/direct-output.log" 2>&1; then
  echo "corpus output inside a project root was accepted" >&2
  exit 1
fi
grep -q 'inside a corpus project root' "$tmp/direct-output.log" || {
  cat "$tmp/direct-output.log" >&2
  echo "direct output path was rejected for an unexpected reason" >&2
  exit 1
}
[[ ! -e $LINT4D_CORPUS_OUT ]] || {
  echo "direct output path created a directory inside the project root" >&2
  exit 1
}

ln -s "$tmp/workspace/project" "$tmp/project-link"
export LINT4D_CORPUS_OUT="$tmp/project-link/symlink-output"
if "$script" synthetic >"$tmp/symlink-output.log" 2>&1; then
  echo "symlinked corpus output inside a project root was accepted" >&2
  exit 1
fi
grep -q 'inside a corpus project root' "$tmp/symlink-output.log" || {
  echo "symlinked output path was rejected for an unexpected reason" >&2
  exit 1
}
[[ ! -e $tmp/workspace/project/symlink-output ]] || {
  echo "symlinked output path created a directory inside the project root" >&2
  exit 1
}

export LINT4D_CORPUS_OUT="$tmp/out-tmpdir"
export TMPDIR="$tmp/workspace/project"
if "$script" synthetic >"$tmp/tmpdir-output.log" 2>&1; then
  echo "temporary directory inside a project root was accepted" >&2
  exit 1
fi
grep -q 'inside a corpus project root' "$tmp/tmpdir-output.log" || {
  echo "temporary directory was rejected for an unexpected reason" >&2
  exit 1
}

export TMPDIR=/tmp
export LINT4D_CORPUS_OUT="$tmp/workspace/workspace-output"
if "$script" synthetic >"$tmp/workspace-output.log" 2>&1; then
  echo "corpus output inside the LSP workspace root was accepted" >&2
  exit 1
fi
grep -q 'inside a corpus project root' "$tmp/workspace-output.log" || {
  echo "LSP workspace output path was rejected for an unexpected reason" >&2
  exit 1
}
[[ ! -e $LINT4D_CORPUS_OUT ]] || {
  echo "workspace output path created a directory inside the LSP root" >&2
  exit 1
}

export LINT4D_CORPUS_OUT="$tmp/out-output-file-link"
mkdir -p "$LINT4D_CORPUS_OUT/synthetic"
printf 'untouched\n' >"$tmp/workspace/project/protected.txt"
ln -s "$tmp/workspace/project/protected.txt" \
  "$LINT4D_CORPUS_OUT/synthetic/command.json"
if "$script" synthetic >"$tmp/output-file-link.log" 2>&1; then
  echo "symlinked output file inside a corpus root was accepted" >&2
  exit 1
fi
grep -q 'inside a corpus project root' "$tmp/output-file-link.log" || {
  echo "symlinked output file was rejected for an unexpected reason" >&2
  exit 1
}
[[ $(<"$tmp/workspace/project/protected.txt") == untouched ]] || {
  echo "symlinked output file modified a project file" >&2
  exit 1
}

export DCC_FAKE_MODE=failed-lsp
unset TMPDIR
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

export LINT4D_CORPUS_OUT="$tmp/out-cached-stale-fingerprint"
cached_project_out="$LINT4D_CORPUS_OUT/synthetic"
mkdir -p "$cached_project_out"
printf 'cached diagnostics\n' >"$cached_project_out/lsp.txt"
printf 'valid\n' >"$cached_project_out/lsp.validation"
printf '{"run_fingerprint":"stale-fingerprint","server_binary":"%s"}\n' \
  "$DCC_FAKE_SERVER_BINARY" >"$cached_project_out/lsp.meta.json"
rm -f -- "$DCC_FAKE_COMPARE_MARKER"
if "$script" --compare synthetic >"$tmp/stale-fingerprint.log" 2>&1; then
  echo "comparison accepted stale LSP findings" >&2
  exit 1
fi
grep -q 'fingerprint mismatch' "$tmp/stale-fingerprint.log" || {
  cat "$tmp/stale-fingerprint.log" >&2
  echo "stale LSP findings were rejected for an unexpected reason" >&2
  exit 1
}
[[ ! -e $DCC_FAKE_COMPARE_MARKER ]] || {
  echo "comparison ran with stale LSP findings" >&2
  exit 1
}

export LINT4D_CORPUS_OUT="$tmp/out-cached-valid"
cached_project_out="$LINT4D_CORPUS_OUT/synthetic"
mkdir -p "$cached_project_out"
printf 'cached diagnostics\n' >"$cached_project_out/lsp.txt"
printf 'valid\n' >"$cached_project_out/lsp.validation"
printf '{"run_fingerprint":"%s","server_binary":"%s"}\n' \
  "$DCC_FAKE_FINGERPRINT" "$DCC_FAKE_SERVER_BINARY" >"$cached_project_out/lsp.meta.json"
rm -f -- "$DCC_FAKE_COMPARE_MARKER"
"$script" --compare synthetic >"$tmp/cached-valid.log" 2>&1
[[ -e $DCC_FAKE_COMPARE_MARKER ]] || {
  echo "comparison did not accept cached LSP output with valid validation" >&2
  exit 1
}

export DCC_FAKE_MODE=valid-lsp
export DCC_FAKE_INIT_OPTIONS='{"sourcePaths":["C:/custom/rtl"]}'
export LINT4D_CORPUS_OUT="$tmp/out-manifest-init-options"
rm -f -- "$DCC_FAKE_COMPARE_MARKER" "$DCC_FAKE_INIT_OPTIONS_RECORD"
"$script" --lsp "$tmp/bin/pascal-lsp" --compare synthetic \
  >"$tmp/manifest-init-options.log" 2>&1
[[ $(<"$DCC_FAKE_INIT_OPTIONS_RECORD") == "$DCC_FAKE_INIT_OPTIONS" ]] || {
  echo "manifest initialization options were not passed to the LSP driver" >&2
  exit 1
}

unset DCC_FAKE_INIT_OPTIONS
export LINT4D_CORPUS_OUT="$tmp/out-default-init-options"
rm -f -- "$DCC_FAKE_COMPARE_MARKER" "$DCC_FAKE_INIT_OPTIONS_RECORD"
"$script" --lsp "$tmp/bin/pascal-lsp" --compare synthetic \
  >"$tmp/default-init-options.log" 2>&1
[[ $(<"$DCC_FAKE_INIT_OPTIONS_RECORD") == NONE ]] || {
  echo "script overrode the LSP driver's default initialization options" >&2
  exit 1
}

export LINT4D_LSP_TIMEOUT=73
export DCC_FAKE_TIMEOUT_RECORD="$tmp/lsp-timeout-record"
export LINT4D_CORPUS_OUT="$tmp/out-lsp-timeout-option"
rm -f -- "$DCC_FAKE_COMPARE_MARKER" "$DCC_FAKE_TIMEOUT_RECORD"
"$script" --lsp "$tmp/bin/pascal-lsp" --compare --lsp-timeout-secs 1800 synthetic \
  >"$tmp/lsp-timeout-option.log" 2>&1
[[ $(<"$DCC_FAKE_TIMEOUT_RECORD") == 1800 ]] || {
  echo "script did not pass the CLI LSP timeout to the diagnostics driver" >&2
  exit 1
}

export LINT4D_CORPUS_OUT="$tmp/out-lsp-timeout-env"
rm -f -- "$DCC_FAKE_COMPARE_MARKER" "$DCC_FAKE_TIMEOUT_RECORD"
"$script" --lsp "$tmp/bin/pascal-lsp" --compare synthetic \
  >"$tmp/lsp-timeout-env.log" 2>&1
[[ $(<"$DCC_FAKE_TIMEOUT_RECORD") == 73 ]] || {
  echo "script did not retain LINT4D_LSP_TIMEOUT as its default" >&2
  exit 1
}

for scenario in dcc-errors:1 dcc-error-output:1 dcc-fatal:1 dcc-nonzero-no-error:0; do
  IFS=: read -r DCC_FAKE_MODE expected_count <<<"$scenario"
  export DCC_FAKE_MODE
  export LINT4D_CORPUS_OUT="$tmp/out-$DCC_FAKE_MODE"
  error_project_out="$LINT4D_CORPUS_OUT/synthetic"
  mkdir -p "$error_project_out"
  for stale_file in lsp.txt lsp.meta.json lsp.status lsp.validation report.md compare.log; do
    printf 'stale output\n' >"$error_project_out/$stale_file"
  done
  rm -f -- "$DCC_FAKE_COMPARE_MARKER"
  "$script" --compare synthetic >"$tmp/$DCC_FAKE_MODE.log" 2>&1
  expected_validation="excluded: dcc errors ($expected_count)"
  [[ $(<"$error_project_out/dcc.validation") == "$expected_validation" ]] || {
    echo "$DCC_FAKE_MODE did not exclude the comparison baseline" >&2
    exit 1
  }
  grep -q "$expected_validation" "$error_project_out/report.md" || {
    echo "$DCC_FAKE_MODE was not listed in the comparison report" >&2
    exit 1
  }
  [[ -e $DCC_FAKE_COMPARE_MARKER ]] || {
    echo "$DCC_FAKE_MODE exclusion report was not generated" >&2
    exit 1
  }
  [[ ! -s $error_project_out/lsp.txt ]] || {
    echo "$DCC_FAKE_MODE reused stale LSP findings" >&2
    exit 1
  }
  for stale_file in lsp.meta.json lsp.status lsp.validation; do
    [[ ! -e $error_project_out/$stale_file ]] || {
      echo "$DCC_FAKE_MODE retained stale $stale_file" >&2
      exit 1
    }
  done
  for report_file in report.md compare.log; do
    if grep -q 'stale output' "$error_project_out/$report_file"; then
      echo "$DCC_FAKE_MODE retained stale $report_file" >&2
      exit 1
    fi
  done
done
