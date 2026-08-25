#!/usr/bin/env python3
# ME-S-MANAGED-TOOLBOX
"""ME-S default filesystem File toolbox."""

from __future__ import annotations

import contextlib
import fnmatch
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import stat
import sys
import tempfile
import unicodedata
from dataclasses import dataclass
from typing import Any, Iterator


def fail_startup(message: str) -> "None":
    print(message, file=sys.stderr, flush=True)
    raise SystemExit(1)


if sys.version_info[:2] != (3, 12):
    fail_startup(
        "File toolbox requires Python 3.12; "
        f"received {sys.version_info.major}.{sys.version_info.minor}"
    )

sys.stdin.reconfigure(encoding="utf-8", errors="strict")
sys.stdout.reconfigure(encoding="utf-8", errors="strict", newline="\n")
sys.stderr.reconfigure(encoding="utf-8", errors="strict", newline="\n")


ROOT = Path.cwd().resolve(strict=True)
LOCK_PATH = ROOT / ".me" / "file-toolbox.lock"
HASH_PATTERN = re.compile(r"^[0-9a-f]{8}$")
HUNK_PATTERN = re.compile(
    r"^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@(?: .*)?$"
)
SEARCH_TIMEOUT_SECONDS = 120
SEARCH_TIMEOUT_TIP = (
    "Search a smaller path, reduce depth, or use narrower globs."
)
MAX_EDIT_OPERATIONS = 128
EDIT_TIP = (
    "The file was edited. Every previously readable edit range for this file "
    "is now invalid. Before editing it again, you MUST use File.Read to inspect "
    "a wider continuous range around every intended target and establish fresh "
    "line numbers and editable ranges. File.Search never establishes an editable "
    "range."
)
EDIT_BYTES_TIP = (
    "The file was edited. Its previous byte offsets and hash are now stale, "
    "and the new hash is intentionally not returned. Before editing this file "
    "again, you MUST use File.ReadBytes to obtain refreshed bytes and the "
    "latest hash."
)
TEXT_ENCODINGS = [
    "auto",
    "utf-8",
    "utf-16-le",
    "utf-16-be",
    "utf-32-le",
    "utf-32-be",
    "gb18030",
    "big5",
    "shift_jis",
    "euc_kr",
    "windows-1252",
]
ENCODING_CODECS = {
    "utf-8": "utf-8",
    "utf-16-le": "utf-16-le",
    "utf-16-be": "utf-16-be",
    "utf-32-le": "utf-32-le",
    "utf-32-be": "utf-32-be",
    "gb18030": "gb18030",
    "big5": "big5",
    "shift_jis": "shift_jis",
    "euc_kr": "euc_kr",
    "windows-1252": "cp1252",
}
ENCODING_ALIASES = {
    "utf8": "utf-8",
    "utf_8": "utf-8",
    "utf16le": "utf-16-le",
    "utf16be": "utf-16-be",
    "utf32le": "utf-32-le",
    "utf32be": "utf-32-be",
    "gbk": "gb18030",
    "cp936": "gb18030",
    "shift-jis": "shift_jis",
    "sjis": "shift_jis",
    "euc-kr": "euc_kr",
    "cp1252": "windows-1252",
}
BOMS = [
    (b"\x00\x00\xfe\xff", "utf-32-be"),
    (b"\xff\xfe\x00\x00", "utf-32-le"),
    (b"\xef\xbb\xbf", "utf-8"),
    (b"\xfe\xff", "utf-16-be"),
    (b"\xff\xfe", "utf-16-le"),
]
ENCODING_SCHEMA = {"type": "string", "enum": TEXT_ENCODINGS}
CREATE_ENCODING_SCHEMA = {"type": "string", "enum": TEXT_ENCODINGS[1:]}
TOOLS = [
    "Read",
    "ReadBytes",
    "EditBytes",
    "List",
    "Find",
    "Search",
    "Stat",
    "MakeDirectory",
    "Create",
    "Edit",
    "Append",
    "Replace",
    "Copy",
    "Move",
    "Delete",
]


class ToolError(Exception):
    def __init__(
        self,
        code: str,
        message: str,
        retryable: bool = False,
        tip: str | None = None,
    ):
        super().__init__(message)
        self.code = code
        self.message = message
        self.retryable = retryable
        self.tip = tip


TIP_LOCATE_PATH = (
    "Please check the path. Use File.List or File.Find if you need to locate it."
)
TIP_CREATE_PARENT = (
    "Please create the missing parent directory with File.MakeDirectory, then try again."
)
TIP_REGULAR_FILE = "Please choose an existing ordinary file for this operation."
TIP_REFRESH_HASH = "The file has changed. Please inspect it again and retry with its current hash."
TIP_READ_EDIT_RANGE = (
    "Please use File.Read to inspect a wider range around the intended location, "
    "then retry with the refreshed line numbers and editable_ranges."
)


@dataclass(frozen=True)
class TextDocument:
    raw: bytes
    text: str
    encoding: str
    confidence: float
    bom: bytes


@dataclass
class PatchLine:
    kind: str
    text: str
    no_newline: bool = False


@dataclass(frozen=True)
class PatchHunk:
    old_start: int
    old_count: int
    new_start: int
    new_count: int
    lines: tuple[PatchLine, ...]


@dataclass(frozen=True)
class TextLine:
    text: str
    ending: str


@dataclass(frozen=True)
class ResolvedEdit:
    index: int
    operation: str
    start_line: int | None
    end_line: int | None
    before_line: int | None
    source_start: int
    source_end: int
    new_lines: tuple[str, ...]
    replacement_text: str
    replacement_bytes: int


@dataclass(frozen=True)
class ResolvedByteEdit:
    index: int
    target_offset: int
    target_length: int
    source_start: int
    source_end: int
    data: bytes
    kind: str


@dataclass
class EditScope:
    content_hash: str
    ranges: list[tuple[int, int]]
    total_lines: int
    eof: bool


EDIT_SCOPES: dict[str, EditScope] = {}


def merge_ranges(ranges: list[tuple[int, int]]) -> list[tuple[int, int]]:
    merged: list[tuple[int, int]] = []
    for start, end in sorted(ranges):
        if start > end:
            continue
        if merged and start <= merged[-1][1] + 1:
            merged[-1] = (merged[-1][0], max(merged[-1][1], end))
        else:
            merged.append((start, end))
    return merged


def scope_ranges_value(scope: EditScope) -> list[dict[str, int]]:
    return [
        {"start_line": start, "end_line": end}
        for start, end in scope.ranges
    ]


def range_is_covered(scope: EditScope, start: int, end: int) -> bool:
    return any(left <= start and end <= right for left, right in scope.ranges)


def clear_edit_scope(path: Path) -> None:
    EDIT_SCOPES.pop(relative_path(path), None)


def import_edit_scope(data: dict[str, Any], path: Path) -> EditScope | None:
    logical = relative_path(path)
    if "_edit_scope" not in data:
        return EDIT_SCOPES.get(logical)
    raw = data.get("_edit_scope")
    if raw is None:
        EDIT_SCOPES.pop(logical, None)
        return None
    if not isinstance(raw, dict) or raw.get("path") != logical:
        raise ToolError("invalid_internal_scope", "invalid File.Edit scope")
    content_hash = raw.get("hash")
    total_lines = raw.get("total_lines")
    eof = raw.get("eof")
    raw_ranges = raw.get("ranges")
    if (
        not isinstance(content_hash, str)
        or not HASH_PATTERN.fullmatch(content_hash)
        or not isinstance(total_lines, int)
        or total_lines < 0
        or not isinstance(eof, bool)
        or not isinstance(raw_ranges, list)
    ):
        raise ToolError("invalid_internal_scope", "invalid File.Edit scope")
    ranges: list[tuple[int, int]] = []
    for item in raw_ranges:
        if not isinstance(item, dict):
            raise ToolError("invalid_internal_scope", "invalid File.Edit scope range")
        start = item.get("start_line")
        end = item.get("end_line")
        if (
            not isinstance(start, int)
            or not isinstance(end, int)
            or not 1 <= start <= end <= total_lines
        ):
            raise ToolError("invalid_internal_scope", "invalid File.Edit scope range")
        ranges.append((start, end))
    scope = EditScope(content_hash, merge_ranges(ranges), total_lines, eof)
    EDIT_SCOPES[logical] = scope
    return scope


def object_schema(
    properties: dict[str, Any], required: list[str] | None = None
) -> dict[str, Any]:
    schema: dict[str, Any] = {
        "type": "object",
        "properties": properties,
        "additionalProperties": False,
    }
    if required:
        schema["required"] = required
    return schema


PATH_SCHEMA = {
    "type": "string",
    "minLength": 1,
    "description": "A relative path resolves from the workspace. Absolute paths and relative paths that resolve outside the workspace are supported. Results use workspace-relative paths inside the workspace and normalized absolute paths outside it.",
}
HASH_SCHEMA = {"type": "string", "pattern": r"^[0-9a-f]{8}$"}
STRING_ARRAY = {
    "type": "array",
    "items": {"type": "string", "minLength": 1},
    "maxItems": 256,
}

INPUT_SCHEMAS: dict[str, dict[str, Any]] = {
    "Read": object_schema(
        {
            "path": PATH_SCHEMA,
            "start_line": {
                "type": "integer",
                "minimum": 1,
                "description": "Optional inclusive 1-based first line. Omit to begin at line 1.",
            },
            "end_line": {
                "type": "integer",
                "minimum": 1,
                "description": "Optional inclusive 1-based final line. Omit to continue through EOF.",
            },
            "encoding": {**ENCODING_SCHEMA, "default": "auto"},
        },
        ["path"],
    ),
    "ReadBytes": object_schema(
        {
            "path": PATH_SCHEMA,
            "offset": {"type": "integer", "minimum": 0, "default": 0},
            "length": {
                "type": "integer",
                "minimum": 1,
                "maximum": 1048576,
                "default": 65536,
            },
        },
        ["path"],
    ),
    "EditBytes": object_schema(
        {
            "path": PATH_SCHEMA,
            "expected_hash": HASH_SCHEMA,
            "edits": {
                "type": "array",
                "minItems": 1,
                "maxItems": MAX_EDIT_OPERATIONS,
                "description": "Atomic byte edit operations whose offsets all refer to the same original file identified by expected_hash.",
                "items": object_schema(
                    {
                        "target_offset": {
                            "type": "integer",
                            "minimum": 0,
                            "maximum": 2**63 - 1,
                            "description": "Zero-based original byte offset at which the selected half-open range begins.",
                        },
                        "target_length": {
                            "type": "integer",
                            "minimum": 0,
                            "maximum": 2**63 - 1,
                            "description": "Number of original bytes selected. Zero denotes an insertion point.",
                        },
                        "data": {
                            "type": "string",
                            "description": "Replacement bytes as lowercase two-digit hexadecimal values separated by one space. An empty string deletes a non-empty selected range.",
                        },
                    },
                    ["target_offset", "target_length", "data"],
                ),
            },
        },
        ["path", "expected_hash", "edits"],
    ),
    "List": object_schema(
        {
            "path": {**PATH_SCHEMA, "default": "."},
            "depth": {
                "type": "integer",
                "minimum": 1,
                "maximum": 32,
                "default": 1,
            },
            "include_hidden": {"type": "boolean", "default": False},
            "max_entries": {
                "type": "integer",
                "minimum": 1,
                "maximum": 10000,
                "default": 1000,
            },
        }
    ),
    "Find": object_schema(
        {
            "path": {**PATH_SCHEMA, "default": "."},
            "patterns": {
                "type": "array",
                "items": {"type": "string", "minLength": 1},
                "minItems": 1,
                "maxItems": 64,
            },
            "exclude": STRING_ARRAY,
            "include_hidden": {"type": "boolean", "default": False},
            "depth": {
                "type": "integer",
                "minimum": 1,
                "maximum": 32,
                "description": "Maximum levels below path to traverse. 1 includes only direct children. Omit for unlimited recursion.",
            },
            "max_results": {
                "type": "integer",
                "minimum": 1,
                "maximum": 10000,
                "default": 1000,
            },
        },
        ["patterns"],
    ),
    "Search": object_schema(
        {
            "path": {**PATH_SCHEMA, "default": "."},
            "query": {"type": "string", "minLength": 1},
            "regex": {"type": "boolean", "default": False},
            "case_sensitive": {"type": "boolean", "default": True},
            "globs": STRING_ARRAY,
            "depth": {
                "type": "integer",
                "minimum": 1,
                "maximum": 32,
                "description": "Maximum levels below a directory path to search. 1 searches only direct child files. Omit for unlimited recursion.",
            },
            "context_before": {
                "type": "integer",
                "minimum": 0,
                "maximum": 10000,
                "default": 0,
            },
            "context_after": {
                "type": "integer",
                "minimum": 0,
                "maximum": 10000,
                "default": 0,
            },
            "max_matches": {
                "type": "integer",
                "minimum": 1,
                "maximum": 5000,
                "default": 500,
            },
        },
        ["query"],
    ),
    "Stat": object_schema(
        {
            "paths": {
                "type": "array",
                "items": PATH_SCHEMA,
                "minItems": 1,
                "maxItems": 256,
            }
        },
        ["paths"],
    ),
    "MakeDirectory": object_schema(
        {
            "path": PATH_SCHEMA,
            "parents": {"type": "boolean", "default": False},
        },
        ["path"],
    ),
    "Create": object_schema(
        {
            "path": PATH_SCHEMA,
            "content": {"type": "string"},
            "encoding": {**CREATE_ENCODING_SCHEMA, "default": "utf-8"},
            "bom": {"type": "boolean", "default": False},
        },
        ["path", "content"],
    ),
    "Edit": object_schema(
        {
            "path": PATH_SCHEMA,
            "encoding": {**ENCODING_SCHEMA, "default": "auto"},
            "edits": {
                "type": "array",
                "minItems": 1,
                "maxItems": MAX_EDIT_OPERATIONS,
                "description": "Atomic edit operations whose line coordinates all refer to the same original file snapshot established by File.Read.",
                "items": {
                    "oneOf": [
                        object_schema(
                            {
                                "operation": {
                                    "type": "string",
                                    "enum": ["replace"],
                                },
                                "start_line": {
                                    "type": "integer",
                                    "minimum": 1,
                                    "maximum": 2**31 - 1,
                                    "description": "First inclusive 1-based original source line to replace.",
                                },
                                "end_line": {
                                    "type": "integer",
                                    "minimum": 1,
                                    "maximum": 2**31 - 1,
                                    "description": "Last inclusive 1-based original source line to replace.",
                                },
                                "new_lines": {
                                    "type": "array",
                                    "minItems": 1,
                                    "items": {"type": "string"},
                                    "description": "One or more logical replacement lines without CR or LF characters. An empty string is one blank line.",
                                },
                            },
                            ["operation", "start_line", "end_line", "new_lines"],
                        ),
                        object_schema(
                            {
                                "operation": {
                                    "type": "string",
                                    "enum": ["delete"],
                                },
                                "start_line": {
                                    "type": "integer",
                                    "minimum": 1,
                                    "maximum": 2**31 - 1,
                                    "description": "First inclusive 1-based original source line to delete.",
                                },
                                "end_line": {
                                    "type": "integer",
                                    "minimum": 1,
                                    "maximum": 2**31 - 1,
                                    "description": "Last inclusive 1-based original source line to delete.",
                                },
                            },
                            ["operation", "start_line", "end_line"],
                        ),
                        object_schema(
                            {
                                "operation": {
                                    "type": "string",
                                    "enum": ["insert"],
                                },
                                "before_line": {
                                    "type": "integer",
                                    "minimum": 1,
                                    "maximum": 2**31 - 1,
                                    "description": "Original 1-based line before which to insert. total_lines + 1 appends after a newline-terminated file; 1 inserts at the beginning and into an empty file.",
                                },
                                "new_lines": {
                                    "type": "array",
                                    "minItems": 1,
                                    "items": {"type": "string"},
                                    "description": "One or more logical inserted lines without CR or LF characters. An empty string is one blank line.",
                                },
                            },
                            ["operation", "before_line", "new_lines"],
                        ),
                    ]
                },
            },
        },
        ["path", "edits"],
    ),
    "Append": object_schema(
        {
            "path": PATH_SCHEMA,
            "expected_hash": HASH_SCHEMA,
            "encoding": {**ENCODING_SCHEMA, "default": "auto"},
            "content": {"type": "string"},
        },
        ["path", "expected_hash", "content"],
    ),
    "Replace": object_schema(
        {
            "path": PATH_SCHEMA,
            "expected_hash": HASH_SCHEMA,
            "encoding": {**ENCODING_SCHEMA, "default": "auto"},
            "content": {"type": "string"},
        },
        ["path", "expected_hash", "content"],
    ),
    "Copy": object_schema(
        {
            "path": PATH_SCHEMA,
            "destination": PATH_SCHEMA,
            "expected_hash": HASH_SCHEMA,
        },
        ["path", "destination", "expected_hash"],
    ),
    "Move": object_schema(
        {
            "path": PATH_SCHEMA,
            "destination": PATH_SCHEMA,
            "expected_hash": HASH_SCHEMA,
        },
        ["path", "destination", "expected_hash"],
    ),
    "Delete": object_schema(
        {"path": PATH_SCHEMA, "expected_hash": HASH_SCHEMA},
        ["path", "expected_hash"],
    ),
}


OUTPUT_SCHEMAS: dict[str, dict[str, Any]] = {
    "Read": object_schema(
        {
            "path": PATH_SCHEMA,
            "lines": {
                "type": "object",
                "additionalProperties": {"type": ["string", "object"]},
                "description": "Logical text without CR or LF characters, keyed by its 1-based file line number and minimally zero-padded to the width of total_lines. An oversized value may become a safe text_fragments object only in model context.",
            },
            "editable_ranges": {
                "type": "array",
                "description": "The file's complete current File.Edit authorization as merged inclusive 1-based line ranges. Only complete lines returned by successful File.Read calls on the unchanged file are included. Every File.Edit target must be fully authorized by these ranges; Search and other tools do not grant authorization.",
                "items": object_schema(
                    {
                        "start_line": {"type": "integer"},
                        "end_line": {"type": "integer"},
                    },
                    ["start_line", "end_line"],
                ),
            },
            "start_line": {"type": ["integer", "null"]},
            "end_line": {"type": ["integer", "null"]},
            "total_lines": {
                "type": "integer",
                "description": "The complete file's total logical line count, always present on success.",
            },
            "eof": {"type": "boolean"},
            "truncated": {
                "type": "boolean",
                "description": "True when the requested range ends before the file's actual EOF.",
            },
            "tip": {
                "type": "string",
                "description": "Optional plain-language guidance when the requested range could not be returned exactly.",
            },
            "hash": HASH_SCHEMA,
            "size": {"type": "integer"},
            "encoding": CREATE_ENCODING_SCHEMA,
            "encoding_confidence": {"type": "number", "minimum": 0, "maximum": 1},
            "bom": {"type": "boolean"},
        },
        [
            "path",
            "lines",
            "editable_ranges",
            "start_line",
            "end_line",
            "total_lines",
            "eof",
            "truncated",
            "hash",
            "size",
            "encoding",
            "encoding_confidence",
            "bom",
        ],
    ),
    "ReadBytes": object_schema(
        {
            "path": PATH_SCHEMA,
            "data": {
                "type": "string",
                "description": "Bytes as lowercase two-digit hexadecimal values separated by one space.",
            },
            "offset": {"type": "integer"},
            "length": {"type": "integer"},
            "size": {"type": "integer"},
            "eof": {"type": "boolean"},
            "hash": HASH_SCHEMA,
            "tip": {"type": "string"},
        },
        ["path", "data", "offset", "length", "size", "eof", "hash"],
    ),
    "EditBytes": object_schema(
        {
            "path": PATH_SCHEMA,
            "operation": {"type": "string", "enum": ["bytes_edited"]},
            "previous_hash": HASH_SCHEMA,
            "edit_results": {
                "type": "array",
                "items": object_schema(
                    {
                        "index": {"type": "integer"},
                        "state": {"type": "string", "enum": ["succeeded"]},
                        "kind": {
                            "type": "string",
                            "enum": ["replace", "delete", "insert"],
                        },
                        "target_offset": {"type": "integer"},
                        "target_length": {"type": "integer"},
                        "selected_bytes": {"type": "integer"},
                        "replacement_bytes": {"type": "integer"},
                    },
                    [
                        "index",
                        "state",
                        "kind",
                        "target_offset",
                        "target_length",
                        "selected_bytes",
                        "replacement_bytes",
                    ],
                ),
            },
            "previous_size": {"type": "integer"},
            "size": {"type": "integer"},
            "tip": {"type": "string"},
        },
        [
            "path",
            "operation",
            "previous_hash",
            "edit_results",
            "previous_size",
            "size",
            "tip",
        ],
    ),
    "List": object_schema(
        {
            "path": PATH_SCHEMA,
            "entries": {"type": "array", "items": {"type": "object"}},
            "returned": {"type": "integer"},
            "truncated": {"type": "boolean"},
            "tip": {"type": "string"},
        },
        ["path", "entries", "returned", "truncated"],
    ),
    "Find": object_schema(
        {
            "path": PATH_SCHEMA,
            "results": {"type": "array", "items": {"type": "string"}},
            "returned": {"type": "integer"},
            "truncated": {"type": "boolean"},
            "tip": {"type": "string"},
        },
        ["path", "results", "returned", "truncated"],
    ),
    "Search": object_schema(
        {
            "path": PATH_SCHEMA,
            "matches": {
                "type": "array",
                "items": object_schema(
                    {
                        "path": PATH_SCHEMA,
                        "column": {"type": "integer"},
                        "match_length": {"type": "integer"},
                        "before": {
                            "type": "object",
                            "additionalProperties": {"type": "string"},
                            "description": "Exact complete context lines before the match, keyed by their 1-based, minimally zero-padded file line numbers.",
                        },
                        "match_text": {
                            "type": "object",
                            "additionalProperties": {"type": ["string", "object"]},
                            "minProperties": 1,
                            "maxProperties": 1,
                            "description": "The exact matched file line under its 1-based, minimally zero-padded line number. Safe model-context truncation may replace only this value with a text_fragments object.",
                        },
                        "after": {
                            "type": "object",
                            "additionalProperties": {"type": "string"},
                            "description": "Exact complete context lines after the match, keyed by their 1-based, minimally zero-padded file line numbers.",
                        },
                    },
                    [
                        "path",
                        "column",
                        "match_length",
                        "before",
                        "match_text",
                        "after",
                    ],
                ),
            },
            "skipped_binary": {"type": "integer"},
            "returned": {"type": "integer"},
            "truncated": {"type": "boolean"},
            "tip": {"type": "string"},
        },
        ["path", "matches", "skipped_binary", "returned", "truncated"],
    ),
    "Stat": object_schema(
        {
            "entries": {"type": "array", "items": {"type": "object"}},
            "returned": {"type": "integer"},
            "tip": {"type": "string"},
        },
        ["entries", "returned"],
    ),
    "MakeDirectory": object_schema(
        {
            "path": PATH_SCHEMA,
            "operation": {"type": "string"},
            "exists": {"type": "boolean"},
        },
        ["path", "operation", "exists"],
    ),
}

for _tool in ("Create", "Edit", "Append", "Replace"):
    OUTPUT_SCHEMAS[_tool] = object_schema(
        {
            "path": PATH_SCHEMA,
            "operation": {"type": "string"},
            "previous_hash": {"type": ["string", "null"]},
            "hash": HASH_SCHEMA,
            "size": {"type": "integer"},
            "encoding": CREATE_ENCODING_SCHEMA,
            "encoding_confidence": {"type": "number", "minimum": 0, "maximum": 1},
            "bom": {"type": "boolean"},
        },
        [
            "path",
            "operation",
            "hash",
            "size",
            "encoding",
            "encoding_confidence",
            "bom",
        ],
    )
OUTPUT_SCHEMAS["Edit"]["properties"].update(
    {
        "edit_results": {
            "type": "array",
            "items": object_schema(
                {
                    "index": {"type": "integer"},
                    "state": {"type": "string", "enum": ["succeeded"]},
                    "operation": {
                        "type": "string",
                        "enum": ["replace", "delete", "insert"],
                    },
                    "start_line": {"type": "integer"},
                    "end_line": {"type": "integer"},
                    "before_line": {"type": "integer"},
                    "selected_lines": {"type": "integer"},
                    "new_line_count": {"type": "integer"},
                    "replacement_bytes": {"type": "integer"},
                },
                [
                    "index",
                    "state",
                    "operation",
                    "selected_lines",
                    "new_line_count",
                    "replacement_bytes",
                ],
            ),
        },
        "previous_total_lines": {"type": "integer"},
        "total_lines": {"type": "integer"},
        "previous_size": {"type": "integer"},
        "tip": {"type": "string"},
    }
)
OUTPUT_SCHEMAS["Edit"]["required"].extend(
    [
        "edit_results",
        "previous_total_lines",
        "total_lines",
        "previous_size",
        "tip",
    ]
)
OUTPUT_SCHEMAS["Edit"]["properties"].pop("hash")
OUTPUT_SCHEMAS["Edit"]["required"].remove("hash")
OUTPUT_SCHEMAS["Append"]["properties"]["appended_bytes"] = {"type": "integer"}
OUTPUT_SCHEMAS["Copy"] = object_schema(
    {
        "path": PATH_SCHEMA,
        "destination": PATH_SCHEMA,
        "operation": {"type": "string", "enum": ["copied"]},
        "hash": HASH_SCHEMA,
        "size": {"type": "integer"},
    },
    ["path", "destination", "operation", "hash", "size"],
)
OUTPUT_SCHEMAS["Move"] = object_schema(
    {
        "path": PATH_SCHEMA,
        "destination": PATH_SCHEMA,
        "operation": {"type": "string"},
        "previous_hash": HASH_SCHEMA,
        "hash": HASH_SCHEMA,
        "size": {"type": "integer"},
    },
    ["path", "destination", "operation", "previous_hash", "hash", "size"],
)
OUTPUT_SCHEMAS["Delete"] = object_schema(
    {
        "path": PATH_SCHEMA,
        "operation": {"type": "string"},
        "deleted_hash": HASH_SCHEMA,
        "exists": {"type": "boolean"},
    },
    ["path", "operation", "deleted_hash", "exists"],
)


ROUTES = {
    "Read": "Read an inclusive text line range, or the complete file, with conservative automatic encoding detection when exact file content is needed.",
    "ReadBytes": "Read a bounded byte range for binary data, text whose encoding cannot be determined safely, or a File.EditBytes baseline.",
    "EditBytes": "Atomically replace, delete, or insert one or more independently located byte ranges after inspecting them with File.ReadBytes.",
    "List": "Inspect directory contents without invoking a shell.",
    "Find": "Find filesystem paths by glob patterns with optional recursion depth.",
    "Search": "Search text through me-s's integrated ripgrep engine with bounded encoding detection and a fixed 120-second deadline.",
    "Stat": "Inspect existence, type, metadata, and current content hashes.",
    "MakeDirectory": "Create one explicit directory, optionally including its missing parent chain.",
    "Create": "Create a new text file in an explicit encoding, defaulting to UTF-8; never overwrite an existing file.",
    "Edit": "Atomically replace, delete, or insert one or more independently located line ranges in a known text file.",
    "Append": "Append exact text using the existing file's detected encoding without adding a newline.",
    "Replace": "Replace an entire known text file while preserving its detected encoding and BOM.",
    "Copy": "Copy one known regular file to a new destination without changing the source.",
    "Move": "Move one known regular file to a destination that does not exist.",
    "Delete": "Delete one explicit known regular file.",
}

INSTRUCTIONS = {
    "Read": "Line numbers are inclusive and 1-based. Use start_line and end_line as optional bounds: omit start_line to begin at line 1, omit end_line to continue through EOF, or omit both to read the complete file. Every successful result includes total_lines for the complete file. start_line and end_line in the result identify the actual returned range and are null when no line exists in the requested range. The lines object maps each actual file line number to its logical text without any LF, CRLF, or CR terminator; an empty string is one blank line. Keys are minimally zero-padded to the digit width of total_lines solely to preserve numeric order in serialized JSON; interpret them as decimal line numbers. Missing numeric keys in a safely truncated model-visible result are omitted lines, not empty lines.\n\nEDIT AUTHORIZATION: editable_ranges is the complete current set of inclusive 1-based line ranges that File.Edit is allowed to target for this file. It is authorization state, not merely a description of what this one Read requested. Every successful Read adds only the complete ordinary-string lines actually returned and visible to you; repeated reads of the same unchanged file merge adjacent or separate ranges and return the full accumulated set. Safely omitted lines and incomplete text_fragments do not become editable. Replace and delete operations must be fully contained in editable_ranges. An insert before an existing line requires that line to be editable; inserting after the final line requires that the final line and EOF were read; inserting into an empty file requires a successful Read that established the empty EOF. Before File.Edit, read every target line or insertion point and receive the Read result in an earlier model response. File.Search, File.Stat, hashes, remembered content, and any other tool do not grant edit authorization. A successful File.Edit or a detected file change clears the authorization, so call File.Read again before another edit.\n\nSource file size is not artificially capped: the complete file is loaded into memory to detect its encoding, count lines, and compute its hash. Large model-visible results are reduced only by the structured safety envelope, which preserves the JSON shape and absolute line numbers and reports truncate=true. Auto detection checks BOM, Unicode encodings, strict UTF-8, then common legacy encodings conservatively. If auto detection is uncertain, retry only when the encoding is known by setting encoding explicitly; otherwise use ReadBytes.",
    "ReadBytes": "Offsets are zero-based, and source file size is not artificially capped. The result data contains lowercase two-digit hexadecimal bytes separated by one space, without a 0x prefix. length is the number of bytes represented by data, and hash identifies the complete file rather than only the returned range. Use the returned bytes and hash as the baseline for File.EditBytes. If the model-context safety envelope reports truncate:true, data retains only the earliest complete bytes from the requested range; read another range before editing bytes that are not visible. truncate_info.ranges.bytes reports retained_offset_start, retained_offset_end_exclusive, removed_offset_start, and removed_offset_end_exclusive as absolute half-open byte ranges.",
    "EditBytes": (
        "EditBytes atomically applies one or more operations to one file. First use File.ReadBytes to inspect every target range and obtain the complete file hash, then pass that hash as expected_hash. target_offset is a zero-based original byte offset, and target_length selects the half-open original range [target_offset, target_offset + target_length). Every operation is independently located against the same original pre-edit snapshot. Earlier array items never shift later offsets, and array order is not execution order. The tool validates every operation before writing and commits the combined result once. A later operation cannot target bytes created by another operation in the same call; perform dependent work only after another ReadBytes.\n"
        "Use target_length > 0 with non-empty data to replace the selected bytes, target_length > 0 with data=\"\" to delete them, and target_length=0 with non-empty data to insert before target_offset. Offset 0 is the beginning; target_offset equal to the original file size is the only insertion point after the final byte and also inserts into an empty file. An empty insertion is invalid. Replacement ranges must not overlap. One original insertion point may appear only once. An insertion strictly inside a replaced range conflicts, while insertion exactly at either outer boundary is allowed. Every selected range must stay within the original file.\n"
        "data is exact binary content written as lowercase two-digit hexadecimal bytes separated by one space, without 0x prefixes; use an empty string only for deletion. Source file size is not artificially capped; the complete file is loaded into memory for the atomic edit. Unselected bytes and file permissions are preserved. Malformed hexadecimal data, invalid or overlapping ranges, duplicate insertion points, a stale hash, and all other failures leave the file unchanged. A successful result deliberately does not return the new hash. Its old byte offsets and hash are stale: before every later File.EditBytes on this file, call File.ReadBytes again and use the refreshed bytes and hash."
    ),
    "List": "Depth counts levels below path. Results are stable and symbolic-link directories are never traversed.",
    "Find": "Patterns match each reported normalized POSIX path and basename; exclusions match the reported path. Paths inside the workspace are reported relative to it, while paths outside are reported as normalized absolute paths. depth counts levels below path: 1 visits only direct children and 2 visits direct children plus their children. Omit depth for unlimited recursion. If path is a file, it alone is considered. Results are stable and symbolic-link directories are never traversed.",
    "Search": (
        "Literal search is the default and uses me-s's integrated ripgrep engine; regex=true uses Rust/ripgrep linear-time regular-expression syntax, so unsupported constructs such as look-around and backreferences return invalid_regex instead of falling back to another engine. For a directory path, depth counts levels below it: 1 searches only direct child files and 2 also searches files in direct child directories. Omit depth for unlimited recursion. Directory searches obey .gitignore, .ignore, Git excludes, and global Git ignores; hidden entries are skipped and symbolic-link directories are never followed. If path explicitly names a file, that file is searched even when an ignore rule would exclude it. Source file size is not artificially capped, max_matches bounds the returned result, and the complete search has a hard 120-second deadline. A search_timeout error is non-retryable and tells you to search a smaller path, reduce depth, or use narrower globs. "
        "Each match returns before, match_text, and after as line-number-keyed objects using the same 1-based, minimally zero-padded keys and logical line text as File.Read; values never contain line terminators. The sole key in match_text is the matching file line; column and match_length count decoded Unicode characters, not bytes. Example: {\"path\":\"src/main.rs\",\"column\":5,\"match_length\":7,\"before\":{\"041\":\"fn main() {\"},\"match_text\":{\"042\":\"    runtime.start();\"},\"after\":{\"043\":\"}\"}}. Search is only a locator: it never establishes editable_ranges. Before editing a located file, call File.Read for every target range. Never infer unseen edit boundaries from Search results. With top-level truncate:true, missing before or after keys are omitted context lines, and the sole match_text value may instead be a text_fragments object while its line key and match metadata remain intact. "
        "Search performs bounded encoding detection: it reads at most a 64 KiB prefix, handles UTF BOMs and strict UTF-8 first, and uses chardetng only when UTF-8 fails. Detected common East Asian and Windows encodings are strictly stream-decoded without replacement before rg matching; GB2312, CP936, and GBK guesses use the GB18030 superset, and a later invalid UTF-8 line can trigger one bounded legacy fallback. UTF-8 keeps the direct rg path. This lightweight locator detection is intentionally separate from File.Read's complete conservative encoding detection. Binary, malformed, unreadable, or unsupported files count in skipped_binary; use File.Read when exact encoding-aware content is required."
    ),
    "Stat": "A missing path is a normal result. Content hashes are returned only for ordinary files, not directories or symbolic links.",
    "MakeDirectory": "parents defaults to false, requiring the immediate parent to exist. Set parents=true to create every missing directory in the path. The target itself must not already exist; existing files, directories, and symbolic links return already_exists.",
    "Create": "The parent directory must already exist. encoding defaults to utf-8 because a new file has no bytes to inspect; bom defaults to false and is allowed only for UTF encodings. Creation fails if the destination exists.",
    "Edit": (
        "Edit atomically applies one or more explicit operations to one text file. Before Edit, you MUST call File.Read for every target line or insertion point, receive that result, and only then call Edit in a later model response; a Read and Edit emitted together cannot authorize each other. The latest File.Read result returns editable_ranges, the complete current edit authorization for that file. Replace and delete ranges must be fully contained in it. An insertion before an existing line requires that line in editable_ranges; insertion after the final line additionally requires that File.Read established EOF; insertion into an empty file requires a Read-established empty EOF. File.Search, File.Stat, hashes from other tools, generated content, and remembered line numbers never establish permission. Every operation is independently located against the same original pre-edit snapshot. Earlier array items never shift later line numbers; array order is not execution order. The tool validates every operation before writing and commits the combined result once. If one item is malformed, unread, out of range, unencodable, overlapping, duplicated at the same insertion point, or otherwise ambiguous, the entire call fails and the file remains unchanged. A later item cannot target lines created by an earlier item in the same call.\n"
        "Each edits item uses exactly one of three shapes. Replace: {operation:\"replace\", start_line, end_line, new_lines}; start_line and end_line are inclusive original 1-based line numbers, require 1 <= start_line <= end_line <= total_lines, and new_lines must be non-empty. Delete: {operation:\"delete\", start_line, end_line}; it removes those complete original lines and accepts no new_lines. Insert: {operation:\"insert\", before_line, new_lines}; it inserts before the original 1-based before_line and requires non-empty new_lines. before_line=1 inserts at the beginning and into an empty file; before_line=total_lines+1 appends only when the existing final line is terminated. Do not encode an operation indirectly with an empty new_lines array or reversed line range.\n"
        "new_lines is an array of logical lines matching File.Read values. Each array item is exactly one line and MUST NOT contain LF or CR; use an empty string for one blank line. File automatically selects and preserves the file's existing line-ending convention. To change part of a line, replace that whole source line with its complete resulting logical text, including unchanged surrounding characters but no terminator. To merge several source lines, replace their whole range with the resulting logical line or lines. To append after an unterminated final source line, include that final line in a replacement and supply both resulting logical lines.\n"
        "Replacement and deletion ranges must not overlap. An insertion cannot lie strictly inside such a range, and one original insertion point may appear only once; an insertion exactly at a range boundary is allowed. Source file size is not artificially capped; the complete file is loaded into memory. Existing encoding, BOM, permissions, line-ending style, and all unselected text are preserved. A successful Edit clears every editable range for that file. Before any later Edit, use File.Read to inspect a wider continuous range around every intended target so the surrounding context, line numbers, and editable_ranges are all fresh."
    ),
    "Append": "The file must exist and match expected_hash. Existing encoding and BOM are preserved. Content is appended exactly and no newline is added. Unrepresentable text returns encoding_error without modifying the file.",
    "Replace": "The file must exist and match expected_hash. Its detected encoding and BOM are preserved while the complete content is replaced atomically. Unrepresentable text returns encoding_error without modifying the file.",
    "Copy": "The source must be an ordinary file and match expected_hash. The destination parent must exist, and the destination itself must not exist. Copy preserves the source bytes and file permissions, leaves the source unchanged, and returns the shared content hash. It never overwrites a destination.",
    "Move": "The source must match expected_hash and the destination must not exist. A pure move preserves the content hash.",
    "Delete": "The file must match expected_hash. Directories and symbolic links are rejected. Success returns deleted_hash and exists=false.",
}

EXAMPLES = {
    "Read": 'Read a bounded inclusive range:\n{"path":"src/main.rs","start_line":1,"end_line":200}\n\nRead from line 201 through EOF:\n{"path":"src/main.rs","start_line":201}\n\nRead from the beginning through line 80:\n{"path":"src/main.rs","end_line":80}\n\nRead the complete file:\n{"path":"src/main.rs"}',
    "ReadBytes": '{"path":"assets/data.bin","offset":0,"length":65536}',
    "EditBytes": (
        "Assume File.ReadBytes returned offset=0, data=\"00 11 22 33 44 55\", size=6, and hash=0123abcd. Every edit below refers to those six original bytes.\n\n"
        "Replace original bytes 11 22 at [1,3) with aa bb:\n"
        '{"path":"assets/data.bin","expected_hash":"0123abcd","edits":[{"target_offset":1,"target_length":2,"data":"aa bb"}]}'
        "\n\nDelete original bytes 22 33 at [2,4):\n"
        '{"path":"assets/data.bin","expected_hash":"0123abcd","edits":[{"target_offset":2,"target_length":2,"data":""}]}'
        "\n\nInsert de ad before the first byte, and insert ff after the original final byte:\n"
        '{"path":"assets/data.bin","expected_hash":"0123abcd","edits":[{"target_offset":0,"target_length":0,"data":"de ad"},{"target_offset":6,"target_length":0,"data":"ff"}]}'
        "\n\nMultiple edits still use original offsets even when an earlier-position edit changes the length. This replaces original 11 with aa bb and original 44 with cc; result is 00 aa bb 22 33 cc 55:\n"
        '{"path":"assets/data.bin","expected_hash":"0123abcd","edits":[{"target_offset":1,"target_length":1,"data":"aa bb"},{"target_offset":4,"target_length":1,"data":"cc"}]}'
        "\n\nArray order is irrelevant. An insertion at offset 2 and a replacement beginning at offset 2 share an allowed outer boundary; the inserted byte appears before the replacement:\n"
        '{"path":"assets/data.bin","expected_hash":"0123abcd","edits":[{"target_offset":2,"target_length":1,"data":"bb"},{"target_offset":2,"target_length":0,"data":"aa"}]}'
        "\n\nCommon errors that reject the entire call include a range past size, target_length=0 with empty data, malformed or incomplete hexadecimal bytes, overlapping replacement ranges, duplicate insertion points, insertion strictly inside a replacement, a stale expected_hash, or attempting to target data inserted by another item. A successful result has no new hash; always call File.ReadBytes again before another EditBytes."
    ),
    "List": '{"path":"src","depth":2,"include_hidden":false}',
    "Find": 'Unlimited recursion (depth omitted):\n{"path":".","patterns":["**/*.rs"],"exclude":["target/**"]}\n\nOnly direct children:\n{"path":"src","patterns":["*.rs"],"depth":1}',
    "Search": 'Unlimited recursion (depth omitted):\n{"path":"src","query":"ToolboxRuntime","globs":["**/*.rs"]}\n\nOnly files directly inside src:\n{"path":"src","query":"ToolboxRuntime","globs":["*.rs"],"depth":1}',
    "Stat": '{"paths":["Cargo.toml","src/main.rs","missing.txt"]}',
    "MakeDirectory": '{"path":"build/generated/assets","parents":true}',
    "Create": '{"path":"notes.txt","content":"first line\\n","encoding":"utf-8"}',
    "Edit": (
        "First call File.Read for every target, for example {\"path\":\"notes.txt\",\"start_line\":1,\"end_line\":4}. Wait for its result before calling Edit; do not emit Read and Edit together. Assume it returned lines {\"1\":\"aaa\",\"2\":\"bbb\",\"3\":\"ccc\",\"4\":\"ddd\"} and editable_ranges=[{\"start_line\":1,\"end_line\":4}]. This means only lines 1 through 4 and insertion points established from them are currently authorized; it does not authorize any unseen line. new_lines contains logical text only; never add LF or CR.\n\n"
        "Replace original lines 1 and 3 independently. The first replacement adds a line, but the second still targets original line 3:\n"
        '{"path":"notes.txt","edits":[{"operation":"replace","start_line":1,"end_line":1,"new_lines":["111","aaa"]},{"operation":"replace","start_line":3,"end_line":3,"new_lines":["333","ccc"]}]}'
        "\n\nMixed atomic batch; array order is irrelevant. Replace original line 4, insert before original line 2, and delete original line 3:\n"
        '{"path":"notes.txt","edits":[{"operation":"replace","start_line":4,"end_line":4,"new_lines":["last"]},{"operation":"insert","before_line":2,"new_lines":["inserted"]},{"operation":"delete","start_line":3,"end_line":3}]}'
        "\n\nInsertion exactly before a replaced range is a valid outer-boundary insertion; inserted appears before updated:\n"
        '{"path":"notes.txt","edits":[{"operation":"insert","before_line":2,"new_lines":["inserted"]},{"operation":"replace","start_line":2,"end_line":2,"new_lines":["updated"]}]}'
        "\n\nReplace several lines with one and another original line with several:\n"
        '{"path":"notes.txt","edits":[{"operation":"replace","start_line":1,"end_line":2,"new_lines":["combined"]},{"operation":"replace","start_line":4,"end_line":4,"new_lines":["one","two","three"]}]}'
        "\n\nDelete a complete line, and separately replace a line with one blank line. Deletion has no new_lines field; an empty string is a blank line:\n"
        '{"path":"notes.txt","edits":[{"operation":"delete","start_line":1,"end_line":1},{"operation":"replace","start_line":3,"end_line":3,"new_lines":[""]}]}'
        "\n\nInsert at file start and append after a four-line file whose EOF was read:\n"
        '{"path":"notes.txt","edits":[{"operation":"insert","before_line":1,"new_lines":["header"]},{"operation":"insert","before_line":5,"new_lines":["footer"]}]}'
        "\n\nInsert into an empty file before line 1:\n"
        '{"path":"empty.txt","edits":[{"operation":"insert","before_line":1,"new_lines":["first line"]}]}'
        "\n\nAppend after an unterminated final line by replacing that read final line with both resulting logical lines:\n"
        '{"path":"unterminated.txt","edits":[{"operation":"replace","start_line":4,"end_line":4,"new_lines":["original final text","appended"]}]}'
        "\n\nCRLF is detected and preserved automatically; do not put CRLF in new_lines:\n"
        '{"path":"windows.txt","edits":[{"operation":"replace","start_line":2,"end_line":2,"new_lines":["complete changed line"]}]}'
        "\n\nCommon errors: editing without Read, targeting any line outside editable_ranges, using Search as permission, replace with new_lines=[], delete with a new_lines field, insert with start_line/end_line, reversed ranges, new_lines=[\"two\\nlines\"], overlapping ranges, duplicate insertion points, insertion inside another range, or targeting lines created by another item. Any one rejects the entire call. After success all editable ranges for the file are cleared; before another Edit, use File.Read to inspect a wider continuous range around every intended target."
    ),
    "Append": '{"path":"notes.txt","expected_hash":"0123abcd","content":"next line\\n"}',
    "Replace": '{"path":"notes.txt","expected_hash":"0123abcd","content":"complete new content\\n"}',
    "Copy": '{"path":"notes.txt","destination":"archive/notes.txt","expected_hash":"0123abcd"}',
    "Move": '{"path":"notes.txt","destination":"archive/notes.txt","expected_hash":"0123abcd"}',
    "Delete": '{"path":"archive/notes.txt","expected_hash":"0123abcd"}',
}


def send(frame: dict[str, Any]) -> None:
    sys.stdout.write(json.dumps(frame, ensure_ascii=False, separators=(",", ":")) + "\n")
    sys.stdout.flush()


def result(request_id: int, output: Any) -> None:
    send({"id": request_id, "type": "result", "output": output})


def error(request_id: int, exc: ToolError) -> None:
    detail: dict[str, Any] = {
        "code": exc.code,
        "message": exc.message,
        "retryable": exc.retryable,
    }
    if exc.tip is not None:
        detail["tip"] = exc.tip
    send({"id": request_id, "type": "error", "error": detail})


def validate_object(value: Any) -> dict[str, Any]:
    if not isinstance(value, dict):
        raise ToolError("invalid_arguments", "input must be a JSON object")
    return value


def string_arg(data: dict[str, Any], name: str, default: str | None = None) -> str:
    value = data.get(name, default)
    if not isinstance(value, str) or (name != "content" and not value):
        raise ToolError("invalid_arguments", f"{name} must be a non-empty string")
    if "\x00" in value:
        raise ToolError("invalid_arguments", f"{name} contains NUL")
    return value


def logical_lines_arg(data: dict[str, Any], edit_index: int) -> tuple[str, ...]:
    value = data.get("new_lines")
    if not isinstance(value, list):
        raise ToolError(
            "invalid_line_syntax",
            f"edits[{edit_index}].new_lines must be an array of logical lines",
            tip="Please provide new_lines as a JSON array with one string per line and no newline characters inside a string.",
        )
    result: list[str] = []
    for line_index, line in enumerate(value):
        if not isinstance(line, str):
            raise ToolError(
                "invalid_line_syntax",
                f"edits[{edit_index}].new_lines[{line_index}] must be a string",
                tip="Please provide new_lines as a JSON array with one string per line.",
            )
        if "\x00" in line:
            raise ToolError(
                "invalid_line_syntax",
                f"edits[{edit_index}].new_lines[{line_index}] contains NUL",
            )
        if "\r" in line or "\n" in line:
            raise ToolError(
                "invalid_line_syntax",
                f"edits[{edit_index}].new_lines[{line_index}] must not contain CR or LF; provide one array item per logical line",
                tip="Please split the text into separate new_lines array items and remove all CR and LF characters from each item.",
            )
        result.append(line)
    return tuple(result)


def hex_data_arg(data: dict[str, Any], edit_index: int) -> bytes:
    value = data.get("data")
    if not isinstance(value, str):
        raise ToolError(
            "invalid_byte_syntax", f"edits[{edit_index}].data must be a string"
        )
    tokens = [token for token in value.split(" ") if token]
    if any(
        len(token) != 2
        or any(character not in "0123456789abcdefABCDEF" for character in token)
        for token in tokens
    ):
        raise ToolError(
            "invalid_byte_syntax",
            f"edits[{edit_index}].data must contain complete two-digit hexadecimal bytes",
        )
    return bytes(int(token, 16) for token in tokens)


def int_arg(
    data: dict[str, Any], name: str, default: int, minimum: int, maximum: int
) -> int:
    value = data.get(name, default)
    if isinstance(value, bool) or not isinstance(value, int) or not minimum <= value <= maximum:
        raise ToolError(
            "invalid_arguments", f"{name} must be an integer in {minimum}..={maximum}"
        )
    return value


def optional_int_arg(
    data: dict[str, Any], name: str, minimum: int, maximum: int
) -> int | None:
    if name not in data:
        return None
    value = data[name]
    if isinstance(value, bool) or not isinstance(value, int) or not minimum <= value <= maximum:
        raise ToolError(
            "invalid_arguments", f"{name} must be an integer in {minimum}..={maximum}"
        )
    return value


def bool_arg(data: dict[str, Any], name: str, default: bool) -> bool:
    value = data.get(name, default)
    if not isinstance(value, bool):
        raise ToolError("invalid_arguments", f"{name} must be a boolean")
    return value


def encoding_arg(
    data: dict[str, Any], name: str = "encoding", default: str = "auto", allow_auto: bool = True
) -> str:
    value = string_arg(data, name, default).lower()
    value = ENCODING_ALIASES.get(value, value)
    if value not in ENCODING_CODECS and not (allow_auto and value == "auto"):
        allowed = TEXT_ENCODINGS if allow_auto else TEXT_ENCODINGS[1:]
        raise ToolError(
            "invalid_encoding",
            f"{name} must be one of: {', '.join(allowed)}",
        )
    return value


def string_list(
    data: dict[str, Any],
    name: str,
    default: list[str] | None = None,
    required: bool = False,
    max_items: int = 256,
) -> list[str]:
    value = data.get(name, default if default is not None else [])
    if not isinstance(value, list) or (required and not value) or len(value) > max_items:
        raise ToolError("invalid_arguments", f"{name} must be a valid string array")
    if any(not isinstance(item, str) or not item or "\x00" in item for item in value):
        raise ToolError("invalid_arguments", f"{name} must contain non-empty strings")
    return value


def raw_path(value: str) -> Path:
    candidate = Path(value)
    return candidate if candidate.is_absolute() else ROOT / candidate


def existing_path(value: str) -> Path:
    try:
        return raw_path(value).resolve(strict=True)
    except FileNotFoundError as exc:
        raise ToolError("not_found", f"path does not exist: {value}", tip=TIP_LOCATE_PATH) from exc
    except OSError as exc:
        raise ToolError("path_error", f"cannot resolve {value}: {exc}") from exc


def lexical_path(value: str) -> Path:
    candidate = raw_path(value)
    try:
        parent = candidate.parent.resolve(strict=True)
    except FileNotFoundError as exc:
        raise ToolError(
            "parent_not_found",
            f"parent directory does not exist: {value}",
            tip=TIP_CREATE_PARENT,
        ) from exc
    except OSError as exc:
        raise ToolError("path_error", f"cannot resolve parent of {value}: {exc}") from exc
    path = parent / candidate.name
    if path == ROOT:
        raise ToolError("invalid_path", "workspace root cannot be modified")
    if path == LOCK_PATH:
        raise ToolError("protected_path", "File toolbox coordination lock cannot be modified")
    return path


def recursive_lexical_path(value: str) -> Path:
    candidate = raw_path(value)
    if candidate.name in {"", ".", ".."}:
        raise ToolError("invalid_path", "workspace root cannot be modified")
    try:
        parent = candidate.parent.resolve(strict=False)
    except OSError as exc:
        raise ToolError("path_error", f"cannot resolve parent of {value}: {exc}") from exc
    path = parent / candidate.name
    if path == ROOT:
        raise ToolError("invalid_path", "workspace root cannot be modified")
    if path == LOCK_PATH:
        raise ToolError("protected_path", "File toolbox coordination lock cannot be modified")
    return path


def inspection_path(value: str) -> Path:
    candidate = raw_path(value)
    if candidate == ROOT:
        return ROOT
    try:
        parent = candidate.parent.resolve(strict=False)
    except OSError as exc:
        raise ToolError("path_error", f"cannot resolve parent of {value}: {exc}") from exc
    return parent / candidate.name


def public_absolute_path(path: Path) -> str:
    value = path.as_posix()
    if os.name == "nt":
        if value.startswith("//?/UNC/"):
            return f"//{value[8:]}"
        if re.match(r"^//\?/[A-Za-z]:/", value):
            return value[4:]
    return value


def relative_path(path: Path) -> str:
    try:
        relative = path.relative_to(ROOT)
    except ValueError:
        return public_absolute_path(path)
    return "." if not relative.parts else relative.as_posix()


def sha256_bytes(content: bytes) -> str:
    return hashlib.sha256(content).hexdigest()[:8]


def hash_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()[:8]


def require_regular_file(path: Path, logical: str, reject_symlink: bool = False) -> None:
    if path == LOCK_PATH:
        raise ToolError("protected_path", "File toolbox coordination lock cannot be modified")
    lexical = raw_path(logical)
    if reject_symlink and lexical.is_symlink():
        raise ToolError(
            "unsupported_file_type",
            f"symbolic links are not mutable: {logical}",
            tip=TIP_REGULAR_FILE,
        )
    if not path.is_file():
        raise ToolError(
            "unsupported_file_type",
            f"path is not a regular file: {logical}",
            tip=TIP_REGULAR_FILE,
        )


def validate_expected_hash(value: Any) -> str:
    if not isinstance(value, str) or HASH_PATTERN.fullmatch(value) is None:
        raise ToolError(
            "invalid_arguments", "expected_hash must be exactly 8 lowercase hexadecimal characters"
        )
    return value


def verify_hash(path: Path, expected: str) -> str:
    current = hash_file(path)
    if current != expected:
        raise ToolError(
            "conflict",
            f"file changed: expected_hash={expected}, current_hash={current}",
            True,
            TIP_REFRESH_HASH,
        )
    return current


def verify_content_hash(content: bytes, expected: str) -> str:
    current = sha256_bytes(content)
    if current != expected:
        raise ToolError(
            "conflict",
            f"file changed: expected_hash={expected}, current_hash={current}",
            True,
            TIP_REFRESH_HASH,
        )
    return current


def bom_for(raw: bytes) -> tuple[bytes, str] | None:
    for marker, encoding in BOMS:
        if raw.startswith(marker):
            return marker, encoding
    return None


def decode_strict(payload: bytes, encoding: str, logical: str) -> str:
    try:
        text = payload.decode(ENCODING_CODECS[encoding], errors="strict")
    except UnicodeDecodeError as exc:
        raise ToolError(
            "encoding_error", f"file is not valid {encoding}: {logical}"
        ) from exc
    if "\x00" in text:
        raise ToolError(
            "binary_file",
            f"decoded text contains NUL characters: {logical}",
            tip="Please use File.ReadBytes to inspect this file as bytes.",
        )
    return text


def null_ratio(raw: bytes, offset: int, width: int) -> float:
    values = raw[offset::width]
    return values.count(0) / len(values) if values else 0.0


def bomless_unicode_candidate(raw: bytes) -> str | None:
    if len(raw) >= 8 and len(raw) % 4 == 0:
        ratios = [null_ratio(raw, offset, 4) for offset in range(4)]
        if min(ratios[1:]) >= 0.60 and ratios[0] <= 0.20:
            return "utf-32-le"
        if min(ratios[:3]) >= 0.60 and ratios[3] <= 0.20:
            return "utf-32-be"
    if len(raw) >= 4 and len(raw) % 2 == 0:
        even = null_ratio(raw, 0, 2)
        odd = null_ratio(raw, 1, 2)
        if odd >= 0.60 and even <= 0.20:
            return "utf-16-le"
        if even >= 0.60 and odd <= 0.20:
            return "utf-16-be"
    return None


COMMON_CJK = set(
    "的一是在不了有和人这中大为上个国我以要他时来用们生到作地于出就分对成会可主发年动同工也能下过子说产种面而方后多定行学法所民得经十三之进着等部度家电力里如水化高自二理起小物现实加量都两体制机当使点从业本去把性好应开它合还因由其些然前外天政四日那社义事平形相全表间样与关各重新线内数正心反你明看原又么利比或但质气第向道命此变条只没结解问意建月公无系军很情者最立代想已通并提直题党程展五果料象员革位入常文总次品式活设及管特件长求老头基资边流路级少图山统接知较将组见计别她手角期根论运农指几九区强放决西被干做必战先回则任取据处理世车价美间"
)
COMMON_KOREAN = set(
    "가간갈감강개거건게겨경고과관광구국군그기길김나난날남내너년노는니"
    "다대도동되된두들등라러로리마만말명모무문미바박반받방버번보본부"
    "분불비사산상서선성세소속수시신실아안않알앞어언없에여연영오와요"
    "용우원위유은을음의이인일자장재저전정제조주중지진차처천체초최추"
    "출치카큰타통파표하한할해현형호화회후히녕세어입두번째줄"
)


def legacy_quality(text: str, encoding: str) -> float:
    if not text:
        return 1.0
    bad = 0
    non_ascii = 0
    cjk = 0
    common_cjk = 0
    japanese = 0
    korean = 0
    common_korean = 0
    latin = 0
    for character in text:
        point = ord(character)
        category = unicodedata.category(character)
        if character in "\t\r\n":
            continue
        if category in {"Cc", "Cs", "Co", "Cn"}:
            bad += 1
            continue
        if point > 0x7F:
            non_ascii += 1
        if 0x3400 <= point <= 0x9FFF or 0xF900 <= point <= 0xFAFF:
            cjk += 1
            common_cjk += character in COMMON_CJK
        elif 0x3040 <= point <= 0x30FF:
            japanese += 1
        elif 0xAC00 <= point <= 0xD7AF:
            korean += 1
            common_korean += character in COMMON_KOREAN
        elif "LATIN" in unicodedata.name(character, ""):
            latin += 1
    if bad:
        return -1.0 - bad / len(text)
    if non_ascii == 0:
        return 1.0
    visible = max(1, non_ascii)
    base = 0.45
    if encoding in {"gb18030", "big5"}:
        base += 0.20 * cjk / visible
        base += 0.55 * common_cjk / max(1, cjk)
    elif encoding == "shift_jis":
        base += 0.80 * japanese / visible
        base += 0.15 * cjk / visible
    elif encoding == "euc_kr":
        base += 0.15 * korean / visible
        base += 0.55 * common_korean / max(1, korean)
    elif encoding == "windows-1252":
        base += 0.50 * latin / visible
        non_ascii_density = non_ascii / max(1, len(text))
        base -= 0.50 * max(0.0, non_ascii_density - 0.35)
        base -= 0.20 * (cjk + japanese + korean) / visible
    return base


def decode_text_bytes(raw: bytes, logical: str, requested: str = "auto") -> TextDocument:
    detected_bom = bom_for(raw)
    if requested != "auto":
        marker = b""
        if detected_bom is not None:
            marker, bom_encoding = detected_bom
            if bom_encoding != requested:
                raise ToolError(
                    "encoding_mismatch",
                    f"file BOM declares {bom_encoding}, not requested {requested}: {logical}",
                )
        text = decode_strict(raw[len(marker) :], requested, logical)
        if text.encode(ENCODING_CODECS[requested], errors="strict") != raw[len(marker) :]:
            raise ToolError(
                "encoding_error",
                f"file does not round-trip losslessly as requested {requested}: {logical}",
            )
        return TextDocument(raw, text, requested, 1.0, marker)

    if detected_bom is not None:
        marker, encoding = detected_bom
        text = decode_strict(raw[len(marker) :], encoding, logical)
        return TextDocument(raw, text, encoding, 1.0, marker)

    unicode_candidate = bomless_unicode_candidate(raw)
    if unicode_candidate is not None:
        text = decode_strict(raw, unicode_candidate, logical)
        return TextDocument(raw, text, unicode_candidate, 0.95, b"")

    try:
        text = raw.decode("utf-8", errors="strict")
    except UnicodeDecodeError:
        text = ""
    else:
        if "\x00" in text:
            raise ToolError(
                "binary_file",
                f"decoded text contains NUL characters: {logical}",
                tip="Please use File.ReadBytes to inspect this file as bytes.",
            )
        return TextDocument(raw, text, "utf-8", 1.0, b"")

    if b"\x00" in raw:
        raise ToolError(
            "binary_file",
            f"file contains NUL bytes: {logical}",
            tip="Please use File.ReadBytes to inspect this file as bytes.",
        )

    candidates: list[tuple[float, str, str]] = []
    for encoding in ("gb18030", "big5", "shift_jis", "euc_kr", "windows-1252"):
        codec = ENCODING_CODECS[encoding]
        try:
            decoded = raw.decode(codec, errors="strict")
            if decoded.encode(codec, errors="strict") != raw:
                continue
        except (UnicodeDecodeError, UnicodeEncodeError):
            continue
        candidates.append((legacy_quality(decoded, encoding), encoding, decoded))
    candidates.sort(reverse=True)
    if not candidates:
        raise ToolError(
            "binary_file",
            f"file is not recognized as text: {logical}",
            tip="Please use File.ReadBytes to inspect this file as bytes.",
        )
    best_score, best_encoding, best_text = candidates[0]
    if best_score < 0.70:
        names = ", ".join(candidate[1] for candidate in candidates[:3])
        raise ToolError(
            "encoding_uncertain",
            f"text encoding has low confidence ({names}): {logical}; specify encoding explicitly or use ReadBytes",
            tip="If you know the encoding, retry with it explicitly. Otherwise use File.ReadBytes.",
        )
    runner_up = candidates[1][0] if len(candidates) > 1 else 0.0
    gap = best_score - runner_up
    if len(candidates) > 1 and gap < 0.08:
        names = ", ".join(candidate[1] for candidate in candidates[:3])
        raise ToolError(
            "encoding_uncertain",
            f"text encoding is ambiguous ({names}): {logical}; specify encoding explicitly or use ReadBytes",
            tip="If you know the encoding, retry with it explicitly. Otherwise use File.ReadBytes.",
        )
    confidence = min(0.95, 0.78 + max(0.0, gap))
    return TextDocument(raw, best_text, best_encoding, round(confidence, 3), b"")


def encode_text(text: str, encoding: str, bom: bytes, logical: str) -> bytes:
    try:
        payload = text.encode(ENCODING_CODECS[encoding], errors="strict")
    except UnicodeEncodeError as exc:
        character = text[exc.start : exc.end]
        raise ToolError(
            "encoding_error",
            f"text {character!r} cannot be represented as {encoding}: {logical}",
        ) from exc
    return bom + payload


def create_bom(encoding: str, enabled: bool) -> bytes:
    if not enabled:
        return b""
    for marker, candidate in BOMS:
        if candidate == encoding:
            return marker
    raise ToolError("invalid_encoding", f"BOM is not supported for {encoding}")


def read_text_file(path: Path, logical: str, encoding: str = "auto") -> TextDocument:
    content = path.read_bytes()
    return decode_text_bytes(content, logical, encoding)


@contextlib.contextmanager
def mutation_lock() -> Iterator[None]:
    LOCK_PATH.parent.mkdir(parents=True, exist_ok=True)
    with LOCK_PATH.open("a+b") as lock:
        lock.seek(0, os.SEEK_END)
        if lock.tell() == 0:
            lock.write(b"0")
            lock.flush()
        lock.seek(0)
        if os.name == "nt":
            import msvcrt

            msvcrt.locking(lock.fileno(), msvcrt.LK_LOCK, 1)
            try:
                yield
            finally:
                lock.seek(0)
                msvcrt.locking(lock.fileno(), msvcrt.LK_UNLCK, 1)
        else:
            import fcntl

            fcntl.flock(lock.fileno(), fcntl.LOCK_EX)
            try:
                yield
            finally:
                fcntl.flock(lock.fileno(), fcntl.LOCK_UN)


def atomic_replace(path: Path, content: bytes, mode: int) -> None:
    descriptor, temporary_name = tempfile.mkstemp(prefix=f".{path.name}.me-", dir=path.parent)
    temporary = Path(temporary_name)
    try:
        with os.fdopen(descriptor, "wb") as output:
            output.write(content)
            output.flush()
            os.fsync(output.fileno())
        os.chmod(temporary, stat.S_IMODE(mode))
        os.replace(temporary, path)
    finally:
        with contextlib.suppress(FileNotFoundError):
            temporary.unlink()


def atomic_create(path: Path, content: bytes) -> None:
    descriptor, temporary_name = tempfile.mkstemp(prefix=f".{path.name}.me-", dir=path.parent)
    temporary = Path(temporary_name)
    try:
        with os.fdopen(descriptor, "wb") as output:
            output.write(content)
            output.flush()
            os.fsync(output.fileno())
        current_umask = os.umask(0)
        os.umask(current_umask)
        os.chmod(temporary, 0o666 & ~current_umask)
        try:
            os.link(temporary, path)
        except FileExistsError as exc:
            raise ToolError(
                "already_exists",
                f"destination already exists: {relative_path(path)}",
                tip="Please choose a new destination, or inspect the existing path before deciding what to do.",
            ) from exc
    finally:
        with contextlib.suppress(FileNotFoundError):
            temporary.unlink()


def atomic_copy(source: Path, target: Path, expected_hash: str) -> tuple[str, int]:
    descriptor, temporary_name = tempfile.mkstemp(
        prefix=f".{target.name}.me-", dir=target.parent
    )
    temporary = Path(temporary_name)
    digest = hashlib.sha256()
    size = 0
    try:
        source_mode = source.stat().st_mode
        with os.fdopen(descriptor, "wb") as output, source.open("rb") as input_file:
            for chunk in iter(lambda: input_file.read(1024 * 1024), b""):
                digest.update(chunk)
                output.write(chunk)
                size += len(chunk)
            output.flush()
            os.fsync(output.fileno())
        copied_hash = digest.hexdigest()[:8]
        if copied_hash != expected_hash:
            raise ToolError(
                "conflict",
                f"file changed: expected_hash={expected_hash}, current_hash={copied_hash}",
                True,
                TIP_REFRESH_HASH,
            )
        verify_hash(source, expected_hash)
        os.chmod(temporary, stat.S_IMODE(source_mode))
        try:
            os.link(temporary, target)
        except FileExistsError as exc:
            raise ToolError(
                "already_exists",
                f"destination already exists: {relative_path(target)}",
                tip="Please choose a new destination, or inspect the existing path before deciding what to do.",
            ) from exc
        return copied_hash, size
    finally:
        with contextlib.suppress(FileNotFoundError):
            temporary.unlink()


def path_type(mode: int) -> str:
    if stat.S_ISREG(mode):
        return "file"
    if stat.S_ISDIR(mode):
        return "directory"
    if stat.S_ISLNK(mode):
        return "symlink"
    return "other"


def matches_any(path: str, patterns: list[str]) -> bool:
    for pattern in patterns:
        candidates = {pattern}
        pending = [pattern]
        while pending:
            candidate = pending.pop()
            marker = candidate.find("**/")
            if marker >= 0:
                without_empty_directory = candidate[:marker] + candidate[marker + 3 :]
                if without_empty_directory not in candidates:
                    candidates.add(without_empty_directory)
                    pending.append(without_empty_directory)
        if any(fnmatch.fnmatchcase(path, candidate) for candidate in candidates):
            return True
    return False


def walk_files(
    start: Path, include_hidden: bool, depth: int | None = None
) -> Iterator[Path]:
    if start.is_file():
        yield start
        return
    if not start.is_dir():
        raise ToolError("unsupported_file_type", f"search root is not a file or directory: {relative_path(start)}")
    for directory, names, files in os.walk(start, followlinks=False):
        current_depth = len(Path(directory).relative_to(start).parts)
        descendable_directories = sorted(
            name
            for name in names
            if (include_hidden or not name.startswith("."))
            and not (Path(directory) / name).is_symlink()
        )
        names[:] = (
            []
            if depth is not None and current_depth + 1 >= depth
            else descendable_directories
        )
        for name in sorted(files):
            if include_hidden or not name.startswith("."):
                yield Path(directory) / name


def walk_entries(
    start: Path, include_hidden: bool, depth: int | None = None
) -> Iterator[Path]:
    if not start.is_dir():
        yield start
        return
    for directory, names, files in os.walk(start, followlinks=False):
        current_depth = len(Path(directory).relative_to(start).parts)
        visible_directories = sorted(
            name for name in names if include_hidden or not name.startswith(".")
        )
        descendable_directories = [
            name
            for name in visible_directories
            if not (Path(directory) / name).is_symlink()
        ]
        names[:] = (
            []
            if depth is not None and current_depth + 1 >= depth
            else descendable_directories
        )
        for name in visible_directories:
            yield Path(directory) / name
        for name in sorted(files):
            if include_hidden or not name.startswith("."):
                yield Path(directory) / name


def split_text_file_lines(text: str) -> list[str]:
    lines: list[str] = []
    start = 0
    index = 0
    while index < len(text):
        if text[index] == "\r":
            index += 2 if index + 1 < len(text) and text[index + 1] == "\n" else 1
            lines.append(text[start:index])
            start = index
        elif text[index] == "\n":
            index += 1
            lines.append(text[start:index])
            start = index
        else:
            index += 1
    if start < len(text):
        lines.append(text[start:])
    return lines


def line_without_ending(line: str) -> str:
    if line.endswith("\r\n"):
        return line[:-2]
    if line.endswith("\r") or line.endswith("\n"):
        return line[:-1]
    return line


def execute_read(data: dict[str, Any]) -> dict[str, Any]:
    logical = string_arg(data, "path")
    requested_start = optional_int_arg(data, "start_line", 1, 2**31 - 1)
    requested_end = optional_int_arg(data, "end_line", 1, 2**31 - 1)
    effective_start = requested_start if requested_start is not None else 1
    if requested_end is not None and effective_start > requested_end:
        raise ToolError(
            "invalid_arguments",
            "start_line must be less than or equal to end_line",
            tip="Please check the requested line numbers and retry with start_line no greater than end_line.",
        )
    encoding = encoding_arg(data)
    path = existing_path(logical)
    require_regular_file(path, logical)
    document = read_text_file(path, logical, encoding)
    lines = split_text_file_lines(document.text)
    total_lines = len(lines)
    if effective_start > total_lines:
        start_index = total_lines
        selected: list[str] = []
        actual_start: int | None = None
        actual_end: int | None = None
    else:
        start_index = effective_start - 1
        effective_end = requested_end if requested_end is not None else total_lines
        actual_end = min(effective_end, total_lines)
        selected = lines[start_index:actual_end]
        actual_start = effective_start
    eof = actual_end is None or actual_end >= total_lines
    line_number_width = max(1, len(str(len(lines))))
    content_hash = sha256_bytes(document.raw)
    scope_key = relative_path(path)
    scope = EDIT_SCOPES.get(scope_key)
    if scope is None or scope.content_hash != content_hash:
        scope = EditScope(content_hash, [], total_lines, False)
    if selected:
        assert actual_start is not None and actual_end is not None
        scope.ranges = merge_ranges(
            scope.ranges + [(actual_start, actual_end)]
        )
    scope.total_lines = total_lines
    if len(lines) == 0 or (eof and bool(selected)):
        scope.eof = True
    EDIT_SCOPES[scope_key] = scope
    output: dict[str, Any] = {
        "path": scope_key,
        "lines": {
            str(start_index + offset + 1).zfill(line_number_width): line_without_ending(line)
            for offset, line in enumerate(selected)
        },
        "editable_ranges": scope_ranges_value(scope),
        "start_line": actual_start,
        "end_line": actual_end,
        "total_lines": total_lines,
        "eof": eof,
        "truncated": not eof,
        "hash": content_hash,
        "size": len(document.raw),
        "encoding": document.encoding,
        "encoding_confidence": document.confidence,
        "bom": bool(document.bom),
    }
    if total_lines == 0 and (requested_start is not None or requested_end is not None):
        output["tip"] = "The file is empty, so no lines were returned."
    elif effective_start > total_lines:
        output["tip"] = (
            f"The file has {total_lines} lines, so this range contains no lines. "
            "Use total_lines to choose an existing range."
        )
    elif requested_end is not None and requested_end > total_lines:
        output["tip"] = (
            f"The file has {total_lines} lines, so the result ends at line {total_lines}."
        )
    return output


def execute_read_bytes(data: dict[str, Any]) -> dict[str, Any]:
    logical = string_arg(data, "path")
    offset = int_arg(data, "offset", 0, 0, 2**63 - 1)
    length = int_arg(data, "length", 65536, 1, 1048576)
    path = existing_path(logical)
    require_regular_file(path, logical)
    with path.open("rb") as source:
        size = os.fstat(source.fileno()).st_size
        source.seek(min(offset, size))
        chunk = source.read(length)
        source.seek(0)
        digest = hashlib.sha256()
        for block in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(block)
    actual_offset = min(offset, size)
    output = {
        "path": relative_path(path),
        "data": " ".join(f"{byte:02x}" for byte in chunk),
        "offset": actual_offset,
        "length": len(chunk),
        "size": size,
        "eof": actual_offset + len(chunk) >= size,
        "hash": digest.hexdigest()[:8],
    }
    if offset >= size:
        output["tip"] = (
            f"The file has {size} bytes, so this range contains no bytes. "
            "Use size to choose an existing range."
        )
    elif offset + length > size:
        output["tip"] = f"The file has {size} bytes, so the result ends at byte {size}."
    return output


def execute_list(data: dict[str, Any]) -> dict[str, Any]:
    logical = string_arg(data, "path", ".")
    depth = int_arg(data, "depth", 1, 1, 32)
    include_hidden = bool_arg(data, "include_hidden", False)
    max_entries = int_arg(data, "max_entries", 1000, 1, 10000)
    start = existing_path(logical)
    if not start.is_dir():
        raise ToolError(
            "not_directory",
            f"path is not a directory: {logical}",
            tip="Please use File.Stat to inspect the path, then choose an existing directory for File.List.",
        )
    entries: list[dict[str, Any]] = []
    pending: list[tuple[Path, int]] = [(start, 1)]
    truncated = False
    while pending:
        directory, level = pending.pop(0)
        try:
            children = sorted(directory.iterdir(), key=lambda item: item.name)
        except OSError as exc:
            raise ToolError("read_error", f"cannot list {relative_path(directory)}: {exc}") from exc
        next_directories: list[Path] = []
        for child in children:
            if not include_hidden and child.name.startswith("."):
                continue
            info = child.lstat()
            kind = path_type(info.st_mode)
            entry = {
                "path": relative_path(child),
                "type": kind,
                "size": info.st_size,
                "modified_ms": info.st_mtime_ns // 1_000_000,
            }
            entries.append(entry)
            if len(entries) >= max_entries:
                truncated = True
                pending.clear()
                break
            if kind == "directory" and level < depth:
                next_directories.append(child)
        pending.extend((child, level + 1) for child in next_directories)
    output = {
        "path": relative_path(start),
        "entries": entries,
        "returned": len(entries),
        "truncated": truncated,
    }
    if not entries:
        output["tip"] = (
            "No entries were found. Check path, depth, and include_hidden if you expected content."
        )
    return output


def execute_find(data: dict[str, Any]) -> dict[str, Any]:
    logical = string_arg(data, "path", ".")
    patterns = string_list(data, "patterns", required=True, max_items=64)
    exclude = string_list(data, "exclude")
    include_hidden = bool_arg(data, "include_hidden", False)
    depth = optional_int_arg(data, "depth", 1, 32)
    max_results = int_arg(data, "max_results", 1000, 1, 10000)
    start = existing_path(logical)
    results: list[str] = []
    for path in walk_entries(start, include_hidden, depth):
        relative = relative_path(path)
        if matches_any(relative, exclude):
            continue
        if matches_any(relative, patterns) or matches_any(path.name, patterns):
            results.append(relative)
            if len(results) >= max_results:
                return {
                    "path": relative_path(start),
                    "results": results,
                    "returned": len(results),
                    "truncated": True,
                }
    output = {
        "path": relative_path(start),
        "results": results,
        "returned": len(results),
        "truncated": False,
    }
    if not results:
        output["tip"] = (
            "No paths matched. Check patterns, path, depth, exclude, and include_hidden if you expected results."
        )
    return output


def execute_search(data: dict[str, Any]) -> dict[str, Any]:
    logical = string_arg(data, "path", ".")
    query = string_arg(data, "query")
    use_regex = bool_arg(data, "regex", False)
    case_sensitive = bool_arg(data, "case_sensitive", True)
    globs = string_list(data, "globs")
    depth = optional_int_arg(data, "depth", 1, 32)
    context_before = int_arg(data, "context_before", 0, 0, 10000)
    context_after = int_arg(data, "context_after", 0, 0, 10000)
    max_matches = int_arg(data, "max_matches", 500, 1, 5000)
    start = existing_path(logical)
    host = os.environ.get("ME_TOOLBOX_HOST") or shutil.which("me-s")
    if host is None:
        raise ToolError(
            "search_worker_unavailable",
            "File.Search could not locate the integrated me-s search worker.",
            tip="Run File.Search from a managed me-s File toolbox.",
        )
    payload = {
        "path": str(start),
        "query": query,
        "regex": use_regex,
        "case_sensitive": case_sensitive,
        "globs": globs,
        "depth": depth,
        "context_before": context_before,
        "context_after": context_after,
        "max_matches": max_matches,
    }
    process = subprocess.Popen(
        [host, "__toolbox-file-search-worker"],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        encoding="utf-8",
        errors="strict",
    )
    try:
        stdout, stderr = process.communicate(
            json.dumps(payload, ensure_ascii=False, separators=(",", ":")) + "\n",
            timeout=SEARCH_TIMEOUT_SECONDS,
        )
    except subprocess.TimeoutExpired as exc:
        process.kill()
        process.communicate()
        raise ToolError(
            "search_timeout",
            "File.Search timed out after 120 seconds.",
            retryable=False,
            tip=SEARCH_TIMEOUT_TIP,
        ) from exc
    if process.returncode != 0:
        detail = stderr.strip()[:2000]
        message = "The integrated File.Search worker failed."
        if detail:
            message = f"{message} {detail}"
        raise ToolError(
            "search_worker_error",
            message,
            tip="Retry with a smaller search scope. If the failure persists, inspect the me-s installation.",
        )
    try:
        frame = json.loads(stdout)
    except (json.JSONDecodeError, TypeError) as exc:
        raise ToolError(
            "search_worker_error",
            "The integrated File.Search worker returned an invalid response.",
        ) from exc
    if not isinstance(frame, dict) or not isinstance(frame.get("ok"), bool):
        raise ToolError(
            "search_worker_error",
            "The integrated File.Search worker returned an invalid response.",
        )
    if not frame["ok"]:
        detail = frame.get("error")
        if not isinstance(detail, dict):
            raise ToolError(
                "search_worker_error",
                "The integrated File.Search worker returned an invalid error response.",
            )
        raise ToolError(
            str(detail.get("code", "search_worker_error")),
            str(detail.get("message", "File.Search failed.")),
            retryable=bool(detail.get("retryable", False)),
            tip=detail.get("tip") if isinstance(detail.get("tip"), str) else None,
        )
    output = frame.get("output")
    if not isinstance(output, dict):
        raise ToolError(
            "search_worker_error",
            "The integrated File.Search worker returned an invalid result.",
        )
    return output


def execute_stat(data: dict[str, Any]) -> dict[str, Any]:
    logical_paths = string_list(data, "paths", required=True)
    entries: list[dict[str, Any]] = []
    for logical in logical_paths:
        path = inspection_path(logical)
        if not path.exists() and not path.is_symlink():
            entries.append({"path": relative_path(path), "exists": False})
            continue
        info = path.lstat()
        kind = path_type(info.st_mode)
        entry: dict[str, Any] = {
            "path": relative_path(path),
            "exists": True,
            "type": kind,
            "size": info.st_size,
            "modified_ms": info.st_mtime_ns // 1_000_000,
            "readonly": not os.access(path, os.W_OK),
        }
        if kind == "file":
            with path.open("rb") as source:
                digest = hashlib.sha256()
                for block in iter(lambda: source.read(1024 * 1024), b""):
                    digest.update(block)
                opened = os.fstat(source.fileno())
            entry["hash"] = digest.hexdigest()[:8]
            entry["size"] = opened.st_size
            entry["modified_ms"] = opened.st_mtime_ns // 1_000_000
        entries.append(entry)
    output: dict[str, Any] = {"entries": entries, "returned": len(entries)}
    missing = sum(not entry["exists"] for entry in entries)
    if missing:
        output["tip"] = (
            f"{missing} requested path{'s do' if missing != 1 else ' does'} not exist. "
            "Check entries with exists=false before continuing."
        )
    return output


def mutation_result(
    path: Path,
    operation: str,
    previous_hash: str | None,
    content: bytes,
    encoding: str,
    confidence: float,
    bom: bytes,
) -> dict[str, Any]:
    return {
        "path": relative_path(path),
        "operation": operation,
        "previous_hash": previous_hash,
        "hash": sha256_bytes(content),
        "size": len(content),
        "encoding": encoding,
        "encoding_confidence": confidence,
        "bom": bool(bom),
    }


def execute_make_directory(data: dict[str, Any]) -> dict[str, Any]:
    logical = string_arg(data, "path")
    parents = bool_arg(data, "parents", False)
    path = recursive_lexical_path(logical) if parents else lexical_path(logical)
    with mutation_lock():
        try:
            path.mkdir(parents=parents)
        except FileExistsError as exc:
            raise ToolError(
                "already_exists",
                f"destination already exists: {logical}",
                tip="Please choose a new path, or inspect the existing path before deciding what to do.",
            ) from exc
        except OSError as exc:
            raise ToolError(
                "create_directory_error", f"cannot create directory {logical}: {exc}"
            ) from exc
    return {
        "path": relative_path(path),
        "operation": "directory_created",
        "exists": True,
    }


def execute_create(data: dict[str, Any]) -> dict[str, Any]:
    logical = string_arg(data, "path")
    text = string_arg(data, "content", "")
    encoding = encoding_arg(data, default="utf-8", allow_auto=False)
    bom = create_bom(encoding, bool_arg(data, "bom", False))
    content = encode_text(text, encoding, bom, logical)
    path = lexical_path(logical)
    with mutation_lock():
        if path.exists() or path.is_symlink():
            raise ToolError(
                "already_exists",
                f"destination already exists: {logical}",
                tip="Please choose a new path, or inspect the existing file before deciding whether to edit or replace it.",
            )
        atomic_create(path, content)
        clear_edit_scope(path)
    return mutation_result(path, "created", None, content, encoding, 1.0, bom)


def split_patch_lines(patch: str) -> list[str]:
    lines = patch.split("\n")
    if lines and lines[-1] == "":
        lines.pop()
    return [line[:-1] if line.endswith("\r") else line for line in lines]


def patch_header_path(line: str, marker: str) -> str:
    if not line.startswith(marker):
        raise ToolError("invalid_patch", f"patch must begin with {marker.strip()} file header")
    value = line[len(marker) :].split("\t", 1)[0]
    if not value or value == "/dev/null":
        raise ToolError(
            "invalid_patch",
            "ApplyPatch requires an existing file; header paths cannot be empty or /dev/null",
        )
    return value.replace("\\", "/")


def header_matches_path(header: str, logical: str, prefix: str) -> bool:
    normalized = logical.replace("\\", "/")
    while normalized.startswith("./"):
        normalized = normalized[2:]
    return header == normalized or header == f"{prefix}/{normalized}"


def parse_unified_diff(patch: str, logical: str) -> list[PatchHunk]:
    lines = split_patch_lines(patch)
    if len(lines) < 3:
        raise ToolError(
            "invalid_patch",
            "patch requires --- and +++ file headers followed by at least one @@ hunk",
        )
    old_path = patch_header_path(lines[0], "--- ")
    new_path = patch_header_path(lines[1], "+++ ")
    if not header_matches_path(old_path, logical, "a"):
        raise ToolError(
            "invalid_patch",
            f"old header path {old_path!r} does not match path {logical!r}",
        )
    if not header_matches_path(new_path, logical, "b"):
        raise ToolError(
            "invalid_patch",
            f"new header path {new_path!r} does not match path {logical!r}",
        )

    hunks: list[PatchHunk] = []
    index = 2
    changed = False
    while index < len(lines):
        header = lines[index]
        match = HUNK_PATTERN.fullmatch(header)
        if match is None:
            if header.startswith(("--- ", "+++ ", "diff --git ", "index ")):
                detail = "multiple files and git metadata are not supported"
            else:
                detail = f"expected @@ hunk header at patch line {index + 1}"
            raise ToolError("invalid_patch", detail)
        old_start = int(match.group(1))
        old_count = int(match.group(2)) if match.group(2) is not None else 1
        new_start = int(match.group(3))
        new_count = int(match.group(4)) if match.group(4) is not None else 1
        if (old_count > 0 and old_start == 0) or (new_count > 0 and new_start == 0):
            raise ToolError(
                "invalid_patch",
                f"non-empty ranges in hunk {len(hunks) + 1} must start at line 1 or later",
            )
        index += 1
        body: list[PatchLine] = []
        while index < len(lines) and not lines[index].startswith("@@ "):
            line = lines[index]
            if line == "\\ No newline at end of file":
                if not body or body[-1].no_newline:
                    raise ToolError(
                        "invalid_patch",
                        f"misplaced no-newline marker at patch line {index + 1}",
                    )
                body[-1].no_newline = True
                index += 1
                continue
            if not line or line[0] not in " +-":
                raise ToolError(
                    "invalid_patch",
                    f"hunk line {index + 1} must begin with one space, +, or -",
                )
            entry = PatchLine(line[0], line[1:])
            body.append(entry)
            changed = changed or entry.kind in "+-"
            index += 1
        actual_old = sum(entry.kind in " -" for entry in body)
        actual_new = sum(entry.kind in " +" for entry in body)
        if actual_old != old_count or actual_new != new_count:
            raise ToolError(
                "invalid_patch",
                f"hunk {len(hunks) + 1} declares old/new counts {old_count}/{new_count} "
                f"but its body contains {actual_old}/{actual_new}",
            )
        hunks.append(
            PatchHunk(old_start, old_count, new_start, new_count, tuple(body))
        )
    if not hunks:
        raise ToolError("invalid_patch", "patch must contain at least one @@ hunk")
    if not changed:
        raise ToolError("invalid_patch", "patch contains no added or removed lines")
    return hunks


def split_text_lines(text: str) -> list[TextLine]:
    lines: list[TextLine] = []
    start = 0
    for match in re.finditer(r"\r\n|\n|\r", text):
        lines.append(TextLine(text[start : match.start()], match.group(0)))
        start = match.end()
    if start < len(text):
        lines.append(TextLine(text[start:], ""))
    return lines


def prevailing_line_ending(lines: list[TextLine]) -> str:
    counts: dict[str, int] = {}
    for line in lines:
        if line.ending:
            counts[line.ending] = counts.get(line.ending, 0) + 1
    return max(counts, key=counts.get) if counts else "\n"


def apply_unified_diff(text: str, hunks: list[PatchHunk]) -> tuple[str, int, int]:
    original = split_text_lines(text)
    result: list[TextLine] = []
    source_cursor = 0
    preferred_ending = prevailing_line_ending(original)
    added = 0
    removed = 0

    for hunk_index, hunk in enumerate(hunks, 1):
        source_index = hunk.old_start if hunk.old_count == 0 else hunk.old_start - 1
        if source_index < source_cursor:
            raise ToolError(
                "invalid_patch", f"hunk {hunk_index} overlaps or precedes an earlier hunk"
            )
        if source_index > len(original):
            raise ToolError(
                "patch_conflict",
                f"hunk {hunk_index} starts beyond the end of the file",
                True,
            )
        result.extend(original[source_cursor:source_index])
        expected_new_index = (
            hunk.new_start if hunk.new_count == 0 else hunk.new_start - 1
        )
        if len(result) != expected_new_index:
            raise ToolError(
                "invalid_patch",
                f"hunk {hunk_index} new-file start is inconsistent with earlier hunks",
            )
        cursor = source_index
        for entry in hunk.lines:
            if entry.kind in " -":
                if cursor >= len(original):
                    raise ToolError(
                        "patch_conflict",
                        f"hunk {hunk_index} expects content beyond the end of the file",
                        True,
                    )
                current = original[cursor]
                if current.text != entry.text:
                    raise ToolError(
                        "patch_conflict",
                        f"hunk {hunk_index} context mismatch at original line {cursor + 1}",
                        True,
                    )
                actual_no_newline = current.ending == ""
                if actual_no_newline != entry.no_newline:
                    raise ToolError(
                        "patch_conflict",
                        f"hunk {hunk_index} newline marker mismatch at original line {cursor + 1}",
                        True,
                    )
                cursor += 1
                if entry.kind == " ":
                    result.append(current)
                else:
                    removed += 1
            else:
                ending = "" if entry.no_newline else preferred_ending
                result.append(TextLine(entry.text, ending))
                added += 1
        source_cursor = cursor

    result.extend(original[source_cursor:])
    for line_index, line in enumerate(result[:-1], 1):
        if line.ending == "":
            raise ToolError(
                "invalid_patch",
                f"no-newline marker creates a non-final line at new line {line_index}",
            )
    return "".join(line.text + line.ending for line in result), added, removed


def execute_apply_patch(data: dict[str, Any]) -> dict[str, Any]:
    logical = string_arg(data, "path")
    expected = validate_expected_hash(data.get("expected_hash"))
    encoding = encoding_arg(data)
    patch = string_arg(data, "patch")
    with mutation_lock():
        path = existing_path(logical)
        require_regular_file(path, logical, True)
        document = read_text_file(path, logical, encoding)
        raw = document.raw
        previous = verify_content_hash(document.raw, expected)
        hunks = parse_unified_diff(patch, relative_path(path))
        text, added, removed = apply_unified_diff(document.text, hunks)
        updated = encode_text(text, document.encoding, document.bom, logical)
        verify_hash(path, expected)
        atomic_replace(path, updated, path.stat().st_mode)
    output = mutation_result(
        path,
        "patched",
        previous,
        updated,
        document.encoding,
        document.confidence,
        document.bom,
    )
    output["hunks_applied"] = len(hunks)
    output["lines_added"] = added
    output["lines_removed"] = removed
    output["previous_size"] = len(raw)
    return output


def execute_edit(data: dict[str, Any]) -> dict[str, Any]:
    logical = string_arg(data, "path")
    encoding = encoding_arg(data)
    requested_edits = data.get("edits")
    if (
        not isinstance(requested_edits, list)
        or not requested_edits
        or len(requested_edits) > MAX_EDIT_OPERATIONS
    ):
        raise ToolError(
            "invalid_arguments",
            f"edits must contain 1..={MAX_EDIT_OPERATIONS} operation objects",
            tip=f"Please provide a non-empty edits array with at most {MAX_EDIT_OPERATIONS} operations.",
        )
    with mutation_lock():
        path = existing_path(logical)
        require_regular_file(path, logical, True)
        scope = import_edit_scope(data, path)
        if scope is None:
            raise ToolError(
                "read_required",
                f"File.Edit has no readable range for {relative_path(path)}; call File.Read for every target range before editing",
                tip=TIP_READ_EDIT_RANGE,
            )
        document = read_text_file(path, logical, encoding)
        current_hash = sha256_bytes(document.raw)
        if current_hash != scope.content_hash:
            clear_edit_scope(path)
            raise ToolError(
                "stale_read",
                f"{relative_path(path)} changed after File.Read; call File.Read again before editing",
                True,
                TIP_READ_EDIT_RANGE,
            )
        previous = current_hash
        lines = split_text_file_lines(document.text)
        text_lines = split_text_lines(document.text)
        total_lines = len(lines)
        if total_lines != scope.total_lines:
            clear_edit_scope(path)
            raise ToolError(
                "stale_read",
                f"{relative_path(path)} line structure changed after File.Read; call File.Read again before editing",
                True,
                TIP_READ_EDIT_RANGE,
            )
        preferred_ending = prevailing_line_ending(text_lines)
        line_offsets = [0]
        for line in lines:
            line_offsets.append(line_offsets[-1] + len(line))
        resolved: list[ResolvedEdit] = []
        for index, value in enumerate(requested_edits):
            item = validate_object(value)
            operation = item.get("operation")
            if operation == "replace":
                required_fields = {"operation", "start_line", "end_line", "new_lines"}
            elif operation == "delete":
                required_fields = {"operation", "start_line", "end_line"}
            elif operation == "insert":
                required_fields = {"operation", "before_line", "new_lines"}
            else:
                raise ToolError(
                    "invalid_arguments",
                    f"edits[{index}].operation must be replace, delete, or insert",
                    tip="Please choose exactly one supported operation: replace, delete, or insert.",
                )
            unexpected = sorted(set(item) - required_fields)
            missing = sorted(required_fields - set(item))
            if unexpected or missing:
                details = []
                if missing:
                    details.append(f"missing fields: {', '.join(missing)}")
                if unexpected:
                    details.append(f"unexpected fields: {', '.join(unexpected)}")
                raise ToolError(
                    "invalid_arguments",
                    f"edits[{index}] " + "; ".join(details),
                    tip="Please use the exact fields for the selected operation and remove unrelated fields.",
                )
            if operation in {"replace", "delete"}:
                start_line = int_arg(item, "start_line", 0, 1, 2**31 - 1)
                end_line = int_arg(item, "end_line", 0, 1, 2**31 - 1)
                before_line = None
                if not 1 <= start_line <= end_line <= total_lines:
                    raise ToolError(
                        "invalid_range",
                        f"edits[{index}] requires 1 <= start_line <= end_line <= total_lines; "
                        f"received start_line={start_line}, end_line={end_line}, total_lines={total_lines}",
                        tip=TIP_READ_EDIT_RANGE,
                    )
                if not range_is_covered(scope, start_line, end_line):
                    raise ToolError(
                        "unread_range",
                        f"edits[{index}] targets lines {start_line}-{end_line}, which are not fully inside the current editable ranges {scope_ranges_value(scope)}; call File.Read for the missing range",
                        tip=TIP_READ_EDIT_RANGE,
                    )
                new_lines = (
                    logical_lines_arg(item, index)
                    if operation == "replace"
                    else tuple()
                )
                if operation == "replace" and not new_lines:
                    raise ToolError(
                        "invalid_arguments",
                        f"edits[{index}].new_lines cannot be empty for replace; use operation=delete",
                        tip="Please provide at least one replacement line, or use operation=delete to remove the selected lines.",
                    )
                source_start = line_offsets[start_line - 1]
                source_end = line_offsets[end_line]
            else:
                start_line = None
                end_line = None
                before_line = int_arg(item, "before_line", 0, 1, 2**31 - 1)
                if before_line > total_lines + 1:
                    raise ToolError(
                        "invalid_range",
                        f"edits[{index}].before_line must be <= total_lines + 1; "
                        f"received before_line={before_line}, total_lines={total_lines}",
                        tip=TIP_READ_EDIT_RANGE,
                    )
                if total_lines == 0:
                    insertion_read = before_line == 1 and scope.eof
                elif before_line <= total_lines:
                    insertion_read = range_is_covered(scope, before_line, before_line)
                else:
                    insertion_read = scope.eof and range_is_covered(
                        scope, total_lines, total_lines
                    )
                if not insertion_read:
                    raise ToolError(
                        "unread_range",
                        f"edits[{index}] insertion point before line {before_line} was not established by File.Read; call File.Read around that insertion point",
                        tip=TIP_READ_EDIT_RANGE,
                    )
                new_lines = logical_lines_arg(item, index)
                if not new_lines:
                    raise ToolError(
                        "invalid_line_syntax",
                        f"edits[{index}].new_lines cannot be empty for insert",
                        tip="Please provide at least one logical line to insert.",
                    )
                if (
                    before_line == total_lines + 1
                    and total_lines > 0
                    and not (lines[-1].endswith("\r") or lines[-1].endswith("\n"))
                ):
                    raise ToolError(
                        "invalid_line_syntax",
                        f"edits[{index}] cannot insert after an unterminated final line; replace that final line instead",
                        tip="Please read a wider range around the final line, then replace that line with the complete intended result.",
                    )
                source_start = line_offsets[before_line - 1]
                source_end = source_start
            if operation == "delete":
                replacement_text = ""
            elif operation == "replace":
                target_ending = text_lines[end_line - 1].ending or preferred_ending
                preserve_unterminated_eof = (
                    end_line == total_lines and text_lines[end_line - 1].ending == ""
                )
                rendered_lines = [line + target_ending for line in new_lines]
                if rendered_lines and preserve_unterminated_eof:
                    rendered_lines[-1] = new_lines[-1]
                replacement_text = "".join(rendered_lines)
            else:
                if total_lines == 0:
                    target_ending = preferred_ending
                elif before_line <= total_lines:
                    target_ending = text_lines[before_line - 1].ending or preferred_ending
                else:
                    target_ending = text_lines[-1].ending or preferred_ending
                replacement_text = "".join(line + target_ending for line in new_lines)
            replacement = encode_text(
                replacement_text, document.encoding, b"", logical
            )
            resolved.append(
                ResolvedEdit(
                    index=index,
                    operation=operation,
                    start_line=start_line,
                    end_line=end_line,
                    before_line=before_line,
                    source_start=source_start,
                    source_end=source_end,
                    new_lines=new_lines,
                    replacement_text=replacement_text,
                    replacement_bytes=len(replacement),
                )
            )
        for left_index, left in enumerate(resolved):
            for right in resolved[left_index + 1 :]:
                left_inserting = left.source_start == left.source_end
                right_inserting = right.source_start == right.source_end
                conflict = False
                if left_inserting and right_inserting:
                    conflict = left.source_start == right.source_start
                elif left_inserting:
                    conflict = right.source_start < left.source_start < right.source_end
                elif right_inserting:
                    conflict = left.source_start < right.source_start < left.source_end
                else:
                    conflict = max(left.source_start, right.source_start) < min(
                        left.source_end, right.source_end
                    )
                if conflict:
                    raise ToolError(
                        "overlapping_edits",
                        f"edits[{left.index}] and edits[{right.index}] overlap or use the same original insertion point; all edit coordinates must be independent",
                        tip="Please combine overlapping changes into one edit item, or read the updated area and apply dependent changes in a later File.Edit call.",
                    )
        ordered = sorted(
            resolved,
            key=lambda item: (
                item.source_start,
                0 if item.source_start == item.source_end else 1,
                item.source_end,
                item.index,
            ),
        )
        pieces: list[str] = []
        cursor = 0
        for item in ordered:
            pieces.append(document.text[cursor : item.source_start])
            pieces.append(item.replacement_text)
            cursor = item.source_end
        pieces.append(document.text[cursor:])
        updated_text = "".join(pieces)
        updated = encode_text(
            updated_text, document.encoding, document.bom, logical
        )
        mode = path.stat().st_mode
        verify_hash(path, scope.content_hash)
        atomic_replace(path, updated, mode)
        clear_edit_scope(path)
    output = mutation_result(
        path,
        "edited",
        previous,
        updated,
        document.encoding,
        document.confidence,
        document.bom,
    )
    output.pop("hash")
    output.update(
        {
            "edit_results": [
                dict(
                    {
                        "index": item.index,
                        "state": "succeeded",
                        "operation": item.operation,
                        "selected_lines": (
                            0
                            if item.operation == "insert"
                            else item.end_line - item.start_line + 1
                        ),
                        "new_line_count": len(item.new_lines),
                        "replacement_bytes": item.replacement_bytes,
                    },
                    **(
                        {"before_line": item.before_line}
                        if item.operation == "insert"
                        else {
                            "start_line": item.start_line,
                            "end_line": item.end_line,
                        }
                    ),
                )
                for item in sorted(resolved, key=lambda item: item.index)
            ],
            "previous_total_lines": total_lines,
            "total_lines": len(split_text_file_lines(updated_text)),
            "previous_size": len(document.raw),
            "tip": EDIT_TIP,
        }
    )
    return output


def execute_edit_bytes(data: dict[str, Any]) -> dict[str, Any]:
    logical = string_arg(data, "path")
    expected = validate_expected_hash(data.get("expected_hash"))
    requested_edits = data.get("edits")
    if (
        not isinstance(requested_edits, list)
        or not requested_edits
        or len(requested_edits) > MAX_EDIT_OPERATIONS
    ):
        raise ToolError(
            "invalid_arguments",
            f"edits must contain 1..={MAX_EDIT_OPERATIONS} operation objects",
        )
    with mutation_lock():
        path = existing_path(logical)
        require_regular_file(path, logical, True)
        with path.open("rb") as source:
            raw = source.read()
        previous = verify_content_hash(raw, expected)
        original_size = len(raw)
        resolved: list[ResolvedByteEdit] = []
        required_fields = {"target_offset", "target_length", "data"}
        for index, value in enumerate(requested_edits):
            item = validate_object(value)
            unexpected = sorted(set(item) - required_fields)
            missing = sorted(required_fields - set(item))
            if unexpected or missing:
                details = []
                if missing:
                    details.append(f"missing fields: {', '.join(missing)}")
                if unexpected:
                    details.append(f"unexpected fields: {', '.join(unexpected)}")
                raise ToolError(
                    "invalid_arguments", f"edits[{index}] " + "; ".join(details)
                )
            target_offset = int_arg(item, "target_offset", -1, 0, 2**63 - 1)
            target_length = int_arg(item, "target_length", -1, 0, 2**63 - 1)
            replacement = hex_data_arg(item, index)
            if target_offset > original_size or target_length > original_size - target_offset:
                raise ToolError(
                    "invalid_range",
                    f"edits[{index}] range [{target_offset}, {target_offset + target_length}) "
                    f"must fit within the original {original_size}-byte file",
                )
            if target_length == 0 and not replacement:
                raise ToolError(
                    "invalid_byte_syntax",
                    f"edits[{index}].data cannot be empty for an insertion",
                )
            resolved.append(
                ResolvedByteEdit(
                    index=index,
                    target_offset=target_offset,
                    target_length=target_length,
                    source_start=target_offset,
                    source_end=target_offset + target_length,
                    data=replacement,
                    kind=(
                        "insert"
                        if target_length == 0
                        else "delete"
                        if not replacement
                        else "replace"
                    ),
                )
            )
        for left_index, left in enumerate(resolved):
            for right in resolved[left_index + 1 :]:
                left_inserting = left.source_start == left.source_end
                right_inserting = right.source_start == right.source_end
                conflict = False
                if left_inserting and right_inserting:
                    conflict = left.source_start == right.source_start
                elif left_inserting:
                    conflict = right.source_start < left.source_start < right.source_end
                elif right_inserting:
                    conflict = left.source_start < right.source_start < left.source_end
                else:
                    conflict = max(left.source_start, right.source_start) < min(
                        left.source_end, right.source_end
                    )
                if conflict:
                    raise ToolError(
                        "overlapping_edits",
                        f"edits[{left.index}] and edits[{right.index}] overlap or use the same original insertion point; all byte edit coordinates must be independent",
                    )
        ordered = sorted(
            resolved,
            key=lambda item: (
                item.source_start,
                0 if item.source_start == item.source_end else 1,
                item.source_end,
                item.index,
            ),
        )
        pieces: list[bytes] = []
        cursor = 0
        for item in ordered:
            pieces.append(raw[cursor : item.source_start])
            pieces.append(item.data)
            cursor = item.source_end
        pieces.append(raw[cursor:])
        updated = b"".join(pieces)
        mode = path.stat().st_mode
        verify_hash(path, expected)
        atomic_replace(path, updated, mode)
        clear_edit_scope(path)
    return {
        "path": relative_path(path),
        "operation": "bytes_edited",
        "previous_hash": previous,
        "edit_results": [
            {
                "index": item.index,
                "state": "succeeded",
                "kind": item.kind,
                "target_offset": item.target_offset,
                "target_length": item.target_length,
                "selected_bytes": item.target_length,
                "replacement_bytes": len(item.data),
            }
            for item in sorted(resolved, key=lambda item: item.index)
        ],
        "previous_size": original_size,
        "size": len(updated),
        "tip": EDIT_BYTES_TIP,
    }


def execute_append(data: dict[str, Any]) -> dict[str, Any]:
    logical = string_arg(data, "path")
    expected = validate_expected_hash(data.get("expected_hash"))
    encoding = encoding_arg(data)
    appended_text = string_arg(data, "content", "")
    with mutation_lock():
        path = existing_path(logical)
        require_regular_file(path, logical, True)
        document = read_text_file(path, logical, encoding)
        previous = verify_content_hash(document.raw, expected)
        appended = encode_text(appended_text, document.encoding, b"", logical)
        updated = document.raw + appended
        verify_hash(path, expected)
        atomic_replace(path, updated, path.stat().st_mode)
        clear_edit_scope(path)
    output = mutation_result(
        path,
        "appended",
        previous,
        updated,
        document.encoding,
        document.confidence,
        document.bom,
    )
    output["appended_bytes"] = len(appended)
    return output


def execute_replace(data: dict[str, Any]) -> dict[str, Any]:
    logical = string_arg(data, "path")
    expected = validate_expected_hash(data.get("expected_hash"))
    encoding = encoding_arg(data)
    text = string_arg(data, "content", "")
    with mutation_lock():
        path = existing_path(logical)
        require_regular_file(path, logical, True)
        document = read_text_file(path, logical, encoding)
        previous = verify_content_hash(document.raw, expected)
        updated = encode_text(text, document.encoding, document.bom, logical)
        mode = path.stat().st_mode
        verify_hash(path, expected)
        atomic_replace(path, updated, mode)
        clear_edit_scope(path)
    return mutation_result(
        path,
        "replaced",
        previous,
        updated,
        document.encoding,
        document.confidence,
        document.bom,
    )


def execute_copy(data: dict[str, Any]) -> dict[str, Any]:
    logical = string_arg(data, "path")
    destination = string_arg(data, "destination")
    expected = validate_expected_hash(data.get("expected_hash"))
    with mutation_lock():
        source = existing_path(logical)
        require_regular_file(source, logical, True)
        target = lexical_path(destination)
        if target.exists() or target.is_symlink():
            raise ToolError(
                "already_exists",
                f"destination already exists: {destination}",
                tip="Please choose a new destination, or inspect the existing path before deciding what to do.",
            )
        copied_hash, size = atomic_copy(source, target, expected)
        clear_edit_scope(target)
    return {
        "path": relative_path(source),
        "destination": relative_path(target),
        "operation": "copied",
        "hash": copied_hash,
        "size": size,
    }


def execute_move(data: dict[str, Any]) -> dict[str, Any]:
    logical = string_arg(data, "path")
    destination = string_arg(data, "destination")
    expected = validate_expected_hash(data.get("expected_hash"))
    with mutation_lock():
        source = existing_path(logical)
        require_regular_file(source, logical, True)
        target = lexical_path(destination)
        if target.exists() or target.is_symlink():
            raise ToolError(
                "already_exists",
                f"destination already exists: {destination}",
                tip="Please choose a new destination, or inspect the existing path before deciding what to do.",
            )
        previous = verify_hash(source, expected)
        size = source.stat().st_size
        try:
            source.rename(target)
        except OSError as exc:
            raise ToolError("move_error", f"cannot move {logical} to {destination}: {exc}") from exc
        clear_edit_scope(source)
        clear_edit_scope(target)
    return {
        "path": relative_path(source),
        "destination": relative_path(target),
        "operation": "moved",
        "previous_hash": previous,
        "hash": previous,
        "size": size,
    }


def execute_delete(data: dict[str, Any]) -> dict[str, Any]:
    logical = string_arg(data, "path")
    expected = validate_expected_hash(data.get("expected_hash"))
    with mutation_lock():
        path = existing_path(logical)
        require_regular_file(path, logical, True)
        deleted_hash = verify_hash(path, expected)
        try:
            path.unlink()
        except OSError as exc:
            raise ToolError("delete_error", f"cannot delete {logical}: {exc}") from exc
        clear_edit_scope(path)
    return {
        "path": relative_path(path),
        "operation": "deleted",
        "deleted_hash": deleted_hash,
        "exists": False,
    }


EXECUTORS = {
    "Read": execute_read,
    "ReadBytes": execute_read_bytes,
    "EditBytes": execute_edit_bytes,
    "List": execute_list,
    "Find": execute_find,
    "Search": execute_search,
    "Stat": execute_stat,
    "MakeDirectory": execute_make_directory,
    "Create": execute_create,
    "Edit": execute_edit,
    "Append": execute_append,
    "Replace": execute_replace,
    "Copy": execute_copy,
    "Move": execute_move,
    "Delete": execute_delete,
}


def handle(request: Any) -> None:
    if not isinstance(request, dict) or not isinstance(request.get("id"), int):
        raise ToolError("invalid_request", "request must contain an integer id")
    request_id = request["id"]
    command = request.get("cmd")
    if command == "getTools":
        result(request_id, TOOLS)
        return
    if command == "getBrief":
        result(
            request_id,
            "Read, search, copy, and safely mutate files and explicitly create directories. Relative paths resolve from the workspace; absolute paths and relative paths that resolve outside the workspace are supported. Paths inside the workspace are returned relative to it, while outside paths are returned as normalized absolute paths. PATH SUPPORT IS CAPABILITY, NOT AUTHORIZATION: obey the governing external-path safety rule before any modification outside the workspace. Source file size is not artificially capped; operations that need complete contents load them into memory, while bounded query parameters limit model-visible results. Line-oriented results and edits use logical lines without CR or LF; File preserves the file's detected line-ending convention automatically. Exact text reads and writes conservatively detect common Unicode, East Asian, and Windows encodings, preserve the original encoding and BOM, and reject uncertain or lossy writes. File.Search instead uses me-s's integrated ripgrep engine with bounded 64 KiB encoding detection and strict streaming transcoding for common legacy encodings, follows ignore rules, and has a fixed 120-second deadline. File.Edit is limited to ranges actually returned by File.Read, clears those ranges after success, and validates the remembered file version internally. Binary operations use zero-based byte ranges and canonical hexadecimal data. Other mutations use an 8-character SHA-256-derived concurrency fingerprint. This short value detects stale edits; it is not a security integrity digest. Recoverable failures may include a short tip that states the next useful action in plain language.",
        )
        return
    tool = request.get("tool")
    if tool == "ApplyPatch":
        raise ToolError(
            "tool_disabled",
            "File.ApplyPatch is disabled. Use File.Edit instead.",
        )
    if tool not in TOOLS:
        raise ToolError("unknown_tool", f"unknown File tool: {tool}")
    if command == "getInputSchema":
        result(request_id, INPUT_SCHEMAS[tool])
    elif command == "getOutputSchema":
        result(request_id, OUTPUT_SCHEMAS[tool])
    elif command == "getInstructions":
        result(request_id, INSTRUCTIONS[tool])
    elif command == "getRoute":
        result(request_id, ROUTES[tool])
    elif command == "getExamples":
        result(request_id, EXAMPLES[tool])
    elif command == "execute":
        data = validate_object(request.get("input"))
        allowed = set(INPUT_SCHEMAS[tool]["properties"])
        if tool == "Edit":
            # _edit_scope is injected by ME-S from persisted EDB state.
            # expected_hash is accepted only as a harmless legacy field; Edit
            # never trusts it and always uses the remembered Read scope.
            allowed.update({"_edit_scope", "expected_hash"})
        unexpected = sorted(set(data) - allowed)
        if unexpected:
            raise ToolError(
                "invalid_arguments", f"unexpected input fields: {', '.join(unexpected)}"
            )
        result(request_id, EXECUTORS[tool](data))
    else:
        raise ToolError("unknown_command", f"unsupported command: {command}")


for line in sys.stdin:
    request_id = 0
    try:
        request = json.loads(line)
        if isinstance(request, dict) and isinstance(request.get("id"), int):
            request_id = request["id"]
        handle(request)
    except ToolError as exc:
        error(request_id, exc)
    except (OSError, ValueError, TypeError, shutil.Error) as exc:
        error(request_id, ToolError("execution_error", str(exc)))
