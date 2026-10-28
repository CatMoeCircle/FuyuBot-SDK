//! Python 脚本的中间插件。
//!
//! 宿主进程里它是 ABI 插件（`define_plugin!` 展开出的那整套导出符号 + 持有 `HostApi`）；
//! 脚本进程里它是原生后端（链接进来的 `fuyubot_bridge_*`，读写自己的 stdio）。
//! 同一份 dll 两个进程各加载一次，走两条完全不同的代码路径（见设计文档 4.1）。

use std::sync::Arc;

use fuyubot_plugin_api::{HostApi, HostExt, Plugin, define_plugin};
use fuyubot_sdk_host::{Lang, ScriptHost, scripts_dir, spawn_scripts};

struct PythonHost {
    api: HostApi,
    host: Option<Arc<ScriptHost>>,
}

impl Plugin for PythonHost {
    fn init(api: HostApi) -> Result<Self, String> {
        Ok(Self { api, host: None })
    }

    fn start(&mut self, config_json: &str) -> Result<(), String> {
        let dir = scripts_dir(config_json);
        // `spawn: false`（`fuyubot plugin list` 传的）= 只发现不启动。
        let spawn = spawn_scripts(config_json);
        // `ScriptHost::new` 要趁着宿主还在 `plugin_scope` 里调用，它创建的是继承本
        // 插件 owner 的根作用域——热加载进来的脚本靠它认祖归宗。
        let host = Arc::new(ScriptHost::new(self.api, dir, spawn));
        let outcomes = host.start_all(Lang::Python);
        for (name, outcome) in &outcomes {
            if let Err(e) = outcome {
                self.api.warn(&format!("脚本 {name} 启动失败: {e}"));
            } else {
                self.api.info(&format!("脚本 {name} 已启动"));
            }
        }
        self.api.info(&format!(
            "fuyubot-python-host 就绪，共 {} 个脚本{}",
            host.script_count(),
            if spawn {
                ""
            } else {
                "（只发现模式：不装依赖、不起进程）"
            }
        ));
        // 依赖装完、进程起来之后才挂 watch，否则 `uv sync` 的写文件会被误判成
        // 「新增脚本」（设计文档 S3 第一条细节）。只发现模式本来就没起进程，
        // 也不必 watch。
        if spawn {
            host.watch(Lang::Python);
        }
        self.host = Some(host);
        Ok(())
    }

    fn stop(&mut self) {
        if let Some(host) = self.host.take() {
            // 显式收 watch 线程：dll 卸载之后还在它代码段里跑的线程 = 段错误。
            host.stop_watch();
            host.stop_all();
        }
    }
}

define_plugin!(PythonHost, "fuyubot.python-host");
