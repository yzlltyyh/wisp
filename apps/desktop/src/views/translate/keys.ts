/**
 * 翻译 / 输入视图共用的按键处理。实现在组件库（shared/keys/menuKeys.ts，OCR 的风格菜单同样要用），
 * 这里只是转出。
 *
 * **浏览器加速键**：Windows 的 WebView2 默认启用浏览器加速键（wry 的 `browser_accelerator_keys` 默认为 true，外壳没有
 * 统一吞掉）——⌘R / Ctrl+R 重载页面（面板会话随之结束、Pinned 重新 pinned_init）、Ctrl+Shift+R 强制重载、Ctrl+P 打印
 * 预览。视图把这几个键绑定成自己的动作；动作当前不可用时**也要吞掉**（绑定返回 undefined，不返回 false），否则键盘
 * 路由不调用 preventDefault，按键原样落到 WebView。更好的做法是在 Rust 侧关掉面板 / Pinned 窗口的浏览器加速键，或由外壳在
 * 全局层统一吞掉。
 */
export { BROWSER_ACCELERATORS, swallow, useMenuKeys } from '../../shared/keys/menuKeys'
