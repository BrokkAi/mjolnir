"""Serialize pilot attempt changes with readers of the run directory."""
from contextlib import contextmanager
import fcntl
import os
import sys


@contextmanager
def pilot_lock(root):
    with (root / "pilot.lock").open("a+b") as lock:
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            try:
                lock.seek(0)
                pid = lock.readline(32).strip()
            except OSError:
                pid = b""
            holder = f" (PID {pid.decode('ascii')})" if pid.isdigit() else ""
            print(f"waiting for the pilot run on {root}{holder} to release pilot.lock "
                  "(pilot runs exit at quota holds or completion)", file=sys.stderr, flush=True)
            fcntl.flock(lock, fcntl.LOCK_EX)
        try:
            lock.truncate(0)
            lock.write(f"{os.getpid()}\n".encode("ascii"))
            lock.flush()
            yield
        finally:
            lock.truncate(0)
