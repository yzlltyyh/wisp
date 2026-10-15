"""scripts/perf/macos-webkit.py：找出属于某个进程的 WebKit XPC 进程（measure-macos.sh 调用）。

WebKit 的 XPC 进程（com.apple.WebKit.WebContent / GPU / Networking …）的父进程是 launchd，只能按「负责进程」
（libSystem 的 responsibility_get_pid_responsible_for_pid，无需 sudo）归属：
- 经 LaunchServices 启动的应用（Finder、open、登录项）是它自己的负责进程，WebKit 进程的负责进程也是它 → 精确匹配；
- 从终端 / IDE 直接执行可执行文件时，应用与它的 WebKit 进程的负责进程都是终端 → 只能近似匹配：负责进程相同，
  且启动时间不早于应用（留 1 秒余量）。同一终端里之后启动的其他 WebKit 应用会被一并算进来。

用法：/usr/bin/python3 -I scripts/perf/macos-webkit.py <pid>
输出一行 `<mode> <owner> <pids>`：
  mode   exact（精确）| approx（近似）
  owner  负责进程的 pid（取不到时为 -）
  pids   逗号分隔的 WebKit 进程 pid，没有时为 -
进程不存在时以 2 退出。

      /usr/bin/python3 -I scripts/perf/macos-webkit.py --footprint <pid,pid,…>
输出一行 `pid:KiB,…`：各进程的 phys_footprint（proc_pid_rusage 的 ri_phys_footprint，与活动监视器的「内存」、
`footprint` 工具同一口径，不含共享的系统框架页）。已退出的进程跳过，一个都取不到时输出 -。
同一用户的进程不需要 sudo。

      /usr/bin/python3 -I scripts/perf/macos-webkit.py --scan <pid>
一次调用完成上面两步（run-smoke.mjs 的逐秒采样用，省一次解释器启动）：输出一行 `<mode> <owner> <pids> <pid:KiB,…>`，
第四列是这些 WebKit 进程的 phys_footprint（一个都取不到时为 -）。进程不存在时以 2 退出。

只用系统自带 python3 的标准库（ctypes）。
"""

import ctypes
import os
import struct
import subprocess
import sys

# <sys/proc_info.h>：PROC_PIDTBSDINFO 返回 struct proc_bsdinfo（136 字节），pbi_start_tvsec / tvusec 在偏移 120
PROC_PIDTBSDINFO = 3
BSDINFO_SIZE = 136
START_OFFSET = 120
# 近似匹配时 WebKit 进程允许早于应用启动的余量（秒）
SLACK_S = 1.0
WEBKIT_PREFIX = "com.apple.WebKit."

_lib = ctypes.CDLL("/usr/lib/libSystem.B.dylib")
_responsible = _lib.responsibility_get_pid_responsible_for_pid
_responsible.restype = ctypes.c_int
_responsible.argtypes = [ctypes.c_int]
_pidinfo = _lib.proc_pidinfo
_pidinfo.restype = ctypes.c_int
_pidinfo.argtypes = [ctypes.c_int, ctypes.c_int, ctypes.c_uint64, ctypes.c_void_p, ctypes.c_int]
_rusage = _lib.proc_pid_rusage
_rusage.restype = ctypes.c_int
_rusage.argtypes = [ctypes.c_int, ctypes.c_int, ctypes.c_void_p]

# <sys/resource.h>：RUSAGE_INFO_V2；ri_phys_footprint 是 uuid[16] 之后第 8 个 uint64
RUSAGE_INFO_V2 = 2


class RusageInfoV2(ctypes.Structure):
    _fields_ = [("uuid", ctypes.c_uint8 * 16)] + [
        (name, ctypes.c_uint64)
        for name in (
            "user_time system_time pkg_idle_wkups interrupt_wkups pageins wired_size resident_size "
            "phys_footprint proc_start_abstime proc_exit_abstime child_user_time child_system_time "
            "child_pkg_idle_wkups child_interrupt_wkups child_pageins child_elapsed_abstime "
            "diskio_bytesread diskio_byteswritten"
        ).split()
    ]


def footprint_kib(pid):
    """phys_footprint（KiB）；进程不存在或无权读取时为 None"""
    info = RusageInfoV2()
    if _rusage(pid, RUSAGE_INFO_V2, ctypes.byref(info)) != 0:
        return None
    return info.phys_footprint // 1024


def responsible(pid):
    """负责进程的 pid；取不到时为 None"""
    r = _responsible(pid)
    return r if r > 0 else None


def start_time(pid):
    """进程启动时间（Unix 秒）；进程不存在或无权读取时为 None"""
    buf = ctypes.create_string_buffer(BSDINFO_SIZE)
    if _pidinfo(pid, PROC_PIDTBSDINFO, 0, buf, BSDINFO_SIZE) != BSDINFO_SIZE:
        return None
    sec, usec = struct.unpack_from("<QQ", buf, START_OFFSET)
    return sec + usec / 1e6


def webkit_pids():
    """全部 com.apple.WebKit.* 进程（ps 的 comm 是可执行文件的完整路径）"""
    out = subprocess.run(
        ["/bin/ps", "-axo", "pid=,comm="], capture_output=True, text=True, check=True
    ).stdout
    for line in out.splitlines():
        pid, _, comm = line.strip().partition(" ")
        if pid.isdigit() and os.path.basename(comm.strip()).startswith(WEBKIT_PREFIX):
            yield int(pid)


def footprints(pids):
    """`pid:KiB,…`（已退出 / 无权读取的进程跳过）；一个都取不到时为 -"""
    out = []
    for p in pids:
        kib = footprint_kib(p)
        if kib is not None:
            out.append(f"{p}:{kib}")
    return ",".join(out) or "-"


def main():
    if len(sys.argv) == 3 and sys.argv[1] == "--footprint":
        print(footprints(int(p) for p in sys.argv[2].split(",") if p.isdigit()))
        return 0
    scan = len(sys.argv) == 3 and sys.argv[1] == "--scan"
    if scan:
        sys.argv = [sys.argv[0], sys.argv[2]]
    if len(sys.argv) != 2 or not sys.argv[1].isdigit():
        print("用法：macos-webkit.py <pid> | --scan <pid> | --footprint <pid,…>", file=sys.stderr)
        return 64
    pid = int(sys.argv[1])
    started = start_time(pid)
    if started is None:
        return 2
    owner = responsible(pid)
    exact = owner == pid
    found = []
    for w in webkit_pids():
        r = responsible(w)
        if r is None or owner is None or r != owner:
            continue
        if exact:
            found.append(w)
        else:
            t = start_time(w)
            if t is not None and t >= started - SLACK_S:
                found.append(w)
    found.sort()
    pids = ",".join(str(p) for p in found) or "-"
    fields = ["exact" if exact else "approx", str(owner) if owner is not None else "-", pids]
    if scan:
        fields.append(footprints(found))
    print(" ".join(fields))
    return 0


if __name__ == "__main__":
    sys.exit(main())
