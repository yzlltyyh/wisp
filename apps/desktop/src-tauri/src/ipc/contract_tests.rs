//! 命令契约的 Rust ↔ TS 双向比对。事件名由 wisp-types 的 `event_names_match_frontend` 比对；
//! 这里比对命令：
//! 1. `commands!` 列表（[`super::COMMAND_NAMES`]）== 各 `cmd_*.rs` 里全部 `#[tauri::command]` 函数（没有漏注册的命令）；
//! 2. Rust 命令集合 == 前端 `COMMAND_NAMES`（commands.ts）== 前端 `CommandMap` 的键（含 `OverlayCommandMap`）；
//! 3. 每条命令的 Rust 参数名（去掉 State / WebviewWindow / AppHandle 等注入参数与 `on_event` 通道）
//!    == `CommandMap[cmd].args` 的键；
//! 4. 带 `on_event: Channel<…>` 的命令集合 == 前端 `StreamCommandName`；
//! 5. 每个命令都写成 `#[tauri::command(rename_all = "snake_case")]`，参数名不以 `_` 开头；
//! 6. 每个需要校验的参数（字符串、浮点数、集合、含这些字段的 DTO；见 [`BOUNDED`]）都出现在函数体里某个
//!    `check_*(…)` / `validate_*(…)` 调用的实参中（长度上限与有限性在 ipc 层校验）。
//!
//! 源码通过 `include_str!` 读入（文件改动会触发重新编译）。解析器只认本仓库的写法：Rust 函数签名
//! `pub async fn name(param: Type, …)`，TS 接口条目 `name: { args: { k: T; … } | NoArgs | Record<string, never>; ret: … }`。

use std::collections::{BTreeMap, BTreeSet};

/// 各命令文件的源码
const RUST_SOURCES: &[(&str, &str)] = &[
    ("cmd_app.rs", include_str!("cmd_app.rs")),
    ("cmd_clipboard.rs", include_str!("cmd_clipboard.rs")),
    ("cmd_hotkeys.rs", include_str!("cmd_hotkeys.rs")),
    ("cmd_overlay.rs", include_str!("cmd_overlay.rs")),
    ("cmd_provider.rs", include_str!("cmd_provider.rs")),
    ("cmd_settings.rs", include_str!("cmd_settings.rs")),
    ("cmd_stream.rs", include_str!("cmd_stream.rs")),
    ("cmd_sync.rs", include_str!("cmd_sync.rs")),
    ("cmd_update.rs", include_str!("cmd_update.rs")),
    ("cmd_window.rs", include_str!("cmd_window.rs")),
];

const TS_COMMANDS: &str = include_str!("../../../src/shared/ipc/commands.ts");
const TS_OVERLAY_COMMANDS: &str = include_str!("../../../src/shared/ipc/commands.overlay.ts");

/// Tauri 注入的参数类型（按 `<` 之前的路径精确匹配；不来自前端参数对象）
const INJECTED: &[&str] = &[
    "State",
    "tauri::State",
    "WebviewWindow",
    "tauri::WebviewWindow",
    "Webview",
    "tauri::Webview",
    "Window",
    "tauri::Window",
    "AppHandle",
    "tauri::AppHandle",
];

/// 不需要在 ipc 层校验的参数类型（可包一层 `Option<…>`）：整数、bool、无字段枚举与只有 bool 字段的结构——反序列化成功即合法、有界。
/// 其余类型（`String`、`f64`、`Vec`、`serde_json::Value`、含字符串 / 浮点数 / 集合字段的 DTO）都必须校验
const BOUNDED: &[&str] = &[
    "u8",
    "u16",
    "u32",
    "u64",
    "i32",
    "i64",
    "bool",
    "HideReason",
    "HotkeyAction",
    "ActionOrigin",
    "PermissionKind",
    "PathKind",
    "PasteMode",
    "ClipFilter",
    "SendTarget",
    "Protocol",
    "ResizeSpring",
    "OsKind",
    // 全是 bool 字段的结构
    "SyncContent",
];

/// 解析出的一条 Rust 命令
#[derive(Debug, Default)]
struct RustCommand {
    file: &'static str,
    attr: String,
    args: BTreeSet<String>,
    /// 来自前端的参数：(参数名, 类型)
    typed_args: Vec<(String, String)>,
    channels: Vec<String>,
    underscored: Vec<String>,
    /// 函数体（不含外层花括号）
    body: String,
}

/// 从 `start` 处的开括号开始，返回与之配对的闭括号的位置
fn matching(s: &str, start: usize, open: char, close: char) -> usize {
    let mut depth = 0usize;
    for (i, c) in s[start..].char_indices() {
        if c == open {
            depth += 1;
        } else if c == close {
            depth -= 1;
            if depth == 0 {
                return start + i;
            }
        }
    }
    panic!("unbalanced {open}{close} starting at {start}");
}

/// 按顶层分隔符切分（忽略 `<>` / `()` / `[]` / `{}` 内部的分隔符）
fn split_top(s: &str, seps: &[char]) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut cur = String::new();
    let mut prev = '\0';
    for c in s.chars() {
        match c {
            '<' | '(' | '[' | '{' => depth += 1,
            // `=>` / `->` 里的 `>` 不是括号
            '>' if prev != '=' && prev != '-' => depth -= 1,
            ')' | ']' | '}' => depth -= 1,
            _ => {}
        }
        if depth == 0 && seps.contains(&c) {
            out.push(std::mem::take(&mut cur));
        } else {
            cur.push(c);
        }
        prev = c;
    }
    out.push(cur);
    out.into_iter()
        .map(|p| p.trim().to_owned())
        .filter(|p| !p.is_empty())
        .collect()
}

/// 去掉 `//` 行注释
fn strip_line_comments(s: &str) -> String {
    s.lines()
        .map(|l| match l.find("//") {
            Some(i) => &l[..i],
            None => l,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn parse_rust_commands() -> BTreeMap<String, RustCommand> {
    let mut out = BTreeMap::new();
    for (file, src) in RUST_SOURCES {
        let src = strip_line_comments(src);
        let mut rest = src.as_str();
        while let Some(pos) = rest.find("#[tauri::command") {
            let attr_end = rest[pos..].find(']').expect("attribute end") + pos;
            let attr = rest[pos..=attr_end].to_owned();
            let after = &rest[attr_end..];
            let fn_pos = after.find("fn ").expect("fn after #[tauri::command]");
            let name_start = fn_pos + 3;
            let paren = after[name_start..].find('(').expect("(") + name_start;
            let name = after[name_start..paren].trim();
            let name = name.split('<').next().unwrap_or(name).trim().to_owned();
            let close = matching(after, paren, '(', ')');
            let mut cmd = RustCommand {
                file,
                attr,
                ..RustCommand::default()
            };
            for param in split_top(&after[paren + 1..close], &[',']) {
                let (pname, ty) = param.split_once(':').expect("param name: type");
                let pname = pname.trim().trim_start_matches("mut ").trim().to_owned();
                let base = ty.split('<').next().unwrap_or(ty).trim();
                if INJECTED.contains(&base) {
                    continue;
                }
                if base == "Channel" || base == "tauri::ipc::Channel" {
                    cmd.channels.push(pname);
                    continue;
                }
                if pname.starts_with('_') {
                    cmd.underscored.push(pname.clone());
                }
                cmd.typed_args.push((pname.clone(), ty.trim().to_owned()));
                cmd.args.insert(pname);
            }
            let body_open = after[close..].find('{').expect("fn body") + close;
            let body_close = matching(after, body_open, '{', '}');
            cmd.body = after[body_open + 1..body_close].to_owned();
            assert!(
                out.insert(name.clone(), cmd).is_none(),
                "命令 {name} 在 cmd_*.rs 中定义了两次"
            );
            rest = &after[close..];
        }
    }
    out
}

/// 类型是否属于 [`BOUNDED`]（允许 `Option<…>` 包裹）
fn is_bounded(ty: &str) -> bool {
    let ty = ty.trim();
    let inner = ty
        .strip_prefix("Option<")
        .and_then(|t| t.strip_suffix('>'))
        .unwrap_or(ty)
        .trim();
    BOUNDED.contains(&inner)
}

/// 去掉 `"…"` 字符串字面量的内容（字段名 `"text"` 不算引用了参数 `text`）
fn strip_string_literals(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_str = false;
    let mut escaped = false;
    for c in s.chars() {
        if in_str {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_str = false;
                out.push('"');
            }
        } else {
            if c == '"' {
                in_str = true;
            }
            out.push(c);
        }
    }
    out
}

fn is_ident_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// 函数体里所有 `check_*(…)` / `validate_*(…)` 调用的实参中出现的标识符
fn validated_idents(body: &str) -> BTreeSet<String> {
    let body = strip_line_comments(&strip_string_literals(body));
    let mut out = BTreeSet::new();
    for prefix in ["check_", "validate_"] {
        let mut from = 0;
        while let Some(pos) = body[from..].find(prefix) {
            let start = from + pos;
            from = start + prefix.len();
            if body[..start].chars().next_back().is_some_and(is_ident_char) {
                continue;
            }
            let name_end = body[start..]
                .find(|c: char| !is_ident_char(c))
                .map_or(body.len(), |i| start + i);
            if !body[name_end..].starts_with('(') {
                continue;
            }
            let close = matching(&body, name_end, '(', ')');
            out.extend(
                body[name_end + 1..close]
                    .split(|c: char| !is_ident_char(c))
                    .filter(|t| !t.is_empty())
                    .map(str::to_owned),
            );
        }
    }
    out
}

/// 取 `marker` 之后第一个 `{` 开始的块体（不含外层花括号）
fn block_after<'a>(src: &'a str, marker: &str) -> &'a str {
    let pos = src.find(marker).unwrap_or_else(|| panic!("找不到 {marker}"));
    let open = src[pos..].find('{').expect("{") + pos;
    let close = matching(src, open, '{', '}');
    &src[open + 1..close]
}

/// 解析 TS 接口体：`name: { args: …; ret: … }` → name → 参数键集合
fn parse_ts_map(body: &str, out: &mut BTreeMap<String, BTreeSet<String>>) {
    let body = strip_line_comments(body);
    for entry in split_top(&body, &['\n', ';', ',']) {
        let Some((name, value)) = entry.split_once(':') else {
            panic!("无法解析 CommandMap 条目：{entry}");
        };
        let name = name.trim().to_owned();
        let value = value.trim();
        assert!(value.starts_with('{'), "CommandMap.{name} 应为对象类型：{value}");
        let inner = &value[1..matching(value, 0, '{', '}')];
        let args_pos = inner
            .find("args:")
            .unwrap_or_else(|| panic!("CommandMap.{name} 缺少 args"));
        let args = inner[args_pos + "args:".len()..].trim_start();
        let keys = if args.starts_with("NoArgs") || args.starts_with("Record<string, never>") {
            BTreeSet::new()
        } else {
            assert!(
                args.starts_with('{'),
                "CommandMap.{name}.args 应为对象字面量 / NoArgs：{args}"
            );
            let obj = &args[1..matching(args, 0, '{', '}')];
            split_top(obj, &['\n', ';', ','])
                .into_iter()
                .map(|member| {
                    let key = member.split_once(':').map_or(member.as_str(), |(k, _)| k);
                    key.trim()
                        .trim_start_matches("readonly ")
                        .trim_end_matches('?')
                        .trim()
                        .to_owned()
                })
                .collect()
        };
        assert!(out.insert(name.clone(), keys).is_none(), "CommandMap.{name} 重复");
    }
}

/// `export const COMMAND_NAMES = [ 'a', 'b', … ]` 中的字符串
fn parse_ts_command_names() -> Vec<String> {
    let pos = TS_COMMANDS.find("export const COMMAND_NAMES").expect("COMMAND_NAMES");
    let open = TS_COMMANDS[pos..].find('[').expect("[") + pos;
    let close = matching(TS_COMMANDS, open, '[', ']');
    quoted(&TS_COMMANDS[open + 1..close])
}

/// `export type StreamCommandName = 'a' | 'b'` 中的字符串
fn parse_ts_stream_names() -> BTreeSet<String> {
    let pos = TS_COMMANDS
        .find("export type StreamCommandName")
        .expect("StreamCommandName");
    let line_end = TS_COMMANDS[pos..].find('\n').expect("line end") + pos;
    quoted(&TS_COMMANDS[pos..line_end]).into_iter().collect()
}

/// 单引号字符串字面量
fn quoted(s: &str) -> Vec<String> {
    s.split('\'').skip(1).step_by(2).map(str::to_owned).collect()
}

fn ts_command_map() -> BTreeMap<String, BTreeSet<String>> {
    let mut map = BTreeMap::new();
    parse_ts_map(block_after(TS_COMMANDS, "export interface CommandMap"), &mut map);
    parse_ts_map(
        block_after(TS_OVERLAY_COMMANDS, "export interface OverlayCommandMap"),
        &mut map,
    );
    map
}

#[test]
fn handler_list_matches_command_functions() {
    let rust = parse_rust_commands();
    let registered: BTreeSet<&str> = super::COMMAND_NAMES.iter().copied().collect();
    assert_eq!(registered.len(), super::COMMAND_NAMES.len(), "commands! 列表里有重复项");
    let defined: BTreeSet<&str> = rust.keys().map(String::as_str).collect();
    assert_eq!(
        registered, defined,
        "ipc/mod.rs 的 commands! 列表与 cmd_*.rs 中的 #[tauri::command] 函数不一致"
    );
    for (name, cmd) in &rust {
        assert!(
            cmd.attr.replace(' ', "") == r#"#[tauri::command(rename_all="snake_case")]"#,
            "{}::{name} 必须写成 #[tauri::command(rename_all = \"snake_case\")]，实际为 {}",
            cmd.file,
            cmd.attr
        );
        assert!(
            cmd.underscored.is_empty(),
            "{}::{name} 的参数名不得以 _ 开头（它就是前端参数对象的键）：{:?}",
            cmd.file,
            cmd.underscored
        );
    }
}

#[test]
fn commands_match_frontend() {
    let rust = parse_rust_commands();
    let ts_names = parse_ts_command_names();
    let ts_set: BTreeSet<&str> = ts_names.iter().map(String::as_str).collect();
    assert_eq!(ts_set.len(), ts_names.len(), "commands.ts 的 COMMAND_NAMES 有重复项");
    let rust_set: BTreeSet<&str> = super::COMMAND_NAMES.iter().copied().collect();
    let only_rust: Vec<_> = rust_set.difference(&ts_set).collect();
    let only_ts: Vec<_> = ts_set.difference(&rust_set).collect();
    assert!(
        only_rust.is_empty() && only_ts.is_empty(),
        "命令集合不一致：只在 Rust {only_rust:?}，只在 TS COMMAND_NAMES {only_ts:?}"
    );

    let map = ts_command_map();
    let map_set: BTreeSet<&str> = map.keys().map(String::as_str).collect();
    assert_eq!(
        map_set, ts_set,
        "CommandMap（含 OverlayCommandMap）的键与 COMMAND_NAMES 不一致"
    );

    let mut mismatches = Vec::new();
    for (name, cmd) in &rust {
        let ts_args = &map[name.as_str()];
        if &cmd.args != ts_args {
            mismatches.push(format!("{name}: Rust {:?} ≠ TS {:?}", cmd.args, ts_args));
        }
    }
    assert!(
        mismatches.is_empty(),
        "参数键不一致（Rust 参数名即前端参数对象的键）：\n{}",
        mismatches.join("\n")
    );
}

#[test]
fn stream_commands_match_frontend() {
    let rust = parse_rust_commands();
    let mut rust_streams = BTreeSet::new();
    for (name, cmd) in &rust {
        match cmd.channels.as_slice() {
            [] => {}
            [one] => {
                assert_eq!(one, "on_event", "{name} 的通道参数必须叫 on_event");
                rust_streams.insert(name.clone());
            }
            more => panic!("{name} 有多个通道参数：{more:?}"),
        }
    }
    assert_eq!(
        rust_streams,
        parse_ts_stream_names(),
        "带 on_event 通道的命令与前端 StreamCommandName 不一致"
    );
}

#[test]
fn every_param_is_validated() {
    let mut missing = Vec::new();
    for (name, cmd) in parse_rust_commands() {
        let validated = validated_idents(&cmd.body);
        for (arg, ty) in &cmd.typed_args {
            if !is_bounded(ty) && !validated.contains(arg) {
                missing.push(format!("{}::{name}({arg}: {ty})", cmd.file));
            }
        }
    }
    assert!(
        missing.is_empty(),
        "以下参数没有经过 check_* / validate_* 校验（见 ipc/mod.rs 与 ipc/validate.rs）：\n{}",
        missing.join("\n")
    );
}

#[test]
fn parser_handles_repo_shapes() {
    let parts = split_top(
        "app_state: State<'_, AppState>, req: TranslateReq, on_event: Channel<StreamEvent>",
        &[','],
    );
    assert_eq!(parts.len(), 3);
    let mut map = BTreeMap::new();
    parse_ts_map(
        "a: { args: { x: number; y: Array<{ k: string }> | null }; ret: void }\n\
         // 注释\n\
         b: { args: NoArgs; ret: T.Foo[] }\n\
         c: {\n    args: { action: T.HotkeyAction; text: string | null }\n    ret: void\n  }",
        &mut map,
    );
    assert_eq!(map["a"], BTreeSet::from(["x".to_owned(), "y".to_owned()]));
    assert!(map["b"].is_empty());
    assert_eq!(map["c"], BTreeSet::from(["action".to_owned(), "text".to_owned()]));

    // 校验调用的识别：字段名字面量不算引用；路径前缀、`?`、`if let` 包裹都能识别；名字只是包含 check_ 的函数不算
    let body = r#"
        check_len("query", &query, MAX)?;
        if let Some(t) = &text { crate::ipc::check_id("text", t)?; }
        validate_provider(&provider)?;
        my_check_x(other);
        check_non_negative("height", f64::from(h))?;
    "#;
    let ids = validated_idents(body);
    for present in ["query", "t", "provider", "h"] {
        assert!(ids.contains(present), "{present}");
    }
    for absent in ["other", "text", "height"] {
        assert!(!ids.contains(absent), "{absent}");
    }
    assert!(is_bounded("u64") && is_bounded("Option<u8>") && is_bounded("HotkeyAction"));
    assert!(!is_bounded("String") && !is_bounded("Option<String>") && !is_bounded("f64") && !is_bounded("Style"));
}
