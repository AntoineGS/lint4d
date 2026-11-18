#!/usr/bin/env bash
# Shared validation for dcc switch-derivation probes.

dcc_validate_compiler_banner() {
  local expected_version=$1
  local banner=$2

  [[ $banner == *"compiler version $expected_version" ]]
}

dcc_classify_error_probe() {
  local code=$1
  local compiler_status=$2
  local output=$3
  local error_code
  local error_pattern
  local warning_pattern
  local error_count
  local expected_error_count
  local warning_count
  local error_line
  local warning_line

  if [[ ! $code =~ ^W[0-9]{4}$ || ! $compiler_status =~ ^[0-9]+$ ]]; then
    printf 'dcc-switch-probe: invalid code or compiler status\n' >&2
    return 2
  fi

  error_code="E${code#W}"
  error_pattern="(^|[[:space:]])Error: ${error_code}([[:space:]]|$)"
  warning_pattern="(^|[[:space:]])Warning: ${code}([[:space:]]|$)"
  error_count=$(grep -Ec '(^|[[:space:]])(Error|Fatal):' <<< "$output" || true)
  expected_error_count=$(grep -Ec "$error_pattern" <<< "$output" || true)
  warning_count=$(grep -Ec "$warning_pattern" <<< "$output" || true)
  error_line=$(grep -E "$error_pattern" <<< "$output" | head -n 1 || true)
  warning_line=$(grep -E "$warning_pattern" <<< "$output" | head -n 1 || true)

  if [[ $compiler_status == 1 \
    && $error_count == 1 \
    && $expected_error_count == 1 \
    && $warning_count == 0 ]]; then
    printf 'yes\t%s\n' "$error_line"
    return 0
  fi

  if [[ $compiler_status == 0 \
    && $error_count == 0 \
    && $expected_error_count == 0 \
    && $warning_count == 1 ]]; then
    printf 'no\t%s\n' "$warning_line"
    return 0
  fi

  printf 'dcc-switch-probe: inconclusive ERROR probe for %s (status %s)\n' \
    "$code" "$compiler_status" >&2
  return 1
}
