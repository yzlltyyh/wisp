# 模板资产

提示词模板的权威版本。`src/templates.rs` 用 `include_str!` 把它们编译进二进制（运行时零文件 IO），`tests/template_sync.rs` 核对文件格式与嵌入内容，`tests/snapshots/` 的快照测试覆盖渲染与装配结果。

离线翻译模型 Index-Translate 不用这里的模板：它对提示词格式敏感，按官方的单条指令格式（中文语言名、风格与术语写成约束要求）在 `src/index.rs`（`PromptProfile::IndexTranslate`）里拼装。改这里的翻译模板不影响离线翻译。

## 约定

- 保留模板里的换行和缩进；每个文件以**恰好一个**换行结尾（编辑器约定），编译期去掉这一个换行；不允许 CRLF（`../.gitattributes` 强制 LF 检出）。
- 文件名即模板名：去掉 `.txt` 的相对路径，例如 `styles/natural.system`、`ocr/lines.user`（见 `wisp_prompts::template`）。
- 风格模板（`styles/*` 与自定义风格的 system / tail）只替换 `{{source_lang}}` / `{{target_lang}}`，其余 `{{x}}` 原样保留（原文不会被提升到 system）。
- 低置信度时 `{{source_lang}}` 渲染为 `the source language`；紧跟限定词（a / an / the / single / bilingual 等）作定语时渲染为 `source-language`（「a single source-language word」「Edit the source-language text」），模板措辞不必改。

## 清单

| 文件 | 用途 |
| --- | --- |
| `hard_rules.txt` | 共用硬规则（末行 `{{glossary}}`），只拼到 6 个内置风格、自定义风格和 refine 改进步骤 |
| `glossary_block.txt` | 术语区块的标题与说明，其后逐行追加 `- src => tgt` |
| `user_skeleton.txt` | 翻译 / 词典式 user 消息骨架 |
| `styles/<id>.system.txt` | 6 个内置风格的 system |
| `styles/<id>.tail.txt` | 6 个内置风格的 tail |
| `dictionary.system.txt` | 词典式 system |
| `dictionary.technical.extra.txt` / `dictionary.casual.extra.txt` | 追加在词典式 Rules 末尾的一行 |
| `dictionary.tail.txt` | 词典式 tail |
| `dictionary.example.txt` | 词典式少样本示例（当前为零样本，不注入，只作渲染夹具） |
| `polish.system.txt` / `proofread.system.txt` / `edit.user.txt` | 润色、校对 |
| `ocr/base.txt` | OCR 基础块 |
| `ocr/{lines,paragraphs,markdown}.txt` 与 `.user.txt` | OCR 三种转录模式 |
| `ocr/translate.txt` / `ocr/translate.user.txt` | 截图直接翻译 |
| `refine/reflect.system.txt` / `reflect.user.txt` / `improve.user.txt` | 提升译文（先反思、再改进） |

## 修改模板

直接改这里的文件，然后运行 `cargo test -p wisp-prompts`：`tests/snapshots/` 的快照 diff 会列出受影响的提示词，确认无误后用 `INSTA_UPDATE=always cargo test -p wisp-prompts --test snapshots` 更新快照。改动提示词措辞时建议再跑一次评测运行器（`wisp-eval`，见 `src/eval/mod.rs`）对比前后结果。
