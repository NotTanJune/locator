#!/usr/bin/env python3
"""Exercise search exit keys and terminal restoration in a real Unix PTY.

Usage: python3 scripts/test-search-exit.py target/debug/lctr
Uses only the standard library and disposable fixture files.
"""

import fcntl
import os
from pathlib import Path
import pty
import re
import select
import signal
import struct
import subprocess
import sys
import tempfile
import termios
import time


def screen_text(output):
    """Reconstruct the cursor-positioned cells emitted by Crossterm/Ratatui."""
    cells = [[" "] * 240 for _ in range(40)]
    row = column = 0
    tokens = re.findall(rb"\x1b\[[0-?]*[ -/]*[@-~]|\x1b[^\[]|[^\x1b]+", output)
    for token in tokens:
        if token.startswith(b"\x1b["):
            command = token[-1:]
            arguments = token[2:-1]
            if command not in (b"H", b"f", b"G", b"C", b"D", b"J", b"K"):
                continue
            values = [int(value or b"0") for value in arguments.split(b";")]
            if command in (b"H", b"f"):
                row = max(0, values[0] - 1)
                column = max(0, (values[1] if len(values) > 1 else 1) - 1)
            elif command == b"G":
                column = max(0, values[0] - 1)
            elif command == b"C":
                column += values[0] or 1
            elif command == b"D":
                column = max(0, column - (values[0] or 1))
            elif command == b"J" and values[0] == 2:
                cells = [[" "] * 240 for _ in range(40)]
            elif command == b"K" and row < 40:
                for index in range(column, 240):
                    cells[row][index] = " "
        elif not token.startswith(b"\x1b"):
            for char in token.decode("utf-8", errors="replace"):
                if char == "\r":
                    column = 0
                elif char == "\n":
                    row += 1
                elif char >= " ":
                    if row < 40 and column < 240:
                        cells[row][column] = char
                    column += 1
    return "\n".join("".join(line) for line in cells).encode()


def session(binary, root, env, focus, key):
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 240, 0, 0))
    original = termios.tcgetattr(slave)

    def controlling_terminal():
        os.setsid()
        fcntl.ioctl(0, termios.TIOCSCTTY, 0)

    process = subprocess.Popen(
        # Keep the session leader alive long enough to inspect terminal flags.
        # macOS revokes the slave's ioctls when its session leader exits.
        [sys.executable, "-c",
         "import subprocess,sys,termios; "
         "result=subprocess.run(sys.argv[1:]); "
         "print('LCTR_TEST_LFLAG='+str(termios.tcgetattr(0)[3]), flush=True); "
         "sys.exit(result.returncode)",
         str(binary), "search", str(root), "--no-update-check"],
        stdin=slave, stdout=slave, stderr=slave, env=env,
        preexec_fn=controlling_terminal,
    )
    output = bytearray()

    def read_until(marker, timeout=5):
        deadline = time.monotonic() + timeout
        while marker not in screen_text(output):
            assert process.poll() is None, f"early exit: {output[-1500:]!r}"
            assert time.monotonic() < deadline, f"missing {marker!r}: {screen_text(output).decode()}"
            if select.select([master], [], [], 0.02)[0]:
                output.extend(os.read(master, 65536))

    try:
        read_until(b"Type to search")
        started = time.monotonic()
        os.write(master, b"report")
        if env["LCTR_DB"].endswith("missing.sqlite"):
            os.write(master, b"\r")
        if focus != "searching":
            # Ratatui emits changed cells, so the existing "results " prefix
            # need not be repeated when the completion suffix is drawn.
            read_until(b"complete")
        search_ms = (time.monotonic() - started) * 1000
        if focus in ("results", "semantic", "semantic_error"):
            os.write(master, b"\t")
            read_until(b"j/k move")
            if focus.startswith("semantic"):
                os.write(master, b"mmmmmmm")
                read_until(b"mode:semantic")
                if focus == "semantic_error":
                    read_until(b"embedding model not found")
                else:
                    read_until(b"600 results complete")
                    read_until(b"similarity")
        elif focus == "help":
            os.write(master, b"\x1bOP")  # F1 in the legacy terminal protocol.
            read_until(b"other keys close help")
        # Allow the preview debounce to run before exercising exit.
        if focus != "searching":
            time.sleep(0.3)
        started = time.monotonic()
        os.write(master, key)
        deadline = started + 2
        while process.poll() is None and time.monotonic() < deadline:
            if select.select([master], [], [], 0.01)[0]:
                output.extend(os.read(master, 65536))
        assert process.poll() == 0, f"exit failed in {focus}: {process.poll()}"
        exit_ms = (time.monotonic() - started) * 1000
        while select.select([master], [], [], 0)[0]:
            chunk = os.read(master, 65536)
            if not chunk:
                break
            output.extend(chunk)
        restored = re.search(rb"LCTR_TEST_LFLAG=(\d+)", output)
        assert restored, "missing terminal restoration check"
        flags = termios.ICANON | termios.ECHO | termios.ISIG
        assert int(restored[1]) & flags == original[3] & flags, "terminal flags not restored"
        assert b"\x1b[?1049l" in output, "alternate screen not restored"
        assert b"panicked at" not in output, "panic output corrupted the terminal"
        name = "Esc" if key == b"\x1b" else "Ctrl+C"
        print(f"PASS {focus:9} {name:6} search={search_ms:.0f}ms exit={exit_ms:.0f}ms", flush=True)
    finally:
        if process.poll() is None:
            os.killpg(process.pid, signal.SIGKILL)
            process.wait()
        os.close(master)
        os.close(slave)


def main():
    binary = Path(sys.argv[1] if len(sys.argv) > 1 else "target/debug/lctr").resolve()
    with tempfile.TemporaryDirectory(prefix="lctr-exit-") as temporary:
        base = Path(temporary)
        root = base / "files"
        root.mkdir()
        for index in range(600):
            (root / f"report-{index:04}.txt").write_text("Search preview fixture.\n")
        env = os.environ.copy()
        env.update(TERM="xterm-256color", LCTR_CONFIG_DIR=str(base / "config"),
                   LCTR_DB=str(base / "index.sqlite"), LCTR_ICONS="0",
                   LCTR_EMBED_FAKE="1", LCTR_DATA_DIR=str(base / "data"))
        env.pop("LCTR_INPUT_TRACE", None)
        subprocess.run(
            [str(binary), "scan", str(root), "--em", "--no-stage-index", "--no-update-check"],
            env=env, check=True, capture_output=True, timeout=30,
        )
        for backend in ("indexed", "live"):
            if backend == "live":
                env["LCTR_DB"] = str(base / "missing.sqlite")
            print(backend, flush=True)
            for focus in ("search", "results", "help", "searching"):
                for key in (b"\x1b", b"\x03"):
                    session(binary, root, env, focus, key)
            if backend == "indexed":
                for key in (b"\x1b", b"\x03"):
                    session(binary, root, env, "semantic", key)
                    error_env = dict(env, LCTR_EMBED_FAKE="0")
                    session(binary, root, error_env, "semantic_error", key)


if __name__ == "__main__":
    main()
