//! `script_plugin!` 两种依赖都声明的形态：两个列表各归各位、顺序不变。

use fuyubot_sdk_script::__macro_support::Plugin;
use fuyubot_sdk_script::script_plugin;

script_plugin!(
    "test.both",
    js,
    "both.js",
    inject = ["a.b"],
    soft_inject = ["c.d"]
);

#[test]
fn both_lists_are_declared_in_order() {
    assert_eq!(ScriptPlugin::inject(), vec!["a.b".to_owned()]);
    assert_eq!(ScriptPlugin::soft_inject(), vec!["c.d".to_owned()]);
}
