"""Real-terminal smoke checks for hizuke duplicate prompts (Unix only)."""
import errno
import json
import os
from pathlib import Path
import pty
import select
import signal
import subprocess
import sys
import tempfile
import time


BIN = Path(sys.argv[1]).resolve()


def interactive(args, answers, working_directory=None):
    master, slave = pty.openpty()
    process = subprocess.Popen(
        [str(BIN), *map(str, args)], stdin=slave, stdout=slave, stderr=slave,
        close_fds=True, cwd=working_directory,
    )
    os.close(slave)
    transcript = bytearray()
    deadline = time.monotonic() + 15
    answered = 0
    try:
        while time.monotonic() < deadline:
            ready, _, _ = select.select([master], [], [], 0.1)
            if ready:
                try:
                    chunk = os.read(master, 65536)
                except OSError as error:
                    if error.errno == errno.EIO:
                        break
                    raise
                if not chunk:
                    break
                transcript.extend(chunk)
                while answered < len(answers):
                    prompt, response = answers[answered]
                    if prompt not in transcript:
                        break
                    if isinstance(response, int):
                        os.kill(process.pid, response)
                    else:
                        if callable(response):
                            response = response()
                        os.write(master, response)
                    answered += 1
            if process.poll() is not None and not ready:
                break
        else:
            raise AssertionError(f"interactive command timed out: {transcript!r}")
        code = process.wait(timeout=2)
        assert answered == len(answers), transcript.decode(errors="replace")
        return code, transcript.decode(errors="replace")
    finally:
        if process.poll() is None:
            process.kill()
            process.wait()
        os.close(master)


def fixtures(root):
    for name in ("a.jpg", "b.jpg"):
        path = root / name
        path.write_bytes(b"identical image payload")
        os.utime(path, (1704164645, 1704164645))


def snapshot(root):
    return {
        str(path.relative_to(root)): ("dir" if path.is_dir() else path.read_bytes())
        for path in root.rglob("*")
    }


def keeper_selection_and_undo():
    with tempfile.TemporaryDirectory(prefix="hizuke-pty-keep-") as temporary:
        root = Path(temporary)
        fixtures(root)
        inode_a = (root / "a.jpg").stat().st_ino
        inode_b = (root / "b.jpg").stat().st_ino
        code, transcript = interactive(
            [root, "--timezone", "utc"],
            [(b"Keep [1-2]", b"2\n"), (b"Apply these changes?", b"y\n")],
        )
        assert code == 0, transcript
        final = root / "2024-01-02 03.04.05.jpg"
        assert final.read_bytes() == b"identical image payload"
        assert final.stat().st_ino == inode_b, "selected keeper must be the original b.jpg"
        assert not (root / "a.jpg").exists()
        assert not (root / "b.jpg").exists()
        archived = list(root.glob(".hizuke/transactions/*/duplicates/*"))
        assert len(archived) == 1, archived
        assert archived[0].stat().st_ino == inode_a
        assert archived[0].read_bytes() == b"identical image payload"
        undo = subprocess.run(
            [str(BIN), "undo", str(root), "--yes"],
            stdin=subprocess.DEVNULL, capture_output=True, text=True,
        )
        assert undo.returncode == 0, undo.stderr
        assert (root / "a.jpg").stat().st_ino == inode_a
        assert (root / "b.jpg").stat().st_ino == inode_b
        assert not final.exists()
        print("PASS: PTY bare-directory keeper 2 -> preserved b inode, a quarantined, undo restores both")


def quit_without_changes():
    with tempfile.TemporaryDirectory(prefix="hizuke-pty-quit-") as temporary:
        root = Path(temporary)
        fixtures(root)
        before = snapshot(root)
        code, transcript = interactive(
            ["apply", root, "--timezone", "utc", "--yes"],
            [(b"Keep [1-2]", b"q\n")],
        )
        assert code != 0, transcript
        assert "cancelled" in transcript
        assert snapshot(root) == before
        assert not (root / ".hizuke").exists()
        print("PASS: PTY q cancels before creating state or changing any image")


def reject_final_confirmation_without_changes():
    with tempfile.TemporaryDirectory(prefix="hizuke-pty-no-") as temporary:
        root = Path(temporary)
        fixtures(root)
        before = snapshot(root)
        code, transcript = interactive(
            ["apply", root, "--timezone", "utc"],
            [(b"Keep [1-2]", b"2\n"), (b"Apply these changes?", b"n\n")],
        )
        assert code != 0, transcript
        assert "cancelled" in transcript
        assert snapshot(root) == before
        print("PASS: PTY keeper selection then final n leaves all files unchanged")


def bare_current_directory_interactive():
    with tempfile.TemporaryDirectory(prefix="hizuke-pty-default-") as temporary:
        root = Path(temporary)
        fixture = root / "camera.jpg"
        fixture.write_bytes(b"unique photo")
        original_inode = fixture.stat().st_ino
        code, transcript = interactive(
            [], [(b"Apply these changes?", b"y\n")], working_directory=root,
        )
        assert code == 0, transcript
        photos = list(root.glob("*.jpg"))
        assert len(photos) == 1
        assert photos[0].name != "camera.jpg"
        assert photos[0].stat().st_ino == original_inode
        print("PASS: PTY bare no-argument command reviews and applies current directory")


def json_stays_readonly_in_a_terminal():
    with tempfile.TemporaryDirectory(prefix="hizuke-pty-json-") as temporary:
        root = Path(temporary)
        fixtures(root)
        before = snapshot(root)
        code, transcript = interactive([root, "--json", "--color", "always"], [])
        assert code == 0, transcript
        plan = json.loads(transcript)
        assert plan["schema_version"] == 1
        assert plan["pending_groups"] == 1
        assert snapshot(root) == before
        assert "\x1b" not in transcript
        print("PASS: PTY bare --json previews without prompts, changes, or terminal escapes")


def sigint_at_duplicate_prompt_cancels_without_writes():
    with tempfile.TemporaryDirectory(prefix="hizuke-pty-sigint-") as temporary:
        root = Path(temporary)
        fixtures(root)
        before = snapshot(root)
        code, transcript = interactive(
            [root], [(b"Keep [1-2]", signal.SIGINT)],
        )
        assert code == 130, transcript
        assert "cancelled" in transcript
        assert snapshot(root) == before
        print("PASS: PTY SIGINT at duplicate prompt exits 130 without changing files")


def quiet_keeps_questions_visible():
    with tempfile.TemporaryDirectory(prefix="hizuke-pty-quiet-") as temporary:
        root = Path(temporary)
        fixtures(root)
        before = snapshot(root)
        code, transcript = interactive(
            [root, "--quiet"],
            [(b"Keep [1-2]", b"a\n"), (b"Apply these changes?", b"n\n")],
        )
        assert code == 130, transcript
        assert "Directory:" not in transcript
        assert snapshot(root) == before
        print("PASS: PTY --quiet suppresses chatter while retaining duplicate/approval questions")


def duplicate_changed_while_user_considers_confirmation():
    with tempfile.TemporaryDirectory(prefix="hizuke-pty-stale-") as temporary:
        root = Path(temporary)
        fixtures(root)

        def change_keeper_then_approve():
            (root / "b.jpg").write_bytes(b"photo edited by another application")
            return b"y\n"

        code, transcript = interactive(
            [root],
            [(b"Keep [1-2]", b"2\n"), (b"Apply these changes?", change_keeper_then_approve)],
        )
        assert code == 1, transcript
        assert (root / "a.jpg").read_bytes() == b"identical image payload"
        assert (root / "b.jpg").read_bytes() == b"photo edited by another application"
        assert not (root / ".hizuke").exists()
        print("PASS: PTY duplicate edited during confirmation is detected before any rename/archive")


def destination_appearing_after_preview_is_not_overwritten():
    with tempfile.TemporaryDirectory(prefix="hizuke-pty-destination-") as temporary:
        root = Path(temporary)
        original = root / "camera.jpg"
        original.write_bytes(b"original image")
        os.utime(original, (1704164645, 1704164645))
        target = root / "2024-01-02 03.04.05.jpg"

        def add_destination_then_approve():
            target.write_bytes(b"new file from another application")
            return b"y\n"

        code, transcript = interactive(
            [root, "--timezone", "utc"],
            [(b"Apply these changes?", add_destination_then_approve)],
        )
        assert code == 1, transcript
        assert original.read_bytes() == b"original image"
        assert target.read_bytes() == b"new file from another application"
        assert not list(root.glob(".hizuke/transactions/*/staging/*"))
        print("PASS: PTY destination created after planning is preserved without moving source")


def redirected_stdout_defaults_to_preview_even_with_terminal_input():
    with tempfile.TemporaryDirectory(prefix="hizuke-pty-redirect-") as temporary:
        root = Path(temporary)
        fixtures(root)
        before = snapshot(root)
        master, slave = pty.openpty()
        process = subprocess.Popen(
            [str(BIN), str(root)], stdin=slave, stdout=subprocess.PIPE, stderr=slave,
        )
        os.close(slave)
        try:
            stdout, _ = process.communicate(timeout=5)
            assert process.returncode == 0, stdout
            assert b"ASK" in stdout and b"Keep [" not in stdout
            assert b"\x1b" not in stdout
            assert snapshot(root) == before
            print("PASS: redirected stdout defaults to read-only preview despite terminal stdin/stderr")
        finally:
            if process.poll() is None:
                process.kill()
                process.wait()
            os.close(master)


keeper_selection_and_undo()
quit_without_changes()
reject_final_confirmation_without_changes()
bare_current_directory_interactive()
json_stays_readonly_in_a_terminal()
sigint_at_duplicate_prompt_cancels_without_writes()
quiet_keeps_questions_visible()
duplicate_changed_while_user_considers_confirmation()
destination_appearing_after_preview_is_not_overwritten()
redirected_stdout_defaults_to_preview_even_with_terminal_input()
