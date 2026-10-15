/**
 * Overlay 首帧核心（index.ts，静态加载）与交互模块（select.ts，首帧之后动态加载）之间的接口。只有类型。
 *
 * 拆分的理由：Overlay 入口的统计口径是「入口 + 静态 import + CSS ≤ 15 KB」，动态 import 不计；
 * 首帧路径（订阅 → init → 冻结帧 → 暗化层 → ready）只依赖核心，交互模块与 init / 拉帧并行加载，用户开始拖拽之前
 * 早已就绪（本地自定义协议，毫秒级）。交互模块加载完之前，核心自己处理 Esc。交互模块不直接引用 IPC（命令经 host 转交），
 * 动态 chunk 因此没有依赖，入口里也就没有预加载依赖表。
 */
import type { OverlayCommit, OverlayCommitAck, OverlayInit } from '../shared/ipc/overlay'

export interface OverlayHost {
  readonly root: HTMLElement
  /** 卸载信号：所有事件监听都挂在它上面（页面只挂载一次；测试里每个用例卸载） */
  readonly signal: AbortSignal
  /** 冻结帧（Windows / X11；macOS 隐藏）：像素尺寸 = 显示器物理像素 */
  readonly frame: HTMLCanvasElement
  /** 暗化层（`--dim-overlay`），选区在它上面挖洞 */
  readonly dim: HTMLElement
  /**
   * 取消本次截图（Esc / 右键 / commit 失败）：释放资源并 `overlay_cancel`。commit 之后再调用时，编排器只拆掉 Overlay
   * （请求已在 commit 时发出、面板照常出现），并不中止请求
   */
  cancel(): void
  /** `overlay_commit`：Rust 立即裁剪、发请求、把面板显示在 Overlay 下方，返回飞入目标 */
  commit(c: OverlayCommit): Promise<OverlayCommitAck>
  /** 飞入与淡出结束：释放冻结帧与位图，`overlay_done` */
  done(): void
  /** 拉取本会话的 RGBA 帧（`fetchRgbaBitmap` + `resolveAssetUrl`，会话结束时中止） */
  load(url: string): Promise<ImageBitmap>
  /** 把位图铺满冻结帧 canvas（Windows / X11：half → 全分辨率） */
  paint(bitmap: ImageBitmap): void
}

export interface Interaction {
  /** 新会话：重置全部状态 */
  begin(init: OverlayInit): void
  /**
   * 首帧（冻结帧）已上屏：开始拉全分辨率帧——Windows / X11 换掉 1/2 帧，放大镜从此可见。`full` = 首帧本身
   * 就是全分辨率帧（没有 half_url、页面自己画冻结帧，例如 macOS 原生冻结帧不可用时的回退）：直接用画好的冻结帧，不再拉取、解码第二次
   */
  shown(full: boolean): void
  /** 会话结束：不再响应输入 */
  end(): void
}
