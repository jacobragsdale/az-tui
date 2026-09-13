#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = ["pyte"]
# ///
"""Drives az-tui under a pty against scripts/fake-kubectl and prints what it
painted, so a change can be seen end to end without a cluster.

    scripts/walk.py                 # the release binary, a scratch config
    scripts/walk.py --keys '2 ] / orders'

Every frame is read through pyte, so what is asserted is the screen a person
would see. Exits 1 when an expectation is not met.
"""
import argparse
import fcntl
import os
import pty
import select
import signal
import struct
import sys
import tempfile
import termios
import time

import pyte

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
COLUMNS, LINES = 140, 40

CONFIG = """
[[clusters]]
name = "qa"
context = "aks-qa"
namespaces = ["dev", "qa", "uat"]

[[clusters]]
name = "prod"
context = "aks-prod"
namespaces = ["prod"]
"""


class Walk:
    def __init__(self, binary, scratch, env):
        self.screen = pyte.Screen(COLUMNS, LINES)
        self.stream = pyte.ByteStream(self.screen)
        self.pid, self.fd = pty.fork()
        if self.pid == 0:
            os.execve(binary, [binary], env)
        fcntl.ioctl(self.fd, termios.TIOCSWINSZ, struct.pack("HHHH", LINES, COLUMNS, 0, 0))
        self.scratch = scratch
        # Set once the pty has closed: the binary was not there, or it died
        # mid-walk. Every expectation after that fails at once rather than
        # spinning out its timeout.
        self.dead = False

    def pump(self, seconds):
        deadline = time.time() + seconds
        while not self.dead and time.time() < deadline:
            ready, _, _ = select.select([self.fd], [], [], 0.05)
            if not ready:
                continue
            try:
                data = os.read(self.fd, 65536)
            except OSError:
                data = b""
            if not data:
                self.dead = True
                return
            # ratatui asks where the cursor is when it clears the screen for
            # the repaint after `kubectl exec`; a pty with nobody on the other
            # end would leave it waiting.
            if b"\x1b[6n" in data:
                os.write(self.fd, b"\x1b[1;1R")
            self.stream.feed(data)

    def text(self):
        return "\n".join(self.screen.display)

    def send(self, keys):
        os.write(self.fd, keys.encode())
        # A key sent on the heels of an Esc would read as Alt-key.
        if keys.endswith("\x1b"):
            self.pump(0.3)

    def expect(self, needle, seconds=5.0):
        deadline = time.time() + seconds
        while True:
            if needle in self.text():
                return True
            if self.dead or time.time() >= deadline:
                break
            self.pump(0.05)
        print(f"--- expected {needle!r}, screen was:\n{self.text()}", file=sys.stderr)
        return False

    def quit(self):
        # Ctrl-C rather than q: q is swallowed by whatever a failed walk
        # left open, and a walk that hangs is worse than one that fails.
        self.send("\x03")
        deadline = time.time() + 5
        while time.time() < deadline:
            self.pump(0.1)
            try:
                pid, _ = os.waitpid(self.pid, os.WNOHANG)
            except ChildProcessError:
                return
            if pid:
                return
        os.kill(self.pid, signal.SIGKILL)
        try:
            os.waitpid(self.pid, 0)
        except ChildProcessError:
            pass


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", default=os.path.join(ROOT, "target", "release", "az-tui"))
    parser.add_argument("--keys", default="", help="keys to press after the first frame, space separated")
    parser.add_argument("--show", action="store_true", help="print the final screen")
    options = parser.parse_args()

    scratch = tempfile.mkdtemp(prefix="az-tui-walk-")
    os.makedirs(os.path.join(scratch, "az-tui"))
    # The app runs `kubectl`; the fake answers to that name from a scratch bin.
    os.makedirs(os.path.join(scratch, "bin"))
    os.symlink(os.path.join(ROOT, "scripts", "fake-kubectl"), os.path.join(scratch, "bin", "kubectl"))
    with open(os.path.join(scratch, "az-tui", "config.toml"), "w") as f:
        f.write(CONFIG)
    env = dict(os.environ)
    env.update({
        "PATH": os.path.join(scratch, "bin") + os.pathsep + env.get("PATH", ""),
        "XDG_CONFIG_HOME": scratch,
        "XDG_DATA_HOME": scratch,
        "FAKE_KUBECTL_LOG": scratch,
        "TERM": "xterm-256color",
        "COLUMNS": str(COLUMNS),
        "LINES": str(LINES),
    })
    env.pop("NO_COLOR", None)

    walk = Walk(options.binary, scratch, env)
    ok = True
    ok &= walk.expect("1 qa/dev")
    ok &= walk.expect("orders-worker-5c4d3e-q8zt")
    ok &= walk.expect("1 qa/dev ✗ 1")
    ok &= walk.expect("3 qa/uat ✗ 1")
    ok &= walk.expect("5 pods")
    for key in options.keys.split():
        walk.send({"Enter": "\r", "Esc": "\x1b", "Tab": "\t", "Space": " "}.get(key, key))
        walk.pump(0.4)
    if not options.keys:
        walk.send("4")
        ok &= walk.expect("orders-api-7d9f5b-prd02")
        ok &= walk.expect("Terminating")
        # ] from prod walks Secrets and Registries, then wraps to qa/dev.
        walk.send("]")
        ok &= walk.expect("Env ▾")
        walk.send("]")
        ok &= walk.expect("Repository")
        walk.send("]")
        ok &= walk.expect("orders-worker-5c4d3e-q8zt")
        walk.send("/worker\r")
        ok &= walk.expect("1/5 · Name")
        ok &= walk.expect("CrashLoopBackOff  ↻17")
        walk.send("\x1b")
        walk.send("3")
        ok &= walk.expect("ImagePullBackOff")
        ok &= walk.expect("2 pods")
        walk.send("?")
        ok &= walk.expect("refresh now")
        walk.send("\x1b")
        # The log: Enter opens it on the pod under the cursor and it streams.
        walk.send("1")
        walk.send("/worker\r")
        walk.send("\r")
        ok &= walk.expect("Log · following · orders-worker-5c4d3e-q8zt · api")
        ok &= walk.expect("api line 1")
        ok &= walk.expect("api line 3", seconds=4)
        walk.send("d")
        ok &= walk.expect("Describe · orders-worker-5c4d3e-q8zt")
        ok &= walk.expect("Successfully pulled image")
        walk.send("v")
        ok &= walk.expect("YAML · orders-worker")
        walk.send("\x1b")
        walk.send("\x1b")
        walk.send("\x1b")
        # Restart: asks first, x again sends the delete.
        walk.send("/worker\r")
        ok &= walk.expect("1/5 · Name")
        walk.send("x")
        ok &= walk.expect("Restart orders-worker-5c4d3e-q8zt?")
        ok &= walk.expect("Deployment orders-worker replaces it")
        walk.send("x")
        ok &= walk.expect("Deleted orders-worker-5c4d3e-q8zt; Deployment orders-worker is putting a new one up")
        # Scale: the owner's count fills the box, Enter sends it.
        walk.send("=")
        ok &= walk.expect("now 3 desired · 3 ready")
        walk.send("\x7f4\r")
        ok &= walk.expect("deployment/orders-worker scale sent")
        # b hands the terminal to kubectl exec; the fake shell exits at once
        # and the TUI repaints.
        walk.send("b")
        ok &= walk.expect("1 qa/dev", seconds=6)
        ok &= walk.expect("orders-worker-5c4d3e-q8zt", seconds=6)
        # e on the pod: its events, narrowed to it; Enter goes back to it.
        walk.send("e")
        ok &= walk.expect("Events ▾")
        ok &= walk.expect("BackOff")
        ok &= walk.expect("[Pod] [Describe] [YAML]")
        walk.send("\r")
        ok &= walk.expect("Pods ▾")
        # m: the configmaps; Enter shows a key's value; j walks the keys.
        walk.send("m")
        ok &= walk.expect("ConfigMaps ▾")
        ok &= walk.expect("orders-config")
        walk.send("/orders\r")
        walk.send("\r")
        ok &= walk.expect("Value · orders-config · DB_HOST")
        ok &= walk.expect("orders-db.dev.svc")
        walk.send("j")
        ok &= walk.expect("Value · orders-config · FEATURES")
        ok &= walk.expect("a=1")
        walk.send("\x1b")
        walk.send("\x1b")
        # s: the secrets; v reveals one key for sixty seconds; prod refuses.
        walk.send("s")
        ok &= walk.expect("Secrets ▾")
        ok &= walk.expect("orders-tls")
        walk.send("/db\r")
        ok &= walk.expect("› password  7 bytes")
        walk.send("v")
        ok &= walk.expect("clears in")
        ok &= walk.expect("hunter2")
        # Each tab keeps its own kind: prod opens on its pods; s asks for its
        # secrets, which it refuses.
        walk.send("4")
        ok &= walk.expect("Pods ▾")
        walk.send("s")
        ok &= walk.expect("prod/prod secrets: Error from server (Forbidden)")
        walk.send("p")
        walk.send("1")
        walk.send("p")
    if options.show:
        print(walk.text())
    walk.quit()

    log = os.path.join(scratch, "calls.log")
    if os.path.exists(log):
        with open(log) as f:
            calls = f.read().splitlines()
    else:
        print("--- kubectl was never called", file=sys.stderr)
        calls = []
        ok = False
    print(f"{len(calls)} kubectl calls; first: {calls[0] if calls else '-'}")
    wanted = [
        "--context aks-qa --request-timeout=10s get pods -o json -n dev",
        "--context aks-qa --request-timeout=10s delete pod orders-worker-5c4d3e-q8zt -n dev --wait=false",
        "--context aks-qa --request-timeout=10s get deployment/orders-worker -n dev -o json",
        "--context aks-qa --request-timeout=10s scale deployment/orders-worker -n dev --replicas=4",
        "--context aks-qa exec -it -n dev orders-worker-5c4d3e-q8zt -- sh -c command -v bash >/dev/null 2>&1 && exec bash || exec sh",
        "--context aks-qa --request-timeout=10s get events -o json -n dev",
        "--context aks-qa --request-timeout=10s get configmaps -o json -n dev",
        "--context aks-qa --request-timeout=10s get secrets -o json -n dev",
        "--context aks-qa --request-timeout=10s get secret orders-db -n dev -o json",
        "--context aks-prod --request-timeout=10s get secrets -o json -n prod",
    ] if not options.keys else []
    for line in wanted:
        if line not in calls:
            print(f"--- expected a call {line!r} in:\n" + "\n".join(calls), file=sys.stderr)
            ok = False
    cache = os.path.join(scratch, "az-tui", "cache.json")
    if not os.path.exists(cache):
        print("--- no cache was written on quit", file=sys.stderr)
        ok = False
    else:
        with open(cache) as f:
            written = f.read()
        for absent in ("hunter2", "aHVudGVyMg", "password", "LOG_LEVEL", "Back-off restarting"):
            if absent in written:
                print(f"--- {absent!r} reached the cache", file=sys.stderr)
                ok = False
    print("ok" if ok else "FAILED")
    sys.exit(0 if ok else 1)


if __name__ == "__main__":
    main()
