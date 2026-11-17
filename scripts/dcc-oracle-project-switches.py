#!/usr/bin/env python3
"""Emit dcc switches for a case's selected .dproj warning properties."""

from __future__ import annotations

import re
import sys
import xml.etree.ElementTree as ET
from pathlib import Path


PROPERTY_REFERENCE = re.compile(r"\$\(([^)]+)\)")
COMPARISON = re.compile(r"^(.*?)\s*(==|!=)\s*(.*?)$")


def local_name(tag: str) -> str:
    return tag.rsplit("}", 1)[-1]


def switch_properties(table_path: Path) -> tuple[set[str], set[str]]:
    warning_ids: set[str] = set()
    in_codes = False
    for line in table_path.read_text(encoding="utf-8").splitlines():
        if line.startswith("["):
            in_codes = line == "[codes]"
            continue
        if in_codes and line.strip():
            fields = line.split()
            if len(fields) != 5:
                raise ValueError(f"{table_path}: malformed code row {line!r}")
            if fields[3] != "-":
                warning_ids.add(f"dcc_{fields[3].lower()}")
    return warning_ids | {"dcc_hints", "dcc_warnings"}, warning_ids


def strip_outer_parentheses(expression: str) -> str:
    expression = expression.strip()
    while expression.startswith("(") and expression.endswith(")"):
        depth = 0
        quote: str | None = None
        closes_at_end = False
        for index, character in enumerate(expression):
            if quote:
                if character == quote:
                    quote = None
            elif character in "'\"":
                quote = character
            elif character == "(":
                depth += 1
            elif character == ")":
                depth -= 1
                if depth == 0:
                    closes_at_end = index == len(expression) - 1
                    break
        if not closes_at_end:
            break
        expression = expression[1:-1].strip()
    return expression


def split_boolean(expression: str, operator: str) -> list[str] | None:
    depth = 0
    quote: str | None = None
    parts: list[str] = []
    start = 0
    index = 0
    lowered = expression.lower()
    while index < len(expression):
        character = expression[index]
        if quote:
            if character == quote:
                quote = None
        elif character in "'\"":
            quote = character
        elif character == "(":
            depth += 1
        elif character == ")":
            depth -= 1
        elif depth == 0 and lowered.startswith(operator, index):
            before = expression[index - 1] if index else " "
            end = index + len(operator)
            after = expression[end] if end < len(expression) else " "
            if before.isspace() and after.isspace():
                parts.append(expression[start:index].strip())
                start = end
                index = end
                continue
        index += 1
    if not parts:
        return None
    parts.append(expression[start:].strip())
    return parts


def expand(
    value: str,
    properties: dict[str, str],
    unknown: set[str],
    *,
    missing_is_unknown: bool,
) -> tuple[str, bool]:
    is_unknown = False

    def replace(match: re.Match[str]) -> str:
        nonlocal is_unknown
        key = match.group(1).strip().lower()
        if key in unknown:
            is_unknown = True
            return ""
        if key not in properties:
            is_unknown |= missing_is_unknown
            return ""
        return properties[key]

    expanded = PROPERTY_REFERENCE.sub(replace, value)
    if "$(" in expanded:
        is_unknown = True
    return expanded, is_unknown


def condition_value(
    expression: str, properties: dict[str, str], unknown: set[str]
) -> bool | None:
    expression = strip_outer_parentheses(expression)
    alternatives = split_boolean(expression, "or")
    if alternatives:
        results = [condition_value(part, properties, unknown) for part in alternatives]
        if any(result is True for result in results):
            return True
        return False if all(result is False for result in results) else None
    conjunction = split_boolean(expression, "and")
    if conjunction:
        results = [condition_value(part, properties, unknown) for part in conjunction]
        if any(result is False for result in results):
            return False
        return True if all(result is True for result in results) else None

    comparison = COMPARISON.match(expression)
    if not comparison:
        return None
    left, operator, right = comparison.groups()

    def operand(raw: str) -> tuple[str, bool]:
        raw = raw.strip()
        if len(raw) >= 2 and raw[0] == raw[-1] and raw[0] in "'\"":
            raw = raw[1:-1]
        return expand(raw, properties, unknown, missing_is_unknown=False)

    left_value, left_unknown = operand(left)
    right_value, right_unknown = operand(right)
    if left_unknown or right_unknown:
        return None
    equal = left_value.casefold() == right_value.casefold()
    return equal if operator == "==" else not equal


def read_case_properties(
    project_path: Path, selected_config: str, selected_platform: str
) -> tuple[dict[str, str], set[str]]:
    root = ET.parse(project_path).getroot()
    if any(
        local_name(element.tag) in {"Import", "ImportGroup"}
        for element in root.iter()
    ):
        raise ValueError(
            f"{project_path}: recorder does not evaluate imported MSBuild files"
        )
    property_groups = [
        child for child in root if local_name(child.tag) == "PropertyGroup"
    ]
    nested_property_groups = [
        element
        for element in root.iter()
        if local_name(element.tag) == "PropertyGroup"
        and element not in property_groups
    ]
    if nested_property_groups:
        raise ValueError(
            f"{project_path}: recorder only evaluates root-level PropertyGroup elements"
        )
    properties = {"config": selected_config, "platform": selected_platform}
    global_properties = {
        name for name, value in (("config", selected_config), ("platform", selected_platform)) if value
    }
    unknown: set[str] = set()

    for group in property_groups:
        group_condition = group.attrib.get("Condition")
        if group_condition:
            matches = condition_value(group_condition, properties, unknown)
            if matches is None:
                raise ValueError(
                    f"{project_path}: cannot evaluate PropertyGroup Condition {group_condition!r}"
                )
            if not matches:
                continue

        for element in list(group):
            name = local_name(element.tag)
            key = name.lower()
            if key in global_properties:
                continue
            condition = element.attrib.get("Condition")
            if condition:
                matches = condition_value(condition, properties, unknown)
                if matches is None:
                    raise ValueError(
                        f"{project_path}: cannot evaluate {name} Condition {condition!r}"
                    )
                if not matches:
                    continue
            raw = "".join(element.itertext()).strip()
            value, is_unknown = expand(
                raw, properties, unknown, missing_is_unknown=True
            )
            if is_unknown:
                unknown.add(key)
                properties.pop(key, None)
            else:
                unknown.discard(key)
                properties[key] = value

    return properties, unknown


def dcc_flags(
    project_path: Path,
    table_path: Path,
    selected_config: str,
    selected_platform: str,
) -> list[str]:
    allowed, warning_ids = switch_properties(table_path)
    properties, unknown = read_case_properties(
        project_path, selected_config, selected_platform
    )
    flags: list[str] = []
    for key, switch_prefix in (("dcc_hints", "-H"), ("dcc_warnings", "-W")):
        if key not in properties:
            continue
        if key in unknown:
            raise ValueError(f"{project_path}: {key} is unevaluable")
        value = properties[key].strip().casefold()
        if value not in {"true", "false"}:
            raise ValueError(f"{project_path}: {key} must be true or false, got {value!r}")
        flags.append(f"{switch_prefix}{'+' if value == 'true' else '-'}")

    for key in sorted(warning_ids):
        if key not in properties and key not in unknown:
            continue
        if key in unknown:
            raise ValueError(f"{project_path}: {key} is unevaluable")
        value = properties[key].strip().casefold()
        if value not in {"true", "false", "error"}:
            raise ValueError(
                f"{project_path}: {key} must be true, false, or error, got {value!r}"
            )
        sign = {"true": "+", "false": "-", "error": "^"}[value]
        flags.append(f"-W{sign}{key.removeprefix('dcc_').upper()}")

    # The `allowed` set includes only switchable warning properties plus the
    # two global switches. Keep this assertion next to the table-derived list
    # so the recorder cannot silently start accepting unrelated DCC settings.
    assert warning_ids | {"dcc_hints", "dcc_warnings"} == allowed
    return flags


def main(argv: list[str]) -> int:
    if len(argv) != 5:
        print(
            "usage: dcc-oracle-project-switches.py <project.dproj> "
            "<switch-table.txt> <config> <platform>",
            file=sys.stderr,
        )
        return 2
    project_path, table_path = Path(argv[1]), Path(argv[2])
    try:
        for flag in dcc_flags(project_path, table_path, argv[3], argv[4]):
            print(flag)
    except (OSError, ET.ParseError, ValueError) as error:
        print(error, file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
