//! `script_plugin!` 的**老形态**（不声明依赖）：照旧能编译，两个列表都是空的。
//!
//! 每个 `tests/*.rs` 是一个**独立 crate**，所以四种形态可以各自展开一次
//! `define_plugin!` ——「一个 crate 只能展开一次」的约束逼出了这四个文件。
//!
//! 这些测试不 spawn 解释器、不碰 `Bridge`：只验「宏能不能编译 + 依赖有没有进 trait」。

use fuyubot_sdk_script::__macro_support::Plugin;
use fuyubot_sdk_script::script_plugin;

script_plugin!("test.plain", python, "plain.py");

#[test]
fn the_legacy_form_declares_no_dependencies() {
    // 不声明 = 空列表 = 宿主退回读 `[plugin.<名字>].inject`。
    assert!(ScriptPlugin::inject().is_empty());
    assert!(ScriptPlugin::soft_inject().is_empty());
}
