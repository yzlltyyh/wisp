/**
 * 视觉稿与展示页用的示例文案（只在浏览器 mock 中加载）。内容刻意中英混排、带 Markdown-lite 结构，
 * 用来检验 CJK 排版（行高 1.6、中英文间距、标点不悬挂）与增量渲染。
 */
export const SOURCE_EN =
  'Wisp stays out of your way: it idles at near-zero CPU, wakes in milliseconds, and streams the translation right next to your cursor.'

export const TRANSLATION_ZH =
  'Wisp 从不打扰你：空闲时几乎不占 CPU，唤出只需几毫秒，译文就在光标旁边逐字流出。'

export const TRANSLATION_LONG_ZH =
  'Wisp 从不打扰你：空闲时几乎不占 CPU，唤出只需几毫秒，译文就在光标旁边逐字流出。面板高度随内容平滑增长，' +
  '达到上限后改为内部滚动；你在原应用里继续滚动、选择文字，面板都不会抢走焦点。'

export const LEARN_SAMPLE = `**译文**
它在后台安静待命，几乎不占用 CPU。

**词汇**
- **idle** /ˈaɪdl/ *v.* 空转、待机
- **wake** /weɪk/ *v.* 唤醒；在这里指从托盘唤出面板
- **stream** /striːm/ *v.* 以流的方式逐段输出

**语法**
\`stays out of your way\` 是固定搭配，意为「不碍事、不打扰」。`

export const EDIT_SAMPLE = `Thanks for the quick review. I've fixed the issues you mentioned and pushed a new build.

---
改动：补上冠词 the；"fix" 改为现在完成时；拆分长句。`

export const OCR_TEXT = `性能预算
面板唤出 p95 ≤ 100 ms
首 token p95 ≤ 1.2 s
空闲 CPU ≈ 0%
常驻内存 ≤ 60 MB`

export const OCR_TRANSLATION = `Performance budget
Panel show p95 ≤ 100 ms
First token p95 ≤ 1.2 s
Idle CPU ≈ 0%
Resident memory ≤ 60 MB`

export const INPUT_DRAFT =
  '把这段话翻译成英文：周五之前把设计稿和实现计划一起交付，周末前完成联调。'
