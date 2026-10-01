"""Check pane identity and rendered bars against a real, isolated Zellij."""

import errno
import fcntl
import json
import os
import pty
import shutil
import signal
import socket
import struct
import subprocess
import tempfile
import termios
import threading
from collections.abc import Iterator
from dataclasses import dataclass
from pathlib import Path
from typing import Any

import pyte
import pytest

from .conftest import poll_until


class TerminalScreen(pyte.Screen):
    # Zellij sends private device queries which pyte does not implement.
    def report_device_status(self, mode: int, **kwargs: Any) -> None:
        pass

    def report_device_attributes(self, mode: int = 0, **kwargs: Any) -> None:
        pass


@dataclass
class LiveZellij:
    root: Path
    executable: Path
    zellij: str
    session: str
    env: dict[str, str]
    screen: TerminalScreen

    def action(
        self, *args: str, check: bool = True
    ) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            [self.zellij, "--session", self.session, "action", *args],
            cwd=self.root,
            env=self.env,
            capture_output=True,
            text=True,
            timeout=10,
            check=check,
        )

    def panes(self) -> list[dict[str, Any]]:
        return json.loads(self.action("list-panes", "--json", "--all").stdout)

    def sidebar(self, *args: str) -> None:
        subprocess.run(
            [str(self.executable), "sidebar", *args],
            cwd=self.root,
            env=self.env
            | {
                "ZELLIJ": "0",
                "ZELLIJ_SESSION_NAME": self.session,
                "ZELLIJ_PANE_ID": "0",
            },
            capture_output=True,
            text=True,
            timeout=20,
            check=True,
        )


@pytest.fixture
def live_zellij(workmux_exe_path: Path) -> Iterator[LiveZellij]:
    zellij = shutil.which("zellij")
    if zellij is None:
        pytest.skip("Zellij is not installed")
    help_result = subprocess.run(
        [zellij, "action", "override-layout", "--help"],
        capture_output=True,
        text=True,
        timeout=10,
        check=False,
    )
    if help_result.returncode != 0:
        pytest.skip("Zellij does not support override-layout")

    with tempfile.TemporaryDirectory(prefix="wm-zellij-", dir="/tmp") as directory:
        root = Path(directory)
        session = root.name
        env = {
            key: value
            for key, value in os.environ.items()
            if not key.startswith(("ZELLIJ", "TMUX", "WORKMUX"))
        }
        env.update(
            HOME=directory,
            XDG_CONFIG_HOME=str(root / "config"),
            XDG_CACHE_HOME=str(root / "cache"),
            XDG_DATA_HOME=str(root / "data"),
            XDG_STATE_HOME=str(root / "state"),
            XDG_RUNTIME_DIR=str(root / "runtime"),
            TMPDIR=directory,
            ZELLIJ_LOG_DIR=str(root / "logs"),
            TERM="xterm-256color",
            SHELL="/bin/bash",
            WORKMUX_BACKEND="zellij",
            WORKMUX_TEST="1",
        )
        (root / "runtime").mkdir(mode=0o700)
        config = root / "config" / "zellij" / "config.kdl"
        config.parent.mkdir(parents=True)
        config.write_text(
            "show_startup_tips false\nshow_release_notes false\n"
            'session_serialization false\ndefault_shell "/bin/bash"\n'
        )
        (root / ".workmux.yaml").write_text("sidebar:\n  width: 24\n  height: 4\n")
        subprocess.run(["git", "init", "-q", directory], env=env, check=True)

        key = 0xCBF29CE484222325
        for byte in session.encode():
            key = ((key ^ byte) * 0x100000001B3) & 0xFFFFFFFFFFFFFFFF
        # Keep the renderer connected without a daemon changing the layout during assertions.
        with socket.socket(socket.AF_UNIX) as listener:
            listener.bind(str(root / f"workmux-sidebar-{key:016x}.sock"))
            listener.listen(32)
            pid, master = pty.fork()
            if pid == 0:
                os.chdir(root)
                os.execve(zellij, [zellij, "--session", session], env)
            fcntl.ioctl(master, termios.TIOCSWINSZ, struct.pack("HHHH", 45, 160, 0, 0))
            screen = TerminalScreen(160, 45)
            stream = pyte.ByteStream(screen)
            output = bytearray()

            def drain() -> None:
                while True:
                    try:
                        chunk = os.read(master, 65536)
                    except OSError as error:
                        if error.errno == errno.EIO:
                            return
                        raise
                    if not chunk:
                        return
                    output.extend(chunk)
                    stream.feed(chunk)
                    if b"\x1b]11;?" in chunk:
                        os.write(master, b"\x1b]11;rgb:0000/0000/0000\x1b\\")

            reader = threading.Thread(target=drain, daemon=True)
            reader.start()
            harness = LiveZellij(root, workmux_exe_path, zellij, session, env, screen)
            try:

                def ready() -> bool:
                    result = harness.action(
                        "list-panes", "--json", "--all", check=False
                    )
                    return result.returncode == 0 and bool(json.loads(result.stdout))

                assert poll_until(ready, timeout=15), repr(output[-2000:])
                # CLI actions use the last client to send a keystroke.
                os.write(master, b"\r")
                assert poll_until(
                    lambda: (
                        "Zellij" in screen.display[0] and "Ctrl" in screen.display[-1]
                    ),
                    timeout=10,
                ), screen.display
                yield harness
            finally:
                subprocess.run(
                    [zellij, "kill-session", session],
                    env=env,
                    capture_output=True,
                    timeout=10,
                    check=False,
                )
                if not poll_until(lambda: os.waitpid(pid, os.WNOHANG)[0] != 0):
                    os.kill(pid, signal.SIGKILL)
                    os.waitpid(pid, 0)
                reader.join(timeout=2)
                os.close(master)


def ui_geometry(panes: list[dict[str, Any]]) -> dict[int, tuple[int, ...]]:
    return {
        pane["id"]: tuple(
            pane[key] for key in ("pane_x", "pane_y", "pane_columns", "pane_rows")
        )
        for pane in panes
        if pane["is_plugin"] and not pane["is_suppressed"]
    }


@pytest.mark.parametrize("position", ["left", "top"])
@pytest.mark.parametrize(
    "command", [(), ("--", "sleep", "300")], ids=["shell", "command"]
)
def test_sidebar_retains_panes_and_rendered_bars(
    live_zellij: LiveZellij, position: str, command: tuple[str, ...]
):
    live_zellij.action("new-pane", "--direction", "right", "--name", "dev1", *command)
    before = live_zellij.panes()
    original_ids = {pane["id"] for pane in before if not pane["is_plugin"]}
    assert len(original_ids) == 2
    original_bars = ui_geometry(before)
    top_prefix = live_zellij.screen.display[0][:20]
    bottom_prefix = live_zellij.screen.display[-1][:20]

    for _ in range(2):
        live_zellij.sidebar("on", "--position", position)
        after = live_zellij.panes()
        terminals = [pane for pane in after if not pane["is_plugin"]]
        sidebars = [pane for pane in terminals if pane["title"] == "workmux-sidebar"]
        assert len(sidebars) == 1, terminals
        assert {pane["id"] for pane in terminals} == original_ids | {
            sidebars[0]["id"]
        }, terminals
        assert ui_geometry(after) == original_bars
        content = sorted(
            (pane for pane in terminals if pane["id"] in original_ids),
            key=lambda pane: pane["pane_x"],
        )
        assert content[0]["pane_y"] == content[1]["pane_y"]
        assert abs(content[0]["pane_columns"] - content[1]["pane_columns"]) <= 1
        assert poll_until(
            lambda: (
                live_zellij.screen.display[0].startswith(top_prefix)
                and live_zellij.screen.display[-1].startswith(bottom_prefix)
            )
        ), live_zellij.screen.display

    live_zellij.sidebar("off")
    after = live_zellij.panes()
    assert {pane["id"] for pane in after if not pane["is_plugin"]} == original_ids
    assert ui_geometry(after) == original_bars


def test_sidebar_retains_nested_content(live_zellij: LiveZellij):
    live_zellij.action("new-pane", "--direction", "right")
    live_zellij.action("new-pane", "--direction", "down")
    before = [pane for pane in live_zellij.panes() if not pane["is_plugin"]]
    assert len(before) == 3
    original_ids = {pane["id"] for pane in before}
    original_order = sorted(before, key=lambda pane: (pane["pane_x"], pane["pane_y"]))
    live_zellij.sidebar("on")
    content = {
        pane["id"]: pane for pane in live_zellij.panes() if not pane["is_plugin"]
    }
    assert len(content) == 4
    assert original_ids <= content.keys()
    left, top, bottom = [content[pane["id"]] for pane in original_order]
    assert top["pane_x"] == bottom["pane_x"] == left["pane_x"] + left["pane_columns"]
    assert top["pane_y"] == left["pane_y"]
    assert bottom["pane_y"] == top["pane_y"] + top["pane_rows"]
    assert top["pane_rows"] + bottom["pane_rows"] == left["pane_rows"]
    assert abs(left["pane_columns"] - top["pane_columns"]) <= 1


def test_damaged_bars_can_be_reloaded_without_closing_panes(live_zellij: LiveZellij):
    top_prefix = live_zellij.screen.display[0][:20]
    bottom_prefix = live_zellij.screen.display[-1][:20]
    # Replaying dump-layout reproduces the old alias loss without restarting user commands.
    layout = live_zellij.action("dump-layout").stdout
    live_zellij.action(
        "override-layout",
        "--apply-only-to-active-tab",
        "--retain-existing-terminal-panes",
        "--layout-string",
        layout,
    )
    bars = ui_geometry(live_zellij.panes())
    live_zellij.sidebar("on")
    pane_ids = {(pane["id"], pane["is_plugin"]) for pane in live_zellij.panes()}
    live_zellij.action("start-or-reload-plugin", "zellij:tab-bar")
    live_zellij.action("start-or-reload-plugin", "zellij:status-bar")
    after = live_zellij.panes()
    assert {(pane["id"], pane["is_plugin"]) for pane in after} == pane_ids
    assert ui_geometry(after) == bars
    assert poll_until(
        lambda: (
            live_zellij.screen.display[0].startswith(top_prefix)
            and live_zellij.screen.display[-1].startswith(bottom_prefix)
        )
    ), live_zellij.screen.display
