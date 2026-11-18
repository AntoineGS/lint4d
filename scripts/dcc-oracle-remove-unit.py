#!/usr/bin/env python3
"""Remove one uses entry using the same conservative edit policy as the LSP quick fix."""

import argparse
import re
import sys


IDENTIFIER = re.compile(r"[A-Za-z_][A-Za-z_0-9]*")


def comment_end(source, start):
    if source.startswith("//", start):
        end = start + 2
        while end < len(source) and source[end] not in "\r\n":
            end += 1
        if source.startswith("\r\n", end):
            return end + 2, True, False
        if end < len(source):
            return end + 1, True, False
        return end, True, False

    if source[start] == "{":
        directive = source.startswith("{$", start)
        depth = 1
        cursor = start + 1
        while cursor < len(source):
            if source[cursor] == "{":
                depth += 1
            elif source[cursor] == "}":
                depth -= 1
                if depth == 0:
                    return cursor + 1, False, directive
            cursor += 1
        raise ValueError("unterminated braced comment")

    if source.startswith("(*", start):
        directive = source.startswith("(*$", start)
        depth = 1
        cursor = start + 2
        while cursor + 1 < len(source):
            if source.startswith("(*", cursor):
                depth += 1
                cursor += 2
            elif source.startswith("*)", cursor):
                depth -= 1
                cursor += 2
                if depth == 0:
                    return cursor, False, directive
            else:
                cursor += 1
        raise ValueError("unterminated parenthesized comment")

    raise ValueError("not a comment")


def quoted_end(source, start):
    quote = source[start]
    cursor = start + 1
    while cursor < len(source):
        if source[cursor] == quote:
            if cursor + 1 < len(source) and source[cursor + 1] == quote:
                cursor += 2
                continue
            return cursor + 1
        cursor += 1
    raise ValueError("unterminated string")


def lex(source):
    tokens = []
    cursor = 0
    while cursor < len(source):
        character = source[cursor]
        if character.isspace():
            cursor += 1
            continue
        if character == "{" or source.startswith("(*", cursor) or source.startswith("//", cursor):
            cursor, _, _ = comment_end(source, cursor)
            continue
        if character in "'\"":
            cursor = quoted_end(source, cursor)
            continue
        match = IDENTIFIER.match(source, cursor)
        if match:
            tokens.append(("word", match.group(), cursor, match.end()))
            cursor = match.end()
            continue
        if character in ".,;":
            tokens.append((character, character, cursor, cursor + 1))
        cursor += 1
    return tokens


def uses_clauses(source, tokens):
    clauses = []
    for index, token in enumerate(tokens):
        if token[0] != "word" or token[1].casefold() != "uses":
            continue
        delimiters = [
            (position, candidate)
            for position, candidate in enumerate(tokens[index + 1 :], index + 1)
            if candidate[0] == ";"
        ]
        if not delimiters:
            raise ValueError("uses clause has no semicolon")
        semicolon_index, semicolon = delimiters[0]
        clause_start = token[2]
        clause_end = semicolon[3]
        clauses.append((index, semicolon_index, clause_start, clause_end))
    return clauses


def imports_in_clause(tokens, token_start, token_end):
    entries = []
    separators = []
    cursor = token_start + 1
    item_start = cursor
    while cursor <= token_end:
        if cursor == token_end or tokens[cursor][0] in {",", ";"}:
            item_tokens = tokens[item_start:cursor]
            name_tokens = []
            item_cursor = 0
            if item_tokens and item_tokens[0][0] == "word":
                name_tokens.append(item_tokens[0])
                item_cursor = 1
                while (
                    item_cursor + 1 < len(item_tokens)
                    and item_tokens[item_cursor][0] == "."
                    and item_tokens[item_cursor + 1][0] == "word"
                ):
                    name_tokens.extend(item_tokens[item_cursor : item_cursor + 2])
                    item_cursor += 2
            if not name_tokens:
                raise ValueError("uses entry has no unit name")
            name = "".join(token[1] for token in name_tokens)
            entries.append((name, name_tokens[0][2], name_tokens[-1][3]))
            if cursor < token_end and tokens[cursor][0] == ",":
                separators.append(tokens[cursor][2])
            item_start = cursor + 1
        cursor += 1
    return entries, separators


def comments_in(source, start, end):
    comments = []
    cursor = start
    while cursor < end:
        if source.startswith("//", cursor) or source[cursor] in "{(":
            if source.startswith("(*", cursor) or source.startswith("//", cursor) or source[cursor] == "{":
                comment_start = cursor
                cursor, line_comment, directive = comment_end(source, cursor)
                if cursor > end:
                    raise ValueError("comment crosses uses edit range")
                if directive:
                    raise ValueError("directives inside uses edits are unsupported")
                comments.append((comment_start, cursor, line_comment))
                continue
        if source[cursor] in "'\"":
            cursor = quoted_end(source, cursor)
        else:
            cursor += 1
    return comments


def trailing_whitespace(source):
    index = len(source)
    while index and source[index - 1].isspace():
        index -= 1
    return source[index:]


def leading_whitespace(source):
    index = 0
    while index < len(source) and source[index].isspace():
        index += 1
    return source[:index]


def preserve_comments(source, start, end, fallback, preserve_separator):
    comments = comments_in(source, start, end)
    if not comments:
        return fallback if preserve_separator else ""

    first_start = comments[0][0]
    prefix = trailing_whitespace(source[start:first_start]) or fallback
    result = prefix
    for index, (comment_start, comment_end, line_comment) in enumerate(comments):
        if (
            index
            and result
            and result[-1] not in "\n\r "
            and not line_comment
        ):
            result += " "
        result += source[comment_start:comment_end]

    _, last_end, last_is_line = comments[-1]
    suffix = leading_whitespace(source[last_end:end])
    if last_is_line and suffix.startswith("\r\n"):
        suffix = suffix[2:]
    elif last_is_line and suffix.startswith(("\n", "\r")):
        suffix = suffix[1:]
    result += suffix
    if preserve_separator and result and not result[-1].isspace():
        result += " "
    return result


def remove_unit(source, requested_unit):
    tokens = lex(source)
    candidates = []
    for uses_index, semicolon_index, clause_start, _ in uses_clauses(source, tokens):
        entries, commas = imports_in_clause(tokens, uses_index, semicolon_index)
        comments_in(source, clause_start, tokens[semicolon_index][3])
        for index, (name, start, end) in enumerate(entries):
            if name.casefold() == requested_unit.casefold():
                candidates.append(
                    (uses_index, semicolon_index, clause_start, entries, commas, index, start, end)
                )

    if len(candidates) != 1:
        raise ValueError(
            f"expected one uses entry named {requested_unit}, found {len(candidates)}"
        )

    (
        _,
        semicolon_index,
        clause_start,
        entries,
        commas,
        target_index,
        target_start,
        _,
    ) = candidates[0]
    semicolon = tokens[semicolon_index][2]

    if len(entries) == 1:
        edit_start, edit_end = clause_start, semicolon + 1
        replacement = ""
    elif target_index + 1 < len(entries):
        start = (
            clause_start + len("uses")
            if target_index == 0
            else commas[target_index - 1] + 1
        )
        next_start = entries[target_index + 1][1]
        fallback = source[start:target_start]
        edit_start, edit_end = start, next_start
        replacement = preserve_comments(source, start, next_start, fallback, True)
    else:
        edit_start = commas[target_index - 1]
        edit_end = semicolon
        replacement = preserve_comments(source, edit_start, edit_end, "", False)

    return source[:edit_start] + replacement + source[edit_end:]


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--unit", required=True)
    args = parser.parse_args()
    try:
        sys.stdout.write(remove_unit(sys.stdin.read(), args.unit))
    except ValueError as error:
        print(f"dcc-oracle-remove-unit: {error}", file=sys.stderr)
        return 2
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
