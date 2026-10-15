//! Linux 托盘宿主的探测。全是可移植代码、不写 `cfg(target_os)`，只在 Linux 上被调用。
//!
//! - [`indicator_library_present`]：tray-icon 经 libappindicator-sys 在运行时 dlopen AppIndicator 库，**加载失败时直接
//!   panic**（release 构建 `panic = "abort"`，进程消失）。建托盘之前按动态链接器的查找方式确认库存在、且是本进程架构的
//!   ELF（读 ELF 头，避免把 multiarch 目录里的 i386 库当成可用）：`LD_LIBRARY_PATH`、AppImage 的 `$APPDIR`、
//!   `/etc/ld.so.conf`（递归展开 `include`）列出的目录、默认目录（含 Flatpak 的 `/app/lib`、NixOS），以及
//!   `/etc/ld.so.cache` 里记录的完整路径（自定义目录、Flatpak 运行时都在其中）；
//! - [`sni_watcher_present`]：库在、但会话里没有 `org.kde.StatusNotifierWatcher`（GNOME 没装 AppIndicator 扩展：
//!   Fedora Workstation、Debian / Arch 原版 GNOME）时，托盘对象能建成功但图标不可见。经会话总线 `NameHasOwner` 查询
//!   （`app::session_bus`：依次尝试 `busctl` / `gdbus` / `dbus-send`，各限时 [`PROBE_TIMEOUT`]，只在启动时查，不常驻）；
//!   都不可用时返回 None，按「可见」处理，不误报。**阻塞**，调用方放到阻塞线程池。
//!
//! 为什么不直接 dlopen / 用 zbus：desktop 没有直接依赖 libloading / zbus（它们只是 tray-icon、keyring 的间接依赖），
//! 引入需要改 desktop 的 Cargo.toml 与 Cargo.lock，并在应用层写 `cfg(target_os = "linux")`；
//! 等 wisp-platform 或 `windows/native/linux.rs` 提供原生实现后替换本模块。

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

/// tray-icon（libappindicator-sys）尝试加载的库名，顺序与它一致
pub const INDICATOR_LIBS: [&str; 4] = [
    "libayatana-appindicator3.so.1",
    "libappindicator3.so.1",
    "libayatana-appindicator3.so",
    "libappindicator3.so",
];

/// StatusNotifierItem 宿主在会话总线上的名字（libappindicator 只认这一个）
pub const WATCHER_NAME: &str = "org.kde.StatusNotifierWatcher";

/// 单个 D-Bus 查询命令的超时
pub use crate::app::session_bus::PROBE_TIMEOUT;

/// ld.so.conf 的 include 最多展开几层（防止循环包含）
const MAX_INCLUDE_DEPTH: u8 = 4;

/// tray-icon 依赖的 AppIndicator 库能否被本进程加载（见模块说明）
pub fn indicator_library_present() -> bool {
    let cache = std::fs::read("/etc/ld.so.cache").ok();
    find_library(&INDICATOR_LIBS, &search_dirs(), cache.as_deref(), native_elf()).is_some()
}

/// 在 `dirs` 与 ld.so.cache 记录的路径里找第一个文件名在 `names` 中、且是本进程架构 ELF 的库
pub fn find_library(
    names: &[&str],
    dirs: &[PathBuf],
    ld_cache: Option<&[u8]>,
    native: Option<(u8, u16)>,
) -> Option<PathBuf> {
    let from_dirs = dirs.iter().flat_map(|dir| names.iter().map(move |n| dir.join(n)));
    let from_cache = ld_cache.map(|c| cache_paths(c, names)).unwrap_or_default();
    from_dirs.chain(from_cache).find(|p| elf_matches(p, native))
}

/// 动态链接器会查找的目录：`LD_LIBRARY_PATH`、`$APPDIR`（AppImage）、`/etc/ld.so.conf` 与默认目录
fn search_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(v) = std::env::var_os("LD_LIBRARY_PATH") {
        dirs.extend(std::env::split_paths(&v).filter(|p| p.is_absolute()));
    }
    let appdir = std::env::var_os("APPDIR").map(PathBuf::from);
    for root in appdir.iter().map(PathBuf::as_path).chain([Path::new("/")]) {
        dirs.extend(default_dirs(root));
    }
    ld_conf_dirs(Path::new("/etc/ld.so.conf"), 0, &mut dirs);
    // Flatpak 应用自带的库与 NixOS 的系统库
    dirs.push(PathBuf::from("/app/lib"));
    dirs.push(PathBuf::from("/run/current-system/sw/lib"));
    dirs.dedup();
    dirs
}

/// 以 `root` 为根的默认库目录（含 Debian 系的 multiarch 子目录）
pub fn default_dirs(root: &Path) -> Vec<PathBuf> {
    const MULTIARCH: [&str; 6] = [
        "x86_64-linux-gnu",
        "aarch64-linux-gnu",
        "arm-linux-gnueabihf",
        "i386-linux-gnu",
        "riscv64-linux-gnu",
        "powerpc64le-linux-gnu",
    ];
    let mut dirs = Vec::new();
    for base in ["usr/lib", "usr/lib64", "lib", "lib64", "usr/local/lib"] {
        let base = root.join(base);
        dirs.extend(MULTIARCH.iter().map(|arch| base.join(arch)));
        dirs.push(base);
    }
    dirs
}

/// `/etc/ld.so.conf` 列出的目录：递归展开 `include <通配>`（相对路径相对于所在文件的目录），忽略注释；兼容旧格式
/// 一行多个目录（逗号 / 冒号 / 空白分隔）与 `目录=类型` 写法
pub fn ld_conf_dirs(path: &Path, depth: u8, out: &mut Vec<PathBuf>) {
    if depth > MAX_INCLUDE_DEPTH {
        return;
    }
    let Ok(text) = std::fs::read_to_string(path) else {
        return;
    };
    let base = path.parent().unwrap_or_else(|| Path::new("/"));
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or_default().trim();
        if let Some(rest) = line.strip_prefix("include")
            && rest.starts_with(char::is_whitespace)
        {
            for file in glob(base, rest.trim()) {
                ld_conf_dirs(&file, depth + 1, out);
            }
        } else if line.starts_with('/') {
            out.extend(
                line.split(|c: char| c == ',' || c == ':' || c.is_whitespace())
                    .filter(|d| d.starts_with('/'))
                    .map(|d| PathBuf::from(d.split('=').next().unwrap_or(d))),
            );
        }
    }
}

/// 只支持文件名部分带 `*` / `?` 的通配（ld.so.conf 的 include 实际就是 `/etc/ld.so.conf.d/*.conf`），结果按名字排序
pub fn glob(base: &Path, pattern: &str) -> Vec<PathBuf> {
    let pattern = base.join(pattern);
    let (Some(dir), Some(name)) = (pattern.parent(), pattern.file_name().and_then(|n| n.to_str())) else {
        return Vec::new();
    };
    if !name.contains(['*', '?']) {
        return vec![pattern.clone()];
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| wildcard_match(name, n))
        })
        .collect();
    files.sort();
    files
}

/// `*`（任意串）/ `?`（任意一个字符）通配（遇到不匹配时回溯到最近的 `*`）
pub fn wildcard_match(pattern: &str, text: &str) -> bool {
    let (p, t) = (pattern.as_bytes(), text.as_bytes());
    let (mut pi, mut ti) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while ti < t.len() {
        if pi < p.len() && (p[pi] == b'?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == b'*' {
            star = Some((pi, ti));
            pi += 1;
        } else if let Some((sp, st)) = star {
            pi = sp + 1;
            ti = st + 1;
            star = Some((sp, st + 1));
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|c| *c == b'*')
}

/// ld.so.cache 的字符串表里、文件名是 `names` 之一的完整路径。不解析二进制结构：新旧两种格式的字符串表都是以 NUL
/// 结尾的字符串，库的完整路径以 `/` 开头
pub fn cache_paths(cache: &[u8], names: &[&str]) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = cache
        .split(|b| *b == 0)
        .filter(|s| s.first() == Some(&b'/'))
        .filter_map(|s| std::str::from_utf8(s).ok())
        .map(Path::new)
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| names.contains(&n))
        })
        .map(Path::to_path_buf)
        .collect();
    out.dedup();
    out
}

/// 本进程的 ELF（EI_CLASS，e_machine）；未知架构返回 None（此时只检查是不是 ELF）
pub fn native_elf() -> Option<(u8, u16)> {
    Some(match std::env::consts::ARCH {
        "x86_64" => (2, 62),
        "aarch64" => (2, 183),
        "x86" => (1, 3),
        "arm" => (1, 40),
        "riscv64" => (2, 243),
        "powerpc64" => (2, 21),
        "s390x" => (2, 22),
        "loongarch64" => (2, 258),
        _ => return None,
    })
}

/// 文件是本进程能加载的 ELF：魔数、位数（EI_CLASS）与机器类型（e_machine，按 EI_DATA 的字节序）都对得上
pub fn elf_matches(path: &Path, native: Option<(u8, u16)>) -> bool {
    let mut header = [0u8; 20];
    let Ok(mut file) = File::open(path) else {
        return false;
    };
    if file.read_exact(&mut header).is_err() || header[..4] != *b"\x7fELF" {
        return false;
    }
    let Some((class, machine)) = native else {
        return true;
    };
    let e_machine = match header[5] {
        1 => u16::from_le_bytes([header[18], header[19]]),
        2 => u16::from_be_bytes([header[18], header[19]]),
        _ => return false,
    };
    header[4] == class && e_machine == machine
}

/// 会话总线上有没有 StatusNotifierWatcher（**阻塞**，最多约 3 × [`PROBE_TIMEOUT`]）；三种查询工具都不可用时为 None
pub fn sni_watcher_present() -> Option<bool> {
    crate::app::session_bus::name_has_owner(WATCHER_NAME)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 写一个只有 ELF 头的假库
    fn fake_elf(path: &Path, class: u8, little_endian: bool, machine: u16) {
        let mut header = [0u8; 64];
        header[..4].copy_from_slice(b"\x7fELF");
        header[4] = class;
        header[5] = if little_endian { 1 } else { 2 };
        let m = if little_endian {
            machine.to_le_bytes()
        } else {
            machine.to_be_bytes()
        };
        header[18..20].copy_from_slice(&m);
        std::fs::write(path, header).expect("write elf");
    }

    #[test]
    fn default_dirs_cover_standard_locations() {
        let dirs = default_dirs(Path::new("/"));
        assert!(dirs.iter().any(|d| d == Path::new("/usr/lib/x86_64-linux-gnu")));
        assert!(dirs.iter().any(|d| d == Path::new("/usr/lib64")));
        assert!(dirs.iter().any(|d| d == Path::new("/lib/aarch64-linux-gnu")));
        let appimage = default_dirs(Path::new("/tmp/.mount_wisp"));
        assert!(appimage.iter().any(|d| d == Path::new("/tmp/.mount_wisp/usr/lib")));
    }

    #[test]
    fn libraries_must_match_this_process_architecture() {
        let dir = tempfile::tempdir().expect("tempdir");
        let x64 = (2, 62);
        let lib = dir.path().join("libayatana-appindicator3.so.1");
        fake_elf(&lib, 2, true, 62);
        assert!(elf_matches(&lib, Some(x64)));
        assert!(elf_matches(&lib, None), "未知架构只检查魔数");
        // i386 库（multiarch 目录里常见）不能当成 x86_64 可用
        let i386 = dir.path().join("i386.so");
        fake_elf(&i386, 1, true, 3);
        assert!(!elf_matches(&i386, Some(x64)));
        // 大端机器类型按 EI_DATA 解析
        let s390 = dir.path().join("s390.so");
        fake_elf(&s390, 2, false, 22);
        assert!(elf_matches(&s390, Some((2, 22))));
        // 不是 ELF / 太短 / 不存在
        std::fs::write(dir.path().join("text.so"), b"INPUT(-lfoo)").expect("write");
        assert!(!elf_matches(&dir.path().join("text.so"), None));
        assert!(!elf_matches(&dir.path().join("missing.so"), None));
    }

    #[test]
    fn library_is_found_in_dirs_or_in_the_ld_cache() {
        let dir = tempfile::tempdir().expect("tempdir");
        let custom = dir.path().join("opt").join("lib");
        std::fs::create_dir_all(&custom).expect("mkdir");
        let lib = custom.join("libappindicator3.so.1");
        fake_elf(&lib, 2, true, 62);
        let native = Some((2, 62));
        // 目录里找得到
        assert_eq!(
            find_library(&INDICATOR_LIBS, std::slice::from_ref(&custom), None, native),
            Some(lib.clone())
        );
        // 目录里没有，但 ld.so.cache 记录了它（自定义目录 / Flatpak）。缓存里只有 `/` 开头的绝对路径，
        // 临时目录不是这种形式的平台（Windows）跳过这一段
        if lib.to_string_lossy().starts_with('/') {
            let mut cache = b"glibc-ld.so.cache1.1\0\0\x07\0".to_vec();
            cache.extend_from_slice(b"libappindicator3.so.1\0");
            cache.extend_from_slice(lib.to_string_lossy().as_bytes());
            cache.extend_from_slice(b"\0/usr/lib/libc.so.6\0");
            assert_eq!(
                find_library(&INDICATOR_LIBS, &[dir.path().to_path_buf()], Some(&cache), native),
                Some(lib.clone())
            );
        }
        // 架构不对：不算找到
        assert_eq!(
            find_library(&INDICATOR_LIBS, std::slice::from_ref(&custom), None, Some((2, 183))),
            None
        );
        assert_eq!(find_library(&INDICATOR_LIBS, &[], Some(b"\0\0"), native), None);
    }

    #[test]
    fn cache_paths_only_return_matching_absolute_paths() {
        let cache = b"\x01\x02libayatana-appindicator3.so.1\0/usr/lib/x86_64-linux-gnu/libayatana-appindicator3.so.1\0\
            /usr/lib/x86_64-linux-gnu/libfoo.so\0relative/libappindicator3.so.1\0/usr/lib/x86_64-linux-gnu/libayatana-appindicator3.so.1\0";
        assert_eq!(
            cache_paths(cache, &INDICATOR_LIBS),
            vec![PathBuf::from("/usr/lib/x86_64-linux-gnu/libayatana-appindicator3.so.1")]
        );
    }

    #[test]
    fn ld_so_conf_includes_are_expanded() {
        let dir = tempfile::tempdir().expect("tempdir");
        let conf_d = dir.path().join("ld.so.conf.d");
        std::fs::create_dir_all(&conf_d).expect("mkdir");
        std::fs::write(
            dir.path().join("ld.so.conf"),
            "# comment\ninclude ld.so.conf.d/*.conf\n/opt/legacy/lib, /opt/other=libc6\n",
        )
        .expect("conf");
        std::fs::write(conf_d.join("b-gnome.conf"), "/opt/gnome/lib  # trailing\n").expect("b");
        std::fs::write(
            conf_d.join("a-cuda.conf"),
            "/usr/local/cuda/lib64\ninclude nested.inc\n",
        )
        .expect("a");
        std::fs::write(conf_d.join("nested.inc"), "/opt/nested\n").expect("nested");
        std::fs::write(conf_d.join("ignored.txt"), "/opt/ignored\n").expect("ignored");
        // 自己包含自己：深度上限会截断
        std::fs::write(conf_d.join("loop.conf"), "include loop.conf\n").expect("loop");
        let mut dirs = Vec::new();
        ld_conf_dirs(&dir.path().join("ld.so.conf"), 0, &mut dirs);
        let expected: Vec<PathBuf> = [
            "/usr/local/cuda/lib64",
            "/opt/nested",
            "/opt/gnome/lib",
            "/opt/legacy/lib",
            "/opt/other",
        ]
        .iter()
        .map(PathBuf::from)
        .collect();
        assert_eq!(dirs, expected);
    }

    #[test]
    fn wildcards_match_like_the_shell() {
        assert!(wildcard_match("*.conf", "x86_64-linux-gnu.conf"));
        assert!(wildcard_match("*.conf", ".conf"));
        assert!(!wildcard_match("*.conf", "x.conf.bak"));
        assert!(wildcard_match("lib?.so*", "liba.so.1"));
        assert!(!wildcard_match("lib?.so", "libab.so"));
        assert!(wildcard_match("*a*b*", "xxaYYbzz"));
        assert!(wildcard_match("exact", "exact"));
        assert!(!wildcard_match("exact", "exactly"));
    }
}
