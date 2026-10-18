// 发布构建时隐藏控制台窗口，不要删掉这一行
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    glacc_auto_lib::run();
}
