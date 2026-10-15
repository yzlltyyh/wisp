/**
 * 剪贴板的预览：非首屏代码，由 `../previewModule.ts` 加载并缓存
 * （不用 `React.lazy`：视图显示后在空闲时预取，之后同步拿到组件，第一次悬停 / 按 Space 不经过
 * Suspense、不等网络）。两者打包成一个 chunk。
 *
 * - HoverCard：悬停 300 ms 后画在面板内部、覆盖在列表上（不另开窗口，不预留卡片高度）：文本前 3 行、图片大图、
 *   文件列表、链接、颜色值；接收指针（指针在卡片上慢下来时保持、点卡片等于点对应的行），指针离开列表或正常速度
 *   扫过时收起；
 * - FullPreview：Space 打开，占满列表区：文本全文（可切换等宽字体、高亮搜索词、超长只渲染前 10 万字）、图片按面板
 *   大小显示、文件路径列表与「在 Finder 中显示」、链接与「打开链接」、颜色的 HEX / RGB / HSL；Esc 或 Space 返回，
 *   ↑↓ 切换条目。
 *
 * 原图（`clipboard_get` 的 image_url）只在这两处请求，卸载时把 `<img>` 的 src 清空，解码后的位图随之释放；
 * 文件条目只显示路径，不读文件内容、不列目录。
 */
export { FullPreview, type FullPreviewProps } from './FullPreview'
export { HoverCard, type HoverCardProps } from './HoverCard'
