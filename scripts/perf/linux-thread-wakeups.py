#!/usr/bin/env python3
"""scripts/perf/linux-thread-wakeups.py：空闲唤醒与 CPU 的按线程归因工具

按线程定位一个 Linux 进程的空闲唤醒与 CPU：

- 计数窗口：首尾各读一次 /proc/<pid>/task/*/{comm,status,stat}，给出每个线程的自愿 / 非自愿上下文切换数与 CPU（utime + stime），
  以及整个进程的 CPU（/proc/<pid>/stat）。窗口里新出现的线程从 0 算起；
- 归因窗口（可选，--strace 秒数 > 0）：计数窗口之后 `sudo strace -f -tt -T -p <pid>`（只跟踪与等待 / 唤醒有关的系统调用），统计每个
  线程的系统调用次数，原始输出另存（--strace-out）。strace 需要 ptrace 权限（Ubuntu 的 yama ptrace_scope = 1 时要 sudo）；
- 每个线程此刻的等待点（/proc/<pid>/task/*/wchan）。

用法：
  linux-thread-wakeups.py <pid> <计数秒数> [--warmup <秒>] [--strace <秒>] [--strace-out <文件>] [--label <文本>] [--out <文件>]
输出 JSON（--out 缺省时打印到标准输出）。调用栈级别的归因用 gdb（`sudo gdb -p <pid> -batch -ex 'thread apply all bt 40'`），不在本脚本里。
"""

import argparse
import collections
import datetime
import json
import os
import re
import subprocess
import sys
import time

SCRIPT_VERSION = 1
HZ = os.sysconf("SC_CLK_TCK")
WAIT_SYSCALLS = (
    "futex,poll,ppoll,epoll_wait,epoll_pwait,epoll_pwait2,select,pselect6,nanosleep,clock_nanosleep,"
    "recvmsg,sendmsg,read,write,timerfd_settime"
)


def read(path):
    try:
        with open(path, encoding="utf-8", errors="replace") as f:
            return f.read()
    except OSError:
        return ""


def snapshot(pid):
    """线程 id → (名字, 自愿切换, 非自愿切换, CPU tick)"""
    out = {}
    base = f"/proc/{pid}/task"
    for tid in os.listdir(base):
        d = os.path.join(base, tid)
        status = read(os.path.join(d, "status"))

        def field(key):
            m = re.search(rf"^{key}:\s*(\d+)", status, re.M)
            return int(m.group(1)) if m else 0

        stat = read(os.path.join(d, "stat"))
        rest = stat.rsplit(")", 1)[-1].split()
        ticks = int(rest[11]) + int(rest[12]) if len(rest) > 12 else 0
        out[int(tid)] = (
            read(os.path.join(d, "comm")).strip(),
            field("voluntary_ctxt_switches"),
            field("nonvoluntary_ctxt_switches"),
            ticks,
        )
    return out


def process_ticks(pid):
    rest = read(f"/proc/{pid}/stat").rsplit(")", 1)[-1].split()
    return int(rest[11]) + int(rest[12])


def wchans(pid):
    base = f"/proc/{pid}/task"
    return {
        int(t): (read(os.path.join(base, t, "comm")).strip(), read(os.path.join(base, t, "wchan")).strip())
        for t in os.listdir(base)
    }


def strace(pid, seconds, out_path):
    """strace -f 跟踪 `seconds` 秒，返回每个线程的系统调用计数"""
    cmd = ["strace", "-f", "-tt", "-T", "-p", str(pid), "-e", f"trace={WAIT_SYSCALLS}", "-o", out_path]
    if os.geteuid() != 0:
        cmd = ["sudo", "-n"] + cmd
    proc = subprocess.Popen(cmd, stderr=subprocess.DEVNULL)
    time.sleep(seconds)
    subprocess.run((["sudo", "-n"] if os.geteuid() != 0 else []) + ["kill", "-INT", str(proc.pid)], check=False)
    proc.wait(timeout=30)
    calls = collections.defaultdict(collections.Counter)
    pat = re.compile(r"^(\d+)\s+\S+\s+(\w+)\(")
    for line in open(out_path, encoding="utf-8", errors="replace"):
        m = pat.match(line)
        if m:
            calls[int(m.group(1))][m.group(2)] += 1
    return calls


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("pid", type=int)
    ap.add_argument("seconds", type=float)
    ap.add_argument("--warmup", type=float, default=0.0)
    ap.add_argument("--strace", type=float, default=0.0, help="计数窗口之后 strace 归因的秒数（0 = 不做）")
    ap.add_argument("--strace-out", default=None, help="strace 原始输出的路径（缺省为 --out 旁边的 .strace.txt 或临时文件）")
    ap.add_argument("--label", default=None)
    ap.add_argument("--out", default=None)
    args = ap.parse_args()
    pid = args.pid

    time.sleep(args.warmup)
    started = datetime.datetime.now().astimezone()
    t0, p0, m0 = snapshot(pid), process_ticks(pid), time.monotonic()
    time.sleep(args.seconds)
    t1, p1, secs = snapshot(pid), process_ticks(pid), time.monotonic() - m0
    waits = wchans(pid)

    rows = []
    for tid, (name, vol, nonvol, ticks) in t1.items():
        _, v0, n0, c0 = t0.get(tid, (name, 0, 0, 0))
        rows.append(
            {
                "tid": tid,
                "thread": name,
                "voluntary": vol - v0,
                "nonvoluntary": nonvol - n0,
                "cpu_ms": round((ticks - c0) * 1000 / HZ, 1),
                "new_in_window": tid not in t0,
                "wchan": waits.get(tid, ("", ""))[1],
            }
        )
    rows.sort(key=lambda r: (-r["voluntary"], r["tid"]))
    result = {
        "tool": "scripts/perf/linux-thread-wakeups.py",
        "tool_version": SCRIPT_VERSION,
        "label": args.label,
        "started_at": started.isoformat(timespec="seconds"),
        "host": {"kernel": os.uname().release, "machine": os.uname().machine, "loadavg_end": [round(x, 2) for x in os.getloadavg()]},
        "pid": pid,
        "executable": os.path.realpath(f"/proc/{pid}/exe") if os.path.exists(f"/proc/{pid}/exe") else None,
        "warmup_s": args.warmup,
        "idle_s": round(secs, 1),
        "process_cpu_ms": round((p1 - p0) * 1000 / HZ, 1),
        "process_cpu_percent": round((p1 - p0) / HZ / secs * 100, 4),
        "threads": len(t1),
        "voluntary_total": sum(r["voluntary"] for r in rows),
        "voluntary_per_s": round(sum(r["voluntary"] for r in rows) / secs, 3),
        "per_thread": [r for r in rows if r["voluntary"] or r["nonvoluntary"] or r["cpu_ms"]],
        "quiet_threads": sorted({r["thread"] for r in rows if not (r["voluntary"] or r["nonvoluntary"] or r["cpu_ms"])}),
    }
    if args.strace > 0:
        out_path = args.strace_out or ((args.out + ".strace.txt") if args.out else f"/tmp/wisp-strace-{pid}.txt")
        calls = strace(pid, args.strace, out_path)
        names = {tid: v[0] for tid, v in t1.items()}
        result["strace_s"] = args.strace
        result["strace_output"] = out_path
        result["strace_syscalls_per_thread"] = {f"{names.get(t, '?')}#{t}": dict(c) for t, c in sorted(calls.items())}

    text = json.dumps(result, ensure_ascii=False, indent=1)
    if args.out:
        with open(args.out, "w", encoding="utf-8") as f:
            f.write(text + "\n")
    else:
        print(text)
    return 0


if __name__ == "__main__":
    sys.exit(main())
