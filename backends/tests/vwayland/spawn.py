#!/usr/bin/env python3
"""Spawn helper for the pcu vwayland live test.

Spawns a headless py-vwayland compositor, launches the pywayland test
client inside it, waits for the client's READY signal, then prints one
JSON line to stdout:

    {"sock": "<ipc socket path>", "width": 1280, "height": 720}

and blocks until stdin closes (or a "quit" line), at which point the
compositor (and the client inside it) is killed. The Rust integration
test drives this as a child process: read the JSON line, run the
Executor round trip, close stdin.

The helper also dies if its parent dies (PR_SET_PDEATHSIG) and handles
SIGTERM, so leaked compositors don't survive a crashed test run.

Requires: pip install py-vwayland pywayland
"""

import ctypes
import json
import os
import signal
import sys
import tempfile
import time

HERE = os.path.dirname(os.path.abspath(__file__))


def _die_with_parent():
    # Linux: deliver SIGTERM to us if the spawning process goes away.
    try:
        libc = ctypes.CDLL("libc.so.6", use_errno=True)
        PR_SET_PDEATHSIG = 1
        libc.prctl(PR_SET_PDEATHSIG, signal.SIGTERM)
    except Exception:
        pass


def main():
    from vwayland import spawn

    _die_with_parent()
    ready_dir = tempfile.mkdtemp(prefix="pcu-vw-ready-")
    ready_path = os.path.join(ready_dir, "READY")
    event_log = os.path.join(ready_dir, "events.log")
    comp = spawn(width=1280, height=720, headless=True)

    def _shutdown(*_):
        try:
            comp.kill()
        finally:
            sys.exit(0)

    signal.signal(signal.SIGTERM, _shutdown)
    signal.signal(signal.SIGINT, _shutdown)

    try:
        comp.launch(["python3", os.path.join(HERE, "client.py"), ready_path, event_log])
        for _ in range(100):  # 50s max
            if os.path.exists(ready_path):
                break
            time.sleep(0.5)
        else:
            print("client never became ready", file=sys.stderr)
            sys.exit(2)
        info = comp.info()
        from vwayland import runtime_root
        print(json.dumps({
            "sock": str(runtime_root() / info["id"] / "ipc.sock"),
            "width": info["width"],
            "height": info["height"],
            "event_log": event_log,
        }), flush=True)
        # hold the compositor alive until the test is done
        try:
            for line in sys.stdin:
                if line.strip() == "quit":
                    break
        except (EOFError, KeyboardInterrupt):
            pass
    finally:
        comp.kill()


if __name__ == "__main__":
    main()
