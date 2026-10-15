//! 模板文件与嵌入内容的一致性：`templates/` 下的每个 `.txt` 都以恰好一个 LF 换行结尾，
//! `include_str!` 嵌入的内容等于文件去掉末尾换行（编译期去掉，见 `src/templates.rs`）。

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

fn crate_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn template(name: &str) -> String {
    let path = crate_dir().join("templates").join(name);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("读取 {}: {e}", path.display()))
}

fn collect_txt(root: &Path, dir: &Path, out: &mut BTreeSet<String>) {
    for entry in fs::read_dir(dir).expect("read templates dir") {
        let path = entry.expect("dir entry").path();
        if path.is_dir() {
            collect_txt(root, &path, out);
        } else if path.extension().is_some_and(|e| e == "txt") {
            let rel = path.strip_prefix(root).expect("relative path");
            out.insert(rel.to_string_lossy().replace('\\', "/"));
        }
    }
}

#[test]
fn template_files_have_exactly_one_trailing_newline_and_lf() {
    let mut names = BTreeSet::new();
    collect_txt(
        &crate_dir().join("templates"),
        &crate_dir().join("templates"),
        &mut names,
    );
    for name in names {
        let t = template(&name);
        assert!(t.ends_with('\n') && !t.ends_with("\n\n"), "{name} 末尾应恰好一个换行");
        assert!(!t.contains('\r'), "{name} 应使用 LF 换行");
    }
}

#[test]
fn embedded_templates_equal_files() {
    // include_str! 嵌入的内容 = 文件去掉末尾换行（运行时零文件 IO）
    for name in wisp_prompts::template_names() {
        let embedded = wisp_prompts::template(name, wisp_prompts::TemplateVariant::En).expect("embedded template");
        let file = template(&format!("{name}.txt"));
        assert_eq!(format!("{embedded}\n"), file, "{name}");
    }
}
