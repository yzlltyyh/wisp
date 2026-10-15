//! 最小的 D-Bus 线协议实现（纯逻辑，只依赖 std）：签名解析、值的编解码、消息头编解码。
//!
//! Wisp 在 Linux 上只需要读 xdg-desktop-portal 的 Settings（明暗、强调色、动效、对比度）并监听其
//! `SettingChanged` 信号。为这点需求引入 zbus（连同异步运行时）不划算，而且 wisp-platform 只能使用工作区已锁定的
//! 依赖；因此这里按 D-Bus 规范实现够用的子集：全部基本类型、数组、结构体、字典项、variant 的编解码（大小端皆可读），
//! 以及 METHOD_CALL 的编码与任意消息的解码。传输与认证在 `dbus.rs`。

use std::fmt;

/// 嵌套深度上限（规范允许 32 层数组 + 32 层结构体，这里取总深度 64，防御恶意数据）
const MAX_DEPTH: usize = 64;
/// 单条消息的上限（规范为 128 MiB；portal 的设置远小于此，取 16 MiB 足够）
pub const MAX_MESSAGE_LEN: usize = 16 * 1024 * 1024;

/// 编解码错误
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WireError(pub String);

impl fmt::Display for WireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "dbus wire: {}", self.0)
    }
}

impl std::error::Error for WireError {}

fn err<T>(msg: impl Into<String>) -> Result<T, WireError> {
    Err(WireError(msg.into()))
}

/// 单个完整类型
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ty {
    Byte,
    Bool,
    I16,
    U16,
    I32,
    U32,
    I64,
    U64,
    Double,
    Str,
    Path,
    Sig,
    UnixFd,
    Variant,
    Array(Box<Ty>),
    Struct(Vec<Ty>),
    DictEntry(Box<Ty>, Box<Ty>),
}

impl Ty {
    /// 对齐字节数
    fn align(&self) -> usize {
        match self {
            Ty::Byte | Ty::Sig | Ty::Variant => 1,
            Ty::I16 | Ty::U16 => 2,
            Ty::Bool | Ty::I32 | Ty::U32 | Ty::Str | Ty::Path | Ty::UnixFd | Ty::Array(_) => 4,
            Ty::I64 | Ty::U64 | Ty::Double | Ty::Struct(_) | Ty::DictEntry(..) => 8,
        }
    }

    /// 输出签名
    pub fn signature(&self) -> String {
        let mut s = String::new();
        self.write_sig(&mut s);
        s
    }

    fn write_sig(&self, out: &mut String) {
        match self {
            Ty::Byte => out.push('y'),
            Ty::Bool => out.push('b'),
            Ty::I16 => out.push('n'),
            Ty::U16 => out.push('q'),
            Ty::I32 => out.push('i'),
            Ty::U32 => out.push('u'),
            Ty::I64 => out.push('x'),
            Ty::U64 => out.push('t'),
            Ty::Double => out.push('d'),
            Ty::Str => out.push('s'),
            Ty::Path => out.push('o'),
            Ty::Sig => out.push('g'),
            Ty::UnixFd => out.push('h'),
            Ty::Variant => out.push('v'),
            Ty::Array(inner) => {
                out.push('a');
                inner.write_sig(out);
            }
            Ty::Struct(fields) => {
                out.push('(');
                for f in fields {
                    f.write_sig(out);
                }
                out.push(')');
            }
            Ty::DictEntry(k, v) => {
                out.push('{');
                k.write_sig(out);
                v.write_sig(out);
                out.push('}');
            }
        }
    }
}

/// 解析签名（可能包含多个完整类型）
pub fn parse_signature(sig: &str) -> Result<Vec<Ty>, WireError> {
    let bytes = sig.as_bytes();
    if bytes.len() > 255 {
        return err("signature too long");
    }
    let mut pos = 0;
    let mut out = Vec::new();
    while pos < bytes.len() {
        out.push(parse_one(bytes, &mut pos, 0)?);
    }
    Ok(out)
}

fn parse_one(b: &[u8], pos: &mut usize, depth: usize) -> Result<Ty, WireError> {
    if depth > MAX_DEPTH {
        return err("signature nested too deeply");
    }
    let Some(&c) = b.get(*pos) else {
        return err("unexpected end of signature");
    };
    *pos += 1;
    Ok(match c {
        b'y' => Ty::Byte,
        b'b' => Ty::Bool,
        b'n' => Ty::I16,
        b'q' => Ty::U16,
        b'i' => Ty::I32,
        b'u' => Ty::U32,
        b'x' => Ty::I64,
        b't' => Ty::U64,
        b'd' => Ty::Double,
        b's' => Ty::Str,
        b'o' => Ty::Path,
        b'g' => Ty::Sig,
        b'h' => Ty::UnixFd,
        b'v' => Ty::Variant,
        b'a' => {
            if b.get(*pos) == Some(&b'{') {
                *pos += 1;
                let k = parse_one(b, pos, depth + 1)?;
                if matches!(k, Ty::Array(_) | Ty::Struct(_) | Ty::DictEntry(..) | Ty::Variant) {
                    return err("dict key must be a basic type");
                }
                let v = parse_one(b, pos, depth + 1)?;
                if b.get(*pos) != Some(&b'}') {
                    return err("unterminated dict entry");
                }
                *pos += 1;
                Ty::Array(Box::new(Ty::DictEntry(Box::new(k), Box::new(v))))
            } else {
                Ty::Array(Box::new(parse_one(b, pos, depth + 1)?))
            }
        }
        b'(' => {
            let mut fields = Vec::new();
            loop {
                match b.get(*pos) {
                    Some(b')') => {
                        *pos += 1;
                        break;
                    }
                    Some(_) => fields.push(parse_one(b, pos, depth + 1)?),
                    None => return err("unterminated struct"),
                }
            }
            if fields.is_empty() {
                return err("empty struct");
            }
            Ty::Struct(fields)
        }
        other => return err(format!("unknown type code {:?}", other as char)),
    })
}

/// D-Bus 值
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Byte(u8),
    Bool(bool),
    I16(i16),
    U16(u16),
    I32(i32),
    U32(u32),
    I64(i64),
    U64(u64),
    Double(f64),
    Str(String),
    Path(String),
    Sig(String),
    UnixFd(u32),
    Variant(Box<Value>),
    /// 元素类型 + 元素（元素类型用于空数组的签名）
    Array(Ty, Vec<Value>),
    Struct(Vec<Value>),
    DictEntry(Box<Value>, Box<Value>),
}

impl Value {
    /// 值的类型
    pub fn ty(&self) -> Ty {
        match self {
            Value::Byte(_) => Ty::Byte,
            Value::Bool(_) => Ty::Bool,
            Value::I16(_) => Ty::I16,
            Value::U16(_) => Ty::U16,
            Value::I32(_) => Ty::I32,
            Value::U32(_) => Ty::U32,
            Value::I64(_) => Ty::I64,
            Value::U64(_) => Ty::U64,
            Value::Double(_) => Ty::Double,
            Value::Str(_) => Ty::Str,
            Value::Path(_) => Ty::Path,
            Value::Sig(_) => Ty::Sig,
            Value::UnixFd(_) => Ty::UnixFd,
            Value::Variant(_) => Ty::Variant,
            Value::Array(t, _) => Ty::Array(Box::new(t.clone())),
            Value::Struct(fields) => Ty::Struct(fields.iter().map(Value::ty).collect()),
            Value::DictEntry(k, v) => Ty::DictEntry(Box::new(k.ty()), Box::new(v.ty())),
        }
    }

    /// 字符串数组 `as`
    pub fn str_array<I: IntoIterator<Item = S>, S: Into<String>>(items: I) -> Self {
        Value::Array(Ty::Str, items.into_iter().map(|s| Value::Str(s.into())).collect())
    }

    /// 去掉任意层 variant 包装
    pub fn unwrap_variant(&self) -> &Value {
        let mut v = self;
        while let Value::Variant(inner) = v {
            v = inner;
        }
        v
    }

    /// 取字符串（含 object path / signature）
    pub fn as_str(&self) -> Option<&str> {
        match self.unwrap_variant() {
            Value::Str(s) | Value::Path(s) | Value::Sig(s) => Some(s),
            _ => None,
        }
    }

    /// 取无符号整数（宽松：接受各种整数类型）
    pub fn as_u32(&self) -> Option<u32> {
        match *self.unwrap_variant() {
            Value::Byte(v) => Some(u32::from(v)),
            Value::U16(v) => Some(u32::from(v)),
            Value::U32(v) => Some(v),
            Value::I16(v) => u32::try_from(v).ok(),
            Value::I32(v) => u32::try_from(v).ok(),
            Value::U64(v) => u32::try_from(v).ok(),
            Value::I64(v) => u32::try_from(v).ok(),
            _ => None,
        }
    }

    /// 取布尔
    pub fn as_bool(&self) -> Option<bool> {
        match *self.unwrap_variant() {
            Value::Bool(v) => Some(v),
            _ => None,
        }
    }

    /// 取浮点（宽松：整数也接受）
    pub fn as_f64(&self) -> Option<f64> {
        match *self.unwrap_variant() {
            Value::Double(v) => Some(v),
            Value::I32(v) => Some(f64::from(v)),
            Value::U32(v) => Some(f64::from(v)),
            _ => None,
        }
    }

    /// 取 `(ddd)`
    pub fn as_f64_triple(&self) -> Option<(f64, f64, f64)> {
        match self.unwrap_variant() {
            Value::Struct(f) if f.len() == 3 => Some((f[0].as_f64()?, f[1].as_f64()?, f[2].as_f64()?)),
            _ => None,
        }
    }

    /// 把 `a{kv}` 视为键值对迭代
    pub fn dict_entries(&self) -> impl Iterator<Item = (&Value, &Value)> {
        let items: &[Value] = match self.unwrap_variant() {
            Value::Array(_, items) => items,
            _ => &[],
        };
        items.iter().filter_map(|e| match e {
            Value::DictEntry(k, v) => Some((k.as_ref(), v.as_ref())),
            _ => None,
        })
    }
}

// ---------------------------------------------------------------- 编码

/// 小端编码器
pub struct Encoder {
    pub buf: Vec<u8>,
}

impl Encoder {
    pub fn new() -> Self {
        Self {
            buf: Vec::with_capacity(256),
        }
    }

    fn pad(&mut self, align: usize) {
        while !self.buf.len().is_multiple_of(align) {
            self.buf.push(0);
        }
    }

    /// 写入一个值（按自身类型对齐）
    pub fn write(&mut self, v: &Value) -> Result<(), WireError> {
        self.write_depth(v, 0)
    }

    fn write_depth(&mut self, v: &Value, depth: usize) -> Result<(), WireError> {
        if depth > MAX_DEPTH {
            return err("value nested too deeply");
        }
        self.pad(v.ty().align());
        match v {
            Value::Byte(x) => self.buf.push(*x),
            Value::Bool(x) => self.buf.extend(u32::from(*x).to_le_bytes()),
            Value::I16(x) => self.buf.extend(x.to_le_bytes()),
            Value::U16(x) => self.buf.extend(x.to_le_bytes()),
            Value::I32(x) => self.buf.extend(x.to_le_bytes()),
            Value::U32(x) | Value::UnixFd(x) => self.buf.extend(x.to_le_bytes()),
            Value::I64(x) => self.buf.extend(x.to_le_bytes()),
            Value::U64(x) => self.buf.extend(x.to_le_bytes()),
            Value::Double(x) => self.buf.extend(x.to_le_bytes()),
            Value::Str(s) | Value::Path(s) => {
                if s.as_bytes().contains(&0) {
                    return err("string contains NUL");
                }
                let len = u32::try_from(s.len()).map_err(|_| WireError("string too long".into()))?;
                self.buf.extend(len.to_le_bytes());
                self.buf.extend(s.as_bytes());
                self.buf.push(0);
            }
            Value::Sig(s) => self.write_sig(s)?,
            Value::Variant(inner) => {
                self.write_sig(&inner.ty().signature())?;
                self.write_depth(inner, depth + 1)?;
            }
            Value::Array(elem, items) => {
                let len_pos = self.buf.len();
                self.buf.extend([0u8; 4]);
                self.pad(elem.align());
                let start = self.buf.len();
                for item in items {
                    if &item.ty() != elem {
                        return err("array element type mismatch");
                    }
                    self.write_depth(item, depth + 1)?;
                }
                let len = u32::try_from(self.buf.len() - start).map_err(|_| WireError("array too long".into()))?;
                self.buf[len_pos..len_pos + 4].copy_from_slice(&len.to_le_bytes());
            }
            Value::Struct(fields) => {
                for f in fields {
                    self.write_depth(f, depth + 1)?;
                }
            }
            Value::DictEntry(k, val) => {
                self.write_depth(k, depth + 1)?;
                self.write_depth(val, depth + 1)?;
            }
        }
        Ok(())
    }

    fn write_sig(&mut self, s: &str) -> Result<(), WireError> {
        let len = u8::try_from(s.len()).map_err(|_| WireError("signature too long".into()))?;
        self.buf.push(len);
        self.buf.extend(s.as_bytes());
        self.buf.push(0);
        Ok(())
    }
}

impl Default for Encoder {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------- 解码

/// 解码器（支持大小端）
pub struct Decoder<'a> {
    data: &'a [u8],
    pos: usize,
    big_endian: bool,
}

impl<'a> Decoder<'a> {
    /// `data` 必须从消息起始处开始（对齐以消息起点计算）
    pub fn new(data: &'a [u8], big_endian: bool) -> Self {
        Self {
            data,
            pos: 0,
            big_endian,
        }
    }

    /// 当前位置
    pub fn pos(&self) -> usize {
        self.pos
    }

    /// 跳到指定位置
    pub fn seek(&mut self, pos: usize) {
        self.pos = pos;
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], WireError> {
        let end = self.pos.checked_add(n).ok_or_else(|| WireError("overflow".into()))?;
        let out = self
            .data
            .get(self.pos..end)
            .ok_or_else(|| WireError("unexpected end of data".into()))?;
        self.pos = end;
        Ok(out)
    }

    /// 按对齐跳过填充
    pub fn align(&mut self, align: usize) -> Result<(), WireError> {
        let pad = (align - self.pos % align) % align;
        self.take(pad).map(|_| ())
    }

    fn arr<const N: usize>(&mut self) -> Result<[u8; N], WireError> {
        let b = self.take(N)?;
        let mut out = [0u8; N];
        out.copy_from_slice(b);
        Ok(out)
    }

    fn u16(&mut self) -> Result<u16, WireError> {
        let b = self.arr::<2>()?;
        Ok(if self.big_endian {
            u16::from_be_bytes(b)
        } else {
            u16::from_le_bytes(b)
        })
    }

    /// 读 u32（调用方负责对齐）
    pub fn u32(&mut self) -> Result<u32, WireError> {
        let b = self.arr::<4>()?;
        Ok(if self.big_endian {
            u32::from_be_bytes(b)
        } else {
            u32::from_le_bytes(b)
        })
    }

    fn u64(&mut self) -> Result<u64, WireError> {
        let b = self.arr::<8>()?;
        Ok(if self.big_endian {
            u64::from_be_bytes(b)
        } else {
            u64::from_le_bytes(b)
        })
    }

    fn string(&mut self) -> Result<String, WireError> {
        let len = self.u32()? as usize;
        let bytes = self.take(len)?;
        if self.take(1)? != [0] {
            return err("string not NUL-terminated");
        }
        String::from_utf8(bytes.to_vec()).map_err(|_| WireError("string is not UTF-8".into()))
    }

    fn signature(&mut self) -> Result<String, WireError> {
        let len = self.take(1)?[0] as usize;
        let bytes = self.take(len)?;
        if self.take(1)? != [0] {
            return err("signature not NUL-terminated");
        }
        String::from_utf8(bytes.to_vec()).map_err(|_| WireError("signature is not ASCII".into()))
    }

    /// 按类型读一个值
    pub fn read(&mut self, ty: &Ty) -> Result<Value, WireError> {
        self.read_depth(ty, 0)
    }

    fn read_depth(&mut self, ty: &Ty, depth: usize) -> Result<Value, WireError> {
        if depth > MAX_DEPTH {
            return err("value nested too deeply");
        }
        self.align(ty.align())?;
        Ok(match ty {
            Ty::Byte => Value::Byte(self.take(1)?[0]),
            Ty::Bool => match self.u32()? {
                0 => Value::Bool(false),
                1 => Value::Bool(true),
                _ => return err("invalid boolean"),
            },
            Ty::I16 => Value::I16(self.u16()? as i16),
            Ty::U16 => Value::U16(self.u16()?),
            Ty::I32 => Value::I32(self.u32()? as i32),
            Ty::U32 => Value::U32(self.u32()?),
            Ty::UnixFd => Value::UnixFd(self.u32()?),
            Ty::I64 => Value::I64(self.u64()? as i64),
            Ty::U64 => Value::U64(self.u64()?),
            Ty::Double => Value::Double(f64::from_bits(self.u64()?)),
            Ty::Str => Value::Str(self.string()?),
            Ty::Path => Value::Path(self.string()?),
            Ty::Sig => Value::Sig(self.signature()?),
            Ty::Variant => {
                let sig = self.signature()?;
                let mut types = parse_signature(&sig)?;
                if types.len() != 1 {
                    return err("variant must contain exactly one type");
                }
                let inner_ty = types.remove(0);
                Value::Variant(Box::new(self.read_depth(&inner_ty, depth + 1)?))
            }
            Ty::Array(elem) => {
                let len = self.u32()? as usize;
                if len > MAX_MESSAGE_LEN {
                    return err("array too long");
                }
                self.align(elem.align())?;
                let end = self.pos + len;
                if end > self.data.len() {
                    return err("array exceeds data");
                }
                let mut items = Vec::new();
                while self.pos < end {
                    items.push(self.read_depth(elem, depth + 1)?);
                }
                if self.pos != end {
                    return err("array length mismatch");
                }
                Value::Array((**elem).clone(), items)
            }
            Ty::Struct(fields) => {
                let mut out = Vec::with_capacity(fields.len());
                for f in fields {
                    out.push(self.read_depth(f, depth + 1)?);
                }
                Value::Struct(out)
            }
            Ty::DictEntry(k, v) => {
                let key = self.read_depth(k, depth + 1)?;
                let val = self.read_depth(v, depth + 1)?;
                Value::DictEntry(Box::new(key), Box::new(val))
            }
        })
    }
}

// ---------------------------------------------------------------- 消息

/// 消息类型
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageType {
    MethodCall,
    MethodReturn,
    Error,
    Signal,
}

/// 解码后的消息
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Message {
    pub kind: Option<MessageType>,
    pub serial: u32,
    pub path: Option<String>,
    pub interface: Option<String>,
    pub member: Option<String>,
    pub error_name: Option<String>,
    pub reply_serial: Option<u32>,
    pub destination: Option<String>,
    pub sender: Option<String>,
    pub signature: String,
    pub body: Vec<Value>,
}

/// 编码一个 METHOD_CALL（小端）
pub fn encode_method_call(
    serial: u32,
    destination: &str,
    path: &str,
    interface: &str,
    member: &str,
    body: &[Value],
    flags: u8,
) -> Result<Vec<u8>, WireError> {
    let mut body_enc = Encoder::new();
    let mut signature = String::new();
    for v in body {
        body_enc.write(v)?;
        signature.push_str(&v.ty().signature());
    }
    let mut fields = vec![
        header_field(1, Value::Path(path.to_owned())),
        header_field(2, Value::Str(interface.to_owned())),
        header_field(3, Value::Str(member.to_owned())),
        header_field(6, Value::Str(destination.to_owned())),
    ];
    if !signature.is_empty() {
        fields.push(header_field(8, Value::Sig(signature)));
    }
    let header_ty = Ty::Struct(vec![Ty::Byte, Ty::Variant]);
    let mut enc = Encoder::new();
    enc.buf.extend([b'l', 1, flags, 1]);
    let body_len = u32::try_from(body_enc.buf.len()).map_err(|_| WireError("body too long".into()))?;
    enc.buf.extend(body_len.to_le_bytes());
    enc.buf.extend(serial.to_le_bytes());
    enc.write(&Value::Array(header_ty, fields))?;
    enc.pad(8);
    enc.buf.extend(body_enc.buf);
    Ok(enc.buf)
}

fn header_field(code: u8, v: Value) -> Value {
    Value::Struct(vec![Value::Byte(code), Value::Variant(Box::new(v))])
}

/// 根据固定头部（前 16 字节）算出整条消息的长度
pub fn message_len(prefix: &[u8; 16]) -> Result<usize, WireError> {
    let big = match prefix[0] {
        b'l' => false,
        b'B' => true,
        _ => return err("invalid endianness"),
    };
    let rd = |b: [u8; 4]| if big { u32::from_be_bytes(b) } else { u32::from_le_bytes(b) } as usize;
    let body_len = rd([prefix[4], prefix[5], prefix[6], prefix[7]]);
    let fields_len = rd([prefix[12], prefix[13], prefix[14], prefix[15]]);
    let header = 16usize
        .checked_add(fields_len)
        .ok_or_else(|| WireError("overflow".into()))?;
    let header = header.div_ceil(8) * 8;
    let total = header
        .checked_add(body_len)
        .ok_or_else(|| WireError("overflow".into()))?;
    if total > MAX_MESSAGE_LEN {
        return err("message too long");
    }
    Ok(total)
}

/// 解码完整消息
pub fn decode_message(data: &[u8]) -> Result<Message, WireError> {
    if data.len() < 16 {
        return err("message too short");
    }
    let big = match data[0] {
        b'l' => false,
        b'B' => true,
        _ => return err("invalid endianness"),
    };
    let kind = match data[1] {
        1 => Some(MessageType::MethodCall),
        2 => Some(MessageType::MethodReturn),
        3 => Some(MessageType::Error),
        4 => Some(MessageType::Signal),
        _ => None,
    };
    let mut d = Decoder::new(data, big);
    d.seek(4);
    let body_len = d.u32()? as usize;
    let serial = d.u32()?;
    let fields = d.read(&Ty::Array(Box::new(Ty::Struct(vec![Ty::Byte, Ty::Variant]))))?;
    d.align(8)?;
    let body_start = d.pos();
    if body_start + body_len != data.len() {
        return err("body length mismatch");
    }
    let mut msg = Message {
        kind,
        serial,
        ..Default::default()
    };
    if let Value::Array(_, items) = fields {
        for item in items {
            let Value::Struct(parts) = item else { continue };
            let (Some(Value::Byte(code)), Some(value)) = (parts.first(), parts.get(1)) else {
                continue;
            };
            let value = value.unwrap_variant();
            match code {
                1 => msg.path = value.as_str().map(str::to_owned),
                2 => msg.interface = value.as_str().map(str::to_owned),
                3 => msg.member = value.as_str().map(str::to_owned),
                4 => msg.error_name = value.as_str().map(str::to_owned),
                5 => msg.reply_serial = value.as_u32(),
                6 => msg.destination = value.as_str().map(str::to_owned),
                7 => msg.sender = value.as_str().map(str::to_owned),
                8 => msg.signature = value.as_str().unwrap_or_default().to_owned(),
                _ => {}
            }
        }
    }
    if !msg.signature.is_empty() {
        for ty in parse_signature(&msg.signature)? {
            msg.body.push(d.read(&ty)?);
        }
    }
    Ok(msg)
}

/// SASL EXTERNAL 认证的首行：`AUTH EXTERNAL <uid 的十进制字符串的十六进制>`
pub fn auth_external_line(uid: u32) -> String {
    let hex: String = uid.to_string().bytes().map(|b| format!("{b:02x}")).collect();
    format!("AUTH EXTERNAL {hex}\r\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signature_roundtrip() {
        for sig in ["a{sa{sv}}", "(ddd)", "ssv", "as", "a(yv)", "u", "aay", "a{oa{sa{sv}}}"] {
            let parsed = parse_signature(sig).expect(sig);
            let back: String = parsed.iter().map(Ty::signature).collect();
            assert_eq!(back, sig);
        }
        assert!(parse_signature("a{vs}").is_err(), "字典键必须是基本类型");
        assert!(parse_signature("(").is_err());
        assert!(parse_signature("()").is_err());
        assert!(parse_signature("z").is_err());
    }

    fn sample_settings() -> Value {
        let inner =
            |k: &str, v: Value| Value::DictEntry(Box::new(Value::Str(k.into())), Box::new(Value::Variant(Box::new(v))));
        let ns = |name: &str, entries: Vec<Value>| {
            Value::DictEntry(
                Box::new(Value::Str(name.into())),
                Box::new(Value::Array(
                    Ty::DictEntry(Box::new(Ty::Str), Box::new(Ty::Variant)),
                    entries,
                )),
            )
        };
        Value::Array(
            Ty::DictEntry(
                Box::new(Ty::Str),
                Box::new(Ty::Array(Box::new(Ty::DictEntry(
                    Box::new(Ty::Str),
                    Box::new(Ty::Variant),
                )))),
            ),
            vec![
                ns(
                    "org.freedesktop.appearance",
                    vec![
                        inner("color-scheme", Value::U32(1)),
                        inner(
                            "accent-color",
                            Value::Struct(vec![Value::Double(0.2), Value::Double(0.5), Value::Double(0.9)]),
                        ),
                    ],
                ),
                ns(
                    "org.gnome.desktop.interface",
                    vec![inner("enable-animations", Value::Bool(false))],
                ),
                ns("empty", vec![]),
            ],
        )
    }

    #[test]
    fn value_roundtrip_through_encoder_and_decoder() {
        let v = sample_settings();
        let mut enc = Encoder::new();
        // 前置一个字节，验证对齐以消息起点计算
        enc.write(&Value::Byte(7)).unwrap();
        enc.write(&v).unwrap();
        let mut d = Decoder::new(&enc.buf, false);
        assert_eq!(d.read(&Ty::Byte).unwrap(), Value::Byte(7));
        let back = d.read(&v.ty()).unwrap();
        assert_eq!(back, v);
        let entries: Vec<_> = back.dict_entries().collect();
        assert_eq!(entries.len(), 3);
        let accent = entries[0]
            .1
            .dict_entries()
            .find(|(k, _)| k.as_str() == Some("accent-color"))
            .and_then(|(_, v)| v.as_f64_triple());
        assert_eq!(accent, Some((0.2, 0.5, 0.9)));
    }

    #[test]
    fn method_call_roundtrip() {
        let body = [Value::str_array(["org.freedesktop.appearance"])];
        let bytes = encode_method_call(
            9,
            "org.freedesktop.portal.Desktop",
            "/org/freedesktop/portal/desktop",
            "org.freedesktop.portal.Settings",
            "ReadAll",
            &body,
            0,
        )
        .unwrap();
        let prefix: [u8; 16] = bytes[..16].try_into().unwrap();
        assert_eq!(message_len(&prefix).unwrap(), bytes.len());
        let msg = decode_message(&bytes).unwrap();
        assert_eq!(msg.kind, Some(MessageType::MethodCall));
        assert_eq!(msg.serial, 9);
        assert_eq!(msg.member.as_deref(), Some("ReadAll"));
        assert_eq!(msg.destination.as_deref(), Some("org.freedesktop.portal.Desktop"));
        assert_eq!(msg.path.as_deref(), Some("/org/freedesktop/portal/desktop"));
        assert_eq!(msg.signature, "as");
        assert_eq!(msg.body, body);
    }

    /// 手工构造一条大端的 SIGNAL 消息，验证解码大端数据
    #[test]
    fn decodes_big_endian_signal() {
        // 先用小端编码出各部分，再逐字段换成大端（这里直接手写字节更直观）
        let mut m: Vec<u8> = vec![b'B', 4, 0, 1];
        let body: Vec<u8> = {
            let mut b = Vec::new();
            // s "ns"
            b.extend(2u32.to_be_bytes());
            b.extend(b"ns\0");
            b.push(0); // 对齐到 4
            // s "k"
            b.extend(1u32.to_be_bytes());
            b.extend(b"k\0");
            // v: 签名 "u" + u32
            b.push(1);
            b.extend(b"u\0");
            b.extend([0, 0, 0]); // 对齐：此时 pos = 4+3+1+4+2+3 = 17 → 补到 20
            b.extend(2u32.to_be_bytes());
            b
        };
        m.extend((body.len() as u32).to_be_bytes());
        m.extend(5u32.to_be_bytes());
        // 头字段：MEMBER "SettingChanged" 与 SIGNATURE "ssv"
        let mut fields: Vec<u8> = Vec::new();
        let fields_start = 16;
        // (y v) 结构体对齐 8：16 已对齐
        fields.push(3);
        fields.push(1);
        fields.extend(b"s\0");
        fields.extend(14u32.to_be_bytes());
        fields.extend(b"SettingChanged\0");
        while !(fields_start + fields.len()).is_multiple_of(8) {
            fields.push(0);
        }
        fields.push(8);
        fields.push(1);
        fields.extend(b"g\0");
        fields.push(3);
        fields.extend(b"ssv\0");
        m.extend((fields.len() as u32).to_be_bytes());
        m.extend(&fields);
        while !m.len().is_multiple_of(8) {
            m.push(0);
        }
        m.extend(&body);
        let prefix: [u8; 16] = m[..16].try_into().unwrap();
        assert_eq!(message_len(&prefix).unwrap(), m.len());
        let msg = decode_message(&m).unwrap();
        assert_eq!(msg.kind, Some(MessageType::Signal));
        assert_eq!(msg.member.as_deref(), Some("SettingChanged"));
        assert_eq!(msg.body.len(), 3);
        assert_eq!(msg.body[0].as_str(), Some("ns"));
        assert_eq!(msg.body[1].as_str(), Some("k"));
        assert_eq!(msg.body[2].as_u32(), Some(2));
    }

    #[test]
    fn rejects_truncated_and_hostile_input() {
        assert!(decode_message(&[0; 8]).is_err());
        assert!(decode_message(b"x\x01\x00\x01\0\0\0\0\0\0\0\0\0\0\0\0").is_err());
        let mut prefix = [0u8; 16];
        prefix[0] = b'l';
        prefix[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(message_len(&prefix).is_err());
        // 声称长度巨大的数组
        let mut enc = Encoder::new();
        enc.buf.extend(u32::MAX.to_le_bytes());
        let mut d = Decoder::new(&enc.buf, false);
        assert!(d.read(&Ty::Array(Box::new(Ty::Byte))).is_err());
    }

    #[test]
    fn auth_line_hex_encodes_uid_digits() {
        assert_eq!(auth_external_line(1000), "AUTH EXTERNAL 31303030\r\n");
        assert_eq!(auth_external_line(0), "AUTH EXTERNAL 30\r\n");
    }
}
