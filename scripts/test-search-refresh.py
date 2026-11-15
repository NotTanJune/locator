#!/usr/bin/env python3
"""Check that a watcher refresh does not redraw the search screen's chrome.

While the search screen sits idle, the live watcher reruns the query when files
change. That rerun must not show the spinner title, change the status line, or
resize the detail pane. This runs every search mode in a real Unix PTY.

Usage: python3 scripts/test-search-refresh.py target/debug/lctr
Uses only the standard library and disposable fixture files.
"""

import fcntl
import importlib.util
import os
from pathlib import Path
import pty
import select
import signal
import struct
import subprocess
import sys
import tempfile
import termios
import time

# Importing the sibling script must not leave a __pycache__ folder in the repository.
sys.dont_write_bytecode = True
_spec = importlib.util.spec_from_file_location(
    "search_exit", Path(__file__).with_name("test-search-exit.py"))
_search_exit = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(_search_exit)
screen_text = _search_exit.screen_text

MODES = ["contains", "exact", "prefix", "suffix", "fuzzy", "regex", "glob", "semantic"]
ROWS, COLUMNS = 40, 240


def chrome(screen):
    """The parts of the screen a background refresh must leave alone."""
    lines = screen.decode().split("\n")
    title = next((line for line in lines if "results (" in line or "searching" in line), "")
    title = title.split("preview")[0].strip(" ╭╮─")
    status = next((line for line in lines if line.startswith("root ")), "")
    status = status.split(" results", 1)[1] if " results" in status else status
    detail_rows = [index for index, line in enumerate(lines) if "j/k move" in line]
    return {
        "title": "searching" if "searching" in title else "results",
        # The row count may change when the refresh finds new files.
        "status": "".join(ch for ch in status if not ch.isdigit()).strip(),
        "detail_row": detail_rows[0] if detail_rows else None,
        "similarity": any(line.startswith("similarity") for line in lines),
        # The highlighted row: the selection must survive the refresh.
        "selected": next((line.split("\u258c", 1)[1].split()[0] for line in lines
                          if "\u258c" in line), None),
    }


def session(binary, root, env, mode):
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLUMNS, 0, 0))

    def controlling_terminal():
        os.setsid()
        fcntl.ioctl(0, termios.TIOCSCTTY, 0)

    process = subprocess.Popen(
        [str(binary), "search", str(root), "--no-update-check"],
        stdin=slave, stdout=slave, stderr=slave, env=env, cwd=str(root.parent),
        preexec_fn=controlling_terminal,
    )
    output = bytearray()

    def pump(timeout):
        if select.select([master], [], [], timeout)[0]:
            output.extend(os.read(master, 65536))
            return True
        return False

    def read_until(marker, timeout=10):
        deadline = time.monotonic() + timeout
        while marker not in screen_text(output):
            assert process.poll() is None, f"early exit: {output[-1500:]!r}"
            assert time.monotonic() < deadline, (
                f"missing {marker!r}: {screen_text(output).decode()}")
            pump(0.02)

    try:
        read_until(b"Type to search")
        read_until(b"watch on")
        query = "report"
        if mode == "exact":
            query = "report-0001.txt"
        elif mode == "suffix":
            query = ".txt"
        os.write(master, query.encode())
        read_until(b"complete")
        os.write(master, b"\t")
        read_until(b"j/k move")
        for _ in range(MODES.index(mode)):
            os.write(master, b"m")
        read_until(f"mode:{mode}".encode())
        if mode == "glob":
            # A glob needs a wildcard to match these names. Add it from the search box.
            os.write(master, b"/")
            read_until(b"type to filter")
            os.write(master, b"*")
            read_until(b"report*")
            time.sleep(0.5)
            os.write(master, b"\t")
            read_until(b"j/k move")
        read_until(b"complete")
        if mode == "semantic":
            read_until(b"similarity")
        # Let the preview and any startup watcher batch settle.
        quiet_since = time.monotonic()
        while time.monotonic() - quiet_since < 1.5:
            if pump(0.05):
                quiet_since = time.monotonic()
        before = chrome(screen_text(output))
        assert before["title"] == "results", f"{mode}: not settled: {before}"
        assert before["detail_row"] is not None, f"{mode}: no detail pane"
        assert before["selected"], f"{mode}: no selected row"
        if mode == "semantic":
            assert before["similarity"], "semantic: no similarity line before the refresh"

        # Change the tree several times, as a busy folder does.
        changes = []
        deadline = time.monotonic() + 4
        made = 0
        next_change = time.monotonic()
        while time.monotonic() < deadline:
            if made < 4 and time.monotonic() >= next_change:
                (root / f"report-new-{mode}-{made}.txt").write_text("new\n")
                made += 1
                next_change = time.monotonic() + 0.7
            if pump(0.02):
                now = chrome(screen_text(output))
                if now != before:
                    changes.append(now)
        screen = screen_text(output).decode()
        assert not changes, f"{mode}: chrome changed during a watcher refresh: {changes[:3]}"
        # Prove the refresh ran: filename modes that match the new files show them.
        refreshed = "n/a"
        if mode in ("contains", "prefix", "fuzzy", "regex", "glob", "suffix"):
            assert "(604)" in screen, f"{mode}: new files never appeared:\n{screen}"
            refreshed = "604 rows"
        print(f"PASS {mode:9} refresh={refreshed}", flush=True)
        return True
    except AssertionError as error:
        print(f"FAIL {mode:9} {str(error)[:600]}", flush=True)
        return False
    finally:
        if process.poll() is None:
            os.killpg(process.pid, signal.SIGKILL)
            process.wait()
        os.close(master)
        os.close(slave)


def main():
    binary = Path(sys.argv[1] if len(sys.argv) > 1 else "target/debug/lctr").resolve()
    only = sys.argv[2:] or MODES
    failed = []
    for mode in only:
        with tempfile.TemporaryDirectory(prefix="lctr-refresh-") as temporary:
            base = Path(temporary).resolve()
            root = base / "files"
            root.mkdir()
            for index in range(600):
                (root / f"report-{index:04}.txt").write_text("Search preview fixture.\n")
            env = os.environ.copy()
            env.update(TERM="xterm-256color", HOME=str(base / "home"),
                       LCTR_CONFIG_DIR=str(base / "config"),
                       LCTR_DB=str(base / "index.db"), LCTR_ICONS="0",
                       LCTR_EMBED_FAKE="1", LCTR_DATA_DIR=str(base / "data"))
            env.pop("LCTR_INPUT_TRACE", None)
            (base / "home").mkdir()
            subprocess.run(
                [str(binary), "scan", str(root), "--em", "--no-update-check"],
                env=env, cwd=str(base), check=True, capture_output=True, timeout=60,
            )
            if not session(binary, root, env, mode):
                failed.append(mode)
    if failed:
        sys.exit(f"failed: {', '.join(failed)}")


if __name__ == "__main__":
    main()
