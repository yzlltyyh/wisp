//! glossary.json 的读写与导入导出格式。
//!
//! 条目规范化（[`normalize`]）：去掉首尾空白；`source` 为空的条目丢弃；`source` 重复时后出现的覆盖先出现的（位置取先出现的）；
//! `note` 为空串视为没有。`source == target` 表示「不译」，原样保留。
//!
//! 导入支持：JSON（`{ "entries": [...] }` 的 glossary.json 格式，或直接是条目数组）与 CSV / TSV（`source,target[,note]`，
//! 第一行若是表头 `source,target…` 则跳过；字段可用双引号包裹，`""` 表示一个引号）。导出为 JSON 或 CSV（按扩展名）。

use std::collections::HashMap;
use std::path::Path;

use wisp_types::{GlossaryEntry, GlossaryFile};

use crate::error::IpcError;
use crate::settings::{fsio, migrate};

/// 读取 glossary.json（不存在时返回默认值；损坏时备份并返回默认值）
pub fn load(path: &Path) -> Result<GlossaryFile, IpcError> {
    Ok(load_with_backup(path)?.value)
}

/// 读取 glossary.json，同时返回损坏时的备份路径与宽松反序列化的问题列表
pub fn load_with_backup(path: &Path) -> Result<fsio::Loaded<GlossaryFile>, IpcError> {
    let mut loaded = fsio::load_document::<GlossaryFile>(path, |v| {
        migrate::run(v, migrate::DOC_SCHEMA_VERSION, migrate::GLOSSARY_STEPS)
    })?;
    loaded.value.schema_version = migrate::DOC_SCHEMA_VERSION;
    loaded.value.entries = normalize(std::mem::take(&mut loaded.value.entries));
    Ok(loaded)
}

/// 原子写入 glossary.json
pub fn save(path: &Path, file: &GlossaryFile) -> Result<(), IpcError> {
    fsio::write_atomic(path, &encode(file)?)?;
    Ok(())
}

/// 序列化（带缩进、换行结尾）
pub fn encode(file: &GlossaryFile) -> Result<Vec<u8>, IpcError> {
    Ok(fsio::to_pretty_json(file)?)
}

/// 规范化条目（见模块文档）
pub fn normalize(entries: Vec<GlossaryEntry>) -> Vec<GlossaryEntry> {
    let mut out: Vec<GlossaryEntry> = Vec::with_capacity(entries.len());
    let mut pos: HashMap<String, usize> = HashMap::with_capacity(entries.len());
    for e in entries {
        let source = e.source.trim().to_owned();
        if source.is_empty() {
            continue;
        }
        let entry = GlossaryEntry {
            target: e.target.trim().to_owned(),
            note: e.note.map(|n| n.trim().to_owned()).filter(|n| !n.is_empty()),
            source,
        };
        match pos.get(&entry.source) {
            Some(&i) => out[i] = entry,
            None => {
                pos.insert(entry.source.clone(), out.len());
                out.push(entry);
            }
        }
    }
    out
}

/// 把导入的条目合并进现有条目：同 `source` 的被替换，新的追加到末尾
pub fn merge(existing: &[GlossaryEntry], imported: Vec<GlossaryEntry>) -> Vec<GlossaryEntry> {
    let mut all = existing.to_vec();
    all.extend(imported);
    normalize(all)
}

/// 导入文件的格式（按扩展名）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Json,
    Csv,
    Tsv,
}

impl Format {
    /// 按扩展名判断（大小写不敏感）；未知扩展名按内容猜：以 `{` / `[` 开头为 JSON，含制表符为 TSV，否则 CSV
    pub fn detect(path: &Path, content: &str) -> Self {
        let ext = path
            .extension()
            .map(|e| e.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();
        match ext.as_str() {
            "json" => Self::Json,
            "csv" => Self::Csv,
            "tsv" | "tab" => Self::Tsv,
            _ => {
                let head = content.trim_start();
                if head.starts_with('{') || head.starts_with('[') {
                    Self::Json
                } else if head.lines().next().is_some_and(|l| l.contains('\t')) {
                    Self::Tsv
                } else {
                    Self::Csv
                }
            }
        }
    }
}

/// 解析导入文件；格式错误返回 `invalid_argument`（不含文件内容）
pub fn parse_import(content: &str, format: Format) -> Result<Vec<GlossaryEntry>, IpcError> {
    let content = content.strip_prefix('\u{feff}').unwrap_or(content);
    let entries = match format {
        Format::Json => {
            let value: serde_json::Value = serde_json::from_str(content).map_err(|e| {
                IpcError::invalid_argument(format!("glossary file is not valid JSON (line {})", e.line()))
            })?;
            if value.is_array() {
                let (file, _) = wisp_types::from_value_lenient::<GlossaryFile>(serde_json::json!({ "entries": value }))
                    .map_err(|_| IpcError::invalid_argument("glossary file has an unexpected shape"))?;
                file.entries
            } else if value.is_object() {
                let (file, _) = wisp_types::from_value_lenient::<GlossaryFile>(value)
                    .map_err(|_| IpcError::invalid_argument("glossary file has an unexpected shape"))?;
                file.entries
            } else {
                return Err(IpcError::invalid_argument("glossary file has an unexpected shape"));
            }
        }
        Format::Csv => parse_delimited(content, ','),
        Format::Tsv => parse_delimited(content, '\t'),
    };
    Ok(normalize(entries))
}

fn parse_delimited(content: &str, sep: char) -> Vec<GlossaryEntry> {
    let mut out = Vec::new();
    for (i, record) in records(content, sep).into_iter().enumerate() {
        let mut fields = record.into_iter();
        let source = fields.next().unwrap_or_default();
        let target = fields.next().unwrap_or_default();
        let note = fields.next().filter(|n| !n.trim().is_empty());
        if i == 0 && source.trim().eq_ignore_ascii_case("source") && target.trim().eq_ignore_ascii_case("target") {
            continue;
        }
        out.push(GlossaryEntry { source, target, note });
    }
    out
}

/// 最小 CSV 解析：支持引号字段（字段内可含分隔符与换行，`""` 转义引号）、CRLF；忽略空行
fn records(content: &str, sep: char) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    let mut row: Vec<String> = Vec::new();
    let mut field = String::new();
    let mut in_quotes = false;
    let mut chars = content.chars().peekable();
    let mut field_started = false;
    while let Some(c) = chars.next() {
        if in_quotes {
            match c {
                '"' if chars.peek() == Some(&'"') => {
                    chars.next();
                    field.push('"');
                }
                '"' => in_quotes = false,
                _ => field.push(c),
            }
            continue;
        }
        match c {
            '"' if !field_started => {
                in_quotes = true;
                field_started = true;
            }
            c if c == sep => {
                row.push(std::mem::take(&mut field));
                field_started = false;
            }
            '\r' => {}
            '\n' => {
                row.push(std::mem::take(&mut field));
                field_started = false;
                if row.iter().any(|f| !f.trim().is_empty()) {
                    rows.push(std::mem::take(&mut row));
                } else {
                    row.clear();
                }
            }
            _ => {
                field.push(c);
                field_started = true;
            }
        }
    }
    row.push(field);
    if row.iter().any(|f| !f.trim().is_empty()) {
        rows.push(row);
    }
    rows
}

/// 导出为 CSV（带表头 `source,target,note`；需要时加引号）
pub fn to_csv(entries: &[GlossaryEntry]) -> String {
    fn quote(s: &str) -> String {
        if s.contains([',', '"', '\n', '\r']) || s.starts_with(' ') || s.ends_with(' ') {
            format!("\"{}\"", s.replace('"', "\"\""))
        } else {
            s.to_owned()
        }
    }
    let mut out = String::from("source,target,note\n");
    for e in entries {
        out.push_str(&quote(&e.source));
        out.push(',');
        out.push_str(&quote(&e.target));
        out.push(',');
        out.push_str(&quote(e.note.as_deref().unwrap_or("")));
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(s: &str, t: &str, n: Option<&str>) -> GlossaryEntry {
        GlossaryEntry {
            source: s.to_owned(),
            target: t.to_owned(),
            note: n.map(str::to_owned),
        }
    }

    #[test]
    fn normalize_trims_dedupes_and_drops_empty() {
        let out = normalize(vec![
            e(" token ", " 令牌 ", Some("  ")),
            e("", "x", None),
            e("API", "API", None),
            e("token", "记号", Some("新")),
        ]);
        assert_eq!(out, vec![e("token", "记号", Some("新")), e("API", "API", None)]);
    }

    #[test]
    fn merge_replaces_by_source_and_appends_new() {
        let existing = vec![e("a", "1", None), e("b", "2", None)];
        let merged = merge(&existing, vec![e("b", "3", None), e("c", "4", None)]);
        assert_eq!(merged, vec![e("a", "1", None), e("b", "3", None), e("c", "4", None)]);
    }

    #[test]
    fn imports_json_file_and_array() {
        let file = r#"{"schema_version":1,"entries":[{"source":"token","target":"令牌"},{"bad":1}]}"#;
        assert_eq!(
            parse_import(file, Format::Json).expect("file"),
            vec![e("token", "令牌", None)]
        );
        let arr = r#"[{"source":"a","target":"b","note":"n"}]"#;
        assert_eq!(
            parse_import(arr, Format::Json).expect("array"),
            vec![e("a", "b", Some("n"))]
        );
        assert!(parse_import("{oops", Format::Json).is_err());
        assert!(parse_import("42", Format::Json).is_err());
    }

    #[test]
    fn imports_csv_with_header_quotes_and_crlf() {
        let csv = "\u{feff}source,target,note\r\ntoken,令牌,\r\n\"a, b\",\"say \"\"hi\"\"\",\"multi\nline\"\r\n\r\n";
        let out = parse_import(csv, Format::Csv).expect("csv");
        assert_eq!(
            out,
            vec![e("token", "令牌", None), e("a, b", "say \"hi\"", Some("multi\nline"))]
        );
    }

    #[test]
    fn imports_tsv() {
        let out = parse_import("LLM\t大语言模型\nGPU\tGPU\t不译", Format::Tsv).expect("tsv");
        assert_eq!(out, vec![e("LLM", "大语言模型", None), e("GPU", "GPU", Some("不译"))]);
    }

    #[test]
    fn csv_round_trip() {
        let entries = vec![e("a, b", "x\"y", Some("n")), e("plain", "p", None)];
        let csv = to_csv(&entries);
        assert_eq!(parse_import(&csv, Format::Csv).expect("parse"), entries);
    }

    #[test]
    fn format_detection() {
        assert_eq!(Format::detect(Path::new("g.JSON"), ""), Format::Json);
        assert_eq!(Format::detect(Path::new("g.csv"), ""), Format::Csv);
        assert_eq!(Format::detect(Path::new("g.tsv"), ""), Format::Tsv);
        assert_eq!(Format::detect(Path::new("g.txt"), "  [{}]"), Format::Json);
        assert_eq!(Format::detect(Path::new("g.txt"), "a\tb"), Format::Tsv);
        assert_eq!(Format::detect(Path::new("g"), "a,b"), Format::Csv);
    }

    #[test]
    fn save_and_load_round_trip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("glossary.json");
        let file = GlossaryFile {
            schema_version: 1,
            entries: vec![e("token", "令牌", None)],
        };
        save(&path, &file).expect("save");
        assert_eq!(load(&path).expect("load"), file);
    }
}
