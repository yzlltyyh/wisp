//! `.desktop` 文件索引与图标主题查找（只依赖 std）。
//!
//! 来源应用的名称与图标：pid → `/proc/<pid>/exe` 的文件名 + WM_CLASS → 匹配 `.desktop`（`StartupWMClass`、
//! 文件 id、`Exec` 的程序名）→ `Name[<locale>]` / `Icon=` → 在图标主题（当前主题 → hicolor）里找最接近的 PNG。
//! 索引在第一次使用时构建，应用目录的修改时间变化后重建；全部是一次性的文件读取，不在任何热路径上。
//! 常驻内存只保留当前语言用得到的 `Name[<locale>]`（GNOME 应用通常带 80 多种翻译，全部保留会多占 1–3 MB），
//! 几百个条目合计约 100 KB。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

/// 一个应用条目
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesktopEntry {
    /// 文件 id（不含 `.desktop`，子目录用 `-` 连接），例如 `org.gnome.Nautilus`、`firefox`
    pub id: String,
    pub name: String,
    /// (locale, 名称)，例如 ("zh_CN", "文件")；只保留与当前进程候选 locale 匹配的条目
    pub localized_names: Vec<(String, String)>,
    pub icon: Option<String>,
    /// `Exec` 的程序名（basename）
    pub exec_basename: Option<String>,
    pub startup_wm_class: Option<String>,
    pub no_display: bool,
}

impl DesktopEntry {
    /// 按候选 locale（由具体到一般）取名称
    pub fn localized_name(&self, locales: &[String]) -> &str {
        for loc in locales {
            if let Some((_, n)) = self.localized_names.iter().find(|(l, _)| l == loc) {
                return n;
            }
        }
        &self.name
    }
}

/// 解析一个 `.desktop` 文件的 `[Desktop Entry]` 段；不是可见的应用条目（Type≠Application、Hidden=true）时返回 None。
/// `locales` 是候选 locale（由具体到一般），只保留这些语言的 `Name[...]`
pub fn parse(id: &str, text: &str, locales: &[String]) -> Option<DesktopEntry> {
    let mut in_main = false;
    let mut name = None;
    let mut localized = Vec::new();
    let mut icon = None;
    let mut exec = None;
    let mut wm_class = None;
    let mut no_display = false;
    let mut hidden = false;
    let mut is_app = false;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') {
            in_main = line == "[Desktop Entry]";
            continue;
        }
        if !in_main {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let value = unescape_value(value.trim());
        if let Some(loc) = key.strip_prefix("Name[").and_then(|k| k.strip_suffix(']')) {
            if locales.iter().any(|l| l == loc) {
                localized.push((loc.to_owned(), value));
            }
            continue;
        }
        match key {
            "Name" => name = Some(value),
            "Icon" if !value.is_empty() => icon = Some(value),
            "Exec" => exec = exec_basename(&value),
            "StartupWMClass" if !value.is_empty() => wm_class = Some(value),
            "NoDisplay" => no_display = value.eq_ignore_ascii_case("true"),
            "Hidden" => hidden = value.eq_ignore_ascii_case("true"),
            "Type" => is_app = value == "Application",
            _ => {}
        }
    }
    if !is_app || hidden {
        return None;
    }
    Some(DesktopEntry {
        id: id.to_owned(),
        name: name?,
        localized_names: localized,
        icon,
        exec_basename: exec,
        startup_wm_class: wm_class,
        no_display,
    })
}

/// 处理 desktop entry 规范中的转义（`\s` `\n` `\t` `\r` `\\`）
fn unescape_value(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    let mut chars = v.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('s') => out.push(' '),
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('\\') => out.push('\\'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// 从 `Exec=` 取程序名：跳过 `env VAR=…` 前缀，`flatpak run <app-id>` 取 app id，去掉引号与路径
pub fn exec_basename(exec: &str) -> Option<String> {
    let tokens = split_exec(exec);
    let mut iter = tokens.iter().map(String::as_str).peekable();
    let mut program = iter.next()?;
    if basename(program) == "env" {
        // env [-选项] [VAR=值]… 程序
        loop {
            let next = iter.next()?;
            if next.starts_with('-') || next.contains('=') {
                continue;
            }
            program = next;
            break;
        }
    }
    if basename(program) == "flatpak" && iter.peek() == Some(&"run") {
        iter.next();
        // flatpak run [--选项…] <app-id>
        let app = iter.find(|t| !t.starts_with('-'))?;
        return Some(app.to_owned());
    }
    let base = basename(program);
    (!base.is_empty() && !base.starts_with('%')).then(|| base.to_owned())
}

fn basename(p: &str) -> &str {
    p.rsplit('/').next().unwrap_or(p)
}

/// 按 desktop entry 规范切分 Exec（双引号内可含空格，`\` 转义）
fn split_exec(exec: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    let mut has_token = false;
    let mut chars = exec.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                in_quotes = !in_quotes;
                has_token = true;
            }
            '\\' if in_quotes => {
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
            }
            c if c.is_whitespace() && !in_quotes => {
                if has_token || !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                    has_token = false;
                }
            }
            c => {
                cur.push(c);
                has_token = true;
            }
        }
    }
    if has_token || !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// 从 LC_ALL / LC_MESSAGES / LANG 的值得到候选 locale（由具体到一般），例如
/// `zh_CN.UTF-8` → [`zh_CN`, `zh`]；`sr_RS@latin` → [`sr_RS@latin`, `sr_RS`, `sr@latin`, `sr`]
pub fn locale_candidates(value: &str) -> Vec<String> {
    let value = value.trim();
    if value.is_empty() || value == "C" || value == "POSIX" {
        return Vec::new();
    }
    let (base, modifier) = match value.split_once('@') {
        Some((b, m)) => (b, Some(m)),
        None => (value, None),
    };
    let base = base.split('.').next().unwrap_or(base);
    let lang = base.split('_').next().unwrap_or(base);
    let country = base.contains('_').then_some(base);
    let mut out = Vec::new();
    if let (Some(c), Some(m)) = (country, modifier) {
        out.push(format!("{c}@{m}"));
    }
    if let Some(c) = country {
        out.push(c.to_owned());
    }
    if let Some(m) = modifier {
        out.push(format!("{lang}@{m}"));
    }
    out.push(lang.to_owned());
    out
}

/// 当前进程的候选 locale
pub fn process_locales() -> Vec<String> {
    for key in ["LC_ALL", "LC_MESSAGES", "LANG"] {
        if let Ok(v) = std::env::var(key)
            && !v.trim().is_empty()
        {
            return locale_candidates(&v);
        }
    }
    Vec::new()
}

/// XDG 数据目录（`$XDG_DATA_HOME` 在前），另加 flatpak / snap 的导出目录
pub fn data_dirs() -> Vec<PathBuf> {
    let mut out = Vec::new();
    let home = std::env::var_os("HOME").map(PathBuf::from);
    match std::env::var_os("XDG_DATA_HOME").filter(|v| !v.is_empty()) {
        Some(d) => out.push(PathBuf::from(d)),
        None => {
            if let Some(h) = &home {
                out.push(h.join(".local/share"));
            }
        }
    }
    if let Some(h) = &home {
        out.push(h.join(".local/share/flatpak/exports/share"));
    }
    let sys = std::env::var("XDG_DATA_DIRS")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| "/usr/local/share:/usr/share".to_owned());
    for d in sys.split(':').filter(|d| !d.is_empty()) {
        out.push(PathBuf::from(d));
    }
    out.push(PathBuf::from("/var/lib/flatpak/exports/share"));
    out.push(PathBuf::from("/var/lib/snapd/desktop"));
    let mut seen = std::collections::HashSet::new();
    out.retain(|p| seen.insert(p.clone()));
    out
}

/// `applications/` 下扫描的子目录层数上限
const MAX_SUBDIR_DEPTH: usize = 2;

/// 「最多每隔一段时间检查一次」的节流（索引是否过期要 stat 每个应用目录）。只有真正检查时才重新计时——
/// 频繁的查询（剪贴板历史连续取图标 / 名称）不会把检查无限推后
#[derive(Debug, Clone, Copy)]
pub struct Recheck {
    last: Instant,
}

impl Recheck {
    /// 刚检查过（或刚构建）
    pub fn new(now: Instant) -> Self {
        Self { last: now }
    }

    /// 距上次检查已满 `every`：返回 true 并从 `now` 重新计时；否则返回 false，计时不变
    pub fn due(&mut self, now: Instant, every: Duration) -> bool {
        if now.saturating_duration_since(self.last) < every {
            return false;
        }
        self.last = now;
        true
    }
}

/// `.desktop` 索引
#[derive(Debug, Default)]
pub struct DesktopIndex {
    entries: Vec<DesktopEntry>,
    by_wm_class: HashMap<String, usize>,
    by_id: HashMap<String, usize>,
    by_exec: HashMap<String, usize>,
    /// 构建时各应用目录的修改时间（用于判断是否需要重建）
    stamps: Vec<(PathBuf, Option<SystemTime>)>,
}

impl DesktopIndex {
    /// 扫描各数据目录下的 `applications/`（最多两层子目录，例如 `kde4/`、`wine/Programs/`）；同名 id 以先出现的
    /// 目录为准（XDG 规范）。`locales` 见 [`parse`]
    pub fn build(data_dirs: &[PathBuf], locales: &[String]) -> Self {
        let mut idx = DesktopIndex::default();
        for dir in data_dirs {
            let apps = dir.join("applications");
            idx.stamps.push((apps.clone(), mtime(&apps)));
            idx.scan_dir(&apps, "", 0, locales);
        }
        idx.entries.shrink_to_fit();
        idx
    }

    /// 从已读入的条目构建（单测用）
    #[cfg(test)]
    pub fn from_entries(entries: Vec<DesktopEntry>) -> Self {
        let mut idx = DesktopIndex::default();
        for e in entries {
            idx.insert(e);
        }
        idx
    }

    fn scan_dir(&mut self, dir: &Path, prefix: &str, depth: usize, locales: &[String]) {
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        let mut files: Vec<_> = rd.flatten().collect();
        files.sort_by_key(|e| e.file_name());
        for ent in files {
            let path = ent.path();
            let name = ent.file_name().to_string_lossy().into_owned();
            let is_dir = ent.file_type().map(|t| t.is_dir()).unwrap_or(false);
            if is_dir {
                if depth < MAX_SUBDIR_DEPTH {
                    self.scan_dir(&path, &format!("{prefix}{name}-"), depth + 1, locales);
                }
                continue;
            }
            let Some(stem) = name.strip_suffix(".desktop") else {
                continue;
            };
            let id = format!("{prefix}{stem}");
            if self.by_id.contains_key(&id.to_ascii_lowercase()) {
                continue;
            }
            // 单个文件上限 256 KB，防御异常文件
            let Ok(meta) = ent.metadata() else { continue };
            if meta.len() > 256 * 1024 {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            if let Some(entry) = parse(&id, &text, locales) {
                self.insert(entry);
            }
        }
    }

    fn insert(&mut self, e: DesktopEntry) {
        let i = self.entries.len();
        self.by_id.entry(e.id.to_ascii_lowercase()).or_insert(i);
        if let Some(c) = &e.startup_wm_class {
            self.by_wm_class.entry(c.to_ascii_lowercase()).or_insert(i);
        }
        if let Some(x) = &e.exec_basename {
            // 被 NoDisplay 隐藏的条目（各种辅助程序）不参与程序名匹配，避免 `sh`、`python3` 之类误命中
            if !e.no_display {
                self.by_exec.entry(x.to_ascii_lowercase()).or_insert(i);
            }
        }
        self.entries.push(e);
    }

    /// 应用目录有变化（新装 / 卸载应用）时需要重建
    pub fn is_stale(&self) -> bool {
        self.stamps.iter().any(|(p, t)| mtime(p) != *t)
    }

    /// 条目数
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    /// 按 WM_CLASS（class / instance）与可执行文件名查找
    pub fn find(&self, class: Option<&str>, instance: Option<&str>, exe: Option<&str>) -> Option<&DesktopEntry> {
        let keys: Vec<String> = [class, instance]
            .into_iter()
            .flatten()
            .map(|s| s.trim().to_ascii_lowercase())
            .filter(|s| !s.is_empty())
            .collect();
        let exe = exe.map(|s| s.trim().to_ascii_lowercase()).filter(|s| !s.is_empty());
        let hit = keys
            .iter()
            .find_map(|k| self.by_wm_class.get(k).copied())
            .or_else(|| keys.iter().find_map(|k| self.by_id.get(k).copied()))
            // 反向域名 id 的最后一段（`org.gnome.Nautilus` ↔ class `nautilus`）
            .or_else(|| {
                keys.iter().find_map(|k| {
                    self.entries
                        .iter()
                        .position(|e| e.id.rsplit('.').next().is_some_and(|last| last.eq_ignore_ascii_case(k)))
                })
            })
            .or_else(|| exe.as_ref().and_then(|x| self.by_exec.get(x).copied()))
            .or_else(|| keys.iter().find_map(|k| self.by_exec.get(k).copied()))?;
        self.entries.get(hit)
    }

    /// 按可执行文件名查找（无窗口的进程用）
    pub fn find_by_exec(&self, exe: &str) -> Option<&DesktopEntry> {
        self.by_exec
            .get(&exe.to_ascii_lowercase())
            .and_then(|&i| self.entries.get(i))
    }
}

fn mtime(p: &Path) -> Option<SystemTime> {
    std::fs::metadata(p).and_then(|m| m.modified()).ok()
}

/// 图标主题的搜索根目录（XDG 图标主题规范：`~/.icons`、各数据目录下的 `icons/`）
pub fn icon_base_dirs(data_dirs: &[PathBuf]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(h) = std::env::var_os("HOME") {
        out.push(PathBuf::from(h).join(".icons"));
    }
    out.extend(data_dirs.iter().map(|d| d.join("icons")));
    out
}

/// 常见的固定尺寸目录
const ICON_SIZES: &[u32] = &[16, 22, 24, 32, 36, 48, 64, 72, 96, 128, 192, 256, 512];

/// 尺寸偏好顺序：先不小于目标的（由小到大），再小于目标的（由大到小）
pub fn size_preference(target: u32) -> Vec<u32> {
    let mut larger: Vec<u32> = ICON_SIZES.iter().copied().filter(|&s| s >= target).collect();
    larger.sort_unstable();
    let mut smaller: Vec<u32> = ICON_SIZES.iter().copied().filter(|&s| s < target).collect();
    smaller.sort_unstable_by(|a, b| b.cmp(a));
    larger.extend(smaller);
    larger
}

/// 读取主题的 `Inherits=`
fn theme_parents(bases: &[PathBuf], theme: &str) -> Vec<String> {
    for b in bases {
        let Ok(text) = std::fs::read_to_string(b.join(theme).join("index.theme")) else {
            continue;
        };
        for line in text.lines() {
            if let Some(v) = line.trim().strip_prefix("Inherits=") {
                return v
                    .split(',')
                    .map(|s| s.trim().to_owned())
                    .filter(|s| !s.is_empty())
                    .collect();
            }
        }
        return Vec::new();
    }
    Vec::new()
}

/// 查找图标的 PNG 文件。`icon` 可以是绝对路径或图标名；`theme` 为当前图标主题（沿 Inherits 链查找，最后是
/// hicolor 与 `/usr/share/pixmaps`）。只返回 PNG（没有 SVG 光栅化器；只有 SVG 时由调用方退回窗口图标）
pub fn find_icon_png(icon: &str, size: u32, theme: Option<&str>, data_dirs: &[PathBuf]) -> Option<PathBuf> {
    if icon.starts_with('/') {
        let p = PathBuf::from(icon);
        return (p.extension().is_some_and(|e| e.eq_ignore_ascii_case("png")) && p.is_file()).then_some(p);
    }
    // 图标名不应包含路径分隔符
    if icon.contains('/') || icon.is_empty() {
        return None;
    }
    let bases = icon_base_dirs(data_dirs);
    let mut themes: Vec<String> = Vec::new();
    // 广度优先：当前主题 → 它声明的父主题（按声明顺序）→ 祖父主题…；最多 8 个，防御循环继承
    let mut queue: std::collections::VecDeque<String> = theme.into_iter().map(str::to_owned).collect();
    while let Some(t) = queue.pop_front() {
        if themes.contains(&t) || themes.len() >= 8 {
            continue;
        }
        let parents = theme_parents(&bases, &t);
        themes.push(t);
        queue.extend(parents);
    }
    if !themes.iter().any(|t| t == "hicolor") {
        themes.push("hicolor".to_owned());
    }
    let file = format!("{icon}.png");
    let sizes = size_preference(size);
    for t in &themes {
        for s in &sizes {
            for b in &bases {
                for dir in [format!("{s}x{s}/apps"), format!("apps/{s}"), format!("{s}x{s}@2/apps")] {
                    let p = b.join(t).join(&dir).join(&file);
                    if p.is_file() {
                        return Some(p);
                    }
                }
            }
        }
    }
    for d in data_dirs {
        let p = d.join("pixmaps").join(&file);
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recheck_is_not_postponed_by_frequent_queries() {
        let every = Duration::from_secs(10);
        let t0 = Instant::now();
        let mut gate = Recheck::new(t0);
        // 每 3 s 查询一次：第 12 s 的查询必须触发检查（旧实现每次查询都重新计时，永远不检查）
        for s in [3, 6, 9] {
            assert!(!gate.due(t0 + Duration::from_secs(s), every), "{s} s");
        }
        assert!(gate.due(t0 + Duration::from_secs(12), every));
        // 检查之后从那一刻重新计时
        assert!(!gate.due(t0 + Duration::from_secs(21), every));
        assert!(gate.due(t0 + Duration::from_secs(22), every));
    }

    const FIREFOX: &str = "[Desktop Entry]\nVersion=1.0\nName=Firefox Web Browser\nName[zh_CN]=Firefox 网络浏览器\nExec=/usr/lib/firefox/firefox %u\nIcon=firefox\nType=Application\nStartupWMClass=firefox\n\n[Desktop Action new-window]\nName=New Window\nExec=firefox --new-window\n";

    fn zh() -> Vec<String> {
        vec!["zh_CN".into(), "zh".into()]
    }

    #[test]
    fn parses_main_section_only() {
        let e = parse("firefox", FIREFOX, &zh()).unwrap();
        assert_eq!(e.name, "Firefox Web Browser");
        assert_eq!(e.icon.as_deref(), Some("firefox"));
        assert_eq!(e.exec_basename.as_deref(), Some("firefox"));
        assert_eq!(e.startup_wm_class.as_deref(), Some("firefox"));
        assert_eq!(e.localized_name(&["zh_CN".into(), "zh".into()]), "Firefox 网络浏览器");
        assert_eq!(e.localized_name(&["de".into()]), "Firefox Web Browser");
    }

    #[test]
    fn skips_non_applications_and_hidden() {
        assert!(parse("x", "[Desktop Entry]\nName=X\nType=Link\n", &[]).is_none());
        assert!(parse("x", "[Desktop Entry]\nName=X\nType=Application\nHidden=true\n", &[]).is_none());
        assert!(
            parse("x", "[Desktop Entry]\nType=Application\n", &[]).is_none(),
            "没有 Name"
        );
    }

    #[test]
    fn keeps_only_current_locale_names() {
        let text = "[Desktop Entry]\nName=Files\nName[de]=Dateien\nName[zh_CN]=文件\nName[zh_TW]=檔案\nName[fr]=Fichiers\nType=Application\n";
        let e = parse("org.gnome.Nautilus", text, &zh()).unwrap();
        assert_eq!(e.localized_names, vec![("zh_CN".to_owned(), "文件".to_owned())]);
        assert_eq!(e.localized_name(&zh()), "文件");
        let e = parse("org.gnome.Nautilus", text, &[]).unwrap();
        assert!(e.localized_names.is_empty());
        assert_eq!(e.localized_name(&[]), "Files");
    }

    #[test]
    fn exec_parsing() {
        assert_eq!(
            exec_basename("env GDK_BACKEND=x11 /opt/app/bin/app --flag %F").as_deref(),
            Some("app")
        );
        assert_eq!(
            exec_basename("/usr/bin/flatpak run --branch=stable --arch=x86_64 org.telegram.desktop -- %u").as_deref(),
            Some("org.telegram.desktop")
        );
        assert_eq!(exec_basename("\"/opt/My App/run\" %U").as_deref(), Some("run"));
        assert_eq!(exec_basename("%F"), None);
        assert_eq!(exec_basename(""), None);
        assert_eq!(unescape_value(r"a\sb\\c"), r"a b\c");
    }

    #[test]
    fn locale_fallback_chain() {
        assert_eq!(locale_candidates("zh_CN.UTF-8"), vec!["zh_CN", "zh"]);
        assert_eq!(
            locale_candidates("sr_RS.UTF-8@latin"),
            vec!["sr_RS@latin", "sr_RS", "sr@latin", "sr"]
        );
        assert_eq!(locale_candidates("de"), vec!["de"]);
        assert!(locale_candidates("C").is_empty());
        assert!(locale_candidates("").is_empty());
    }

    fn entry(id: &str, exec: Option<&str>, wm: Option<&str>, no_display: bool) -> DesktopEntry {
        DesktopEntry {
            id: id.into(),
            name: id.into(),
            localized_names: vec![],
            icon: Some(id.into()),
            exec_basename: exec.map(Into::into),
            startup_wm_class: wm.map(Into::into),
            no_display,
        }
    }

    #[test]
    fn index_matching_order() {
        let idx = DesktopIndex::from_entries(vec![
            entry(
                "org.gnome.Terminal",
                Some("gnome-terminal"),
                Some("Gnome-terminal"),
                false,
            ),
            entry("org.gnome.Nautilus", Some("nautilus"), None, false),
            entry("code", Some("code"), Some("Code"), false),
            entry("helper", Some("python3"), None, true),
        ]);
        assert_eq!(idx.len(), 4);
        assert_eq!(
            idx.find(Some("Gnome-terminal"), Some("gnome-terminal-server"), None)
                .unwrap()
                .id,
            "org.gnome.Terminal"
        );
        assert_eq!(
            idx.find(Some("org.gnome.Nautilus"), None, None).unwrap().id,
            "org.gnome.Nautilus"
        );
        assert_eq!(
            idx.find(Some("Nautilus"), Some("nautilus"), None).unwrap().id,
            "org.gnome.Nautilus"
        );
        assert_eq!(idx.find(None, None, Some("code")).unwrap().id, "code");
        assert!(
            idx.find(None, None, Some("python3")).is_none(),
            "NoDisplay 条目不参与程序名匹配"
        );
        assert!(idx.find(Some("unknown"), None, None).is_none());
        assert_eq!(idx.find_by_exec("Code").unwrap().id, "code");
    }

    #[test]
    fn size_preference_order() {
        assert_eq!(&size_preference(64)[..4], &[64, 72, 96, 128]);
        assert_eq!(size_preference(1000)[0], 512);
        assert_eq!(size_preference(1000).len(), ICON_SIZES.len());
    }

    #[test]
    fn finds_png_in_theme_tree() {
        let tmp = std::env::temp_dir().join(format!("wisp-icon-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let data = tmp.join("share");
        let theme_dir = data.join("icons/MyTheme");
        std::fs::create_dir_all(theme_dir.join("48x48/apps")).unwrap();
        std::fs::write(theme_dir.join("index.theme"), "[Icon Theme]\nInherits=hicolor\n").unwrap();
        std::fs::write(theme_dir.join("48x48/apps/demo.png"), b"png").unwrap();
        std::fs::create_dir_all(data.join("icons/hicolor/128x128/apps")).unwrap();
        std::fs::write(data.join("icons/hicolor/128x128/apps/other.png"), b"png").unwrap();
        std::fs::create_dir_all(data.join("pixmaps")).unwrap();
        std::fs::write(data.join("pixmaps/legacy.png"), b"png").unwrap();
        let dirs = vec![data.clone()];
        assert_eq!(
            find_icon_png("demo", 32, Some("MyTheme"), &dirs),
            Some(theme_dir.join("48x48/apps/demo.png"))
        );
        assert_eq!(
            find_icon_png("other", 32, Some("MyTheme"), &dirs),
            Some(data.join("icons/hicolor/128x128/apps/other.png"))
        );
        assert_eq!(
            find_icon_png("legacy", 32, None, &dirs),
            Some(data.join("pixmaps/legacy.png"))
        );
        assert_eq!(find_icon_png("missing", 32, None, &dirs), None);
        assert_eq!(find_icon_png("../evil", 32, None, &dirs), None);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn index_build_from_disk_and_staleness() {
        let tmp = std::env::temp_dir().join(format!("wisp-desktop-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let apps = tmp.join("applications");
        std::fs::create_dir_all(apps.join("kde4")).unwrap();
        std::fs::write(apps.join("firefox.desktop"), FIREFOX).unwrap();
        std::fs::write(
            apps.join("kde4/okular.desktop"),
            "[Desktop Entry]\nName=Okular\nType=Application\nExec=okular %U\nIcon=okular\n",
        )
        .unwrap();
        let idx = DesktopIndex::build(std::slice::from_ref(&tmp), &zh());
        assert_eq!(idx.len(), 2);
        assert_eq!(idx.find(None, Some("okular"), None).unwrap().id, "kde4-okular");
        assert!(!idx.is_stale());
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
