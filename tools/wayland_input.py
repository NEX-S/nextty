#!/usr/bin/env python3
"""Measure synthetic Wayland key -> PTY -> presentation in a private Mutter session.

The session record contains an `env` mapping for an isolated GNOME compositor.
Results exclude physical keyboard, compositor input hardware, and panel scanout.
CPU/PSS include the terminal frontend and its daemon, excluding the fixture and compositor.
"""
import argparse
import fcntl
import hashlib
import json
import os
from pathlib import Path
import random
import select
import signal
import struct
import subprocess
import sys
import termios
import time
import tty

from native_perf import cost as process_cost, percentile as quantiles


def fixture(directory, busy_hz, pattern):
    tty.setraw(0)
    control = os.open(directory / "control", os.O_RDWR | os.O_NONBLOCK)
    with (directory / "fixture.jsonl").open("w", buffering=1) as events:
        def emit(value):
            events.write(json.dumps(value) + "\n")

        def output(data):
            while data:
                data = data[os.write(1, data):]

        output(b"\x1b[?25l\x1b[2J\x1b[H")
        emit({"ready": True, "pid": os.getpid()})
        phase, pending, index, scroll, deadline = "idle", b"", 0, 0, time.monotonic()
        colored = []
        while True:
            busy = phase == "busy"
            ready, _, _ = select.select([0, control], [], [], max(0, deadline - time.monotonic()) if busy else None)
            if control in ready:
                pending += os.read(control, 4096)
                while b"\n" in pending:
                    line, pending = pending.split(b"\n", 1)
                    command = json.loads(line)
                    if command["op"] == "quit":
                        return
                    phase, deadline = command["phase"], time.monotonic()
                    if phase == "busy":
                        height, columns, _, _ = struct.unpack("HHHH", fcntl.ioctl(0, termios.TIOCGWINSZ, b"\0" * 8))
                        output(f"\x1b[2;{height}r\x1b[{height};1H".encode())
                        if pattern == "colors" and not colored:
                            for frame in range(64):
                                line = bytearray()
                                for col in range(columns):
                                    r, g, b = (frame*17+col*13)&255, (frame*7+col*29)&255, (frame*37+col*11)&255
                                    line.extend(f"\x1b[38;2;{r};{g};{b};48;2;{b};{r};{g}m{chr(33+(frame+col)%90)}".encode())
                                colored.append(bytes(line)+b"\x1b[0m\r\n")
                    emit({"ack": command["seq"], "phase": phase})
            if 0 in ready:
                received = time.monotonic()
                data = os.read(0, 4096)
                if not data:
                    return
                for byte in data:
                    index += 1
                    marker = 0x20000 + index
                    begin = time.monotonic()
                    output(f"\x1b7\x1b[H\x1b[48;2;{marker >> 16};{marker >> 8 & 255};{marker & 255}m \x1b[0m\x1b8".encode())
                    emit({"index": index, "marker": marker, "byte": byte, "phase": phase,
                          "raw_read": received, "write_begin": begin})
            if phase == "busy" and time.monotonic() >= deadline:
                if colored:
                    output(colored[scroll % len(colored)])
                else:
                    output((f"\x1b[32mrow {scroll:06}\x1b[0m 中文 UTF-8 λ → " + "0123456789 " * 4 + "\r\n").encode())
                scroll += 1
                deadline += 1 / busy_hz


def rows(path):
    return [json.loads(line) for line in path.read_text().splitlines()] if path.exists() else []


def cost(pids):
    return {role: process_cost(pid) for role, pid in pids.items()}


def traces(path):
    frames, presented = {}, {}
    complete = path.read_text().rpartition("\n")[0] if path.exists() else ""
    for line in complete.splitlines():
        f = line.split()
        if f[0] == "frame":
            frames[int(f[1])] = {"begin": float(f[2]), "marker": int(f[7]), "focused": bool(int(f[12]))}
        elif f[0] == "presented" and f[4] == "0":
            if f[3] != "1":
                raise RuntimeError("presentation feedback must use CLOCK_MONOTONIC")
            presented[int(f[1])] = float(f[2])
    return frames, presented


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    parser.add_argument("--session-record", required=True, type=Path)
    parser.add_argument("--output-dir", required=True, type=Path)
    parser.add_argument("--samples", type=int, default=100)
    parser.add_argument("--busy-hz", type=float, default=1000)
    parser.add_argument("--busy-pattern", choices=("text", "colors"), default="text")
    args = parser.parse_args()
    if args.samples < 1 or not 0 < args.busy_hz <= 10000:
        parser.error("samples must be positive and busy-hz in (0, 10000]")
    binary = args.binary.resolve()
    help_text = subprocess.run([str(binary), "--help"], check=True, capture_output=True, text=True).stdout
    workspace = "\nWorkspace input:\n" in help_text
    workspace_name = f"input-bench-{os.getpid()}"
    directory = args.output_dir.resolve()
    directory.mkdir(parents=True)
    control_path = directory / "control"
    os.mkfifo(control_path)
    config = directory / "config.toml"
    config.write_text('font_family = "monospace"\nfont_size_px = 16.0\n')
    env = dict(os.environ, **json.loads(args.session_record.read_text())["env"])
    for name in ("DISPLAY", "IBUS_ADDRESS", "LD_PRELOAD", "LIBGL_ALWAYS_SOFTWARE", "GALLIUM_DRIVER"):
        env.pop(name, None)
    env["NEXTTY_TRACE"] = str(directory / "trace.log")
    import gi
    from gi.repository import Gio, GLib
    connection = Gio.DBusConnection.new_for_address_sync(env["DBUS_SESSION_BUS_ADDRESS"],
        Gio.DBusConnectionFlags.AUTHENTICATION_CLIENT | Gio.DBusConnectionFlags.MESSAGE_BUS_CONNECTION, None, None)
    dest = "org.gnome.Mutter.RemoteDesktop"

    def call(path, interface, method, signature=None, values=()):
        return connection.call_sync(dest, path, interface, method, GLib.Variant(signature, values) if signature else None,
                                    None, Gio.DBusCallFlags.NONE, 5000, None).unpack()

    session, = call("/org/gnome/Mutter/RemoteDesktop", dest, "CreateSession")
    call(session, dest + ".Session", "Start")

    def key(symbol, pressed):
        call(session, dest + ".Session", "NotifyKeyboardKeysym", "(ub)", (symbol, pressed))

    def tap(symbol):
        key(symbol, True)
        key(symbol, False)

    report = {"binary": str(binary), "binary_bytes": binary.stat().st_size,
              "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(), "scope": __doc__,
              "samples_per_phase": args.samples, "busy_hz": args.busy_hz, "busy_pattern": args.busy_pattern, "sent": [], "phases": {}}
    app, control = None, None
    try:
        # Mutter creates the session's virtual keyboard on the first key press.
        tap(0xffe1)
        with (directory / "app.log").open("w") as log:
            command = [str(binary)] + (["new", workspace_name] if workspace else []) + ["--gui"]
            command += ["--config", str(config), "-e", sys.executable, __file__, "--fixture", str(directory), str(args.busy_hz), args.busy_pattern]
            app = subprocess.Popen(command, env=env, stdout=log, stderr=subprocess.STDOUT, start_new_session=True)

            def wait(predicate, label, seconds=8):
                until = time.monotonic() + seconds
                while time.monotonic() < until:
                    value = predicate()
                    if value:
                        return value
                    if app.poll() is not None:
                        raise RuntimeError(f"terminal exited {app.returncode}: {label}; see {directory / 'app.log'}")
                    time.sleep(.005)
                raise TimeoutError(label)

            events_path = directory / "fixture.jsonl"
            wait(lambda: any(e.get("ready") for e in rows(events_path)), "PTY fixture ready")
            pids = {"frontend": app.pid}
            for child in Path(f"/proc/{app.pid}/task/{app.pid}/children").read_text().split():
                if b"--server\0" in Path(f"/proc/{child}/cmdline").read_bytes():
                    pids["daemon"] = int(child)
            if workspace and "daemon" not in pids:
                raise RuntimeError("benchmark requires a fresh private daemon for complete process accounting")
            report["pids"] = pids
            control = os.open(control_path, os.O_WRONLY | os.O_NONBLOCK)

            def focused():
                path = directory / "trace.log"
                complete = path.read_text().rpartition("\n")[0] if path.exists() else ""
                states = [line.split()[2] for line in complete.splitlines() if line.startswith("focus ")]
                return states and states[-1] == "1"

            seq = 0

            def phase(name):
                nonlocal seq
                seq += 1
                os.write(control, json.dumps({"op": "phase", "phase": name, "seq": seq}).encode() + b"\n")
                wait(lambda: any(e.get("ack") == seq for e in rows(events_path)), f"phase {name}")

            phase("busy")
            wait(focused, "terminal focused")
            # Mapping can resize the PTY; restore its scroll region after settling.
            time.sleep(1)
            phase("busy")
            tap(ord("w"))
            wait(lambda: any(e.get("index") == 1 for e in rows(events_path)), "warmup key")

            def warm():
                frames, presented = traces(directory / "trace.log")
                return any(f["marker"] == 0x20001 and n in presented for n, f in frames.items())

            wait(warm, "warmup marker presented")
            for name in ("idle", "busy"):
                phase(name)
                time.sleep(1 if name == "busy" else .1)
                before, start = cost(pids), time.monotonic()
                rng, deadline = random.Random(7491), time.monotonic()
                for _ in range(args.samples):
                    time.sleep(max(0, deadline - time.monotonic()))
                    begin = time.monotonic()
                    key(ord("x"), True)
                    pressed = time.monotonic()
                    key(ord("x"), False)
                    report["sent"].append({"index": len(report["sent"]) + 2, "phase": name,
                                           "call_start": begin, "press_return": pressed})
                    deadline += .05 + rng.uniform(-.015, .015)
                wait(lambda: sum(e.get("phase") == name and e.get("byte") == ord("x") for e in rows(events_path)) == args.samples,
                     "all injected keys received")
                time.sleep(.2)
                report["phases"][name] = {"start": start, "end": time.monotonic(), "before": before, "after": cost(pids)}
            phase("idle-check")
            time.sleep(.3)
            report["idle_check"] = {"start": time.monotonic(), "before": cost(pids)}
            time.sleep(3)
            report["idle_check"].update(end=time.monotonic(), after=cost(pids))
            os.write(control, b'{"op":"quit"}\n')
            if app.wait(timeout=8) != 0:
                raise RuntimeError("terminal failed after fixture exit")
        frames, presented = traces(directory / "trace.log")
        events = {e["index"]: e for e in rows(events_path) if "index" in e}
        for name, result in report["phases"].items():
            samples, received = [], []
            for sent in report["sent"]:
                if sent["phase"] != name:
                    continue
                event = events[sent["index"]]
                if event["byte"] != ord("x") or event["phase"] != name:
                    raise RuntimeError("injected key identity changed")
                received.append((event["raw_read"] - sent["call_start"]) * 1000)
                visible = [(presented[n], f) for n, f in frames.items() if n in presented and f["marker"] == event["marker"]]
                if visible:
                    at, frame = min(visible, key=lambda pair: pair[0])
                    samples.append(dict(sent, **event, presentation=at, focused=frame["focused"]))
            result.update(raw_samples=received, samples=samples, received=len(received), presented=len(samples),
                          missing_presentations=args.samples-len(samples),
                          synthetic_key_to_pty_ms=quantiles(received),
                          synthetic_key_to_presentation_ms=quantiles([(s["presentation"]-s["call_start"])*1000 for s in samples]))
            if not all(s["focused"] for s in samples):
                raise RuntimeError("presentation samples include an unfocused terminal")
            result["terminal_cpu_cores"] = sum(result["after"][role]["cpu_seconds"]-v["cpu_seconds"] for role,v in result["before"].items())/(result["end"]-result["start"])
            result["terminal_pss_kib"] = sum(v["pss_kib"] for v in result["after"].values())
        idle = report["idle_check"]
        idle["frames"] = sum(idle["start"] <= f["begin"] <= idle["end"] for f in frames.values())
        idle["terminal_cpu_seconds"] = sum(idle["after"][role]["cpu_seconds"]-v["cpu_seconds"] for role,v in idle["before"].items())
        (directory / "report.json").write_text(json.dumps(report, indent=2) + "\n")
        print(json.dumps({"output": str(directory / "report.json"), "pids": pids,
            "phases": {name: {k:v for k,v in data.items() if k in ("received","presented","missing_presentations",
                "synthetic_key_to_pty_ms","synthetic_key_to_presentation_ms","terminal_cpu_cores","terminal_pss_kib")} for name,data in report["phases"].items()},
            "idle": {k:v for k,v in idle.items() if k in ("frames","terminal_cpu_seconds")}}, indent=2))
    finally:
        if control is not None:
            os.close(control)
        control_path.unlink()
        if app and workspace:
            subprocess.run([str(binary), "terminate", workspace_name], env=env, capture_output=True, timeout=5)
        if app and app.poll() is None:
            os.killpg(app.pid, signal.SIGTERM)
            app.wait(timeout=5)
        call(session, dest + ".Session", "Stop")
        connection.close_sync(None)


if __name__ == "__main__":
    if len(sys.argv) > 1 and sys.argv[1] == "--fixture":
        fixture(Path(sys.argv[2]), float(sys.argv[3]), sys.argv[4])
    else:
        main()
