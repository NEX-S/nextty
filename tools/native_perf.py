#!/usr/bin/env python3
"""Measure PTY-output-to-presentation on the current native Wayland desktop.

Uses NEXTTY_TRACE and wp_presentation feedback. This is application output
age at compositor-reported presentation, not physical keyboard-to-photon latency.
Output latency uses each marker's first presentation; redraws still count toward
frame cadence. The fixture's marker rate is configurable; the display decides
presentation cadence.
"""
import argparse
import ctypes
import errno
import fcntl
import hashlib
import json
import math
import os
from pathlib import Path
import signal
import struct
import subprocess
import sys
import termios
import time
import tty


def fixture(path, wait_key=False, seconds=4, warmup=2, producer_hz=240):
    samples = []
    os.write(1, b"\x1b[?25l\x1b[2J\x1b[H")
    os.write(1, "\x1b]2;nextty — GPU performance test\x07".encode())
    if wait_key:
        tty.setraw(0)
        os.write(1, f"Press Enter to start. Keep this window visible; closes in about {2 * (seconds + warmup) + 1:g} seconds.\r\n".encode())
        while b"\r" not in os.read(0, 4096):
            pass
    deadline = time.monotonic()
    marker = 0x10000
    for phase in ("sparse", "scrolling"):
        # The workspace reserves a tab row, and the GUI may resize after startup.
        height = struct.unpack("HHHH", fcntl.ioctl(0, termios.TIOCGWINSZ, b"\0" * 8))[0]
        os.write(1, f"\x1b[2;{height}r\x1b[{height};1H".encode())
        for index in range(round((warmup + seconds) * producer_hz)):
            red, green, blue = marker >> 16, marker >> 8 & 255, marker & 255
            data = f"\x1b7\x1b[H\x1b[48;2;{red};{green};{blue}m \x1b[0m\x1b8".encode()
            if phase == "scrolling":
                data += (f"\x1b[32mframe {index:04}\x1b[0m 中文 UTF-8 λ → " + "0123456789 " * 4 + "\r\n").encode()
            samples.append({"marker": marker, "phase": phase,
                            "record": index >= round(warmup * producer_hz), "write_begin": time.monotonic()})
            while data:
                data = data[os.write(1, data):]
            marker += 1
            deadline += 1 / producer_hz
            time.sleep(max(0, deadline - time.monotonic()))
    Path(path).write_text(json.dumps(samples))
    time.sleep(0.3)


def percentile(values):
    ordered = sorted(values)
    if not ordered:
        return None
    return {f"p{p}": ordered[math.ceil(len(ordered) * p / 100) - 1] for p in (50, 95, 99)}


_cpu_clock = ctypes.CDLL(None).clock_getcpuclockid
_cpu_clock.argtypes = (ctypes.c_int, ctypes.POINTER(ctypes.c_int))
_cpu_clock.restype = ctypes.c_int


def cost(pid):
    result = {"monotonic": time.monotonic()}
    for line in Path(f"/proc/{pid}/smaps_rollup").read_text().splitlines():
        key, *rest = line.split()
        if key in ("Rss:", "Pss:", "Anonymous:"):
            result[key[:-1].lower() + "_kib"] = int(rest[0])
    clock = ctypes.c_int()
    error = _cpu_clock(pid, ctypes.byref(clock))
    if error:
        raise OSError(error, os.strerror(error))
    try:
        result["cpu_seconds"] = time.clock_gettime(clock.value)
    except OSError as error:
        if error.errno != errno.EINVAL:
            raise
        # Linux invalidates a process clock when that process exits.
        raise ProcessLookupError(errno.ESRCH, os.strerror(errno.ESRCH)) from error
    result["threads"] = len(list(Path(f"/proc/{pid}/task").iterdir()))
    result["drm"] = []
    for item in Path(f"/proc/{pid}/fdinfo").iterdir():
        try:
            info = item.read_text()
        except FileNotFoundError:
            continue
        if "drm-driver:" in info:
            result["drm"].append(info)
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    parser.add_argument("--output-dir", type=Path, required=True)
    parser.add_argument("--wait-key", action="store_true", help="press Enter in the visible terminal to start")
    parser.add_argument("--seconds", type=float, default=4, help="recording seconds per phase")
    parser.add_argument("--warmup", type=float, default=2, help="warmup seconds before each phase")
    parser.add_argument("--producer-hz", type=float, default=240, help="fixture output rate; use more than the render cap to measure it")
    args = parser.parse_args()
    if not math.isfinite(args.seconds) or args.seconds <= 0 or not math.isfinite(args.warmup) or args.warmup < 0:
        parser.error("--seconds must be finite and positive; --warmup finite and nonnegative")
    if not math.isfinite(args.producer_hz) or args.producer_hz <= 0:
        parser.error("--producer-hz must be finite and positive")
    binary = args.binary.resolve()
    workspace_name = f"gpu-bench-{os.getpid()}"
    directory = args.output_dir.resolve()
    directory.mkdir(parents=True, exist_ok=True)
    config = directory / "config.toml"
    config.write_text('font_family = "monospace"\nfont_size_px = 16.0\n')
    env = os.environ.copy()
    env.pop("DISPLAY", None)
    env.setdefault("XDG_RUNTIME_DIR", f"/run/user/{os.getuid()}")
    env.setdefault("WAYLAND_DISPLAY", "wayland-0")
    env.setdefault("DBUS_SESSION_BUS_ADDRESS", f"unix:path={env['XDG_RUNTIME_DIR']}/bus")
    env["NEXTTY_TRACE"] = str(directory / "trace.log")
    report = {"binary": str(binary), "binary_bytes": binary.stat().st_size,
              "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
              "scope": __doc__, "config": config.read_text(), "cost_samples": [],
              "seconds_per_phase": args.seconds, "warmup_seconds_per_phase": args.warmup,
              "producer_hz": args.producer_hz,
              "cost_scope": "GUI process only; excludes workspace daemon, child fixture and compositor",
              "driver_environment": {key: env[key] for key in ("VK_DRIVER_FILES", "VK_INSTANCE_LAYERS", "VK_LAYER_VALIDATE_SYNC") if key in env}}
    command = [str(binary), "new", workspace_name, "--gui", "--config", str(config), "-e", sys.executable,
               str(Path(__file__).resolve()), "--fixture", str(directory / "writes.json"),
               str(int(args.wait_key)), str(args.seconds), str(args.warmup), str(args.producer_hz)]
    with (directory / "app.log").open("w") as log:
        app = subprocess.Popen(command,
                               env=env, stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
        try:
            deadline = time.monotonic() + 2 * (args.seconds + args.warmup) + (320 if args.wait_key else 15)
            while app.poll() is None:
                if time.monotonic() > deadline:
                    raise TimeoutError("native fixture did not finish")
                try:
                    report["cost_samples"].append(cost(app.pid))
                except (ProcessLookupError, FileNotFoundError):
                    pass
                time.sleep(0.1)
            if app.returncode:
                raise RuntimeError(f"terminal exited {app.returncode}; see {directory / 'app.log'}")
        finally:
            try:
                listing = subprocess.run([str(binary), "list"], env=env,
                                         check=True, capture_output=True, text=True, timeout=5)
                if any(line.split("\t", 1)[0] == workspace_name for line in listing.stdout.splitlines()):
                    subprocess.run([str(binary), "terminate", workspace_name], env=env,
                                   check=True, capture_output=True, timeout=5)
            finally:
                if app.poll() is None:
                    os.killpg(app.pid, signal.SIGTERM)
                    app.wait(timeout=5)
    frames, presented, callbacks = {}, {}, {}
    window_states = []
    frame_rates = []
    discarded = set()
    for line in (directory / "trace.log").read_text().splitlines():
        fields = line.split()
        if fields[0] == "frame":
            frames[int(fields[1])] = {"begin": float(fields[2]),
                                     "prepared": float(fields[3]), "swapped": float(fields[4]),
                                     "marker": int(fields[6]),
                                     "geometry": [int(fields[8]), int(fields[9]), float(fields[10])],
                                     "focused": bool(int(fields[11]))}
        elif fields[0] == "frame_callback":
            callbacks[int(fields[1])] = {"received": float(fields[2]),
                                        "dispatched": float(fields[3]),
                                        "server_raw_ms_u32": int(fields[4])}
        elif fields[0] == "window_state":
            window_states.append({"received": float(fields[1]), "flags": int(fields[2])})
        elif fields[0] == "frame_rate":
            frame_rates.append({"dispatched": float(fields[1]), "frame_interval_ns": int(fields[2])})
        elif fields[0] == "presented" and fields[4] == "1":
            discarded.add(int(fields[1]))
        elif fields[0] == "presented" and fields[4] == "0":
            if fields[3] != "1":
                raise RuntimeError(f"presentation clock {fields[3]} is not CLOCK_MONOTONIC")
            presented[int(fields[1])] = {"time": float(fields[2]), "flags": int(fields[5]),
                                        "refresh_ns": float(fields[6])}
    writes_path = directory / "writes.json"
    if not writes_path.is_file():
        raise RuntimeError("fixture exited before writing samples; no performance result")
    writes = {item["marker"]: item for item in json.loads(writes_path.read_text())}
    report["window_states"] = window_states
    report["frame_rates"] = frame_rates
    report["callback_clock_scope"] = ("server time decoded as CLOCK_MONOTONIC only for the verified Mutter 50.6/GLib 2.88.4 environment; "
                                      "server batch emission is millisecond truncated, so emission-to-receipt is overestimated by less than 1 ms")
    for callback in callbacks.values():
        raw = callback["server_raw_ms_u32"]
        received_ms = callback["received"] * 1e3
        callback["server_emitted"] = (received_ms - ((received_ms - raw) % 2**32)) / 1e3
    report["phases"] = {}
    for phase in ("sparse", "scrolling"):
        submitted = [(n, f) for n, f in frames.items()
                     if f["marker"] in writes and writes[f["marker"]]["phase"] == phase
                     and writes[f["marker"]]["record"]]
        swap_times = sorted(f["swapped"] for _, f in submitted)
        pairs = [(f, presented[n], writes[f["marker"]]) for n, f in submitted if n in presented]
        first_presentations = {}
        for f, p, w in pairs:
            previous = first_presentations.get(f["marker"])
            if previous is None or p["time"] < previous[0]:
                first_presentations[f["marker"]] = (p["time"], w["write_begin"])
        times = sorted(p["time"] for _, p, _ in pairs)
        callback_pairs = [(f, callbacks[n]) for n, f in submitted if n in callbacks]
        if len(times) < 2:
            raise RuntimeError(f"insufficient presentation feedback for {phase}")
        report["phases"][phase] = {
            "frames_submitted": len(submitted),
            "swap_return_hz": (len(swap_times) - 1) / (swap_times[-1] - swap_times[0]) if len(swap_times) >= 2 else None,
            "swap_return_interval_ms": percentile([(b - a) * 1e3 for a, b in zip(swap_times, swap_times[1:])]),
            "discarded_submissions": sum(n in discarded for n, _ in submitted),
            "frames_presented": len(pairs), "effective_hz": (len(times) - 1) / (times[-1] - times[0]),
            "presentation_interval_ms": percentile([(b - a) * 1e3 for a, b in zip(times, times[1:])]),
            "output_markers_presented": len(first_presentations),
            "pty_write_to_presentation_ms": percentile([(presented - written) * 1e3 for presented, written in first_presentations.values()]),
            "prepare_cpu_ms": percentile([(f["prepared"] - f["begin"]) * 1e3 for f, _, _ in pairs]),
            "present_wrapper_ms": percentile([(f["swapped"] - f["prepared"]) * 1e3 for f, _, _ in pairs]),
            "draw_begin_to_presentation_ms": percentile([(p["time"] - f["begin"]) * 1e3 for f, p, _ in pairs]),
            "write_to_draw_begin_ms": percentile([(f["begin"] - w["write_begin"]) * 1e3 for f, _, w in pairs]),
            "focused_frames": sum(f["focused"] is True for f, _, _ in pairs),
            "unfocused_frames": sum(f["focused"] is False for f, _, _ in pairs),
            "geometry_logical_px_scale": [list(v) for v in sorted({tuple(f["geometry"]) for f, _, _ in pairs})],
            "callback_after_swap_ms": percentile([(c["received"] - f["swapped"]) * 1e3 for f, c in callback_pairs]),
            "callback_server_emission_after_swap_ms": percentile([(c["server_emitted"] - f["swapped"]) * 1e3 for f, c in callback_pairs]),
            "callback_server_emission_to_receipt_ms": percentile([(c["received"] - c["server_emitted"]) * 1e3 for _, c in callback_pairs]),
            "callback_receipt_to_rust_dispatch_ms": percentile([(c["dispatched"] - c["received"]) * 1e3 for _, c in callback_pairs]),
            "presentation_flags": sorted({p["flags"] for _, p, _ in pairs}),
            "refresh_ns": sorted({p["refresh_ns"] for _, p, _ in pairs}),
        }
    report["cost_peak"] = {key: max(item[key] for item in report["cost_samples"])
                           for key in ("rss_kib", "pss_kib", "anonymous_kib", "cpu_seconds", "threads")}
    (directory / "report.json").write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps({"output": str(directory / "report.json"), "phases": report["phases"], "cost_peak": report["cost_peak"]}, indent=2))


if __name__ == "__main__":
    if len(sys.argv) > 1 and sys.argv[1] == "--fixture":
        fixture(sys.argv[2], sys.argv[3] == "1", float(sys.argv[4]), float(sys.argv[5]), float(sys.argv[6]))
    else:
        main()
