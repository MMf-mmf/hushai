"""Advisory lockfile for the ONE physical phone rig.

Tier-2 loopback runs and the voice-assistant matrix share a single USB phone (and the one
Mac screen/speakers), so concurrent runs corrupt each other. Both scripts take this lock
first and fail fast with a clear message when it's held. Stale locks (dead PID) are reclaimed.

Usage:
    from phonelock import phone_lock
    with phone_lock():
        ...drive the phone...
"""
import contextlib
import os

LOCK_PATH = os.path.join(os.path.dirname(os.path.abspath(__file__)), "captures", ".phone.lock")


def _pid_alive(pid: int) -> bool:
    try:
        os.kill(pid, 0)
        return True
    except (ProcessLookupError, PermissionError):
        # PermissionError = alive but not ours; treat as held.
        return True
    except OSError:
        return False


@contextlib.contextmanager
def phone_lock():
    os.makedirs(os.path.dirname(LOCK_PATH), exist_ok=True)
    try:
        fd = os.open(LOCK_PATH, os.O_CREAT | os.O_EXCL | os.O_WRONLY)
    except FileExistsError:
        # Held — or stale (owner died without cleanup).
        try:
            holder = int(open(LOCK_PATH).read().strip() or "0")
        except (ValueError, OSError):
            holder = 0
        if holder and _pid_alive(holder):
            raise SystemExit(
                f"phone rig is in use by PID {holder} (lock {LOCK_PATH}) — "
                "wait for that run to finish, or remove the lock if it's stale."
            )
        os.unlink(LOCK_PATH)  # stale: dead holder
        fd = os.open(LOCK_PATH, os.O_CREAT | os.O_EXCL | os.O_WRONLY)
    try:
        os.write(fd, str(os.getpid()).encode())
        os.close(fd)
        yield
    finally:
        try:
            os.unlink(LOCK_PATH)
        except OSError:
            pass
