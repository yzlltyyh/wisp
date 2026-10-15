#!/usr/bin/env python3
"""scripts/ci/sni-watcher.py：CI 的 Linux 测量用的最小 StatusNotifierWatcher。

GitHub 的 Linux runner 没有桌面环境，会话总线上没有 org.kde.StatusNotifierWatcher（托盘宿主）。Wisp 启动时查不到它就走
「托盘不可用 → 打开设置窗口」的退化路径（tray::verify_host），设置窗口的 WebView 会混进空闲测量。这个脚本在
会话总线上占用这个名字，实现 libappindicator 用到的最小接口（注册条目 / 宿主、三个属性、两个信号），让 Wisp 照常只建
托盘图标。没有任何显示——只为测量。

用法（在 dbus-run-session 里、启动 Wisp 之前）：
    python3 scripts/ci/sni-watcher.py &
需要 python3-gi（gir1.2-glib-2.0）。取得名字后在标准输出打印一行 `sni-watcher: ready`；名字被占用或丢失时以 1 退出；
SIGTERM / SIGINT 正常退出。
"""

import signal
import sys

from gi.repository import Gio, GLib

NAME = "org.kde.StatusNotifierWatcher"
PATH = "/StatusNotifierWatcher"
XML = """
<node>
  <interface name="org.kde.StatusNotifierWatcher">
    <method name="RegisterStatusNotifierItem"><arg name="service" type="s" direction="in"/></method>
    <method name="RegisterStatusNotifierHost"><arg name="service" type="s" direction="in"/></method>
    <property name="RegisteredStatusNotifierItems" type="as" access="read"/>
    <property name="IsStatusNotifierHostRegistered" type="b" access="read"/>
    <property name="ProtocolVersion" type="i" access="read"/>
    <signal name="StatusNotifierItemRegistered"><arg name="service" type="s"/></signal>
    <signal name="StatusNotifierItemUnregistered"><arg name="service" type="s"/></signal>
    <signal name="StatusNotifierHostRegistered"/>
  </interface>
</node>
"""

items = []
loop = GLib.MainLoop()
exit_code = 0


def on_call(conn, sender, path, iface, method, params, invocation):
    if method == "RegisterStatusNotifierItem":
        (service,) = params.unpack()
        # libappindicator 传对象路径（/org/ayatana/…）或总线名：路径前面补上调用方的唯一名
        entry = sender + service if service.startswith("/") else service
        if entry not in items:
            items.append(entry)
        conn.emit_signal(None, PATH, iface, "StatusNotifierItemRegistered", GLib.Variant("(s)", (entry,)))
        print(f"sni-watcher: registered {entry}", flush=True)
        invocation.return_value(None)
    elif method == "RegisterStatusNotifierHost":
        invocation.return_value(None)
    else:
        invocation.return_dbus_error("org.freedesktop.DBus.Error.UnknownMethod", method)


def on_get(conn, sender, path, iface, prop):
    if prop == "RegisteredStatusNotifierItems":
        return GLib.Variant("as", items)
    if prop == "IsStatusNotifierHostRegistered":
        return GLib.Variant("b", True)
    if prop == "ProtocolVersion":
        return GLib.Variant("i", 0)
    return None


def on_bus(conn, name):
    info = Gio.DBusNodeInfo.new_for_xml(XML).interfaces[0]
    conn.register_object(PATH, info, on_call, on_get, None)


def on_acquired(conn, name):
    print("sni-watcher: ready", flush=True)


def on_lost(conn, name):
    global exit_code
    print(f"sni-watcher: cannot own {name} (no session bus, or another watcher is running)", file=sys.stderr, flush=True)
    exit_code = 1
    loop.quit()


def main():
    Gio.bus_own_name(Gio.BusType.SESSION, NAME, Gio.BusNameOwnerFlags.NONE, on_bus, on_acquired, on_lost)
    for sig in (signal.SIGTERM, signal.SIGINT):
        GLib.unix_signal_add(GLib.PRIORITY_DEFAULT, sig, loop.quit)
    loop.run()
    return exit_code


if __name__ == "__main__":
    sys.exit(main())
