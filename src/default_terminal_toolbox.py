#!/usr/bin/env python3
# ME-RUST-MANAGED-TOOLBOX
"""ME-RUST default Terminal toolbox.

This program is a persistent JSONL toolbox process. Python 3.12 hosts
the process while ME-RUST's terminal worker supplies the
cross-platform PTY and VT100 implementation.
"""

from __future__ import annotations

import os
import shutil
import subprocess
import sys
import threading


def fail(message: str) -> "None":
    print(message, file=sys.stderr, flush=True)
    raise SystemExit(1)


if sys.version_info[:2] != (3, 12):
    fail(
        "Terminal toolbox requires Python 3.12; "
        f"received {sys.version_info.major}.{sys.version_info.minor}"
    )

host = os.environ.get("ME_TOOLBOX_HOST") or shutil.which("me-s")
if not host:
    fail("Terminal toolbox cannot find ME-RUST; set ME_TOOLBOX_HOST or install me-s")

worker = subprocess.Popen(
    [host, "__toolbox-terminal-worker"],
    stdin=subprocess.PIPE,
    stdout=subprocess.PIPE,
    stderr=subprocess.PIPE,
    bufsize=0,
)
if worker.stdin is None or worker.stdout is None or worker.stderr is None:
    fail("Terminal toolbox could not open worker pipes")


def copy_stream(source, target) -> None:
    read_available = getattr(source, "read1", source.read)
    try:
        while True:
            chunk = read_available(65536)
            if not chunk:
                break
            target.write(chunk)
            target.flush()
    except (BrokenPipeError, OSError):
        pass


stdout_thread = threading.Thread(
    target=copy_stream,
    args=(worker.stdout, sys.stdout.buffer),
    daemon=True,
)
stderr_thread = threading.Thread(
    target=copy_stream,
    args=(worker.stderr, sys.stderr.buffer),
    daemon=True,
)
stdout_thread.start()
stderr_thread.start()

try:
    copy_stream(sys.stdin.buffer, worker.stdin)
finally:
    try:
        worker.stdin.close()
    except OSError:
        pass

exit_code = worker.wait()
stdout_thread.join(timeout=1)
stderr_thread.join(timeout=1)
raise SystemExit(exit_code)
