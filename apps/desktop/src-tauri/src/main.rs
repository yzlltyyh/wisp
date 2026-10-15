// 发布版在 Windows 上不弹控制台窗口
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    wisp_desktop_lib::run();
}
