//! `script_plugin!` 只声明**软依赖**的形态。
//!
//! 这条专测「第二个可选位单独出现」——它和 `inject` 只差一个关键字，
//! 规则写错时最容易在这里翻车（比如把 `soft_inject` 的值塞进 `inject()`）。

use fuyubot_sdk_script::__macro_support::Plugin;
use fuyubot_sdk_script::script_plugin;

script_plugin!(
    "test.soft",
    python,
    "soft.py",
    soft_inject = ["media.ffmpeg"]
);

#[test]
fn only_the_soft_dependencies_are_declared() {
    assert!(ScriptPlugin::inject().is_empty());
    assert_eq!(ScriptPlugin::soft_inject(), vec!["media.ffmpeg".to_owned()]);
}
