#!/usr/bin/env python3
"""Find unused-unit LSP findings and compare normalized DCC output."""

from __future__ import annotations

import argparse
import json
import os
import posixpath
import re
import secrets
import sys
from collections import Counter
from pathlib import Path


LSP_LINE = re.compile(
    r"^(?P<path>.+)\((?P<line>[0-9]+)\) (?P<severity>\S+) "
    r"(?P<code>\S+) (?P<source>\S+)(?: (?P<text>.*))?$"
)
UNUSED_UNIT_TEXT = re.compile(r"^Unit '([^']+)' is not used$")
DCC_LINE = re.compile(
    r"^(?P<path>.*)\([0-9]+\) "
    r"(?:Error|Warning|Hint|Fatal): (?P<code>[EWHF][0-9]{4}) (?P<text>.*)$"
)
IN_REFERENCE = re.compile(
    r"(?P<prefix>\bin[ \t]*)(?P<quote>['\"])(?P<path>[^'\"]+)(?P=quote)",
    re.IGNORECASE,
)
MODULE_HEADER = re.compile(
    r"^(?:\ufeff)?[ \t]*(?:unit|program|library)[ \t]+"
    r"[^;\r\n]*;[^\r\n]*(?P<newline>\r?\n|$)",
    re.IGNORECASE | re.MULTILINE,
)


def canonical_path(value: str | Path) -> Path:
    return Path(os.path.realpath(value))


def resolve_in_path(value: str, main_source: Path) -> Path | None:
    value = value.replace("\\", os.sep)
    if re.match(r"^[A-Za-z]:" + re.escape(os.sep), value):
        if value[:2].upper() != "Z:":
            return None
        value = value[2:]
    path = Path(value)
    if not path.is_absolute():
        path = main_source.parent / path
    return canonical_path(path)


def write_overlay_main(
    main_source: Path,
    original_source: Path,
    overlay_source_wine: str,
    overlay_main: Path,
) -> None:
    main_source = canonical_path(main_source)
    original_source = canonical_path(original_source)
    text = main_source.read_text(encoding="utf-8", errors="surrogateescape")

    def rewrite_reference(match: re.Match[str]) -> str:
        referenced_source = resolve_in_path(match["path"], main_source)
        if referenced_source != original_source:
            return match[0]
        return (
            match["prefix"]
            + match["quote"]
            + overlay_source_wine
            + match["quote"]
        )

    rewritten = IN_REFERENCE.sub(rewrite_reference, text)
    overlay_main.write_text(rewritten, encoding="utf-8", errors="surrogateescape")


def instrument_overlay_source(source: Path) -> str:
    text = source.read_text(encoding="utf-8", errors="surrogateescape")
    match = MODULE_HEADER.search(text)
    if not match:
        raise ValueError(f"{source}: Pascal module declaration was not found")

    nonce = f"lint4d-overlay-{secrets.token_hex(16)}"
    newline = match["newline"] or "\n"
    message = newline.join(
        (
            "{$IFOPT W+}",
            f"{{$MESSAGE WARN '{nonce}'}}",
            "{$ELSE}",
            "{$WARNINGS ON}",
            f"{{$MESSAGE WARN '{nonce}'}}",
            "{$WARNINGS OFF}",
            "{$ENDIF}",
            "",
        )
    )
    prefix = text[: match.end()]
    if not match["newline"]:
        prefix += newline
    source.write_text(
        prefix + message + text[match.end() :],
        encoding="utf-8",
        errors="surrogateescape",
    )
    return nonce


def normalized_path_key(value: str | Path) -> str:
    value = str(value).replace("\\", "/")
    if re.match(r"^Z:/", value, flags=re.IGNORECASE):
        value = value[2:]
    return posixpath.normpath(value).casefold()


def diagnostic_source_matches(value: str, source: Path) -> bool:
    path = dcc_path(value)
    if normalized_path_key(path) == normalized_path_key(canonical_path(source)):
        return True
    candidate = Path(value.replace("\\", "/"))
    return not candidate.is_absolute() and candidate.name.casefold() == source.name.casefold()


def strip_overlay_sentinel(
    raw_path: Path,
    output_path: Path,
    edited_source: Path,
    nonce: str,
) -> None:
    with raw_path.open(encoding="utf-8", errors="replace") as raw_file:
        lines = raw_file.readlines()
    sentinel_lines: list[int] = []
    for index, line in enumerate(lines):
        if nonce not in line:
            continue
        match = DCC_LINE.match(line.rstrip("\r\n"))
        if not match or match["code"] != "W1054":
            raise ValueError("overlay sentinel was not emitted as a message warning")
        if not diagnostic_source_matches(match["path"], edited_source):
            raise ValueError("overlay sentinel was attributed to a different source")
        sentinel_lines.append(index)
    if len(sentinel_lines) != 1:
        raise ValueError(
            f"expected one overlay sentinel, found {len(sentinel_lines)}"
        )
    sentinel_index = sentinel_lines[0]
    output_path.write_text(
        "".join(line for index, line in enumerate(lines) if index != sentinel_index),
        encoding="utf-8",
    )


def unused_unit_findings(lsp_path: Path, command_path: Path) -> list[tuple[Path, int, str]]:
    with command_path.open(encoding="utf-8") as command_file:
        command = json.load(command_file)
    files = command.get("files")
    if not isinstance(files, list) or not all(isinstance(path, str) for path in files):
        raise ValueError("command JSON files must be an array of paths")
    project_files = {canonical_path(path) for path in files}

    findings: list[tuple[Path, int, str]] = []
    with lsp_path.open(encoding="utf-8") as lsp_file:
        for line_number, line in enumerate(lsp_file, start=1):
            match = LSP_LINE.match(line.rstrip("\r\n"))
            if not match or match["code"] != "lint4d-unused-unit":
                continue
            if match["severity"] != "Hint":
                raise ValueError(
                    f"{lsp_path}:{line_number}: unused-unit finding is not a Hint"
                )
            unit_match = UNUSED_UNIT_TEXT.fullmatch(match["text"] or "")
            if not unit_match:
                raise ValueError(
                    f"{lsp_path}:{line_number}: unrecognized unused-unit message"
                )
            path = canonical_path(match["path"])
            if path in project_files:
                findings.append((path, int(match["line"]), unit_match[1]))
    return findings


def dcc_path(value: str) -> str:
    value = value.replace("\\", "/")
    if re.match(r"^Z:/", value, flags=re.IGNORECASE):
        return os.path.realpath(value[2:])
    return os.path.normpath(value)


def dcc_messages(
    raw_path: Path,
    project_root: Path,
    source_mappings: list[tuple[Path, Path]],
) -> Counter[tuple[str, str, str]]:
    project_root = canonical_path(project_root)
    canonical_mappings = [
        (canonical_path(overlay), canonical_path(original))
        for overlay, original in source_mappings
    ]
    messages: Counter[tuple[str, str, str]] = Counter()
    with raw_path.open(encoding="utf-8", errors="replace") as raw_file:
        for line in raw_file:
            match = DCC_LINE.match(line.rstrip("\r\n"))
            if not match:
                continue
            path = dcc_path(match["path"])
            path_key = normalized_path_key(path)
            for overlay, original in canonical_mappings:
                if path_key == normalized_path_key(overlay):
                    path = str(original)
                    break
            candidate = Path(path)
            if candidate.is_absolute():
                candidate = canonical_path(candidate)
                try:
                    path = candidate.relative_to(project_root).as_posix()
                except ValueError:
                    path = str(candidate)
            else:
                path = candidate.as_posix()
            messages[(path, match["code"], match["text"])] += 1
    return messages


def display_path(path: str, project_root: Path) -> str:
    candidate = Path(path)
    if candidate.is_absolute():
        try:
            return candidate.relative_to(canonical_path(project_root)).as_posix()
        except ValueError:
            return candidate.name
    return path


def first_difference(
    original: Counter[tuple[str, str, str]],
    edited: Counter[tuple[str, str, str]],
    project_root: Path,
) -> str:
    for message in sorted(original.keys() | edited.keys()):
        count = original[message] - edited[message]
        if count == 0:
            continue
        path, code, text = message
        side = "original only" if count > 0 else "edited only"
        return f"{side}: {display_path(path, project_root)} {code} {text}"
    return ""


def compare_outputs(
    original_path: Path,
    edited_path: Path,
    project_root: Path,
    edited_source: Path,
    original_source: Path,
    overlay_main: Path,
    original_main: Path,
) -> str:
    original = dcc_messages(original_path, project_root, [])
    edited = dcc_messages(
        edited_path,
        project_root,
        [
            (edited_source, original_source),
            (overlay_main, original_main),
        ],
    )
    difference = first_difference(original, edited, project_root)
    return f"differs: {difference}" if difference else "identical"


def main() -> int:
    parser = argparse.ArgumentParser()
    subparsers = parser.add_subparsers(dest="command", required=True)
    findings_parser = subparsers.add_parser("findings")
    findings_parser.add_argument("lsp_output", type=Path)
    findings_parser.add_argument("command_json", type=Path)
    overlay_parser = subparsers.add_parser("overlay-main")
    overlay_parser.add_argument("main_source", type=Path)
    overlay_parser.add_argument("original_source", type=Path)
    overlay_parser.add_argument("overlay_source_wine")
    overlay_parser.add_argument("overlay_main", type=Path)
    instrument_parser = subparsers.add_parser("instrument-overlay")
    instrument_parser.add_argument("source", type=Path)
    strip_sentinel_parser = subparsers.add_parser("strip-sentinel")
    strip_sentinel_parser.add_argument("raw_output", type=Path)
    strip_sentinel_parser.add_argument("stripped_output", type=Path)
    strip_sentinel_parser.add_argument("edited_source", type=Path)
    strip_sentinel_parser.add_argument("nonce")
    compare_parser = subparsers.add_parser("compare")
    compare_parser.add_argument("original_dcc", type=Path)
    compare_parser.add_argument("edited_dcc", type=Path)
    compare_parser.add_argument("project_root", type=Path)
    compare_parser.add_argument("edited_source", type=Path)
    compare_parser.add_argument("original_source", type=Path)
    compare_parser.add_argument("overlay_main", type=Path)
    compare_parser.add_argument("original_main", type=Path)
    args = parser.parse_args()

    try:
        if args.command == "findings":
            for path, line, unit in unused_unit_findings(
                args.lsp_output, args.command_json
            ):
                sys.stdout.buffer.write(
                    os.fsencode(path) + b"\0" + str(line).encode() + b"\0"
                    + os.fsencode(unit) + b"\0"
                )
            return 0
        if args.command == "overlay-main":
            write_overlay_main(
                args.main_source,
                args.original_source,
                args.overlay_source_wine,
                args.overlay_main,
            )
            return 0
        if args.command == "instrument-overlay":
            print(instrument_overlay_source(args.source))
            return 0
        if args.command == "strip-sentinel":
            strip_overlay_sentinel(
                args.raw_output,
                args.stripped_output,
                args.edited_source,
                args.nonce,
            )
            return 0
        print(
            compare_outputs(
                args.original_dcc,
                args.edited_dcc,
                args.project_root,
                args.edited_source,
                args.original_source,
                args.overlay_main,
                args.original_main,
            )
        )
    except (OSError, ValueError, json.JSONDecodeError) as error:
        print(f"dcc-corpus-unused-unit: {error}", file=sys.stderr)
        return 2
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
