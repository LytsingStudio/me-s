#!/usr/bin/env python3
# ME-S-MANAGED-TOOLBOX
"""ME-S default WebBrowser toolbox.

The process is a standalone, persistent JSONL toolbox. It lazily installs
pinned Camoufox, Playwright, and PySide6 runtimes plus the matching anti-detect
Firefox build into ME-S's global configuration directory. Camoufox performs
real headed browser interaction on graphical desktops while its native window
stays concealed outside explicit human handoffs. Page observation uses the
browser-generated accessibility tree or a rendered viewport image; action
tools never perform implicit observation.
"""

from __future__ import annotations

import asyncio
import contextlib
import ctypes
import ctypes.util
import importlib
import json
import os
from pathlib import Path
import platform
import queue
import shutil
import signal
import subprocess
import sys
import tempfile
import threading
import time
from typing import Any, Callable, Iterator
from urllib.parse import unquote, urlparse


def fail_startup(message: str) -> "None":
    print(message, file=sys.stderr, flush=True)
    raise SystemExit(1)


if sys.version_info[:2] != (3, 12):
    fail_startup(
        "WebBrowser toolbox requires Python 3.12; "
        f"received {sys.version_info.major}.{sys.version_info.minor}"
    )

sys.stdin.reconfigure(encoding="utf-8", errors="strict")
sys.stdout.reconfigure(encoding="utf-8", errors="strict", newline="\n")
sys.stderr.reconfigure(encoding="utf-8", errors="strict", newline="\n")


CAMOUFOX_VERSION = "0.5.4"
PLAYWRIGHT_VERSION = "1.60.0"
PYSIDE6_VERSION = "6.9.3"
CAMOUFOX_BROWSER_VERSION = "152.0.4-beta.28"
CAMOUFOX_BROWSER_RELEASE = f"official/stable/{CAMOUFOX_BROWSER_VERSION}"
MIN_SNAPSHOT_WAIT_MS = 1_000
MAX_SNAPSHOT_WAIT_MS = 60_000
DEFAULT_OPERATION_HARD_TIMEOUT_MS = 30_000
DEFAULT_CREATE_HARD_TIMEOUT_MS = 20 * 60_000
INSTALL_COMMAND_TIMEOUT_SECONDS = 10 * 60
INSTALL_LOCK_WAIT_SECONDS = 2 * 60
MAX_BROWSER_EVENTS = 100
MAX_BROWSER_EVENT_TEXT = 4_000
WORKER_MODE_ENV = "ME_WEB_BROWSER_WORKER"
HARD_TIMEOUT_GRACE_MS = 70_000
SCREENSHOT_DIRECTORY = Path.cwd() / ".me" / "webbrowser" / "screenshots"
TOOLS = [
    "Create",
    "Navigate",
    "Click",
    "Type",
    "Press",
    "Scroll",
    "RequireHumanAction",
    "Snapshot",
    "Pages",
    "Back",
    "Close",
]


def operation_hard_timeout_ms() -> int:
    configured = os.environ.get("ME_WEB_BROWSER_TEST_OPERATION_TIMEOUT_MS")
    if configured is None:
        return DEFAULT_OPERATION_HARD_TIMEOUT_MS
    try:
        return max(100, min(DEFAULT_OPERATION_HARD_TIMEOUT_MS, int(configured)))
    except ValueError:
        return DEFAULT_OPERATION_HARD_TIMEOUT_MS


def create_hard_timeout_ms() -> int:
    configured = os.environ.get("ME_WEB_BROWSER_TEST_CREATE_TIMEOUT_MS")
    if configured is None:
        return DEFAULT_CREATE_HARD_TIMEOUT_MS
    try:
        return max(100, min(DEFAULT_CREATE_HARD_TIMEOUT_MS, int(configured)))
    except ValueError:
        return DEFAULT_CREATE_HARD_TIMEOUT_MS


class ToolError(Exception):
    def __init__(self, code: str, message: str, retryable: bool = False):
        super().__init__(message)
        self.code = code
        self.message = message
        self.retryable = retryable


def compact_id(prefix: str, sequence: int) -> str:
    if sequence < 1 or sequence > 9_999_999:
        raise ToolError(
            "id_space_exhausted",
            f"WebBrowser exhausted its {prefix!r} identifier space; restart the toolbox runtime",
        )
    return f"{prefix}{sequence:07d}"


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


PAGE_ID = {"type": "string", "pattern": r"^p[0-9]{7}$"}
ELEMENT_ID = {
    "type": "string",
    "pattern": r"^(?:f[0-9]+)*e[0-9]+$",
    "description": "A Playwright ARIA reference copied exactly from [ref=…] in the latest text snapshot. Frame elements use values such as f1e2.",
}
URL = {"type": "string", "minLength": 1, "maxLength": 16_384}
SNAPSHOT_WAIT = {
    "type": "integer",
    "minimum": MIN_SNAPSHOT_WAIT_MS,
    "maximum": MAX_SNAPSHOT_WAIT_MS,
    "description": "Fixed delay in milliseconds before the one-time snapshot is captured.",
}
SNAPSHOT_KIND = {
    "type": "string",
    "enum": ["text", "screen", "both"],
}

INPUT_SCHEMAS: dict[str, dict[str, Any]] = {
    "Create": object_schema({}),
    "Navigate": object_schema({"page_id": PAGE_ID, "url": URL}, ["page_id", "url"]),
    "Click": object_schema(
        {"page_id": PAGE_ID, "element_id": ELEMENT_ID},
        ["page_id", "element_id"],
    ),
    "Type": object_schema(
        {
            "page_id": PAGE_ID,
            "element_id": ELEMENT_ID,
            "content": {"type": "string", "maxLength": 1_000_000},
            "mode": {
                "type": "string",
                "enum": ["replace", "append"],
                "default": "replace",
            },
        },
        ["page_id", "element_id", "content"],
    ),
    "Press": object_schema(
        {
            "page_id": PAGE_ID,
            "key": {"type": "string", "minLength": 1, "maxLength": 128},
            "element_id": ELEMENT_ID,
        },
        ["page_id", "key"],
    ),
    "Scroll": object_schema(
        {
            "page_id": PAGE_ID,
            "delta_x": {
                "type": "integer",
                "minimum": -100_000,
                "maximum": 100_000,
                "default": 0,
            },
            "delta_y": {
                "type": "integer",
                "minimum": -100_000,
                "maximum": 100_000,
                "default": 720,
            },
            "element_id": ELEMENT_ID,
        },
        ["page_id"],
    ),
    "RequireHumanAction": object_schema(
        {
            "page_id": PAGE_ID,
            "instruction": {
                "type": "string",
                "minLength": 1,
                "maxLength": 4_000,
                "description": "A concise explanation of the exact action the user must complete in the visible browser.",
            },
        },
        ["page_id", "instruction"],
    ),
    "Snapshot": object_schema(
        {"page_id": PAGE_ID, "wait_ms": SNAPSHOT_WAIT, "kind": SNAPSHOT_KIND},
        ["page_id", "wait_ms", "kind"],
    ),
    "Pages": object_schema({}),
    "Back": object_schema({"page_id": PAGE_ID}, ["page_id"]),
    "Close": object_schema({"page_id": PAGE_ID}, ["page_id"]),
}

BROWSER_EVENT_SCHEMA = object_schema(
    {
        "kind": {
            "type": "string",
            "enum": ["console", "page_error", "request_failed", "http_error"],
        },
        "level": {"type": "string"},
        "message": {"type": "string"},
        "url": {"type": "string"},
        "method": {"type": "string"},
        "resource_type": {"type": "string"},
        "status": {"type": "integer"},
        "location": {"type": "object"},
    },
    ["kind"],
)
PAGE_RECORD_SCHEMA = object_schema(
    {
        "page_id": PAGE_ID,
        "url": {"type": "string"},
        "title": {"type": "string"},
        "state": {"type": "string", "enum": ["open", "closed"]},
    },
    ["page_id", "url", "title", "state"],
)
SNAPSHOT_SCHEMA = object_schema(
    {
        "page_id": PAGE_ID,
        "snapshot_id": {"type": "integer"},
        "url": {"type": "string"},
        "title": {"type": "string"},
        "state": {"type": "string"},
        "kind": SNAPSHOT_KIND,
        "accessibility_tree": {
            "type": ["string", "object"],
            "description": "Unmodified Playwright ARIA snapshot in AI mode. Present for text and both. A model-context safety crop may return aria_fragments with exact source line ranges; an oversized single text region may use text_fragments with exact byte ranges.",
        },
        "screen_path": {
            "type": "string",
            "description": "Workspace-relative PNG path. Present for screen and both.",
        },
        "dismissed_native_dialogs": {
            "type": "array",
            "items": {"type": "object"},
        },
        "browser_events": {
            "type": "array",
            "items": BROWSER_EVENT_SCHEMA,
            "description": "Console warnings/errors, uncaught page errors, failed requests, and HTTP error responses observed since the previous successful snapshot.",
        },
        "dropped_browser_events": {"type": "integer", "minimum": 0},
    },
    [
        "page_id",
        "snapshot_id",
        "url",
        "title",
        "state",
        "kind",
        "browser_events",
        "dropped_browser_events",
    ],
)
PAGE_CHANGE_SCHEMA = object_schema(
    {
        "page_id": PAGE_ID,
        "change": {
            "type": "string",
            "enum": ["unchanged", "changed", "navigated", "closed"],
        },
        "page": PAGE_RECORD_SCHEMA,
    },
    ["page_id", "change", "page"],
)
OPENED_PAGE_SCHEMA = object_schema(
    {"page_id": PAGE_ID, "page": PAGE_RECORD_SCHEMA},
    ["page_id", "page"],
)

OUTPUT_SCHEMAS: dict[str, dict[str, Any]] = {
    "Create": object_schema({"page_id": PAGE_ID}, ["page_id"]),
    "Navigate": object_schema(
        {"page_id": PAGE_ID, "navigated": {"type": "boolean"}, "url": {"type": "string"}},
        ["page_id", "navigated", "url"],
    ),
    "Click": object_schema(
        {
            "page_id": PAGE_ID,
            "clicked": {"type": "boolean"},
            "opened_page_ids": {"type": "array", "items": PAGE_ID},
        },
        ["page_id", "clicked", "opened_page_ids"],
    ),
    "Type": object_schema(
        {"page_id": PAGE_ID, "typed": {"type": "boolean"}},
        ["page_id", "typed"],
    ),
    "Press": object_schema(
        {"page_id": PAGE_ID, "pressed": {"type": "boolean"}},
        ["page_id", "pressed"],
    ),
    "Scroll": object_schema(
        {"page_id": PAGE_ID, "scrolled": {"type": "boolean"}},
        ["page_id", "scrolled"],
    ),
    "RequireHumanAction": object_schema(
        {
            "state": {
                "type": "string",
                "enum": ["completed", "cancelled", "page_closed"],
            },
            "page_id": PAGE_ID,
            "message": {"type": "string"},
            "target_page": PAGE_CHANGE_SCHEMA,
            "changed_pages": {
                "type": "array",
                "items": PAGE_CHANGE_SCHEMA,
            },
            "opened_pages": {
                "type": "array",
                "items": OPENED_PAGE_SCHEMA,
            },
            "closed_page_ids": {"type": "array", "items": PAGE_ID},
            "active_page_id": {"type": ["string", "null"], "pattern": r"^p[0-9]{7}$"},
        },
        [
            "state",
            "page_id",
            "message",
            "target_page",
            "changed_pages",
            "opened_pages",
            "closed_page_ids",
            "active_page_id",
        ],
    ),
    "Snapshot": SNAPSHOT_SCHEMA,
    "Pages": object_schema(
        {
            "pages": {"type": "array", "items": PAGE_RECORD_SCHEMA},
            "active_page_id": {"type": ["string", "null"], "pattern": r"^p[0-9]{7}$"},
        },
        ["pages", "active_page_id"],
    ),
    "Back": object_schema(
        {"page_id": PAGE_ID, "navigated": {"type": "boolean"}, "url": {"type": "string"}},
        ["page_id", "navigated", "url"],
    ),
    "Close": object_schema(
        {"page_id": PAGE_ID, "closed": {"type": "boolean"}},
        ["page_id", "closed"],
    ),
}

ROUTES = {
    "Create": "Open a new blank page in this toolbox's existing browser context.",
    "Navigate": "Navigate an existing page to an HTTP, HTTPS, or about URL without reading the resulting page.",
    "Click": "Activate a known rendered element such as a link, button, checkbox, option, or control.",
    "Type": "Replace editable content, or insert text at its current caret or selection without clearing it.",
    "Press": "Send a keyboard key or shortcut when page interaction requires Enter, Escape, Tab, arrows, or another key.",
    "Scroll": "Move the real viewport to trigger lazy content, infinite lists, sticky interfaces, or canvas-driven pages; ordinary element clicks scroll automatically.",
    "RequireHumanAction": "Hand one open page to the user only when the rendered site requires direct human interaction that WebBrowser cannot perform, then wait for the user to complete or cancel the handoff.",
    "Snapshot": "After a fixed delay, capture page content once as the browser's raw ARIA snapshot, a reusable path to its rendered viewport screenshot, or both. This is the only tool that returns page content.",
    "Pages": "List all currently open browser pages and their identifiers without reading page content.",
    "Back": "Navigate a page through its real browser history to the previous entry.",
    "Close": "Close one known browser page when it is no longer needed.",
}

INSTRUCTIONS = {
    "Create": "Creates about:blank and returns only page_id. Pages in the same toolbox share cookies and storage; different Agent toolboxes do not.",
    "Navigate": "Performs one navigation and returns after the new document commits. It does not wait for stability and does not return page content. Discard element_id values from the previous document and call Snapshot when observation is needed.",
    "Click": "Use the value inside [ref=…] from the latest text snapshot as element_id, including its complete f… frame prefix when present. Activates that rendered DOM element once in its owning frame and returns without reading the page or waiting for navigation. This is not trusted human input; use RequireHumanAction when a site requires a real person. opened_page_ids contains pages registered immediately by that click; use Pages later if the site opens a page asynchronously. Native JavaScript dialogs are dismissed automatically and reported by the next Snapshot. Because a click may change page structure, take a new text Snapshot before selecting another element whenever the result can affect the DOM.",
    "Type": "Use the value inside [ref=…] from the latest text snapshot, including its complete f… frame prefix when present. mode=replace replaces the current editable content. mode=append preserves existing content and inserts at the current caret or selection; it does not force the caret to the end. Performs one input action and returns without a snapshot. Take a new text Snapshot before another element action if the input may have changed page structure.",
    "Press": "Playwright key names and combinations are accepted, for example Enter, Escape, Tab, ArrowDown, Shift+Tab, and ControlOrMeta+L. If element_id is supplied, that ARIA-referenced element is focused first. Performs one key action and returns without a snapshot. Enter and other keys may navigate or change page structure, so refresh the text Snapshot before reusing page elements when that can occur.",
    "Scroll": "If element_id is supplied, that ARIA-referenced element is scrolled into view once. Otherwise one real wheel action uses delta_x and delta_y. No snapshot is returned.",
    "RequireHumanAction": "Use only when direct human interaction is genuinely required. The browser is revealed until the user completes or cancels the handoff. The result reports page identifiers and change metadata only; call Snapshot explicitly to observe any page.",
    "Snapshot": "wait_ms is a fixed delay, not a stability heuristic, and must be 1000..60000 milliseconds. After that delay the page is sampled once. kind=text returns Playwright's ARIA snapshot verbatim in AI mode with [ref=…] element references, iframe content, states, and viewport-relative [box=x,y,width,height] data. Main-frame refs look like [ref=e6]; iframe refs may look like [ref=f1e2]. Copy only the complete value inside ref=, such as e6 or f1e2, as element_id for later actions. Treat refs as handles for that rendered document: after navigation or a structural page change, take a new text Snapshot and use its new refs. state is the sampled document.readyState, not a claim that all asynchronous work has finished. dismissed_native_dialogs and browser_events report activity accumulated since the previous successful Snapshot and are then drained. kind=screen saves the current rendered viewport as a PNG under the workspace .me directory and returns only screen_path; kind=both returns the ARIA snapshot and screen_path. Snapshot never places the screenshot into model image context. Call Image.View with screen_path only when visual inspection is needed. The path remains reusable until deleted. When the screenshot is no longer needed, call File.Stat to obtain its hash and then File.Delete to remove it. A model without image input may still create a screenshot and receive its path, but cannot inspect it with Image.View. Snapshot is the only WebBrowser tool that returns page content.",
    "Pages": "Returns only currently open page identifiers, URLs, titles, and open state without page content. Use Snapshot for page details.",
    "Back": "Performs one browser-history back action and returns without a snapshot. navigated=false means no previous entry was available.",
    "Close": "Closing a page invalidates all of its element IDs. Closing the last page keeps the BrowserContext alive so Create can open another page.",
}

EXAMPLES = {
    "Create": "{}",
    "Navigate": '{"page_id":"p0000001","url":"https://example.com"}',
    "Click": '{"page_id":"p0000001","element_id":"e4"}\n{"page_id":"p0000001","element_id":"f1e2"}',
    "Type": '{"page_id":"p0000001","element_id":"e7","content":"example text","mode":"replace"}',
    "Press": '{"page_id":"p0000001","element_id":"e7","key":"Enter"}',
    "Scroll": '{"page_id":"p0000001","delta_y":720}',
    "RequireHumanAction": '{"page_id":"p0000001","instruction":"Please complete the verification shown in the browser, then click 完成 in the ME window."}',
    "Snapshot": '{"page_id":"p0000001","wait_ms":1000,"kind":"text"}\n{"page_id":"p0000001","wait_ms":3000,"kind":"both"}',
    "Pages": "{}",
    "Back": '{"page_id":"p0000001"}',
    "Close": '{"page_id":"p0000001"}',
}


def send(frame: dict[str, Any]) -> None:
    sys.stdout.write(json.dumps(frame, ensure_ascii=False, separators=(",", ":")) + "\n")
    sys.stdout.flush()


def result(request_id: int, output: Any) -> None:
    send({"id": request_id, "type": "result", "output": output})


def error(request_id: int, exc: ToolError) -> None:
    send(
        {
            "id": request_id,
            "type": "error",
            "error": {
                "code": exc.code,
                "message": exc.message,
                "retryable": exc.retryable,
            },
        }
    )


def update(request_id: int, content: str) -> None:
    send(
        {
            "id": request_id,
            "type": "update",
            "output": {"stream": "stdout", "content": content},
        }
    )


def config_home() -> Path:
    configured = os.environ.get("ME_CONFIG_HOME")
    if configured:
        return Path(configured).expanduser().resolve()
    if os.name == "nt":
        base = os.environ.get("APPDATA")
        if base:
            return (Path(base) / "me").resolve()
        return (Path.home() / "AppData" / "Roaming" / "me").resolve()
    xdg = os.environ.get("XDG_CONFIG_HOME")
    if xdg:
        return (Path(xdg) / "me").resolve()
    return (Path.home() / ".config" / "me").resolve()


def private_directory(path: Path) -> None:
    path.mkdir(parents=True, exist_ok=True)
    if os.name != "nt":
        path.chmod(0o700)


def process_is_alive(pid: int) -> bool:
    if pid <= 0:
        return False
    if pid == os.getpid():
        return True
    if os.name == "nt":
        process_query_limited_information = 0x1000
        kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
        kernel32.OpenProcess.restype = ctypes.c_void_p
        kernel32.OpenProcess.argtypes = [ctypes.c_ulong, ctypes.c_int, ctypes.c_ulong]
        kernel32.CloseHandle.argtypes = [ctypes.c_void_p]
        handle = kernel32.OpenProcess(
            process_query_limited_information, 0, ctypes.c_ulong(pid)
        )
        if not handle:
            return False
        kernel32.CloseHandle(handle)
        return True
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    except PermissionError:
        return True
    return True


def install_lock_owner(lock: Path) -> int | None:
    try:
        value = json.loads((lock / "owner.json").read_text(encoding="utf-8"))
        pid = value.get("pid")
        return pid if isinstance(pid, int) and not isinstance(pid, bool) else None
    except (FileNotFoundError, OSError, ValueError, TypeError):
        return None


@contextlib.contextmanager
def install_lock(root: Path, progress: Callable[[str], None]) -> Iterator[None]:
    lock = root / "install.lock"
    deadline = time.monotonic() + INSTALL_LOCK_WAIT_SECONDS
    waiting_reported = False
    while True:
        try:
            lock.mkdir()
            (lock / "owner.json").write_text(
                json.dumps({"pid": os.getpid(), "time": time.time()}), encoding="utf-8"
            )
            break
        except FileExistsError:
            try:
                age = time.time() - lock.stat().st_mtime
            except FileNotFoundError:
                continue
            owner = install_lock_owner(lock)
            if owner is not None and not process_is_alive(owner):
                shutil.rmtree(lock, ignore_errors=True)
                continue
            if owner is None and age > 10:
                shutil.rmtree(lock, ignore_errors=True)
                continue
            if not waiting_reported:
                progress(
                    "Waiting for another WebBrowser runtime installation to finish"
                )
                waiting_reported = True
            if time.monotonic() >= deadline:
                raise ToolError(
                    "dependency_install_busy",
                    "another WebBrowser runtime installation is still active after two minutes; "
                    "close the other ME process or retry after its installation finishes",
                    True,
                )
            time.sleep(0.25)
    try:
        yield
    finally:
        shutil.rmtree(lock, ignore_errors=True)


def command_error(command: list[str], completed: subprocess.CompletedProcess[str]) -> str:
    combined = "\n".join(part for part in [completed.stdout, completed.stderr] if part)
    tail = "\n".join(combined.splitlines()[-30:])
    return f"command failed ({completed.returncode}): {' '.join(command)}\n{tail}".strip()


class DependencyRuntime:
    def __init__(self) -> None:
        self.root = config_home()
        root = self.root
        self.runtime_root = root / "runtimes" / "web-browser"
        self.site_packages = (
            self.runtime_root
            / f"python-{sys.version_info.major}.{sys.version_info.minor}"
            / (
                f"camoufox-{CAMOUFOX_VERSION}-playwright-{PLAYWRIGHT_VERSION}"
                f"-pyside6-{PYSIDE6_VERSION}"
            )
        )
        self.package_marker = self.site_packages / ".complete"
        self.browser_cache_base = root / "browsers"
        self.browser_root = self.browser_cache_base / "camoufox"
        self.browser_marker = (
            self.browser_cache_base
            / f"camoufox-{CAMOUFOX_BROWSER_VERSION}.complete"
        )
        private_directory(self.runtime_root)
        private_directory(self.browser_cache_base)

    def _environment(self) -> dict[str, str]:
        environment = os.environ.copy()
        environment["XDG_CACHE_HOME"] = str(self.browser_cache_base)
        if os.name == "nt":
            environment["LOCALAPPDATA"] = str(self.browser_cache_base)
        environment["PYTHONPATH"] = str(self.site_packages) + (
            os.pathsep + environment["PYTHONPATH"]
            if environment.get("PYTHONPATH")
            else ""
        )
        return environment

    def activate_environment(self) -> None:
        os.environ["XDG_CACHE_HOME"] = str(self.browser_cache_base)
        if os.name == "nt":
            os.environ["LOCALAPPDATA"] = str(self.browser_cache_base)

    def ensure(self, progress: Callable[[str], None]) -> None:
        if not self.package_marker.is_file():
            with install_lock(self.runtime_root, progress):
                if not self.package_marker.is_file():
                    progress(
                        "Installing WebBrowser runtime "
                        f"Camoufox {CAMOUFOX_VERSION} + PySide6 {PYSIDE6_VERSION}"
                    )
                    self._install_package()
        if str(self.site_packages) not in sys.path:
            sys.path.insert(0, str(self.site_packages))
        self.activate_environment()
        importlib.invalidate_caches()
        try:
            imported = importlib.import_module("camoufox")
            playwright = importlib.import_module("playwright")
            pyside = importlib.import_module("PySide6")
            importlib.import_module("PySide6.QtWidgets")
        except Exception as exc:
            raise ToolError(
                "dependency_unavailable",
                f"installed WebBrowser dependencies cannot be imported: {exc}",
                True,
            ) from exc
        if getattr(imported, "__path__", None) is None:
            raise ToolError("dependency_unavailable", "invalid Camoufox package", True)
        if getattr(playwright, "__path__", None) is None:
            raise ToolError("dependency_unavailable", "invalid Playwright dependency", True)
        if getattr(pyside, "__version__", None) != PYSIDE6_VERSION:
            raise ToolError(
                "dependency_unavailable",
                f"invalid PySide6 dependency version: {getattr(pyside, '__version__', None)!r}",
                True,
            )
        if not self.browser_is_valid():
            self.browser_marker.unlink(missing_ok=True)
            with install_lock(self.runtime_root, progress):
                if not self.browser_is_valid():
                    progress(
                        "Installing Camoufox browser "
                        f"{CAMOUFOX_BROWSER_VERSION} in ME-S global storage"
                    )
                    self._install_browser()
        self._cleanup_legacy_runtime()

    def browser_executable(self) -> Path | None:
        try:
            from camoufox.multiversion import get_active_path
            from camoufox.pkgman import launch_path

            active = get_active_path()
            if active is None:
                return None
            metadata = json.loads((active / "version.json").read_text(encoding="utf-8"))
            if (
                metadata.get("version") != CAMOUFOX_BROWSER_VERSION.rsplit("-", 1)[0]
                or metadata.get("build") != CAMOUFOX_BROWSER_VERSION.rsplit("-", 1)[1]
            ):
                return None
            executable = Path(launch_path(active))
            properties = (
                active / "Camoufox.app" / "Contents" / "Resources" / "properties.json"
                if platform.system() == "Darwin"
                else active / "properties.json"
            )
            return executable if executable.is_file() and properties.is_file() else None
        except (ImportError, OSError, ValueError, TypeError, KeyError):
            return None

    def browser_is_valid(self) -> bool:
        return self.browser_marker.is_file() and self.browser_executable() is not None

    def _install_package(self) -> None:
        parent = self.site_packages.parent
        private_directory(parent)
        for child in parent.iterdir():
            if child.is_dir() and child.name.startswith("web-browser-install-"):
                shutil.rmtree(child, ignore_errors=True)
        staging = Path(tempfile.mkdtemp(prefix="web-browser-install-", dir=parent))
        command = [
            sys.executable,
            "-m",
            "pip",
            "install",
            "--disable-pip-version-check",
            "--no-input",
            "--only-binary=:all:",
            "--target",
            str(staging),
            f"camoufox=={CAMOUFOX_VERSION}",
            f"playwright=={PLAYWRIGHT_VERSION}",
            f"PySide6-Essentials=={PYSIDE6_VERSION}",
        ]
        try:
            try:
                completed = subprocess.run(
                    command,
                    text=True,
                    capture_output=True,
                    timeout=INSTALL_COMMAND_TIMEOUT_SECONDS,
                    check=False,
                )
            except subprocess.TimeoutExpired as exc:
                raise ToolError(
                    "dependency_install_timeout",
                    "WebBrowser Python dependencies did not finish installing within ten minutes",
                    True,
                ) from exc
            if completed.returncode != 0:
                raise ToolError(
                    "dependency_install_failed", command_error(command, completed), True
                )
            (staging / ".complete").write_text(
                json.dumps(
                    {
                        "camoufox": CAMOUFOX_VERSION,
                        "playwright": PLAYWRIGHT_VERSION,
                        "pyside6": PYSIDE6_VERSION,
                        "python": platform.python_version(),
                        "platform": platform.platform(),
                    }
                ),
                encoding="utf-8",
            )
            if self.site_packages.exists():
                shutil.rmtree(self.site_packages)
            os.replace(staging, self.site_packages)
        finally:
            if staging.exists():
                shutil.rmtree(staging, ignore_errors=True)

    def _install_browser(self) -> None:
        environment = self._environment()
        command = [
            sys.executable,
            "-m",
            "camoufox",
            "fetch",
            CAMOUFOX_BROWSER_RELEASE,
        ]
        try:
            completed = subprocess.run(
                command,
                env=environment,
                text=True,
                capture_output=True,
                timeout=INSTALL_COMMAND_TIMEOUT_SECONDS,
                check=False,
            )
        except subprocess.TimeoutExpired as exc:
            raise ToolError(
                "browser_install_timeout",
                "Camoufox browser did not finish installing within ten minutes",
                True,
            ) from exc
        if completed.returncode != 0:
            raise ToolError("browser_install_failed", command_error(command, completed), True)
        executable = self.browser_executable()
        if executable is None:
            details = command_error(command, completed)
            raise ToolError(
                "browser_install_failed",
                "Camoufox reported success without installing the requested browser. "
                "This usually means its repository request failed.\n"
                + details,
                True,
            )
        self.browser_marker.write_text(
            json.dumps(
                {
                    "camoufox": CAMOUFOX_VERSION,
                    "browser": CAMOUFOX_BROWSER_VERSION,
                    "platform": platform.platform(),
                    "installed_at": time.time(),
                }
            ),
            encoding="utf-8",
        )

    def reinstall_browser(self, progress: Callable[[str], None]) -> None:
        with install_lock(self.runtime_root, progress):
            self.browser_marker.unlink(missing_ok=True)
            progress("Repairing Camoufox browser in ME-S global storage")
            subprocess.run(
                [
                    sys.executable,
                    "-m",
                    "camoufox",
                    "remove",
                    CAMOUFOX_BROWSER_RELEASE,
                    "--yes",
                ],
                env=self._environment(),
                text=True,
                capture_output=True,
                timeout=300,
                check=False,
            )
            self._install_browser()

    def _cleanup_legacy_runtime(self) -> None:
        package_parent = self.site_packages.parent
        if package_parent.is_dir():
            for child in package_parent.iterdir():
                if child != self.site_packages and child.name.startswith(
                    ("camoufox-", "playwright-")
                ):
                    shutil.rmtree(child, ignore_errors=True)
        shutil.rmtree(self.root / "browsers" / "playwright", ignore_errors=True)


HANDOFF_OBSERVATION_JS = """() => {
    const root = document.documentElement;
    const body = document.body;
    const markup = root ? root.outerHTML : "";
    const hashText = value => {
        let hash = 2166136261;
        for (let index = 0; index < value.length; index += 1) {
            hash ^= value.charCodeAt(index);
            hash = Math.imul(hash, 16777619);
        }
        return (hash >>> 0).toString(16).padStart(8, "0");
    };
    const controlState = Array.from(document.querySelectorAll(
        "input,textarea,select,[contenteditable]"
    )).map(element => [
        element.tagName,
        element.type || "",
        element.value || "",
        Boolean(element.checked),
        Number.isInteger(element.selectedIndex) ? element.selectedIndex : -1,
        element.isContentEditable ? element.textContent || "" : ""
    ]);
    return {
        url: location.href,
        time_origin: performance.timeOrigin,
        ready_state: document.readyState,
        markup_hash: hashText(markup),
        markup_length: markup.length,
        control_hash: hashText(JSON.stringify(controlState)),
        scroll_width: root ? root.scrollWidth : 0,
        scroll_height: root ? root.scrollHeight : 0,
        scroll_x: globalThis.scrollX,
        scroll_y: globalThis.scrollY,
        visible_text_length: body && body.innerText ? body.innerText.length : 0
    };
}"""

def object_guid(value: Any) -> str:
    implementation = getattr(value, "_impl_obj", None)
    return str(getattr(implementation, "_guid", id(value)))


def host_fingerprint_os() -> str:
    test_override = os.environ.get("ME_WEB_BROWSER_TEST_FINGERPRINT_OS")
    if os.environ.get("ME_WEB_BROWSER_TEST_HEADLESS") == "1" and test_override:
        if test_override not in {"macos", "windows", "linux"}:
            raise ToolError(
                "unsupported_platform",
                f"invalid test fingerprint OS: {test_override}",
            )
        return test_override
    system = platform.system()
    if system == "Darwin":
        return "macos"
    if system == "Windows":
        return "windows"
    if system == "Linux":
        return "linux"
    raise ToolError("unsupported_platform", f"Camoufox does not support {system}")


def compatible_fingerprint_preset(target_os: str) -> tuple[dict[str, Any], tuple[str, str]]:
    from camoufox.fingerprints import get_random_preset
    from camoufox.webgl.sample import sample_webgl

    database_os = {"windows": "win", "macos": "mac", "linux": "lin"}[target_os]
    for _ in range(256):
        preset = get_random_preset(
            os=target_os,
            ff_version=CAMOUFOX_BROWSER_VERSION,
        )
        if not isinstance(preset, dict):
            continue
        webgl = preset.get("webgl")
        if not isinstance(webgl, dict):
            continue
        vendor = webgl.get("unmaskedVendor")
        renderer = webgl.get("unmaskedRenderer")
        if not isinstance(vendor, str) or not isinstance(renderer, str):
            continue
        try:
            sample_webgl(database_os, vendor, renderer)
        except ValueError:
            continue
        return preset, (vendor, renderer)
    raise ToolError(
        "fingerprint_unavailable",
        f"Camoufox has no internally consistent fingerprint preset for {target_os}",
        True,
    )


def browser_proxy_from_environment() -> dict[str, str] | None:
    value = next(
        (
            os.environ[name].strip()
            for name in (
                "HTTPS_PROXY",
                "https_proxy",
                "HTTP_PROXY",
                "http_proxy",
                "ALL_PROXY",
                "all_proxy",
            )
            if os.environ.get(name, "").strip()
        ),
        "",
    )
    if not value:
        return None
    parsed = urlparse(value if "://" in value else f"http://{value}")
    if not parsed.scheme or not parsed.hostname:
        raise ToolError("invalid_proxy", "WebBrowser proxy environment is invalid")
    hostname = (
        f"[{parsed.hostname}]" if ":" in parsed.hostname else parsed.hostname
    )
    server = f"{parsed.scheme}://{hostname}"
    if parsed.port is not None:
        server += f":{parsed.port}"
    proxy = {"server": server}
    if parsed.username is not None:
        proxy["username"] = unquote(parsed.username)
    if parsed.password is not None:
        proxy["password"] = unquote(parsed.password)
    bypass = os.environ.get("NO_PROXY") or os.environ.get("no_proxy")
    if bypass and bypass.strip():
        proxy["bypass"] = bypass.strip()
    return proxy


class BrowserPresentation:
    """Keeps a headed browser non-visible until an explicit human handoff."""

    def launch_environment(self) -> dict[str, str]:
        return dict(os.environ)

    def start(self) -> None:
        pass

    def conceal(self, require_window: bool = False) -> None:
        del require_window

    def reveal(self) -> None:
        pass

    def shutdown(self) -> None:
        pass


class MacOSBrowserPresentation(BrowserPresentation):
    ASSET_NAME = ".WebBrowser-window-control-macos.dylib"

    def __init__(self) -> None:
        self.library = Path(__file__).resolve().with_name(self.ASSET_NAME)
        if not self.library.is_file():
            raise ToolError(
                "window_control_unavailable",
                f"WebBrowser macOS window-control asset is missing: {self.library}",
            )
        descriptor, state_path = tempfile.mkstemp(prefix="me-camoufox-window-", suffix=".state")
        os.close(descriptor)
        self.state_path = Path(state_path)
        self.acknowledgement_path = self.state_path.with_suffix(".ack")
        with contextlib.suppress(OSError):
            self.acknowledgement_path.unlink()
        self.closed = False
        self._write_state("concealed")

    def _write_state(self, mode: str) -> None:
        temporary = self.state_path.with_name(
            f".{self.state_path.name}.{os.getpid()}.tmp"
        )
        temporary.write_text(f"{mode} {os.getpid()}\n", encoding="utf-8")
        os.replace(temporary, self.state_path)

    def launch_environment(self) -> dict[str, str]:
        environment = super().launch_environment()
        existing = environment.get("DYLD_INSERT_LIBRARIES", "").strip()
        libraries = [str(self.library)]
        if existing:
            libraries.append(existing)
        environment["DYLD_INSERT_LIBRARIES"] = ":".join(libraries)
        environment["ME_CAMOUFOX_PRESENTATION_FILE"] = str(self.state_path)
        environment["ME_CAMOUFOX_PRESENTATION_ACK"] = str(self.acknowledgement_path)
        return environment

    def _wait_for_acknowledgement(self, mode: str, required: bool) -> None:
        deadline = time.monotonic() + (5 if required else 0.25)
        while time.monotonic() < deadline:
            with contextlib.suppress(OSError, UnicodeError):
                if self.acknowledgement_path.read_text(encoding="utf-8").startswith(
                    f"{mode} "
                ):
                    return
            time.sleep(0.025)
        if required:
            raise ToolError(
                "window_control_failed",
                f"Camoufox did not acknowledge the {mode} window state",
                True,
            )

    def conceal(self, require_window: bool = False) -> None:
        if not self.closed:
            self._write_state("concealed")
            self._wait_for_acknowledgement("concealed", require_window)

    def reveal(self) -> None:
        if self.closed:
            raise ToolError("window_control_failed", "browser presentation is closed")
        self._write_state("interactive")
        self._wait_for_acknowledgement("interactive", True)

    def shutdown(self) -> None:
        if self.closed:
            return
        self.closed = True
        with contextlib.suppress(Exception):
            self._write_state("interactive")
            time.sleep(0.05)
        with contextlib.suppress(OSError):
            self.state_path.unlink()
        with contextlib.suppress(OSError):
            self.acknowledgement_path.unlink()


class PollingBrowserPresentation(BrowserPresentation):
    POLL_SECONDS = 0.025

    def __init__(self) -> None:
        self.condition = threading.Condition()
        self.wakeup = threading.Event()
        self.stop_event = threading.Event()
        self.ready = threading.Event()
        self.interactive = False
        self.request_generation = 0
        self.applied_generation = 0
        self.window_count = 0
        self.failure: Exception | None = None
        self.thread: threading.Thread | None = None

    def start(self) -> None:
        self.thread = threading.Thread(
            target=self._run,
            name="me-web-browser-window-control",
            daemon=True,
        )
        self.thread.start()
        if not self.ready.wait(5):
            raise ToolError("window_control_failed", "window control did not start")
        self._check_failure()

    def _run(self) -> None:
        try:
            self._setup()
            self.ready.set()
            while not self.stop_event.is_set():
                with self.condition:
                    interactive = self.interactive
                    generation = self.request_generation
                count = self._apply(interactive)
                with self.condition:
                    self.window_count = count
                    self.applied_generation = max(self.applied_generation, generation)
                    self.condition.notify_all()
                self.wakeup.wait(self.POLL_SECONDS)
                self.wakeup.clear()
        except Exception as exc:
            with self.condition:
                self.failure = exc
                self.condition.notify_all()
            self.ready.set()
        finally:
            with contextlib.suppress(Exception):
                self._teardown()

    def _setup(self) -> None:
        pass

    def _apply(self, interactive: bool) -> int:
        raise NotImplementedError

    def _teardown(self) -> None:
        pass

    def _check_failure(self) -> None:
        if self.failure is not None:
            raise ToolError(
                "window_control_failed",
                f"WebBrowser native window control failed: {self.failure}",
                True,
            ) from self.failure

    def _set_interactive(self, interactive: bool, require_window: bool) -> None:
        deadline = time.monotonic() + (5 if require_window else 1)
        with self.condition:
            self.interactive = interactive
            self.request_generation += 1
            generation = self.request_generation
            self.wakeup.set()
            while True:
                self._check_failure()
                applied = self.applied_generation >= generation
                found = self.window_count > 0 or not require_window
                if applied and found:
                    return
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    if require_window:
                        raise ToolError(
                            "window_control_failed",
                            "WebBrowser could not find its headed browser window",
                            True,
                        )
                    return
                self.condition.wait(min(remaining, 0.1))
                self.wakeup.set()

    def conceal(self, require_window: bool = False) -> None:
        self._set_interactive(False, require_window)

    def reveal(self) -> None:
        self._set_interactive(True, True)

    def shutdown(self) -> None:
        if self.thread is None:
            return
        with contextlib.suppress(Exception):
            self._set_interactive(True, False)
        self.stop_event.set()
        self.wakeup.set()
        self.thread.join(timeout=2)
        self.thread = None


class WindowsBrowserPresentation(PollingBrowserPresentation):
    TH32CS_SNAPPROCESS = 0x00000002
    GWL_EXSTYLE = -20
    WS_EX_TRANSPARENT = 0x00000020
    WS_EX_TOOLWINDOW = 0x00000080
    WS_EX_APPWINDOW = 0x00040000
    WS_EX_LAYERED = 0x00080000
    WS_EX_NOACTIVATE = 0x08000000
    LWA_ALPHA = 0x00000002
    SWP_NOSIZE = 0x0001
    SWP_NOMOVE = 0x0002
    SWP_NOACTIVATE = 0x0010
    SWP_FRAMECHANGED = 0x0020
    HWND_BOTTOM = 1

    class PROCESSENTRY32W(ctypes.Structure):
        _fields_ = [
            ("dwSize", ctypes.c_ulong),
            ("cntUsage", ctypes.c_ulong),
            ("th32ProcessID", ctypes.c_ulong),
            ("th32DefaultHeapID", ctypes.c_size_t),
            ("th32ModuleID", ctypes.c_ulong),
            ("cntThreads", ctypes.c_ulong),
            ("th32ParentProcessID", ctypes.c_ulong),
            ("pcPriClassBase", ctypes.c_long),
            ("dwFlags", ctypes.c_ulong),
            ("szExeFile", ctypes.c_wchar * 260),
        ]

    def _setup(self) -> None:
        self.kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
        self.user32 = ctypes.WinDLL("user32", use_last_error=True)
        self.records: dict[int, int] = {}
        self.cached_descendants: set[int] = set()
        self.descendant_refresh_at = 0.0
        self.kernel32.CreateToolhelp32Snapshot.restype = ctypes.c_void_p
        self.kernel32.CreateToolhelp32Snapshot.argtypes = [ctypes.c_ulong, ctypes.c_ulong]
        self.kernel32.Process32FirstW.argtypes = [
            ctypes.c_void_p,
            ctypes.POINTER(self.PROCESSENTRY32W),
        ]
        self.kernel32.Process32NextW.argtypes = [
            ctypes.c_void_p,
            ctypes.POINTER(self.PROCESSENTRY32W),
        ]
        self.kernel32.CloseHandle.argtypes = [ctypes.c_void_p]
        self.user32.EnumWindows.restype = ctypes.c_int
        self.user32.IsWindowVisible.restype = ctypes.c_int
        self.user32.IsWindowVisible.argtypes = [ctypes.c_void_p]
        self.user32.IsWindow.restype = ctypes.c_int
        self.user32.IsWindow.argtypes = [ctypes.c_void_p]
        self.user32.GetWindowThreadProcessId.argtypes = [
            ctypes.c_void_p,
            ctypes.POINTER(ctypes.c_ulong),
        ]
        self.user32.GetWindowLongPtrW.restype = ctypes.c_ssize_t
        self.user32.GetWindowLongPtrW.argtypes = [ctypes.c_void_p, ctypes.c_int]
        self.user32.SetWindowLongPtrW.restype = ctypes.c_ssize_t
        self.user32.SetWindowLongPtrW.argtypes = [
            ctypes.c_void_p,
            ctypes.c_int,
            ctypes.c_ssize_t,
        ]
        self.user32.SetLayeredWindowAttributes.argtypes = [
            ctypes.c_void_p,
            ctypes.c_ulong,
            ctypes.c_ubyte,
            ctypes.c_ulong,
        ]
        self.user32.SetWindowPos.argtypes = [
            ctypes.c_void_p,
            ctypes.c_void_p,
            ctypes.c_int,
            ctypes.c_int,
            ctypes.c_int,
            ctypes.c_int,
            ctypes.c_uint,
        ]

    def _descendant_processes(self) -> set[int]:
        now = time.monotonic()
        if self.window_count > 0 and now < self.descendant_refresh_at:
            return self.cached_descendants
        snapshot = self.kernel32.CreateToolhelp32Snapshot(self.TH32CS_SNAPPROCESS, 0)
        invalid = ctypes.c_void_p(-1).value
        if snapshot == invalid:
            raise OSError(ctypes.get_last_error(), "CreateToolhelp32Snapshot failed")
        parents: dict[int, int] = {}
        try:
            entry = self.PROCESSENTRY32W()
            entry.dwSize = ctypes.sizeof(entry)
            present = bool(self.kernel32.Process32FirstW(snapshot, ctypes.byref(entry)))
            while present:
                parents[int(entry.th32ProcessID)] = int(entry.th32ParentProcessID)
                present = bool(self.kernel32.Process32NextW(snapshot, ctypes.byref(entry)))
        finally:
            self.kernel32.CloseHandle(snapshot)
        descendants = {os.getpid()}
        changed = True
        while changed:
            changed = False
            for process_id, parent_id in parents.items():
                if parent_id in descendants and process_id not in descendants:
                    descendants.add(process_id)
                    changed = True
        descendants.discard(os.getpid())
        self.cached_descendants = descendants
        self.descendant_refresh_at = now + 1
        return descendants

    def _windows(self) -> list[int]:
        descendants = self._descendant_processes()
        windows: list[int] = []
        callback_type = ctypes.WINFUNCTYPE(ctypes.c_int, ctypes.c_void_p, ctypes.c_void_p)

        @callback_type
        def collect(window: int, parameter: int) -> int:
            del parameter
            process_id = ctypes.c_ulong()
            self.user32.GetWindowThreadProcessId(window, ctypes.byref(process_id))
            if process_id.value in descendants and self.user32.IsWindowVisible(window):
                windows.append(int(window))
            return 1

        if not self.user32.EnumWindows(collect, 0):
            raise OSError(ctypes.get_last_error(), "EnumWindows failed")
        return windows

    def _hide_window(self, window: int) -> None:
        handle = ctypes.c_void_p(window)
        if not self.user32.IsWindow(handle):
            return
        original = int(self.user32.GetWindowLongPtrW(handle, self.GWL_EXSTYLE))
        hidden = (
            (original | self.WS_EX_LAYERED | self.WS_EX_TRANSPARENT | self.WS_EX_NOACTIVATE | self.WS_EX_TOOLWINDOW)
            & ~self.WS_EX_APPWINDOW
        )
        self.user32.SetWindowLongPtrW(handle, self.GWL_EXSTYLE, hidden)
        if not self.user32.SetLayeredWindowAttributes(handle, 0, 0, self.LWA_ALPHA):
            if not self.user32.IsWindow(handle):
                return
            raise OSError(ctypes.get_last_error(), "SetLayeredWindowAttributes failed")
        self.records[window] = original
        self.user32.SetWindowPos(
            handle,
            ctypes.c_void_p(self.HWND_BOTTOM),
            0,
            0,
            0,
            0,
            self.SWP_NOMOVE | self.SWP_NOSIZE | self.SWP_NOACTIVATE | self.SWP_FRAMECHANGED,
        )

    def _restore_window(self, window: int, original: int) -> None:
        handle = ctypes.c_void_p(window)
        self.user32.SetLayeredWindowAttributes(handle, 0, 255, self.LWA_ALPHA)
        self.user32.SetWindowLongPtrW(handle, self.GWL_EXSTYLE, original)
        self.user32.SetWindowPos(
            handle,
            None,
            0,
            0,
            0,
            0,
            self.SWP_NOMOVE | self.SWP_NOSIZE | self.SWP_NOACTIVATE | self.SWP_FRAMECHANGED,
        )

    def _apply(self, interactive: bool) -> int:
        windows = self._windows()
        live = set(windows)
        if interactive:
            for window, original in list(self.records.items()):
                if self.user32.IsWindow(window):
                    self._restore_window(window, original)
            self.records.clear()
        else:
            for window in windows:
                if window not in self.records:
                    self._hide_window(window)
        for window in set(self.records) - live:
            self.records.pop(window, None)
        return len(windows)

    def _teardown(self) -> None:
        for window, original in list(getattr(self, "records", {}).items()):
            if self.user32.IsWindow(window):
                self._restore_window(window, original)
        if hasattr(self, "records"):
            self.records.clear()


class LinuxBrowserPresentation(PollingBrowserPresentation):
    SHAPE_INPUT = 2
    SHAPE_SET = 0
    UNSORTED = 0
    PROP_MODE_REPLACE = 0
    XA_CARDINAL = 6
    XA_WINDOW = 33

    class XRectangle(ctypes.Structure):
        _fields_ = [
            ("x", ctypes.c_short),
            ("y", ctypes.c_short),
            ("width", ctypes.c_ushort),
            ("height", ctypes.c_ushort),
        ]

    def launch_environment(self) -> dict[str, str]:
        environment = super().launch_environment()
        environment["GDK_BACKEND"] = "x11"
        environment["MOZ_ENABLE_WAYLAND"] = "0"
        environment.pop("WAYLAND_DISPLAY", None)
        return environment

    def _setup(self) -> None:
        if not os.environ.get("DISPLAY"):
            raise RuntimeError("desktop Linux window control requires X11 or XWayland DISPLAY")
        x11_name = ctypes.util.find_library("X11") or "libX11.so.6"
        xext_name = ctypes.util.find_library("Xext") or "libXext.so.6"
        self.x11 = ctypes.CDLL(x11_name)
        self.xext = ctypes.CDLL(xext_name)
        self.x11.XOpenDisplay.restype = ctypes.c_void_p
        self.x11.XOpenDisplay.argtypes = [ctypes.c_char_p]
        self.display = self.x11.XOpenDisplay(os.environ["DISPLAY"].encode())
        if not self.display:
            raise RuntimeError(f"could not open X11 display {os.environ['DISPLAY']}")
        self.x11.XDefaultRootWindow.restype = ctypes.c_ulong
        self.x11.XDefaultRootWindow.argtypes = [ctypes.c_void_p]
        self.root = int(self.x11.XDefaultRootWindow(self.display))
        self.x11.XInternAtom.restype = ctypes.c_ulong
        self.x11.XInternAtom.argtypes = [ctypes.c_void_p, ctypes.c_char_p, ctypes.c_int]
        self.x11.XGetWindowProperty.argtypes = [
            ctypes.c_void_p,
            ctypes.c_ulong,
            ctypes.c_ulong,
            ctypes.c_long,
            ctypes.c_long,
            ctypes.c_int,
            ctypes.c_ulong,
            ctypes.POINTER(ctypes.c_ulong),
            ctypes.POINTER(ctypes.c_int),
            ctypes.POINTER(ctypes.c_ulong),
            ctypes.POINTER(ctypes.c_ulong),
            ctypes.POINTER(ctypes.POINTER(ctypes.c_ubyte)),
        ]
        self.x11.XFree.argtypes = [ctypes.c_void_p]
        self.x11.XQueryTree.argtypes = [
            ctypes.c_void_p,
            ctypes.c_ulong,
            ctypes.POINTER(ctypes.c_ulong),
            ctypes.POINTER(ctypes.c_ulong),
            ctypes.POINTER(ctypes.POINTER(ctypes.c_ulong)),
            ctypes.POINTER(ctypes.c_uint),
        ]
        self.x11.XChangeProperty.argtypes = [
            ctypes.c_void_p,
            ctypes.c_ulong,
            ctypes.c_ulong,
            ctypes.c_ulong,
            ctypes.c_int,
            ctypes.c_int,
            ctypes.POINTER(ctypes.c_ubyte),
            ctypes.c_int,
        ]
        self.x11.XDeleteProperty.argtypes = [ctypes.c_void_p, ctypes.c_ulong, ctypes.c_ulong]
        self.x11.XLowerWindow.argtypes = [ctypes.c_void_p, ctypes.c_ulong]
        self.x11.XMapRaised.argtypes = [ctypes.c_void_p, ctypes.c_ulong]
        self.x11.XSync.argtypes = [ctypes.c_void_p, ctypes.c_int]
        self.x11.XCloseDisplay.argtypes = [ctypes.c_void_p]
        self.x11.XGetGeometry.argtypes = [
            ctypes.c_void_p,
            ctypes.c_ulong,
            ctypes.POINTER(ctypes.c_ulong),
            ctypes.POINTER(ctypes.c_int),
            ctypes.POINTER(ctypes.c_int),
            ctypes.POINTER(ctypes.c_uint),
            ctypes.POINTER(ctypes.c_uint),
            ctypes.POINTER(ctypes.c_uint),
            ctypes.POINTER(ctypes.c_uint),
        ]
        self.x_error_handler_type = ctypes.CFUNCTYPE(
            ctypes.c_int, ctypes.c_void_p, ctypes.c_void_p
        )

        @self.x_error_handler_type
        def ignore_stale_window_error(display: int, event: int) -> int:
            del display, event
            return 0

        self.x_error_handler = ignore_stale_window_error
        self.x11.XSetErrorHandler.restype = ctypes.c_void_p
        self.x11.XSetErrorHandler.argtypes = [self.x_error_handler_type]
        self.previous_x_error_handler = self.x11.XSetErrorHandler(
            self.x_error_handler
        )
        event_base = ctypes.c_int()
        error_base = ctypes.c_int()
        self.xext.XShapeQueryExtension.argtypes = [
            ctypes.c_void_p,
            ctypes.POINTER(ctypes.c_int),
            ctypes.POINTER(ctypes.c_int),
        ]
        self.xext.XShapeCombineRectangles.argtypes = [
            ctypes.c_void_p,
            ctypes.c_ulong,
            ctypes.c_int,
            ctypes.c_int,
            ctypes.c_int,
            ctypes.POINTER(self.XRectangle),
            ctypes.c_int,
            ctypes.c_int,
            ctypes.c_int,
        ]
        if not self.xext.XShapeQueryExtension(
            self.display, ctypes.byref(event_base), ctypes.byref(error_base)
        ):
            raise RuntimeError("X11 Shape extension is unavailable")
        self.client_list_atom = self._atom("_NET_CLIENT_LIST_STACKING")
        self.fallback_client_list_atom = self._atom("_NET_CLIENT_LIST")
        self.pid_atom = self._atom("_NET_WM_PID")
        self.opacity_atom = self._atom("_NET_WM_WINDOW_OPACITY")
        self.records: dict[int, dict[str, Any]] = {}
        self.cached_descendants: set[int] = set()
        self.descendant_refresh_at = 0.0

    def _atom(self, name: str) -> int:
        return int(self.x11.XInternAtom(self.display, name.encode(), 0))

    def _property(self, window: int, atom: int, expected_type: int) -> tuple[bool, list[int]]:
        actual_type = ctypes.c_ulong()
        actual_format = ctypes.c_int()
        item_count = ctypes.c_ulong()
        bytes_after = ctypes.c_ulong()
        data = ctypes.POINTER(ctypes.c_ubyte)()
        status = self.x11.XGetWindowProperty(
            self.display,
            ctypes.c_ulong(window),
            ctypes.c_ulong(atom),
            0,
            1_000_000,
            0,
            ctypes.c_ulong(expected_type),
            ctypes.byref(actual_type),
            ctypes.byref(actual_format),
            ctypes.byref(item_count),
            ctypes.byref(bytes_after),
            ctypes.byref(data),
        )
        if status != 0 or not data or actual_type.value == 0:
            if data:
                self.x11.XFree(data)
            return False, []
        try:
            if actual_format.value == 32:
                values = ctypes.cast(data, ctypes.POINTER(ctypes.c_ulong))
                return True, [int(values[index]) for index in range(item_count.value)]
            return False, []
        finally:
            self.x11.XFree(data)

    def _descendant_processes(self) -> set[int]:
        now = time.monotonic()
        if self.window_count > 0 and now < self.descendant_refresh_at:
            return self.cached_descendants
        parents: dict[int, int] = {}
        for entry in Path("/proc").iterdir():
            if not entry.name.isdigit():
                continue
            with contextlib.suppress(OSError, ValueError, IndexError):
                stat = (entry / "stat").read_text(encoding="ascii")
                fields = stat[stat.rfind(")") + 2 :].split()
                parents[int(entry.name)] = int(fields[1])
        descendants = {os.getpid()}
        changed = True
        while changed:
            changed = False
            for process_id, parent_id in parents.items():
                if parent_id in descendants and process_id not in descendants:
                    descendants.add(process_id)
                    changed = True
        descendants.discard(os.getpid())
        self.cached_descendants = descendants
        self.descendant_refresh_at = now + 1
        return descendants

    def _parent_window(self, window: int) -> int:
        root = ctypes.c_ulong()
        parent = ctypes.c_ulong()
        children = ctypes.POINTER(ctypes.c_ulong)()
        child_count = ctypes.c_uint()
        ok = self.x11.XQueryTree(
            self.display,
            ctypes.c_ulong(window),
            ctypes.byref(root),
            ctypes.byref(parent),
            ctypes.byref(children),
            ctypes.byref(child_count),
        )
        if children:
            self.x11.XFree(children)
        if not ok or not parent.value or parent.value == self.root:
            return window
        return int(parent.value)

    def _windows(self) -> list[tuple[int, int]]:
        present, clients = self._property(
            self.root, self.client_list_atom, self.XA_WINDOW
        )
        if not present:
            _, clients = self._property(
                self.root, self.fallback_client_list_atom, self.XA_WINDOW
            )
        descendants = self._descendant_processes()
        result = []
        for client in clients:
            present, process_ids = self._property(client, self.pid_atom, self.XA_CARDINAL)
            if present and process_ids and process_ids[0] in descendants:
                result.append((client, self._parent_window(client)))
        return result

    def _set_opacity(self, window: int, opacity: int) -> None:
        value = ctypes.c_ulong(opacity)
        self.x11.XChangeProperty(
            self.display,
            ctypes.c_ulong(window),
            ctypes.c_ulong(self.opacity_atom),
            ctypes.c_ulong(self.XA_CARDINAL),
            32,
            self.PROP_MODE_REPLACE,
            ctypes.cast(ctypes.byref(value), ctypes.POINTER(ctypes.c_ubyte)),
            1,
        )

    def _empty_input_shape(self, window: int) -> None:
        self.xext.XShapeCombineRectangles(
            self.display,
            ctypes.c_ulong(window),
            self.SHAPE_INPUT,
            0,
            0,
            None,
            0,
            self.SHAPE_SET,
            self.UNSORTED,
        )

    def _restore_input_shape(self, window: int) -> None:
        root = ctypes.c_ulong()
        x = ctypes.c_int()
        y = ctypes.c_int()
        width = ctypes.c_uint()
        height = ctypes.c_uint()
        border = ctypes.c_uint()
        depth = ctypes.c_uint()
        if not self.x11.XGetGeometry(
            self.display,
            ctypes.c_ulong(window),
            ctypes.byref(root),
            ctypes.byref(x),
            ctypes.byref(y),
            ctypes.byref(width),
            ctypes.byref(height),
            ctypes.byref(border),
            ctypes.byref(depth),
        ):
            return
        rectangle = self.XRectangle(0, 0, width.value, height.value)
        self.xext.XShapeCombineRectangles(
            self.display,
            ctypes.c_ulong(window),
            self.SHAPE_INPUT,
            0,
            0,
            ctypes.byref(rectangle),
            1,
            self.SHAPE_SET,
            self.UNSORTED,
        )

    def _hide_window(self, client: int, frame: int) -> None:
        opacity_present, opacity_values = self._property(
            client, self.opacity_atom, self.XA_CARDINAL
        )
        self.records[client] = {
            "frame": frame,
            "opacity_present": opacity_present,
            "opacity": opacity_values[0] if opacity_values else 0xFFFFFFFF,
        }
        self._set_opacity(client, 0)
        self._empty_input_shape(client)
        if frame != client:
            self._empty_input_shape(frame)
        self.x11.XLowerWindow(self.display, ctypes.c_ulong(frame))

    def _restore_window(self, client: int, record: dict[str, Any]) -> None:
        if record["opacity_present"]:
            self._set_opacity(client, record["opacity"])
        else:
            self.x11.XDeleteProperty(
                self.display,
                ctypes.c_ulong(client),
                ctypes.c_ulong(self.opacity_atom),
            )
        self._restore_input_shape(client)
        frame = record["frame"]
        if frame != client:
            self._restore_input_shape(frame)
        self.x11.XMapRaised(self.display, ctypes.c_ulong(frame))

    def _apply(self, interactive: bool) -> int:
        windows = self._windows()
        live = {client for client, _ in windows}
        if interactive:
            for client, record in list(self.records.items()):
                self._restore_window(client, record)
            self.records.clear()
        else:
            for client, frame in windows:
                if client not in self.records:
                    self._hide_window(client, frame)
        for client in set(self.records) - live:
            self.records.pop(client, None)
        self.x11.XSync(self.display, 0)
        return len(windows)

    def _teardown(self) -> None:
        if not getattr(self, "display", None):
            return
        for client, record in list(getattr(self, "records", {}).items()):
            self._restore_window(client, record)
        self.x11.XSync(self.display, 0)
        if getattr(self, "previous_x_error_handler", None):
            self.x11.XSetErrorHandler(
                self.x_error_handler_type(self.previous_x_error_handler)
            )
        self.x11.XCloseDisplay(self.display)
        self.display = None


def create_browser_presentation(test_headless: bool) -> BrowserPresentation:
    if test_headless:
        return BrowserPresentation()
    system = platform.system()
    if system == "Darwin":
        return MacOSBrowserPresentation()
    if system == "Windows":
        return WindowsBrowserPresentation()
    if system == "Linux":
        return LinuxBrowserPresentation()
    raise ToolError("unsupported_platform", f"WebBrowser does not support {system}")


class PageState:
    def __init__(self, page_id: str, page: Any) -> None:
        self.page_id = page_id
        self.page = page
        self.snapshot_id = 0
        self.dismissed_dialogs: list[dict[str, str]] = []
        self.browser_events: list[dict[str, Any]] = []
        self.dropped_browser_events = 0

    def record_browser_event(self, event: dict[str, Any]) -> None:
        normalized = {
            key: value
            for key, value in event.items()
            if value not in (None, "", {}, [])
        }
        for key in ("message", "url"):
            value = normalized.get(key)
            if isinstance(value, str) and len(value) > MAX_BROWSER_EVENT_TEXT:
                normalized[key] = value[:MAX_BROWSER_EVENT_TEXT] + "…"
        if len(self.browser_events) >= MAX_BROWSER_EVENTS:
            self.browser_events.pop(0)
            self.dropped_browser_events += 1
        self.browser_events.append(normalized)

    def take_browser_events(self) -> tuple[list[dict[str, Any]], int]:
        events = self.browser_events
        dropped = self.dropped_browser_events
        self.browser_events = []
        self.dropped_browser_events = 0
        return events, dropped


class HumanActionDialog:
    def __init__(
        self,
        instruction: str,
        target_url: str,
        target_closed: Callable[[], bool],
        test_action: Callable[[], None] | None = None,
    ) -> None:
        self.instruction = instruction
        self.target_url = target_url
        self.target_closed = target_closed
        self.test_action = test_action

    def run(self) -> str:
        if os.environ.get("ME_WEB_BROWSER_TEST_HEADLESS") == "1":
            os.environ.setdefault("QT_QPA_PLATFORM", "offscreen")
        try:
            from PySide6.QtCore import QEventLoop, Qt, QTimer
            from PySide6.QtGui import QCursor
            from PySide6.QtWidgets import (
                QApplication,
                QDialog,
                QFrame,
                QHBoxLayout,
                QLabel,
                QPlainTextEdit,
                QPushButton,
                QVBoxLayout,
            )
        except Exception as exc:
            raise ToolError(
                "human_action_unavailable",
                f"WebBrowser GUI runtime is unavailable: {exc}",
                True,
            ) from exc

        try:
            app = QApplication.instance()
            if app is None:
                app = QApplication(["ME-S WebBrowser"])
                app.setApplicationName("ME-S")
                app.setQuitOnLastWindowClosed(False)

            dialog = QDialog()
            dialog.setObjectName("meHumanActionDialog")
            dialog.setWindowTitle("ME · 等待人工操作")
            dialog.setWindowModality(Qt.WindowModality.NonModal)
            dialog.setWindowFlag(Qt.WindowType.WindowStaysOnTopHint, True)
            dialog.setFixedWidth(560)
            dialog.setStyleSheet(
                """
                QDialog#meHumanActionDialog {
                    background: #11131a;
                    color: #f4f5f9;
                }
                QLabel {
                    background: transparent;
                    color: #f4f5f9;
                }
                QLabel#meHumanActionBrand {
                    background: #6558d9;
                    border-radius: 8px;
                    color: #ffffff;
                    font-size: 13px;
                    font-weight: 700;
                    padding: 5px 9px;
                }
                QLabel#meHumanActionEyebrow {
                    color: #8e95a8;
                    font-size: 11px;
                    font-weight: 600;
                }
                QLabel#meHumanActionTitle {
                    color: #ffffff;
                    font-size: 19px;
                    font-weight: 700;
                }
                QLabel#meHumanActionHint {
                    color: #adb2c0;
                    font-size: 13px;
                }
                QFrame#meHumanActionCard {
                    background: #191c25;
                    border: 1px solid #2b3040;
                    border-radius: 10px;
                }
                QLabel#meHumanActionSection {
                    color: #838a9e;
                    font-size: 11px;
                    font-weight: 600;
                }
                QPlainTextEdit#meHumanActionInstruction {
                    background: transparent;
                    border: none;
                    color: #f3f4f8;
                    font-size: 13px;
                    padding: 0;
                    selection-background-color: #6558d9;
                }
                QPlainTextEdit#meHumanActionInstruction QScrollBar:vertical {
                    background: transparent;
                    margin: 0;
                    width: 8px;
                }
                QPlainTextEdit#meHumanActionInstruction QScrollBar::handle:vertical {
                    background: #3b4050;
                    border-radius: 4px;
                    min-height: 20px;
                }
                QPlainTextEdit#meHumanActionInstruction QScrollBar::add-line:vertical,
                QPlainTextEdit#meHumanActionInstruction QScrollBar::sub-line:vertical {
                    height: 0;
                }
                QPlainTextEdit#meHumanActionInstruction QScrollBar::add-page:vertical,
                QPlainTextEdit#meHumanActionInstruction QScrollBar::sub-page:vertical {
                    background: transparent;
                }
                QLabel#meHumanActionTarget {
                    color: #aeb4c4;
                    font-size: 12px;
                }
                QPushButton {
                    border-radius: 8px;
                    font-size: 13px;
                    font-weight: 600;
                    min-height: 36px;
                    padding: 0 18px;
                }
                QPushButton#meHumanActionCancel {
                    background: #222631;
                    border: 1px solid #303544;
                    color: #c9cdd7;
                }
                QPushButton#meHumanActionCancel:hover {
                    background: #2b3040;
                    border-color: #3b4255;
                }
                QPushButton#meHumanActionCancel:pressed {
                    background: #1d2029;
                }
                QPushButton#meHumanActionComplete {
                    background: #6558d9;
                    border: 1px solid #7569e3;
                    color: #ffffff;
                }
                QPushButton#meHumanActionComplete:hover {
                    background: #7569e3;
                    border-color: #887dec;
                }
                QPushButton#meHumanActionComplete:pressed {
                    background: #5548c2;
                }
                """
            )

            layout = QVBoxLayout(dialog)
            layout.setContentsMargins(26, 24, 26, 22)
            layout.setSpacing(18)

            header = QHBoxLayout()
            header.setSpacing(12)
            brand = QLabel("ME")
            brand.setObjectName("meHumanActionBrand")
            brand.setAlignment(Qt.AlignmentFlag.AlignCenter)
            brand.setFixedSize(40, 32)
            header.addWidget(brand, 0, Qt.AlignmentFlag.AlignTop)

            heading = QVBoxLayout()
            heading.setSpacing(3)
            eyebrow = QLabel("WEB BROWSER · 人工接管")
            eyebrow.setObjectName("meHumanActionEyebrow")
            title = QLabel("请完成浏览器中的操作")
            title.setObjectName("meHumanActionTitle")
            heading.addWidget(eyebrow)
            heading.addWidget(title)
            header.addLayout(heading, 1)
            layout.addLayout(header)

            hint = QLabel(
                "目标页面已切到前台。处理完成后回到这里确认，"
                "ME 随后会读取页面的最新状态。"
            )
            hint.setObjectName("meHumanActionHint")
            hint.setWordWrap(True)
            hint.setTextFormat(Qt.TextFormat.PlainText)
            layout.addWidget(hint)

            card = QFrame()
            card.setObjectName("meHumanActionCard")
            card_layout = QVBoxLayout(card)
            card_layout.setContentsMargins(16, 14, 16, 14)
            card_layout.setSpacing(8)

            instruction_label = QLabel("操作说明")
            instruction_label.setObjectName("meHumanActionSection")
            card_layout.addWidget(instruction_label)

            instruction = QPlainTextEdit()
            instruction.setObjectName("meHumanActionInstruction")
            instruction.setReadOnly(True)
            instruction.setPlainText(self.instruction)
            instruction.setMinimumHeight(58)
            instruction.setMaximumHeight(124)
            instruction.setHorizontalScrollBarPolicy(Qt.ScrollBarPolicy.ScrollBarAlwaysOff)
            card_layout.addWidget(instruction)

            target = QLabel(f"目标页面：{self.target_url}")
            target.setObjectName("meHumanActionTarget")
            target.setWordWrap(True)
            target.setTextFormat(Qt.TextFormat.PlainText)
            target.setTextInteractionFlags(Qt.TextInteractionFlag.TextSelectableByMouse)
            card_layout.addWidget(target)
            layout.addWidget(card)

            buttons = QHBoxLayout()
            buttons.setSpacing(10)
            buttons.addStretch(1)
            cancel = QPushButton("取消")
            cancel.setObjectName("meHumanActionCancel")
            cancel.setCursor(Qt.CursorShape.PointingHandCursor)
            buttons.addWidget(cancel)
            complete = QPushButton("完成")
            complete.setObjectName("meHumanActionComplete")
            complete.setDefault(True)
            complete.setCursor(Qt.CursorShape.PointingHandCursor)
            buttons.addWidget(complete)
            layout.addLayout(buttons)

            outcome: list[str] = []
            event_loop = QEventLoop()

            def finish(value: str) -> None:
                if outcome:
                    return
                outcome.append(value)
                dialog.close()

            complete.clicked.connect(lambda: finish("completed"))
            cancel.clicked.connect(lambda: finish("cancelled"))
            dialog.rejected.connect(lambda: finish("cancelled"))
            dialog.finished.connect(event_loop.quit)

            target_timer = QTimer(dialog)
            target_timer.setInterval(100)

            def check_target() -> None:
                if self.target_closed():
                    finish("page_closed")

            target_timer.timeout.connect(check_target)
            target_timer.start()

            test_result = os.environ.get("ME_WEB_BROWSER_TEST_HUMAN_ACTION_RESULT")
            if test_result is not None:
                if test_result not in {"completed", "cancelled"}:
                    raise ToolError(
                        "human_action_unavailable",
                        "ME_WEB_BROWSER_TEST_HUMAN_ACTION_RESULT is invalid",
                    )

                def finish_test() -> None:
                    try:
                        if self.test_action is not None:
                            self.test_action()
                    finally:
                        finish(test_result)

                QTimer.singleShot(200, finish_test)

            dialog.ensurePolished()
            dialog.adjustSize()

            def center_dialog() -> None:
                screen = QApplication.screenAt(QCursor.pos())
                if screen is None:
                    screen = dialog.screen() or QApplication.primaryScreen()
                if screen is None:
                    return
                frame = dialog.frameGeometry()
                frame.moveCenter(screen.availableGeometry().center())
                dialog.move(frame.topLeft())

            center_dialog()
            dialog.show()
            app.processEvents()
            center_dialog()
            dialog.raise_()
            dialog.activateWindow()
            event_loop.exec()
            target_timer.stop()
            dialog.deleteLater()
            app.processEvents()
            return outcome[0] if outcome else "cancelled"
        except ToolError:
            raise
        except Exception as exc:
            raise ToolError(
                "human_action_failed",
                f"WebBrowser could not display the human-action window: {exc}",
                True,
            ) from exc


class BrowserRuntime:
    def __init__(self) -> None:
        self.dependencies = DependencyRuntime()
        self.camoufox = None
        self.browser = None
        self.context = None
        self.presentation: BrowserPresentation | None = None
        self.next_page_id = 1
        self.pages: dict[str, PageState] = {}
        self.page_objects: dict[str, str] = {}
        self.human_action_active = False

    def _launch(self) -> None:
        from camoufox.addons import ADDONS_DIR, DefaultAddons
        from camoufox.sync_api import Camoufox

        target_os = host_fingerprint_os()
        proxy = browser_proxy_from_environment()
        test_headless = os.environ.get("ME_WEB_BROWSER_TEST_HEADLESS") == "1"
        if not test_headless and platform.system() == "Linux" and not os.environ.get("DISPLAY"):
            raise ToolError(
                "display_unavailable",
                "WebBrowser requires an X11 or XWayland graphical desktop",
            )
        fingerprint_preset, webgl_config = compatible_fingerprint_preset(target_os)
        presentation = create_browser_presentation(test_headless)
        presentation.start()
        executable = self.dependencies.browser_executable()
        if executable is None:
            presentation.shutdown()
            raise ToolError(
                "browser_unavailable",
                "the verified Camoufox browser executable is unavailable; retry Create to repair it",
                True,
            )
        ubo_manifest = Path(ADDONS_DIR) / DefaultAddons.UBO.name / "manifest.json"
        excluded_addons = [] if ubo_manifest.is_file() else list(DefaultAddons)
        manager = None
        try:
            manager = Camoufox(
                # A validated, installed version name keeps Camoufox's own path
                # layout handling intact without permitting an implicit download.
                browser=CAMOUFOX_BROWSER_VERSION,
                ff_version=int(CAMOUFOX_BROWSER_VERSION.split(".", 1)[0]),
                exclude_addons=excluded_addons,
                headless=test_headless,
                os=target_os,
                fingerprint_preset=fingerprint_preset,
                webgl_config=webgl_config,
                humanize=True,
                enable_cache=True,
                env=presentation.launch_environment(),
            )
            browser = manager.__enter__()
            context_options: dict[str, Any] = {
                "accept_downloads": False,
            }
            if proxy is not None:
                context_options["proxy"] = proxy
            context = browser.new_context(**context_options)
        except Exception:
            if manager is not None:
                with contextlib.suppress(Exception):
                    manager.__exit__(*sys.exc_info())
            presentation.shutdown()
            raise
        self.camoufox = manager
        self.browser = browser
        self.context = context
        self.presentation = presentation
        context.on("page", self._register_page)

    def start(self, progress: Callable[[str], None]) -> None:
        if self.browser is not None and self.browser.is_connected():
            return
        self.dependencies.ensure(progress)
        try:
            try:
                self._launch()
            except Exception as first_error:
                message = str(first_error).lower()
                if not any(word in message for word in ("executable", "installed", "missing")):
                    raise
                self.shutdown()
                self.dependencies.reinstall_browser(progress)
                self._launch()
        except ToolError:
            raise
        except Exception as exc:
            self.shutdown()
            raise ToolError(
                "browser_start_failed",
                f"Camoufox could not start: {exc}",
                True,
            ) from exc

    def shutdown(self) -> None:
        with contextlib.suppress(Exception):
            if self.context is not None:
                self.context.close()
        with contextlib.suppress(Exception):
            if self.camoufox is not None:
                self.camoufox.__exit__(None, None, None)
            elif self.browser is not None:
                self.browser.close()
        with contextlib.suppress(Exception):
            if self.presentation is not None:
                self.presentation.shutdown()
        self.context = None
        self.browser = None
        self.camoufox = None
        self.presentation = None
        self.pages.clear()
        self.page_objects.clear()
        self.human_action_active = False

    def _register_page(self, page: Any) -> PageState:
        page_key = object_guid(page)
        existing = self.page_objects.get(page_key)
        if existing:
            return self.pages[existing]
        page_id = compact_id("p", self.next_page_id)
        self.next_page_id += 1
        state = PageState(page_id, page)
        self.pages[page_id] = state
        self.page_objects[page_key] = page_id

        def native_dialog(dialog: Any) -> None:
            state.dismissed_dialogs.append(
                {"type": dialog.type, "message": dialog.message, "action": "dismissed"}
            )
            with contextlib.suppress(Exception):
                dialog.dismiss()

        def console_message(message: Any) -> None:
            with contextlib.suppress(Exception):
                level = str(message.type or "log")
                if level not in ("warning", "error", "assert"):
                    return
                location = message.location
                state.record_browser_event(
                    {
                        "kind": "console",
                        "level": level,
                        "message": str(message.text or ""),
                        "url": str((location or {}).get("url") or ""),
                        "location": dict(location) if location else None,
                    }
                )

        def page_error(error: Any) -> None:
            with contextlib.suppress(Exception):
                state.record_browser_event(
                    {
                        "kind": "page_error",
                        "level": "error",
                        "message": str(error),
                        "url": str(page.url or ""),
                    }
                )

        def request_failed(request: Any) -> None:
            with contextlib.suppress(Exception):
                state.record_browser_event(
                    {
                        "kind": "request_failed",
                        "level": "error",
                        "message": str(request.failure or "request failed"),
                        "url": str(request.url or ""),
                        "method": str(request.method or ""),
                        "resource_type": str(request.resource_type or ""),
                    }
                )

        def http_response(response: Any) -> None:
            with contextlib.suppress(Exception):
                status = int(response.status)
                if status < 400:
                    return
                request = response.request
                state.record_browser_event(
                    {
                        "kind": "http_error",
                        "level": "error",
                        "message": f"HTTP {status} {response.status_text}".strip(),
                        "url": str(response.url or ""),
                        "method": str(request.method or ""),
                        "resource_type": str(request.resource_type or ""),
                        "status": status,
                    }
                )

        page.on("dialog", native_dialog)
        page.on("console", console_message)
        page.on("pageerror", page_error)
        page.on("requestfailed", request_failed)
        page.on("response", http_response)
        return state

    def create(self) -> dict[str, Any]:
        assert self.context is not None
        before = set(self.pages)
        page = self.context.new_page()
        created = [state for page_id, state in self.pages.items() if page_id not in before]
        state = created[0] if len(created) == 1 else self._register_page(page)
        if self.presentation is not None:
            self.presentation.conceal(require_window=True)
        return {"page_id": state.page_id}

    def page(self, page_id: str) -> PageState:
        state = self.pages.get(page_id)
        if state is None or state.page.is_closed():
            raise ToolError(
                "page_not_found",
                f"WebBrowser page {page_id} does not exist in the current toolbox runtime",
            )
        return state

    def _synchronize_pages(self) -> None:
        assert self.context is not None
        for page in self.context.pages:
            self._register_page(page)

    @staticmethod
    def _page_observation(state: PageState) -> dict[str, Any] | None:
        if state.page.is_closed():
            return None
        try:
            frames = []
            for frame in state.page.frames:
                frames.append([frame.url, frame.evaluate(HANDOFF_OBSERVATION_JS)])
            return {
                "url": state.page.url,
                "title": state.page.title(),
                "frames": frames,
            }
        except Exception:
            if state.page.is_closed():
                return None
            return {
                "url": state.page.url,
                "title": "",
                "frames": [],
            }

    def _open_page_observations(self) -> dict[str, dict[str, Any]]:
        observations: dict[str, dict[str, Any]] = {}
        processed: set[str] = set()
        while True:
            self._synchronize_pages()
            pending = [
                (page_id, state)
                for page_id, state in list(self.pages.items())
                if page_id not in processed
            ]
            if not pending:
                break
            for page_id, state in pending:
                processed.add(page_id)
                observation = self._page_observation(state)
                if observation is not None:
                    observations[page_id] = observation
        return observations

    @staticmethod
    def _page_change(
        before: dict[str, Any], after: dict[str, Any]
    ) -> str:
        before_frames = before.get("frames") or []
        after_frames = after.get("frames") or []
        before_origin = (
            before_frames[0][1].get("time_origin") if before_frames else None
        )
        after_origin = after_frames[0][1].get("time_origin") if after_frames else None
        if before.get("url") != after.get("url") or before_origin != after_origin:
            return "navigated"
        return "unchanged" if before == after else "changed"

    @staticmethod
    def _page_record(state: PageState) -> dict[str, Any]:
        if state.page.is_closed():
            return {
                "page_id": state.page_id,
                "url": "",
                "title": "",
                "state": "closed",
            }
        title = ""
        with contextlib.suppress(Exception):
            title = state.page.title()
        return {
            "page_id": state.page_id,
            "url": state.page.url,
            "title": title,
            "state": "open",
        }

    def _focused_page_id(self) -> str | None:
        self._synchronize_pages()
        for page_id, state in list(self.pages.items()):
            if state.page.is_closed():
                continue
            try:
                focused = bool(state.page.evaluate("() => document.hasFocus()"))
            except Exception:
                continue
            if focused:
                return page_id
        return None

    def require_human_action(
        self,
        state: PageState,
        instruction: str,
        notify: Callable[[str], None],
    ) -> dict[str, Any]:
        if self.human_action_active:
            raise ToolError(
                "human_action_busy",
                "another human browser handoff is already active",
            )
        if state.page.is_closed():
            raise ToolError("page_not_found", f"WebBrowser page {state.page_id} is closed")
        before = self._open_page_observations()
        active_page_id = None
        self.human_action_active = True
        try:
            if self.presentation is not None:
                self.presentation.reveal()
            state.page.bring_to_front()
            host = urlparse(state.page.url).hostname or state.page.url
            notify(
                f"Human action required in the visible browser · {host} · "
                "use the foreground ME window to click 完成 or 取消"
            )
            test_action = None
            test_script = os.environ.get("ME_WEB_BROWSER_TEST_HUMAN_ACTION_PAGE_SCRIPT")
            if (
                os.environ.get("ME_WEB_BROWSER_TEST_HEADLESS") == "1"
                and test_script is not None
            ):
                test_action = lambda: state.page.evaluate(test_script)
            outcome = HumanActionDialog(
                instruction,
                state.page.url,
                state.page.is_closed,
                test_action,
            ).run()
            if state.page.is_closed():
                outcome = "page_closed"
            active_page_id = self._focused_page_id()
        except ToolError:
            raise
        except Exception as exc:
            raise ToolError("human_action_failed", str(exc), True) from exc
        finally:
            try:
                if self.presentation is not None:
                    self.presentation.conceal()
            finally:
                self.human_action_active = False

        messages = {
            "completed": "The user indicated that the requested browser action is complete. Inspect the reported page changes before continuing.",
            "cancelled": "The user cancelled the browser handoff. Browser changes are still reported, but do not assume the requested action was completed.",
            "page_closed": "The user closed the target page during the browser handoff. Other browser changes are still reported.",
        }
        result_value: dict[str, Any] = {
            "state": outcome,
            "page_id": state.page_id,
            "message": messages[outcome],
        }
        after = self._open_page_observations()
        before_ids = set(before)
        after_ids = set(after)
        closed_page_ids = [page_id for page_id in before if page_id not in after_ids]

        if state.page_id not in after:
            target_page = {
                "page_id": state.page_id,
                "change": "closed",
                "page": self._page_record(state),
            }
        else:
            target_page = {
                "page_id": state.page_id,
                "change": self._page_change(
                    before[state.page_id], after[state.page_id]
                ),
                "page": self._page_record(state),
            }

        changed_pages = []
        for page_id in before:
            if page_id == state.page_id or page_id not in after:
                continue
            change = self._page_change(before[page_id], after[page_id])
            if change == "unchanged":
                continue
            record = {
                "page_id": page_id,
                "change": change,
                "page": self._page_record(self.pages[page_id]),
            }
            changed_pages.append(record)

        opened_pages = []
        for page_id in after:
            if page_id not in before_ids:
                opened_pages.append(
                    {
                        "page_id": page_id,
                        "page": self._page_record(self.pages[page_id]),
                    }
                )

        result_value.update(
            {
                "target_page": target_page,
                "changed_pages": changed_pages,
                "opened_pages": opened_pages,
                "closed_page_ids": closed_page_ids,
                "active_page_id": active_page_id,
            }
        )
        return result_value

    def navigate(self, state: PageState, url: str) -> dict[str, Any]:
        parsed = urlparse(url)
        if parsed.scheme not in ("http", "https", "about"):
            raise ToolError(
                "unsupported_url",
                "Navigate supports only http, https, and about URLs",
            )
        if parsed.scheme in ("http", "https") and not parsed.netloc:
            raise ToolError("invalid_url", f"URL has no host: {url}")
        try:
            state.page.goto(
                url,
                wait_until="commit",
                timeout=operation_hard_timeout_ms(),
            )
        except Exception as exc:
            raise ToolError("navigation_failed", str(exc), True) from exc
        return {"page_id": state.page_id, "navigated": True, "url": state.page.url}

    @staticmethod
    def _aria_locator(state: PageState, element_id: str) -> Any:
        locator = state.page.locator(f"aria-ref={element_id}")
        try:
            if locator.count() != 1:
                raise ToolError(
                    "stale_element",
                    f"element {element_id} is unavailable; take a new text snapshot and use its latest [ref=...] value",
                )
        except ToolError:
            raise
        except Exception as exc:
            raise ToolError(
                "stale_element",
                f"element {element_id} could not be resolved from the latest text snapshot: {exc}",
            ) from exc
        return locator

    def click(self, state: PageState, element_id: str) -> dict[str, Any]:
        before = set(self.pages)
        locator = self._aria_locator(state, element_id)
        try:
            locator.evaluate(
                """element => {
                    element.scrollIntoView({block: 'center', inline: 'center'});
                    element.click();
                }""",
                timeout=5000,
            )
            state.page._sync(asyncio.sleep(0.1))
        except Exception as exc:
            raise ToolError("click_failed", str(exc), True) from exc
        self._synchronize_pages()
        opened = [page_id for page_id in self.pages if page_id not in before]
        return {
            "page_id": state.page_id,
            "clicked": True,
            "opened_page_ids": opened,
        }

    def type_text(
        self,
        state: PageState,
        element_id: str,
        content: str,
        mode: str,
    ) -> dict[str, Any]:
        locator = self._aria_locator(state, element_id)
        try:
            if mode == "replace":
                locator.fill(content, timeout=operation_hard_timeout_ms())
            else:
                locator.focus(timeout=operation_hard_timeout_ms())
                state.page.keyboard.insert_text(content)
        except Exception as exc:
            raise ToolError("type_failed", str(exc), True) from exc
        return {"page_id": state.page_id, "typed": True}

    def press(
        self,
        state: PageState,
        key: str,
        element_id: str | None,
    ) -> dict[str, Any]:
        try:
            if element_id is None:
                state.page.keyboard.press(key)
            else:
                self._aria_locator(state, element_id).press(
                    key, timeout=operation_hard_timeout_ms()
                )
        except Exception as exc:
            raise ToolError("press_failed", str(exc)) from exc
        return {"page_id": state.page_id, "pressed": True}

    def scroll(
        self,
        state: PageState,
        delta_x: int,
        delta_y: int,
        element_id: str | None,
    ) -> dict[str, Any]:
        try:
            if element_id is not None:
                self._aria_locator(state, element_id).scroll_into_view_if_needed(
                    timeout=5000
                )
            else:
                channel = state.page.mouse._impl_obj._channel
                channel.send_no_reply(
                    "mouseMove",
                    None,
                    {"x": 1, "y": 1},
                    title="Move before scroll",
                )
                channel.send_no_reply(
                    "mouseWheel",
                    None,
                    {"deltaX": delta_x, "deltaY": delta_y},
                    title="Scroll",
                )
                state.page._sync(asyncio.sleep(0.05))
        except Exception as exc:
            raise ToolError("scroll_failed", str(exc), True) from exc
        return {"page_id": state.page_id, "scrolled": True}

    def back(self, state: PageState) -> dict[str, Any]:
        navigated = False

        def observe_navigation(frame: Any) -> None:
            nonlocal navigated
            if frame == state.page.main_frame:
                navigated = True

        state.page.on("framenavigated", observe_navigation)
        try:
            response = state.page.go_back(
                wait_until="commit", timeout=operation_hard_timeout_ms()
            )
        except Exception as exc:
            raise ToolError("history_navigation_failed", str(exc), True) from exc
        finally:
            state.page.remove_listener("framenavigated", observe_navigation)
        return {
            "page_id": state.page_id,
            "navigated": navigated or response is not None,
            "url": state.page.url,
        }

    def list_pages(self) -> dict[str, Any]:
        self._synchronize_pages()
        pages = [
            self._page_record(state)
            for state in self.pages.values()
            if not state.page.is_closed()
        ]
        return {"pages": pages, "active_page_id": self._focused_page_id()}

    def close_page(self, state: PageState) -> dict[str, Any]:
        state.page.close(run_before_unload=False)
        return {"page_id": state.page_id, "closed": True}
    def snapshot(
        self,
        state: PageState,
        wait_ms: int,
        kind: str,
    ) -> dict[str, Any]:
        if state.page.is_closed():
            raise ToolError("page_not_found", f"WebBrowser page {state.page_id} is closed")
        time.sleep(wait_ms / 1000)

        try:
            ready_state = str(
                state.page.evaluate("() => document.readyState") or "unknown"
            )
            title = state.page.title()
            output: dict[str, Any] = {
                "page_id": state.page_id,
                "snapshot_id": state.snapshot_id + 1,
                "url": state.page.url,
                "title": title,
                "state": ready_state,
                "kind": kind,
            }
            if kind in ("text", "both"):
                output["accessibility_tree"] = state.page.aria_snapshot(
                    timeout=operation_hard_timeout_ms(),
                    mode="ai",
                    boxes=True,
                )
            if kind in ("screen", "both"):
                SCREENSHOT_DIRECTORY.mkdir(parents=True, exist_ok=True)
                descriptor, raw_path = tempfile.mkstemp(
                    prefix="web-snapshot-",
                    suffix=".png",
                    dir=SCREENSHOT_DIRECTORY,
                )
                os.close(descriptor)
                path = Path(raw_path)
                try:
                    state.page.screenshot(
                        path=str(path),
                        full_page=False,
                        type="png",
                        timeout=operation_hard_timeout_ms(),
                    )
                except Exception:
                    with contextlib.suppress(OSError):
                        path.unlink()
                    raise
                output["screen_path"] = path.resolve().relative_to(Path.cwd()).as_posix()
        except Exception as exc:
            raise ToolError("snapshot_failed", str(exc), True) from exc

        state.snapshot_id += 1
        output["dismissed_native_dialogs"] = state.dismissed_dialogs
        state.dismissed_dialogs = []
        browser_events, dropped_browser_events = state.take_browser_events()
        output["browser_events"] = browser_events
        output["dropped_browser_events"] = dropped_browser_events
        return output
RUNTIME: BrowserRuntime | None = None


def browser_runtime() -> BrowserRuntime:
    global RUNTIME
    if RUNTIME is None:
        RUNTIME = BrowserRuntime()
    return RUNTIME


def validate_object(value: Any) -> dict[str, Any]:
    if not isinstance(value, dict):
        raise ToolError("invalid_arguments", "input must be a JSON object")
    return value


def string_arg(
    data: dict[str, Any], name: str, default: str | None = None, allow_empty: bool = False
) -> str:
    value = data.get(name, default)
    if not isinstance(value, str) or (not allow_empty and not value):
        raise ToolError("invalid_arguments", f"{name} must be a string")
    if "\x00" in value:
        raise ToolError("invalid_arguments", f"{name} contains NUL")
    return value


def int_arg(
    data: dict[str, Any], name: str, default: int, minimum: int, maximum: int
) -> int:
    value = data.get(name, default)
    if isinstance(value, bool) or not isinstance(value, int) or not minimum <= value <= maximum:
        raise ToolError(
            "invalid_arguments", f"{name} must be an integer in {minimum}..={maximum}"
        )
    return value


def execute(tool: str, data: dict[str, Any], request_id: int) -> Any:
    if tool == "__activePages":
        if (
            RUNTIME is None
            or RUNTIME.browser is None
            or not RUNTIME.browser.is_connected()
        ):
            return {"pages": [], "active_page_id": None}
        return RUNTIME.list_pages()
    runtime = browser_runtime()
    runtime.start(lambda message: update(request_id, message))
    if tool == "Create":
        return runtime.create()
    if tool == "Pages":
        return runtime.list_pages()
    page_id = string_arg(data, "page_id")
    state = runtime.page(page_id)
    if tool == "Navigate":
        return runtime.navigate(state, string_arg(data, "url"))
    if tool == "Click":
        return runtime.click(state, string_arg(data, "element_id"))
    if tool == "Type":
        mode = string_arg(data, "mode", "replace")
        if mode not in ("replace", "append"):
            raise ToolError("invalid_arguments", "mode must be replace or append")
        return runtime.type_text(
            state,
            string_arg(data, "element_id"),
            string_arg(data, "content", "", allow_empty=True),
            mode,
        )
    if tool == "Press":
        element_id = data.get("element_id")
        if element_id is not None and not isinstance(element_id, str):
            raise ToolError("invalid_arguments", "element_id must be a string")
        return runtime.press(state, string_arg(data, "key"), element_id)
    if tool == "Scroll":
        element_id = data.get("element_id")
        if element_id is not None and not isinstance(element_id, str):
            raise ToolError("invalid_arguments", "element_id must be a string")
        return runtime.scroll(
            state,
            int_arg(data, "delta_x", 0, -100_000, 100_000),
            int_arg(data, "delta_y", 720, -100_000, 100_000),
            element_id,
        )
    if tool == "RequireHumanAction":
        return runtime.require_human_action(
            state,
            string_arg(data, "instruction"),
            lambda message: update(request_id, message),
        )
    if tool == "Snapshot":
        kind = string_arg(data, "kind")
        if kind not in ("text", "screen", "both"):
            raise ToolError(
                "invalid_arguments", "kind must be text, screen, or both"
            )
        return runtime.snapshot(
            state,
            int_arg(
                data,
                "wait_ms",
                MIN_SNAPSHOT_WAIT_MS,
                MIN_SNAPSHOT_WAIT_MS,
                MAX_SNAPSHOT_WAIT_MS,
            ),
            kind,
        )
    if tool == "Back":
        return runtime.back(state)
    if tool == "Close":
        return runtime.close_page(state)
    raise ToolError("unknown_tool", f"unknown WebBrowser tool: {tool}")


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
            "Operate real web pages through a persistent headed Camoufox browser context on a graphical desktop. The native window remains concealed during automation and is revealed only for an explicit human handoff. Action tools perform one browser action and return no page content. WebBrowser.Snapshot is the sole page-content observation tool: after a fixed delay it returns Playwright's browser-generated ARIA accessibility tree verbatim, a reusable workspace path to the rendered viewport screenshot, or both. A screenshot is not shown to the model automatically; use Image.View on the returned path only when visual inspection is needed, then remove an unneeded screenshot with File.Stat followed by File.Delete. The normal loop is Create, Navigate, Snapshot(kind=text), act with a ref from that snapshot, then Snapshot again to observe the result. Text snapshots contain native [ref=…] values and viewport-relative boxes. Refs belong to the rendered document that produced them; refresh the text Snapshot after navigation or a structural page change, never invent a ref, and preserve iframe prefixes exactly. Native JavaScript dialogs are dismissed automatically and reported by the next Snapshot. If an operation returns operation_timeout, the unresponsive browser runtime has been restarted: discard every previous page_id and element_id, then Create a new page. When general web search is needed and the user did not select an engine, use Google first and Baidu second; if Google is unavailable, blocked by verification, or inadequate, continue with Baidu. Browser dependencies are installed once in ME-S's global directory; live page state remains private to this toolbox process and does not survive its restart.",
        )
        return
    tool = request.get("tool")
    internal_tool = command == "execute" and tool == "__activePages"
    if tool not in TOOLS and not internal_tool:
        raise ToolError("unknown_tool", f"unknown WebBrowser tool: {tool}")
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
        allowed = set() if internal_tool else set(INPUT_SCHEMAS[tool]["properties"])
        unexpected = sorted(set(data) - allowed)
        if unexpected:
            raise ToolError(
                "invalid_arguments", f"unexpected input fields: {', '.join(unexpected)}"
            )
        result(request_id, execute(tool, data, request_id))
    else:
        raise ToolError("unknown_command", f"unsupported command: {command}")


def worker_main() -> None:
    try:
        for line in sys.stdin:
            request_id = 0
            try:
                request = json.loads(line)
                if isinstance(request, dict) and isinstance(request.get("id"), int):
                    request_id = request["id"]
                handle(request)
            except ToolError as exc:
                error(request_id, exc)
            except (OSError, ValueError, TypeError, KeyError) as exc:
                error(request_id, ToolError("execution_error", str(exc)))
    finally:
        if RUNTIME is not None:
            RUNTIME.shutdown()


class BrowserWorkerProcess:
    def __init__(self) -> None:
        self.process: subprocess.Popen[str] | None = None
        self.frames: queue.Queue[str | None] = queue.Queue()
        self.stderr_lines: list[str] = []
        self.stderr_lock = threading.Lock()

    def start(self) -> None:
        if self.process is not None and self.process.poll() is None:
            return
        environment = os.environ.copy()
        environment[WORKER_MODE_ENV] = "1"
        self.frames = queue.Queue()
        self.stderr_lines = []
        self.process = subprocess.Popen(
            [sys.executable, str(Path(__file__).resolve())],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            encoding="utf-8",
            errors="strict",
            bufsize=1,
            env=environment,
        )
        process = self.process
        frames = self.frames
        stderr_lines = self.stderr_lines
        assert process.stdout is not None
        assert process.stderr is not None

        def read_stdout() -> None:
            try:
                assert process.stdout is not None
                for frame in process.stdout:
                    frames.put(frame)
            finally:
                frames.put(None)

        def read_stderr() -> None:
            assert process.stderr is not None
            for line in process.stderr:
                with self.stderr_lock:
                    stderr_lines.append(line.rstrip())
                    del stderr_lines[:-32]

        threading.Thread(target=read_stdout, daemon=True).start()
        threading.Thread(target=read_stderr, daemon=True).start()

    def stderr_suffix(self) -> str:
        with self.stderr_lock:
            return " | ".join(self.stderr_lines)

    @staticmethod
    def _posix_descendants(root_pid: int) -> list[int]:
        try:
            completed = subprocess.run(
                ["ps", "-axo", "pid=,ppid="],
                check=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.DEVNULL,
                text=True,
                encoding="utf-8",
            )
        except (OSError, subprocess.SubprocessError):
            return []
        children: dict[int, list[int]] = {}
        for line in completed.stdout.splitlines():
            fields = line.split()
            if len(fields) != 2:
                continue
            try:
                pid, parent = map(int, fields)
            except ValueError:
                continue
            children.setdefault(parent, []).append(pid)
        descendants: list[int] = []
        pending = list(children.get(root_pid, []))
        while pending:
            pid = pending.pop()
            descendants.append(pid)
            pending.extend(children.get(pid, []))
        return descendants

    def terminate(self) -> None:
        process = self.process
        self.process = None
        if process is None:
            return
        if process.poll() is None:
            if os.name == "nt":
                with contextlib.suppress(OSError, subprocess.SubprocessError):
                    subprocess.run(
                        ["taskkill", "/PID", str(process.pid), "/T", "/F"],
                        stdout=subprocess.DEVNULL,
                        stderr=subprocess.DEVNULL,
                        timeout=5,
                        check=False,
                    )
            else:
                descendants = self._posix_descendants(process.pid)
                for pid in reversed(descendants):
                    with contextlib.suppress(ProcessLookupError, PermissionError):
                        os.kill(pid, signal.SIGKILL)
                with contextlib.suppress(ProcessLookupError, PermissionError):
                    os.kill(process.pid, signal.SIGKILL)
            with contextlib.suppress(subprocess.TimeoutExpired):
                process.wait(timeout=5)
            if process.poll() is None:
                with contextlib.suppress(OSError):
                    process.kill()
                with contextlib.suppress(subprocess.TimeoutExpired):
                    process.wait(timeout=5)
        for stream in (process.stdin, process.stdout, process.stderr):
            if stream is not None:
                with contextlib.suppress(OSError):
                    stream.close()

    def exchange(self, line: str, request_id: int, timeout_ms: int | None) -> None:
        self.start()
        assert self.process is not None and self.process.stdin is not None
        try:
            self.process.stdin.write(line)
            if not line.endswith("\n"):
                self.process.stdin.write("\n")
            self.process.stdin.flush()
        except OSError as exc:
            suffix = self.stderr_suffix()
            self.terminate()
            raise ToolError(
                "browser_worker_failed",
                f"WebBrowser worker could not receive the request: {exc}"
                + (f"; stderr: {suffix}" if suffix else ""),
                True,
            ) from exc

        deadline = (
            None if timeout_ms is None else time.monotonic() + timeout_ms / 1000
        )
        while True:
            remaining = None if deadline is None else deadline - time.monotonic()
            if remaining is not None and remaining <= 0:
                self.terminate()
                raise ToolError(
                    "operation_timeout",
                    f"WebBrowser operation exceeded its hard {timeout_ms} ms execution limit; "
                    "the unresponsive browser runtime was restarted and all previous page_id and element_id values are invalid",
                    True,
                )
            try:
                frame_line = self.frames.get(timeout=remaining)
            except queue.Empty:
                self.terminate()
                raise ToolError(
                    "operation_timeout",
                    f"WebBrowser operation exceeded its hard {timeout_ms} ms execution limit; "
                    "the unresponsive browser runtime was restarted and all previous page_id and element_id values are invalid",
                    True,
                ) from None
            if frame_line is None:
                suffix = self.stderr_suffix()
                self.terminate()
                raise ToolError(
                    "browser_worker_failed",
                    "WebBrowser worker exited before completing the request"
                    + (f"; stderr: {suffix}" if suffix else ""),
                    True,
                )
            try:
                frame = json.loads(frame_line)
            except (ValueError, TypeError) as exc:
                self.terminate()
                raise ToolError(
                    "browser_worker_protocol_error",
                    f"WebBrowser worker returned invalid JSONL: {exc}",
                    True,
                ) from exc
            if not isinstance(frame, dict) or frame.get("id") != request_id:
                self.terminate()
                raise ToolError(
                    "browser_worker_protocol_error",
                    f"WebBrowser worker returned a response for the wrong request: {frame!r}",
                    True,
                )
            sys.stdout.write(frame_line)
            if not frame_line.endswith("\n"):
                sys.stdout.write("\n")
            sys.stdout.flush()
            if frame.get("type") != "update":
                return


def hard_timeout_grace_ms() -> int:
    configured = os.environ.get("ME_WEB_BROWSER_TEST_HARD_TIMEOUT_GRACE_MS")
    if configured is None:
        return HARD_TIMEOUT_GRACE_MS
    try:
        return max(0, min(60_000, int(configured)))
    except ValueError:
        return HARD_TIMEOUT_GRACE_MS


def request_hard_timeout_ms(request: dict[str, Any]) -> int | None:
    if request.get("cmd") != "execute":
        return operation_hard_timeout_ms()
    tool = request.get("tool")
    if tool == "Create":
        return create_hard_timeout_ms()
    if tool == "RequireHumanAction":
        return None
    if tool == "Snapshot":
        input_value = request.get("input")
        wait_ms = MIN_SNAPSHOT_WAIT_MS
        if isinstance(input_value, dict):
            candidate = input_value.get("wait_ms", MIN_SNAPSHOT_WAIT_MS)
            if isinstance(candidate, int) and not isinstance(candidate, bool):
                wait_ms = max(
                    MIN_SNAPSHOT_WAIT_MS,
                    min(MAX_SNAPSHOT_WAIT_MS, candidate),
                )
        return wait_ms + hard_timeout_grace_ms()
    return operation_hard_timeout_ms()


def supervisor_main() -> None:
    worker = BrowserWorkerProcess()
    try:
        for line in sys.stdin:
            request_id = 0
            try:
                request = json.loads(line)
                if not isinstance(request, dict) or not isinstance(request.get("id"), int):
                    raise ToolError(
                        "invalid_request", "request must contain an integer id"
                    )
                request_id = request["id"]
                worker.exchange(
                    line,
                    request_id,
                    request_hard_timeout_ms(request),
                )
            except ToolError as exc:
                error(request_id, exc)
            except (OSError, ValueError, TypeError, KeyError) as exc:
                error(request_id, ToolError("execution_error", str(exc)))
    finally:
        worker.terminate()


def main() -> None:
    if os.environ.get(WORKER_MODE_ENV) == "1":
        worker_main()
    else:
        supervisor_main()


if __name__ == "__main__":
    main()
