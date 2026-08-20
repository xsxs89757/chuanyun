fn main() {
    // 两个入口都要编译：主窗口和托盘图标是各自独立的顶层组件
    slint_build::compile_with_config("ui/all.slint", slint_build::CompilerConfiguration::new())
        .expect("编译 Slint 界面失败");
}
