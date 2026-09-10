"""Build and run the real TUI and web protocol adapters against each other.

Usage: python scripts/test_device_interop.py [path/to/furumusic]
No user databases, accounts, audio outputs, or external servers are used.
"""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile


def test_binary(repo):
    result = subprocess.run(
        ["cargo", "test", "--locked", "--no-run", "--message-format=json"],
        cwd=repo, text=True, encoding="utf-8", stdout=subprocess.PIPE,
    )
    result.check_returncode()
    binaries = []
    for line in result.stdout.splitlines():
        event = json.loads(line)
        if event.get("reason") == "compiler-artifact" and event.get("profile", {}).get("test") and event.get("executable"):
            binaries.append(event["executable"])
    if len(binaries) != 1:
        raise RuntimeError(f"Expected one player test binary in {repo}, found {binaries}")
    return binaries[0]


def main():
    tui = Path(__file__).resolve().parents[1]
    web = Path(sys.argv[1]).resolve() if len(sys.argv) > 1 else tui.parent / "furumusic"
    tui_bin, web_bin = test_binary(tui), test_binary(web)
    with tempfile.TemporaryDirectory(prefix="furumi-interop-") as directory:
        env = dict(os.environ, FURUMI_INTEROP_DIR=directory)
        processes = []
        try:
            for binary, test in [(web_bin, "federation::devices::interop_tests::localhost_tui_peer"),
                                 (tui_bin, "devices::interop_tests::localhost_web_peer")]:
                listing = subprocess.check_output([binary, "--list"], text=True, encoding="utf-8")
                if f"{test}: test" not in listing:
                    raise RuntimeError(f"Required test {test} is missing from {binary}")
                processes.append(subprocess.Popen(
                    [binary, "--ignored", "--exact", test, "--nocapture"], env=env,
                    stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                    text=True, encoding="utf-8",
                    creationflags=getattr(subprocess, "CREATE_NO_WINDOW", 0),
                ))
            codes = []
            for process in processes:
                output, _ = process.communicate(timeout=45)
                print(output, end="")
                codes.append(process.returncode)
            if any(codes):
                raise RuntimeError(f"Cross-player test failed: exit codes {codes}")
        finally:
            for process in processes:
                if process.poll() is None:
                    process.kill()
                process.wait()
    print("PASS: TUI <-> WEB state, bidirectional handoff, pause/seek, duplicate fencing")


if __name__ == "__main__":
    main()
