#!/usr/bin/env python3
"""scripts/perf/macos-thread-wakeups.py：空闲唤醒与 CPU 的按线程归因工具

按线程定位一个进程的空闲唤醒与 CPU，**不需要 root**（spindump / powermetrics 要 sudo，xctrace 要完整 Xcode）：

- 进程级：每个窗口首尾各读一次 proc_pid_rusage(RUSAGE_INFO_V4) 的中断唤醒（ri_interrupt_wkups，与机器忙不忙无关；
  top 的 idlew 只数「把空闲的 CPU 叫醒」的次数，机器越忙越小）、包空闲唤醒与用户 + 系统 CPU 时间；
- 线程级：每秒读一次每个线程的 CPU 时间（proc_pidinfo 的 PROC_PIDTHREADID64INFO），窗口结束时列出「哪些线程在哪几秒
  有 CPU 增长」与各自的累计 CPU。

单位（两种接口的单位不同，容易混淆）：
- rusage 的 ri_user_time / ri_system_time 是 **Mach 绝对时间 tick**，Apple Silicon 上 1 tick = 125/3 ≈ 41.67 ns，必须
  用 mach_timebase_info 换算成纳秒（Intel 上 numer = denom = 1，换算不改变数值）；
- proc_threadinfo 的 pth_user_time / pth_system_time 由内核从 thread_basic_info（秒 + 微秒）换算，**已经是纳秒**。

线程标识：PROC_PIDLISTTHREADIDS 给的是**线程 id**（与 `sample` 输出里的 `Thread_<id>`、`thread_identifier_info` 一致），
不是 PROC_PIDLISTTHREADS 给的 pthread 句柄。

用法：
  macos-thread-wakeups.py <pid> <采样秒数> [--warmup <秒>] [--label <文本>] [--out <文件>]
输出 JSON（--out 缺省时打印到标准输出），格式见文件末尾的 `result`。
"""

import argparse
import ctypes
import ctypes.util
import datetime
import json
import os
import platform
import sys
import time

SCRIPT_VERSION = 2

libc = ctypes.CDLL(ctypes.util.find_library("c"), use_errno=True)

PROC_PIDLISTTHREADIDS = 28
PROC_PIDTHREADID64INFO = 15
PROC_PIDPATHINFO_MAXSIZE = 4096
RUSAGE_INFO_V4 = 4


class mach_timebase_info_data_t(ctypes.Structure):
    _fields_ = [("numer", ctypes.c_uint32), ("denom", ctypes.c_uint32)]


class rusage_info_v4(ctypes.Structure):
    _fields_ = [("ri_uuid", ctypes.c_uint8 * 16)] + [
        (n, ctypes.c_uint64)
        for n in (
            "ri_user_time ri_system_time ri_pkg_idle_wkups ri_interrupt_wkups ri_pageins ri_wired_size "
            "ri_resident_size ri_phys_footprint ri_proc_start_abstime ri_proc_exit_abstime ri_child_user_time "
            "ri_child_system_time ri_child_pkg_idle_wkups ri_child_interrupt_wkups ri_child_pageins "
            "ri_child_elapsed_abstime ri_diskio_bytesread ri_diskio_byteswritten ri_cpu_time_qos_default "
            "ri_cpu_time_qos_maintenance ri_cpu_time_qos_background ri_cpu_time_qos_utility ri_cpu_time_qos_legacy "
            "ri_cpu_time_qos_user_initiated ri_cpu_time_qos_user_interactive ri_billed_system_time "
            "ri_serviced_system_time ri_logical_writes ri_lifetime_max_phys_footprint ri_instructions ri_cycles "
            "ri_billed_energy ri_serviced_energy ri_interval_max_phys_footprint ri_runnable_time"
        ).split()
    ]


class proc_threadinfo(ctypes.Structure):
    _fields_ = [
        ("pth_user_time", ctypes.c_uint64),
        ("pth_system_time", ctypes.c_uint64),
        ("pth_cpu_usage", ctypes.c_int32),
        ("pth_policy", ctypes.c_int32),
        ("pth_run_state", ctypes.c_int32),
        ("pth_flags", ctypes.c_int32),
        ("pth_sleep_time", ctypes.c_int32),
        ("pth_curpri", ctypes.c_int32),
        ("pth_priority", ctypes.c_int32),
        ("pth_maxpriority", ctypes.c_int32),
        ("pth_name", ctypes.c_char * 64),
    ]


def timebase():
    tb = mach_timebase_info_data_t()
    if libc.mach_timebase_info(ctypes.byref(tb)) != 0 or tb.denom == 0:
        raise OSError("mach_timebase_info failed")
    return tb.numer, tb.denom


NUMER, DENOM = timebase()


def ticks_to_ns(ticks):
    """rusage 的 CPU 时间（Mach tick）→ 纳秒"""
    return ticks * NUMER // DENOM


def rusage(pid):
    ri = rusage_info_v4()
    if libc.proc_pid_rusage(pid, RUSAGE_INFO_V4, ctypes.byref(ri)) != 0:
        raise OSError(ctypes.get_errno(), f"proc_pid_rusage({pid})")
    return ri


def threads(pid):
    """线程 id → (名字, CPU 纳秒)"""
    buf = (ctypes.c_uint64 * 4096)()
    n = libc.proc_pidinfo(pid, PROC_PIDLISTTHREADIDS, ctypes.c_uint64(0), buf, ctypes.sizeof(buf))
    if n <= 0:
        raise OSError(ctypes.get_errno(), f"proc_pidinfo(PROC_PIDLISTTHREADIDS, {pid})")
    out = {}
    for i in range(n // ctypes.sizeof(ctypes.c_uint64)):
        ti = proc_threadinfo()
        got = libc.proc_pidinfo(
            pid, PROC_PIDTHREADID64INFO, ctypes.c_uint64(buf[i]), ctypes.byref(ti), ctypes.sizeof(ti)
        )
        if got > 0:  # 线程可能在两次调用之间退出
            out[int(buf[i])] = (ti.pth_name.decode(errors="replace") or "?", ti.pth_user_time + ti.pth_system_time)
    return out


def proc_path(pid):
    buf = ctypes.create_string_buffer(PROC_PIDPATHINFO_MAXSIZE)
    if libc.proc_pidpath(pid, buf, ctypes.sizeof(buf)) <= 0:
        return None
    return buf.value.decode(errors="replace")


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("pid", type=int)
    ap.add_argument("seconds", type=int)
    ap.add_argument("--warmup", type=float, default=0.0, help="开始采样前等待的秒数")
    ap.add_argument("--label", default=None, help="写进结果的说明（例如测量条件）")
    ap.add_argument("--out", default=None, help="结果 JSON 的路径（缺省打印到标准输出）")
    args = ap.parse_args()
    if args.seconds <= 0:
        ap.error("采样秒数必须大于 0")

    pid = args.pid
    time.sleep(args.warmup)
    started = datetime.datetime.now().astimezone()
    load0 = os.getloadavg()
    r0 = rusage(pid)
    t0 = time.monotonic()
    prev = threads(pid)
    seen = dict(prev)
    active = {}  # 线程 id → [名字, 有 CPU 增长的秒数, CPU 纳秒]
    for _ in range(args.seconds):
        time.sleep(1)
        cur = threads(pid)
        for tid, (name, cpu) in cur.items():
            # 窗口里新出现的线程从 0 算起（它在窗口内用掉的 CPU 全部计入）
            before = prev.get(tid, (name, 0))[1]
            if cpu > before:
                a = active.setdefault(tid, [name, 0, 0])
                a[0] = name
                a[1] += 1
                a[2] += cpu - before
        seen.update(cur)
        prev = cur
    r1 = rusage(pid)
    dt = time.monotonic() - t0
    load1 = os.getloadavg()

    cpu_ns = ticks_to_ns(r1.ri_user_time + r1.ri_system_time) - ticks_to_ns(r0.ri_user_time + r0.ri_system_time)
    result = {
        "tool": "scripts/perf/macos-thread-wakeups.py",
        "tool_version": SCRIPT_VERSION,
        "label": args.label,
        "started_at": started.isoformat(timespec="seconds"),
        "host": {
            "os": f"macOS {platform.mac_ver()[0]}",
            "machine": platform.machine(),
            "loadavg_start": [round(x, 2) for x in load0],
            "loadavg_end": [round(x, 2) for x in load1],
            "mach_timebase": {"numer": NUMER, "denom": DENOM},
        },
        "pid": pid,
        "executable": proc_path(pid),
        "warmup_s": args.warmup,
        "seconds": round(dt, 1),
        "threads_at_end": len(prev),
        "threads_seen": len(seen),
        "interrupt_wakeups_per_s": round((r1.ri_interrupt_wkups - r0.ri_interrupt_wkups) / dt, 3),
        "pkg_idle_wakeups_per_s": round((r1.ri_pkg_idle_wkups - r0.ri_pkg_idle_wkups) / dt, 3),
        # rusage 的 Mach tick 已按 mach_timebase_info 换算
        "cpu_ms": round(cpu_ns / 1e6, 3),
        "cpu_percent": round(cpu_ns / 1e9 / dt * 100, 4),
        "phys_footprint_mib": round(r1.ri_phys_footprint / 1048576, 1),
        # tid = 线程 id（PROC_PIDLISTTHREADIDS），与 `sample` 的 Thread_<id> 一致；cpu_us 来自 pth_*_time（纳秒）
        "active_threads": sorted(
            (
                {"tid": tid, "name": v[0], "active_s": v[1], "cpu_us": round(v[2] / 1e3, 1)}
                for tid, v in active.items()
            ),
            key=lambda x: (-x["active_s"], -x["cpu_us"]),
        ),
    }
    text = json.dumps(result, ensure_ascii=False, indent=1)
    if args.out:
        with open(args.out, "w", encoding="utf-8") as f:
            f.write(text + "\n")
    else:
        print(text)
    return 0


if __name__ == "__main__":
    sys.exit(main())
