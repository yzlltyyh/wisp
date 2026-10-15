// scripts/perf/macos-webview-probe.swift：measure-macos.sh --dry-run 用的最小 WKWebView 应用。
//
// 只做一件事：创建一个 WKWebView（不显示窗口），页面里每 200 ms 忙算 40 ms。这样 WebKit 会拉起
// WebContent / GPU / Networking 三个 XPC 进程，WebContent 上还有可测的 CPU 与唤醒——dry-run 借此验证
// WebView 的归属（经 LaunchServices 启动时精确、从终端启动时近似）以及 WebView 的 RSS / CPU / 唤醒采样。
// 忙算按时间而不是按循环次数：隐藏页面的定时器会被 WebKit 节流到约每秒一次，按次数的小计算在节流后只剩 1% 上下，
// 3 秒的 dry-run 里可能一个 ps 的 10 ms 计时单位都攒不到；按时间算，节流后每秒仍有 40 ms（约 4%）。
// 构建：swiftc -O -o <App>.app/Contents/MacOS/wisp-webview-probe macos-webview-probe.swift（Info.plist 由脚本生成）。
import AppKit
import WebKit

final class ProbeDelegate: NSObject, NSApplicationDelegate {
  private var window: NSWindow?
  private var webView: WKWebView?

  func applicationDidFinishLaunching(_ notification: Notification) {
    let frame = NSRect(x: 0, y: 0, width: 64, height: 64)
    let window = NSWindow(contentRect: frame, styleMask: [.borderless], backing: .buffered, defer: false)
    let webView = WKWebView(frame: frame)
    window.contentView = webView
    let html = """
      <!doctype html><title>wisp perf probe</title><script>
      setInterval(() => { const end = performance.now() + 40; let x = 0; while (performance.now() < end) x++; document.title = String(x) }, 200)
      </script>
      """
    webView.loadHTMLString(html, baseURL: nil)
    self.window = window
    self.webView = webView
  }
}

let app = NSApplication.shared
app.setActivationPolicy(.accessory)
let delegate = ProbeDelegate()
app.delegate = delegate
app.run()
