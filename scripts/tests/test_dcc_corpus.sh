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
case "$DCC_FAKE_MODE" in
  skipped)
    printf '{"skip":"synthetic unavailable"}\n'
    ;;
  incomplete)
    printf '{"incomplete":true}\n'
    ;;
  *)
    echo "unexpected fake cargo mode" >&2
    exit 2
    ;;
esac
SH
cat >"$tmp/bin/wine" <<'SH'
#!/usr/bin/env bash
exit 0
SH
chmod +x "$tmp/bin/cargo" "$tmp/bin/wine"

export PATH="$tmp/bin:$PATH"
export CARGO="$tmp/bin/cargo"
export PYTHON=python3
export LINT4D_CORPUS_MANIFEST="$tmp/corpus.toml"
export DCC_CORPUS_WINEPREFIX="$tmp/prefix"
export DCC_FAKE_DPROJ="$tmp/project.dproj"

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
