#!/usr/bin/env python3
import argparse
import csv
import datetime as dt
import os
import re
import shlex
import signal
import subprocess
import sys
import time
from dataclasses import dataclass
from typing import Optional


PING_TIME_RE = re.compile(r"time[=<]([0-9]*\.?[0-9]+)\s*ms")


def utc_now_iso() -> str:
    return dt.datetime.now(dt.timezone.utc).isoformat()


@dataclass
class PingSample:
    ts_utc: str
    direction: str  # "A->B" or "B->A"
    seq: Optional[int]
    rtt_ms: Optional[float]
    raw: str


def start_process(argv: list[str]) -> subprocess.Popen:
    return subprocess.Popen(
        argv,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        bufsize=1,
        universal_newlines=True,
        preexec_fn=os.setsid,  # own process group so we can kill ping cleanly
    )


def kill_process_group(p: subprocess.Popen) -> None:
    if p.poll() is not None:
        return
    try:
        os.killpg(p.pid, signal.SIGTERM)
    except ProcessLookupError:
        return
    # give it a moment, then SIGKILL if needed
    deadline = time.time() + 2.0
    while time.time() < deadline:
        if p.poll() is not None:
            return
        time.sleep(0.05)
    try:
        os.killpg(p.pid, signal.SIGKILL)
    except ProcessLookupError:
        return


def parse_seq(line: str) -> Optional[int]:
    # common forms: "icmp_seq=12" or "seq=12"
    m = re.search(r"(?:icmp_)?seq=(\d+)", line)
    if not m:
        return None
    try:
        return int(m.group(1))
    except ValueError:
        return None


def parse_rtt_ms(line: str) -> Optional[float]:
    m = PING_TIME_RE.search(line)
    if not m:
        return None
    try:
        return float(m.group(1))
    except ValueError:
        return None


def iter_ping_samples(p: subprocess.Popen, direction: str):
    assert p.stdout is not None
    for raw_line in p.stdout:
        line = raw_line.rstrip("\n")
        # Skip blank lines, but still keep raw for debugging if needed
        if not line.strip():
            continue
        yield PingSample(
            ts_utc=utc_now_iso(),
            direction=direction,
            seq=parse_seq(line),
            rtt_ms=parse_rtt_ms(line),
            raw=line,
        )


def validate_ssh() -> None:
    try:
        subprocess.run(["ssh", "-V"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, check=False)
    except FileNotFoundError:
        print("ERROR: 'ssh' not found on PATH.", file=sys.stderr)
        sys.exit(2)


def validate_ping() -> None:
    try:
        subprocess.run(["ping", "-V"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, check=False)
    except FileNotFoundError:
        print("ERROR: 'ping' not found on PATH.", file=sys.stderr)
        sys.exit(2)


def ssh_ping_command(
    ssh_user: str,
    ssh_host: str,
    ssh_port: int,
    ssh_key: Optional[str],
    ssh_extra: list[str],
    target_ip: str,
    interval_s: float,
    payload_size: int,
    count: Optional[int],
    deadline_s: Optional[int],
) -> list[str]:
    # On the remote side, run: ping -n -i <interval> -s <payload> [-c <count>] [-w <deadline>]
    remote_cmd = ["ping", "-n", "-i", str(interval_s), "-s", str(payload_size)]
    if count is not None:
        remote_cmd += ["-c", str(count)]
    if deadline_s is not None:
        remote_cmd += ["-w", str(deadline_s)]
    remote_cmd += [target_ip]

    argv = ["ssh", "-p", str(ssh_port)]
    if ssh_key:
        argv += ["-i", ssh_key]
    argv += [
        "-o",
        "BatchMode=yes",
        "-o",
        "StrictHostKeyChecking=accept-new",
        "-o",
        "ServerAliveInterval=10",
        "-o",
        "ServerAliveCountMax=3",
    ]
    argv += ssh_extra
    argv += [f"{ssh_user}@{ssh_host}", "--"]
    argv += remote_cmd
    return argv


def local_ping_command(
    target_ip: str,
    interval_s: float,
    payload_size: int,
    count: Optional[int],
    deadline_s: Optional[int],
) -> list[str]:
    argv = ["ping", "-n", "-i", str(interval_s), "-s", str(payload_size)]
    if count is not None:
        argv += ["-c", str(count)]
    if deadline_s is not None:
        argv += ["-w", str(deadline_s)]
    argv += [target_ip]
    return argv


def stats(values: list[float]) -> dict[str, float]:
    if not values:
        return {}
    vs = sorted(values)
    n = len(vs)
    p50 = vs[int(0.50 * (n - 1))]
    p95 = vs[int(0.95 * (n - 1))]
    p99 = vs[int(0.99 * (n - 1))]
    return {
        "count": float(n),
        "min": float(vs[0]),
        "p50": float(p50),
        "p95": float(p95),
        "p99": float(p99),
        "max": float(vs[-1]),
        "avg": float(sum(vs) / n),
    }


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Ping-pong latency test between two AWS instances (both directions) using local ping + remote ping over SSH."
    )
    parser.add_argument("--local-target-ip", required=True, help="Target IP to ping from THIS instance (usually the peer's private IP).")
    parser.add_argument("--remote-target-ip", required=True, help="Target IP to ping from REMOTE instance (usually this instance's private IP).")
    parser.add_argument("--ssh-host", required=True, help="Remote SSH host/IP (peer instance; usually its private IP).")
    parser.add_argument("--ssh-user", default="ec2-user", help="Remote SSH username. Default: ec2-user")
    parser.add_argument("--ssh-port", type=int, default=22, help="Remote SSH port. Default: 22")
    parser.add_argument("--ssh-key", default=None, help="Path to SSH private key for remote access (optional if agent/instance profile handles it).")
    parser.add_argument(
        "--ssh-extra",
        default="",
        help='Extra ssh options as a single string, e.g. \' -o ProxyCommand="..." \'.',
    )
    parser.add_argument("--interval", type=float, default=0.2, help="Ping interval seconds. Default: 0.2")
    parser.add_argument("--payload-size", type=int, default=56, help="Ping payload size (bytes). Default: 56 (classic ping).")
    parser.add_argument("--duration", type=int, default=30, help="Total duration seconds. Default: 30")
    parser.add_argument("--count", type=int, default=None, help="Optional ping count (overrides duration if set).")
    parser.add_argument("--out-dir", default=".", help="Output directory. Default: current dir")
    args = parser.parse_args()

    validate_ping()
    validate_ssh()

    out_dir = os.path.abspath(args.out_dir)
    os.makedirs(out_dir, exist_ok=True)
    ts = dt.datetime.now(dt.timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    csv_path = os.path.join(out_dir, f"ping_pong_{ts}.csv")
    summary_path = os.path.join(out_dir, f"ping_pong_{ts}_summary.txt")

    ssh_extra = shlex.split(args.ssh_extra) if args.ssh_extra.strip() else []
    deadline_s = None if args.count is not None else int(args.duration)

    local_argv = local_ping_command(
        target_ip=args.local_target_ip,
        interval_s=args.interval,
        payload_size=args.payload_size,
        count=args.count,
        deadline_s=deadline_s,
    )
    remote_argv = ssh_ping_command(
        ssh_user=args.ssh_user,
        ssh_host=args.ssh_host,
        ssh_port=args.ssh_port,
        ssh_key=args.ssh_key,
        ssh_extra=ssh_extra,
        target_ip=args.remote_target_ip,
        interval_s=args.interval,
        payload_size=args.payload_size,
        count=args.count,
        deadline_s=deadline_s,
    )

    print("Local command:", " ".join(shlex.quote(x) for x in local_argv), file=sys.stderr)
    print("Remote command:", " ".join(shlex.quote(x) for x in remote_argv), file=sys.stderr)
    print(f"Writing CSV: {csv_path}", file=sys.stderr)

    local_p = start_process(local_argv)
    remote_p = start_process(remote_argv)

    local_rtts: list[float] = []
    remote_rtts: list[float] = []
    local_timeouts = 0
    remote_timeouts = 0

    try:
        with open(csv_path, "w", newline="") as f:
            w = csv.writer(f)
            w.writerow(["ts_utc", "direction", "seq", "rtt_ms", "raw"])

            # Interleave reads from both processes without blocking on either one too long.
            # Simplicity: poll with small sleeps; stdout is line-buffered.
            local_iter = iter_ping_samples(local_p, "A->B")
            remote_iter = iter_ping_samples(remote_p, "B->A")

            end_time = None if args.count is not None else time.time() + float(args.duration)
            while True:
                made_progress = False

                # Read at most one line from each side per loop; keep output interleaved.
                for it, rtts, which in (
                    (local_iter, local_rtts, "local"),
                    (remote_iter, remote_rtts, "remote"),
                ):
                    try:
                        sample = next(it)
                    except StopIteration:
                        sample = None

                    if sample is not None:
                        made_progress = True
                        if sample.rtt_ms is not None:
                            rtts.append(sample.rtt_ms)
                        else:
                            # track obvious timeouts/unreachables from raw text
                            if "Destination Host Unreachable" in sample.raw or "100% packet loss" in sample.raw or "timeout" in sample.raw.lower():
                                if which == "local":
                                    local_timeouts += 1
                                else:
                                    remote_timeouts += 1

                        w.writerow([sample.ts_utc, sample.direction, sample.seq, sample.rtt_ms, sample.raw])

                if end_time is not None and time.time() >= end_time:
                    break

                # Exit early if both pings finished (count mode)
                if args.count is not None and local_p.poll() is not None and remote_p.poll() is not None:
                    break

                if not made_progress:
                    time.sleep(0.05)

    finally:
        kill_process_group(local_p)
        kill_process_group(remote_p)

    a_to_b = stats(local_rtts)
    b_to_a = stats(remote_rtts)

    def fmt(d: dict[str, float]) -> str:
        if not d:
            return "no RTT samples captured"
        return (
            f"n={int(d['count'])} min={d['min']:.3f}ms p50={d['p50']:.3f}ms "
            f"p95={d['p95']:.3f}ms p99={d['p99']:.3f}ms max={d['max']:.3f}ms avg={d['avg']:.3f}ms"
        )

    summary = (
        f"ping_pong_latency.py summary (UTC)\n"
        f"- started_at: {ts}\n"
        f"- local_target_ip (A->B): {args.local_target_ip}\n"
        f"- remote_target_ip (B->A): {args.remote_target_ip}\n"
        f"- ssh_host: {args.ssh_host}\n"
        f"- interval_s: {args.interval}\n"
        f"- payload_size: {args.payload_size}\n"
        f"- duration_s: {args.duration if args.count is None else 'n/a (count mode)'}\n"
        f"- count: {args.count if args.count is not None else 'n/a'}\n"
        f"\n"
        f"A->B: {fmt(a_to_b)}\n"
        f"B->A: {fmt(b_to_a)}\n"
        f"\n"
        f"Timeout-ish lines seen (best-effort): A->B={local_timeouts} B->A={remote_timeouts}\n"
        f"CSV: {csv_path}\n"
    )

    with open(summary_path, "w") as f:
        f.write(summary)

    print(summary, file=sys.stderr)
    print(f"Wrote summary: {summary_path}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

