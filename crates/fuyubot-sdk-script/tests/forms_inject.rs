//! `script_plugin!` 只声明**硬依赖**的形态：`inject` 进 trait，软依赖仍是空列表。

use fuyubot_sdk_script::__macro_support::Plugin;
use fuyubot_sdk_script::script_plugin;

script_plugin!(
    "test.inject",
    js,
    "inject.js",
    inject = ["storage.mongo", "a.b"]
);

#[test]
fn only_the_hard_dependencies_are_declared() {
    assert_eq!(
        ScriptPlugin::inject(),
        vec!["storage.mongo".to_owned(), "a.b".to_owned()],
        "顺序要按宏里写的来"
    );
    assert!(ScriptPlugin::soft_inject().is_empty());
}
