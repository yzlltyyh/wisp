//! 会话总线的最小客户端：连接、SASL EXTERNAL 认证、方法调用（带超时）与信号分发。
//!
//! 只服务于外观设置（xdg-desktop-portal Settings）。读取在独立线程上阻塞进行（空闲时零 CPU），
//! 方法调用通过 channel 等待回复并有硬超时；总线断开后所有调用立即失败，不重连（会话总线与登录会话同生命周期）。

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use super::dbus_wire::{self, Message, MessageType, Value};

/// 信号回调
pub type SignalHandler = Box<dyn Fn(&Message) + Send + Sync>;

/// 已认证的总线连接
pub struct Bus {
    writer: Mutex<UnixStream>,
    serial: AtomicU32,
    pending: Mutex<HashMap<u32, mpsc::Sender<Message>>>,
    alive: AtomicBool,
}

/// 调用失败
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallError {
    /// 总线已断开
    Disconnected,
    /// 超时未收到回复
    Timeout,
    /// 对端返回 D-Bus 错误（错误名）
    Remote(String),
    /// 本地编码 / IO 错误
    Local(String),
}

impl Bus {
    /// 连接会话总线并完成认证与 Hello；`on_signal` 在读取线程上被调用（只做 O(1) 工作）
    pub fn connect_session(timeout: Duration, on_signal: SignalHandler) -> Result<Arc<Bus>, String> {
        let stream = open_session_socket()?;
        stream.set_read_timeout(Some(timeout)).map_err(|e| e.to_string())?;
        stream.set_write_timeout(Some(timeout)).map_err(|e| e.to_string())?;
        let mut reader = BufReader::new(stream.try_clone().map_err(|e| e.to_string())?);
        let mut writer = stream;
        authenticate(&mut reader, &mut writer)?;
        // 握手之后读取线程长期阻塞读取；写超时保留，防止对端卡死时调用方被无限阻塞
        reader.get_ref().set_read_timeout(None).map_err(|e| e.to_string())?;
        let bus = Arc::new(Bus {
            writer: Mutex::new(writer),
            serial: AtomicU32::new(1),
            pending: Mutex::new(HashMap::new()),
            alive: AtomicBool::new(true),
        });
        let weak = Arc::downgrade(&bus);
        std::thread::Builder::new()
            .name("wisp-dbus".into())
            .spawn(move || read_loop(reader, &weak, &on_signal))
            .map_err(|e| e.to_string())?;
        bus.call(
            "org.freedesktop.DBus",
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
            "Hello",
            &[],
            timeout,
        )
        .map_err(|e| format!("Hello failed: {e:?}"))?;
        Ok(bus)
    }

    /// 方法调用，等待回复（硬超时）
    pub fn call(
        &self,
        destination: &str,
        path: &str,
        interface: &str,
        member: &str,
        body: &[Value],
        timeout: Duration,
    ) -> Result<Message, CallError> {
        if !self.alive.load(Ordering::Acquire) {
            return Err(CallError::Disconnected);
        }
        let serial = self.serial.fetch_add(1, Ordering::Relaxed).max(1);
        let bytes = dbus_wire::encode_method_call(serial, destination, path, interface, member, body, 0)
            .map_err(|e| CallError::Local(e.to_string()))?;
        let (tx, rx) = mpsc::channel();
        self.pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(serial, tx);
        let written = self
            .writer
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .write_all(&bytes);
        if let Err(e) = written {
            self.forget(serial);
            return Err(CallError::Local(e.to_string()));
        }
        let reply = rx.recv_timeout(timeout);
        self.forget(serial);
        match reply {
            Ok(msg) if msg.kind == Some(MessageType::Error) => {
                Err(CallError::Remote(msg.error_name.unwrap_or_default()))
            }
            Ok(msg) => Ok(msg),
            Err(mpsc::RecvTimeoutError::Timeout) => Err(CallError::Timeout),
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(CallError::Disconnected),
        }
    }

    /// 订阅信号（AddMatch）
    pub fn add_match(&self, rule: &str, timeout: Duration) -> Result<(), CallError> {
        self.call(
            "org.freedesktop.DBus",
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
            "AddMatch",
            &[Value::Str(rule.to_owned())],
            timeout,
        )
        .map(|_| ())
    }

    fn forget(&self, serial: u32) {
        self.pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&serial);
    }
}

fn read_loop(mut reader: BufReader<UnixStream>, bus: &std::sync::Weak<Bus>, on_signal: &SignalHandler) {
    loop {
        let msg = match read_message(&mut reader) {
            Ok(m) => m,
            Err(e) => {
                log::debug!("D-Bus 连接结束：{e}");
                break;
            }
        };
        let Some(bus) = bus.upgrade() else { break };
        match msg.kind {
            Some(MessageType::MethodReturn | MessageType::Error) => {
                if let Some(reply_to) = msg.reply_serial {
                    let tx = bus
                        .pending
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .remove(&reply_to);
                    if let Some(tx) = tx {
                        let _ = tx.send(msg);
                    }
                }
            }
            Some(MessageType::Signal) => on_signal(&msg),
            _ => {}
        }
    }
    if let Some(bus) = bus.upgrade() {
        bus.alive.store(false, Ordering::Release);
        // 丢弃全部等待者：它们的 recv 立即返回 Disconnected
        bus.pending.lock().unwrap_or_else(PoisonError::into_inner).clear();
    }
}

fn read_message(reader: &mut BufReader<UnixStream>) -> Result<Message, String> {
    let mut prefix = [0u8; 16];
    reader.read_exact(&mut prefix).map_err(|e| e.to_string())?;
    let total = dbus_wire::message_len(&prefix).map_err(|e| e.to_string())?;
    let mut buf = vec![0u8; total];
    buf[..16].copy_from_slice(&prefix);
    reader.read_exact(&mut buf[16..]).map_err(|e| e.to_string())?;
    dbus_wire::decode_message(&buf).map_err(|e| e.to_string())
}

fn authenticate(reader: &mut BufReader<UnixStream>, writer: &mut UnixStream) -> Result<(), String> {
    let uid = current_uid().ok_or("cannot determine uid")?;
    writer.write_all(b"\0").map_err(|e| e.to_string())?;
    writer
        .write_all(dbus_wire::auth_external_line(uid).as_bytes())
        .map_err(|e| e.to_string())?;
    let mut line = String::new();
    reader.read_line(&mut line).map_err(|e| e.to_string())?;
    if !line.starts_with("OK ") {
        return Err(format!("auth rejected: {}", line.trim()));
    }
    writer.write_all(b"BEGIN\r\n").map_err(|e| e.to_string())
}

/// 当前进程的 uid（不依赖 libc：/proc/self 的属主即为进程的有效 uid）
fn current_uid() -> Option<u32> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata("/proc/self").ok().map(|m| m.uid())
}

fn open_session_socket() -> Result<UnixStream, String> {
    let mut last_err = String::from("DBUS_SESSION_BUS_ADDRESS is not set");
    if let Ok(addr) = std::env::var("DBUS_SESSION_BUS_ADDRESS") {
        for entry in parse_addresses(&addr) {
            match connect_entry(&entry) {
                Ok(s) => return Ok(s),
                Err(e) => last_err = e,
            }
        }
    }
    // 回退：systemd 用户会话的默认位置
    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR") {
        let path = std::path::Path::new(&dir).join("bus");
        if let Ok(s) = UnixStream::connect(&path) {
            return Ok(s);
        }
    }
    Err(last_err)
}

/// 地址项：`unix:path=…` 或 `unix:abstract=…`
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BusAddress {
    Path(String),
    Abstract(String),
}

/// 解析 `DBUS_SESSION_BUS_ADDRESS`（分号分隔的多个地址，值可能有 %xx 转义）；只保留 unix 传输
pub fn parse_addresses(raw: &str) -> Vec<BusAddress> {
    let mut out = Vec::new();
    for entry in raw.split(';') {
        let Some(params) = entry.trim().strip_prefix("unix:") else {
            continue;
        };
        for kv in params.split(',') {
            let Some((k, v)) = kv.split_once('=') else { continue };
            let v = unescape(v);
            match k {
                "path" => out.push(BusAddress::Path(v)),
                "abstract" => out.push(BusAddress::Abstract(v)),
                _ => {}
            }
        }
    }
    out
}

fn unescape(v: &str) -> String {
    let bytes = v.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let (Some(h), Some(l)) = (
                bytes.get(i + 1).copied().and_then(hex),
                bytes.get(i + 2).copied().and_then(hex),
            )
        {
            out.push(h << 4 | l);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

fn connect_entry(entry: &BusAddress) -> Result<UnixStream, String> {
    match entry {
        BusAddress::Path(p) => UnixStream::connect(p).map_err(|e| format!("{p}: {e}")),
        BusAddress::Abstract(name) => connect_abstract(name),
    }
}

#[cfg(target_os = "linux")]
fn connect_abstract(name: &str) -> Result<UnixStream, String> {
    use std::os::linux::net::SocketAddrExt;
    let addr = std::os::unix::net::SocketAddr::from_abstract_name(name.as_bytes()).map_err(|e| e.to_string())?;
    UnixStream::connect_addr(&addr).map_err(|e| format!("@{name}: {e}"))
}

#[cfg(not(target_os = "linux"))]
fn connect_abstract(name: &str) -> Result<UnixStream, String> {
    Err(format!("abstract socket @{name} is Linux-only"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_bus_addresses() {
        assert_eq!(
            parse_addresses("unix:path=/run/user/1000/bus"),
            vec![BusAddress::Path("/run/user/1000/bus".into())]
        );
        assert_eq!(
            parse_addresses("unix:abstract=/tmp/dbus-AbC,guid=0123;tcp:host=localhost,port=1;unix:path=/tmp/a%20b"),
            vec![
                BusAddress::Abstract("/tmp/dbus-AbC".into()),
                BusAddress::Path("/tmp/a b".into())
            ]
        );
        assert!(parse_addresses("").is_empty());
        assert_eq!(parse_addresses("unix:path=/x%2"), vec![BusAddress::Path("/x%2".into())]);
    }
}
