//! 连续粘贴的「这一下有没有粘进去」判定（三端共享的纯逻辑）。
//!
//! 事后核验（读输入框的值做前后对比）既要读用户内容、又证明不了失败（网页、远程桌面、终端的粘贴是异步的），所以只在
//! 重放粘贴键之后读一次焦点所在之处的**类别**（[`FocusFacts`]），能确定落空时才把这一条留到下一次。判定是三态的：
//! - [`PasteTarget::Rejects`]：确定落空（经典按钮、文件管理器的文件视图 / 桌面、菜单开着）；
//! - [`PasteTarget::Accepts`]：确定是文本输入控件（只用于日志）；
//! - [`PasteTarget::Unknown`]：其余一切，包括取不到、超时、缺权限。调用方对 Accepts 与 Unknown 一视同仁，照常前进——
//!   误判为落空的代价是下一次粘贴键再粘贴同一条，漏判的代价与没有这套判定时相同。
//!
//! 所有规则只对文本条目生效：文件条目在 Finder / 资源管理器里照样能粘贴，图片条目也有接收它的地方。
//!
//! 各平台怎样得到类别也是纯函数（[`windows_surface`]、[`finder_surface`]），在这里集中单测；平台实现只负责读系统状态。

use crate::types::{FocusFacts, FocusSurface};

/// 这一下粘贴的判定
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PasteTarget {
    /// 焦点在文本输入控件上
    Accepts,
    /// 确定落空
    Rejects(RejectWhy),
    /// 判断不了（照常前进）
    Unknown,
}

/// 落空的原因（只进日志）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RejectWhy {
    /// 焦点在经典按钮 / 复选框 / 单选框上：粘贴键沿着控件落空
    Control,
    /// 焦点在文件管理器的文件视图或桌面上：它们不接收文本
    FileView,
    /// 菜单开着：按键交给了菜单
    Menu,
}

/// 由焦点事实与条目种类得出判定。`facts` 为 None（取不到、超时、缺权限、这个平台不支持）时判断不了。
///
/// 规则按类别逐条列出；以后要加受设置控制的规则（例如网页正文的启发式），在 [`FocusSurface::Web`] 这一支里细分
pub fn classify(facts: Option<&FocusFacts>, text_item: bool) -> PasteTarget {
    let Some(facts) = facts else {
        return PasteTarget::Unknown;
    };
    if !text_item {
        return PasteTarget::Unknown;
    }
    match facts.surface {
        FocusSurface::TextInput => PasteTarget::Accepts,
        FocusSurface::Control => PasteTarget::Rejects(RejectWhy::Control),
        FocusSurface::FileView => PasteTarget::Rejects(RejectWhy::FileView),
        FocusSurface::Menu => PasteTarget::Rejects(RejectWhy::Menu),
        // 网页里点到输入框外（焦点落在正文）判断不了：Slack / Discord 把粘贴转进输入框，Notion、白板类网页在文档级接收
        FocusSurface::Web | FocusSurface::Other => PasteTarget::Unknown,
    }
}

/// Finder 的 bundle id（macOS 只对它做文件视图 / 桌面的识别，不对其他应用发 AX 请求）
pub const FINDER_BUNDLE_ID: &str = "com.apple.finder";

/// macOS：前台应用是 Finder 时，由焦点元素的 `AXRole` 与它是否在某个窗口里（`AXWindow` 属性；None = 没读到）得出类别。
/// 文件视图的四种角色（图标 / 列表 / 分栏 / 表格）与桌面（不属于任何窗口的分组 / 滚动区）是文件视图；输入框（搜索、
/// 重命名、前往文件夹、简介里的注释）是文本输入；其余判断不了
pub fn finder_surface(role: &str, in_window: Option<bool>) -> FocusSurface {
    match role {
        "AXTextField" | "AXTextArea" | "AXComboBox" => FocusSurface::TextInput,
        "AXList" | "AXOutline" | "AXTable" | "AXBrowser" => FocusSurface::FileView,
        "AXGroup" | "AXScrollArea" if in_window == Some(false) => FocusSurface::FileView,
        _ => FocusSurface::Other,
    }
}

/// macOS：前台应用是 Finder 时的焦点类别。`key_focus_pid` 是键盘焦点实际所在的进程（None = 没读到），`finder_role`
/// 读 Finder 焦点元素的角色与窗口归属（交给 [`finder_surface`]），只在键盘焦点确实在 Finder 时调用。
///
/// 前台应用是 Finder 不等于键盘焦点在 Finder：Spotlight、Alfred、Raycast 这类不激活自己的面板盖在 Finder 上时，前台应用
/// （菜单栏、`NSWorkspace`）仍是 Finder，Finder 自己报告的焦点却是面板背后的文件视图——只问 Finder 会把粘进面板的每一下
/// 都判为落空。键盘焦点在别的进程时是「其余」，读不到时判断不了
pub fn finder_focus_surface(
    finder_pid: i32,
    key_focus_pid: Option<i32>,
    finder_role: impl FnOnce() -> Option<(String, Option<bool>)>,
) -> Option<FocusSurface> {
    match key_focus_pid? {
        pid if pid == finder_pid => finder_role().map(|(role, in_window)| finder_surface(&role, in_window)),
        _ => Some(FocusSurface::Other),
    }
}

/// Windows 的类名比较不区分大小写（`RegisterClass` 的规则）
fn class_is(class: &str, name: &str) -> bool {
    class.eq_ignore_ascii_case(name)
}

fn class_starts(class: &str, prefix: &str) -> bool {
    class.len() >= prefix.len() && class.as_bytes()[..prefix.len()].eq_ignore_ascii_case(prefix.as_bytes())
}

/// 浏览器 / Electron / WebView2 的窗口类（Chromium 系的顶层窗口与渲染窗口、Gecko 系的顶层窗口）
fn is_web_class(class: &str) -> bool {
    class_starts(class, "Chrome_") || class_is(class, "MozillaWindowClass")
}

/// 桌面的顶层窗口
fn is_desktop_class(class: &str) -> bool {
    class_is(class, "Progman") || class_is(class, "WorkerW")
}

/// Windows：由前台窗口的类名、前台线程是否处于菜单模式（`GUI_INMENUMODE`）、键盘焦点窗口与它父窗口的类名（没有焦点窗口时
/// 为 None）得出类别。平台实现只读 `GetForegroundWindow` / `GetGUIThreadInfo` / `GetClassName`，不发 UI Automation 请求。
///
/// - 浏览器 / Electron（前台或焦点是 Chromium、Gecko 系的窗口）：网页，先于其余规则；
/// - 菜单模式：菜单；
/// - 经典编辑框（`Edit`、`RichEdit*`）：文本输入；经典按钮（`Button`，含复选框、单选框）：控件；
/// - 资源管理器的文件视图与桌面：`SHELLDLL_DefView` 里的 `DirectUIHWND` / `SysListView32`、`SHELLDLL_DefView` 本身，或前台
///   是桌面且没有焦点窗口。只认 `SHELLDLL_DefView` 这个父窗口——资源管理器的搜索框同样是 `DirectUIHWND`（无窗口的编辑框），
///   别的应用也用 `SysListView32`，单看类名会误判；
/// - XAML / WPF / WinForms / 其余：判断不了
pub fn windows_surface(
    foreground_class: &str,
    menu_mode: bool,
    focus_class: Option<&str>,
    focus_parent_class: Option<&str>,
) -> FocusSurface {
    if is_web_class(foreground_class) || focus_class.is_some_and(is_web_class) {
        return FocusSurface::Web;
    }
    if menu_mode {
        return FocusSurface::Menu;
    }
    let Some(focus) = focus_class else {
        return if is_desktop_class(foreground_class) {
            FocusSurface::FileView
        } else {
            FocusSurface::Other
        };
    };
    let in_shell_view = focus_parent_class.is_some_and(|p| class_is(p, "SHELLDLL_DefView"));
    if class_is(focus, "Edit") || class_starts(focus, "RichEdit") {
        FocusSurface::TextInput
    } else if class_is(focus, "Button") {
        FocusSurface::Control
    } else if class_is(focus, "SHELLDLL_DefView")
        || (in_shell_view && (class_is(focus, "DirectUIHWND") || class_is(focus, "SysListView32")))
    {
        FocusSurface::FileView
    } else {
        FocusSurface::Other
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(surface: FocusSurface) -> FocusFacts {
        FocusFacts { surface }
    }

    #[test]
    fn only_certain_surfaces_reject_and_only_for_text_items() {
        use FocusSurface::*;
        let cases = [
            (TextInput, PasteTarget::Accepts),
            (Control, PasteTarget::Rejects(RejectWhy::Control)),
            (FileView, PasteTarget::Rejects(RejectWhy::FileView)),
            (Menu, PasteTarget::Rejects(RejectWhy::Menu)),
            (Web, PasteTarget::Unknown),
            (Other, PasteTarget::Unknown),
        ];
        for (surface, want) in cases {
            assert_eq!(classify(Some(&facts(surface)), true), want, "{surface:?}");
            // 文件 / 图片条目：Finder、资源管理器照样能粘贴文件，一律判断不了
            assert_eq!(
                classify(Some(&facts(surface)), false),
                PasteTarget::Unknown,
                "{surface:?}"
            );
        }
        // 取不到、超时、缺权限
        assert_eq!(classify(None, true), PasteTarget::Unknown);
    }

    #[test]
    fn finder_views_and_desktop_are_file_views() {
        for role in ["AXList", "AXOutline", "AXTable", "AXBrowser"] {
            assert_eq!(finder_surface(role, Some(true)), FocusSurface::FileView, "{role}");
            assert_eq!(finder_surface(role, None), FocusSurface::FileView, "{role}");
        }
        // 桌面：不属于任何窗口；窗口里的分组（简介面板的折叠区）或没读到窗口属性时判断不了
        assert_eq!(finder_surface("AXGroup", Some(false)), FocusSurface::FileView);
        assert_eq!(finder_surface("AXScrollArea", Some(false)), FocusSurface::FileView);
        assert_eq!(finder_surface("AXGroup", Some(true)), FocusSurface::Other);
        assert_eq!(finder_surface("AXGroup", None), FocusSurface::Other);
        // 搜索框、重命名、前往文件夹、简介里的注释
        for role in ["AXTextField", "AXTextArea", "AXComboBox"] {
            assert_eq!(finder_surface(role, Some(true)), FocusSurface::TextInput, "{role}");
        }
        assert_eq!(
            finder_surface("AXButton", Some(true)),
            FocusSurface::Other,
            "macOS 不做控件规则"
        );
        assert_eq!(
            finder_surface("AXMenuItem", Some(true)),
            FocusSurface::Other,
            "macOS 不做菜单规则"
        );
    }

    #[test]
    fn finder_is_only_asked_when_it_holds_the_keyboard_focus() {
        const FINDER: i32 = 300;
        fn role(r: &'static str, w: Option<bool>) -> impl FnOnce() -> Option<(String, Option<bool>)> {
            move || Some((r.to_owned(), w))
        }
        // 键盘焦点在 Finder：照角色判断
        assert_eq!(
            finder_focus_surface(FINDER, Some(FINDER), role("AXList", Some(true))),
            Some(FocusSurface::FileView)
        );
        assert_eq!(
            finder_focus_surface(FINDER, Some(FINDER), role("AXTextField", Some(true))),
            Some(FocusSurface::TextInput)
        );
        // Finder 自己的焦点读不到（超时、缺权限）
        assert_eq!(finder_focus_surface(FINDER, Some(FINDER), || None), None);
        // Spotlight / Alfred / Raycast 的面板盖在 Finder 上、拿走了键盘焦点：不问 Finder，按「其余」
        let asked = std::cell::Cell::new(false);
        let finder_view = || {
            asked.set(true);
            Some(("AXList".to_owned(), Some(true)))
        };
        assert_eq!(
            finder_focus_surface(FINDER, Some(301), finder_view),
            Some(FocusSurface::Other)
        );
        assert!(!asked.get(), "键盘焦点不在 Finder 时不读它的焦点元素");
        // 键盘焦点所在的进程读不到：判断不了，也不问 Finder
        let finder_view = || {
            asked.set(true);
            Some(("AXList".to_owned(), Some(true)))
        };
        assert_eq!(finder_focus_surface(FINDER, None, finder_view), None);
        assert!(!asked.get());
    }

    #[test]
    fn windows_web_windows_are_never_judged() {
        // 浏览器 / Electron：即便菜单开着、焦点在按钮类的子窗口上也判断不了
        for fg in ["Chrome_WidgetWin_1", "chrome_widgetwin_0", "MozillaWindowClass"] {
            assert_eq!(
                windows_surface(fg, true, Some("Button"), None),
                FocusSurface::Web,
                "{fg}"
            );
            assert_eq!(windows_surface(fg, false, None, None), FocusSurface::Web, "{fg}");
        }
        // WebView2 嵌在普通应用里：焦点是渲染窗口
        assert_eq!(
            windows_surface(
                "WindowsForms10.Window.8.app.0.1",
                false,
                Some("Chrome_RenderWidgetHostHWND"),
                None
            ),
            FocusSurface::Web
        );
    }

    #[test]
    fn windows_classic_controls_menus_and_edits() {
        assert_eq!(
            windows_surface("#32770", false, Some("Button"), Some("#32770")),
            FocusSurface::Control
        );
        assert_eq!(
            windows_surface("#32770", false, Some("button"), None),
            FocusSurface::Control
        );
        assert_eq!(
            windows_surface("Notepad", true, Some("Edit"), Some("Notepad")),
            FocusSurface::Menu
        );
        assert_eq!(
            windows_surface("Notepad", false, Some("Edit"), Some("Notepad")),
            FocusSurface::TextInput
        );
        assert_eq!(
            windows_surface("WordPadClass", false, Some("RICHEDIT50W"), None),
            FocusSurface::TextInput
        );
        // WinForms 的按钮、XAML、WPF、终端：判断不了
        for focus in [
            "WindowsForms10.BUTTON.app.0.141b42a_r6_ad1",
            "Windows.UI.Core.CoreWindow",
            "HwndWrapper[App;;1]",
            "CASCADIA_HOSTING_WINDOW_CLASS",
        ] {
            assert_eq!(
                windows_surface("SomeApp", false, Some(focus), None),
                FocusSurface::Other,
                "{focus}"
            );
        }
    }

    #[test]
    fn windows_shell_views_need_the_shell_parent() {
        // 资源管理器的文件视图、桌面的图标视图
        assert_eq!(
            windows_surface("CabinetWClass", false, Some("DirectUIHWND"), Some("SHELLDLL_DefView")),
            FocusSurface::FileView
        );
        assert_eq!(
            windows_surface("Progman", false, Some("SysListView32"), Some("SHELLDLL_DefView")),
            FocusSurface::FileView
        );
        assert_eq!(windows_surface("WorkerW", false, None, None), FocusSurface::FileView);
        assert_eq!(
            windows_surface("CabinetWClass", false, Some("SHELLDLL_DefView"), None),
            FocusSurface::FileView
        );
        // 资源管理器的搜索框也是 DirectUIHWND，但父窗口不是 SHELLDLL_DefView；别的应用里的列表视图同理
        assert_eq!(
            windows_surface(
                "CabinetWClass",
                false,
                Some("DirectUIHWND"),
                Some("SearchEditBoxWrapperClass")
            ),
            FocusSurface::Other
        );
        assert_eq!(
            windows_surface("RegEdit_RegEdit", false, Some("SysListView32"), Some("RegEdit_RegEdit")),
            FocusSurface::Other
        );
        // 没有焦点窗口、前台也不是桌面
        assert_eq!(windows_surface("SomeApp", false, None, None), FocusSurface::Other);
        // 重命名时的编辑框
        assert_eq!(
            windows_surface("CabinetWClass", false, Some("Edit"), Some("DirectUIHWND")),
            FocusSurface::TextInput
        );
    }
}
