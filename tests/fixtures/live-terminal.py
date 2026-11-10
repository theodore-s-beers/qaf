"""PTY smoke test: pass the qaf unit-test executable built by cargo test --no-run."""

import errno
import fcntl
import os
import pty
import select
import signal
import struct
import subprocess
import sys
import termios
import time

# Acquire the controlling terminal in a fresh interpreter, then replace it with
# the test process. No Python callback runs between fork and exec.
if sys.argv[1] == "--child":
    fcntl.ioctl(0, termios.TIOCSCTTY, 0)
    os.execvp(sys.argv[2], sys.argv[2:])

master, slave = pty.openpty()
fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 80, 0, 0))
original = termios.tcgetattr(slave)


process = subprocess.Popen(
    [
        sys.executable,
        __file__,
        "--child",
        sys.argv[1],
        "live::tests::terminal_probe",
        "--exact",
        "--ignored",
        "--nocapture",
    ],
    stdin=slave,
    stdout=slave,
    stderr=slave,
    start_new_session=True,
    env={**os.environ, "TERM": "xterm-256color", "NO_COLOR": ""},
)
transcript = bytearray()


def until(marker):
    deadline = time.monotonic() + 10
    while marker not in transcript:
        assert time.monotonic() < deadline, (marker, bytes(transcript)[-3000:])
        if select.select([master], [], [], 0.1)[0]:
            try:
                transcript.extend(os.read(master, 65536))
            except OSError as error:
                if error.errno != errno.EIO:
                    raise
                break
        assert process.poll() is None or marker in transcript, bytes(transcript)[-3000:]


try:
    until(b"PROBE READY")
    assert b"\x1b[2mSTATUS STYLE PROBE\x1b[22m" in transcript, (
        "live status lost dim styling"
    )
    os.write(master, b"first\r")
    until(b"WORKING")
    os.write(master, b"follow up\rsaved draft")
    until(b"APPROVAL READY")
    # Exercise resizing while the approval owns input.
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 30, 100, 0, 0))
    os.kill(process.pid, signal.SIGWINCH)
    os.write(master, b"deny\r")
    until(b"DRAFT RESTORED")
    os.write(master, b"\r")
    until(b"CANCEL READY")
    os.write(master, b"\x03")
    until(b"TERMINAL RESTORED")
    assert termios.tcgetattr(slave) == original, "terminal mode was not restored"
    os.write(master, b"finish\n")
    deadline = time.monotonic() + 10
    while process.poll() is None:
        assert time.monotonic() < deadline, bytes(transcript)[-3000:]
        if select.select([master], [], [], 0.01)[0]:
            transcript.extend(os.read(master, 65536))
    assert process.returncode == 0, bytes(transcript)[-3000:]
    print(
        "PTY smoke test passed: queue, draft restoration, approval, resize, Ctrl-C, terminal cleanup"
    )
finally:
    if process.poll() is None:
        process.kill()
        # Avoid an unbounded wait if the platform denies process monitoring.
        for _ in range(100):
            if process.poll() is not None:
                break
            time.sleep(0.01)
    os.close(master)
    os.close(slave)
