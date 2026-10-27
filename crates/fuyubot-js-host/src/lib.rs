//! JS/TS 脚本的中间插件。
//!
//! 宿主进程里它是 ABI 插件，脚本进程里它是原生后端（链接进来的 `fuyubot_bridge_*`）。
//! 与 `fuyubot-python-host` 同构，只差语言参数（bun 而不是 uv）。

use std::sync::Arc;

use fuyubot_plugin_api::{HostApi, HostExt, Plugin, define_plugin};
use fuyubot_sdk_host::{Lang, ScriptHost, scripts_dir, spawn_scripts};

struct JsHost {
    api: HostApi,
    host: Option<Arc<ScriptHost>>,
}

impl Plugin for JsHost {
    fn init(api: HostApi) -> Result<Self, String> {
        Ok(Self { api, host: None })
    }

    fn start(&mut self, config_json: &str) -> Result<(), String> {
        let dir = scripts_dir(config_json);
        // `spawn: false`（`fuyubot plugin list` 传的）= 只发现不启动。
        let spawn = spawn_scripts(config_json);
        // 同 fuyubot-python-host：`new` 必须在插件作用域里跑。
        let host = Arc::new(ScriptHost::new(self.api, dir, spawn));
        let outcomes = host.start_all(Lang::Js);
        for (name, outcome) in &outcomes {
            if let Err(e) = outcome {
                self.api.warn(&format!("脚本 {name} 启动失败: {e}"));
            } else {
                self.api.info(&format!("脚本 {name} 已启动"));
            }
        }
        self.api.info(&format!(
            "fuyubot-js-host 就绪，共 {} 个脚本{}",
            host.script_count(),
            if spawn {
                ""
            } else {
                "（只发现模式：不装依赖、不起进程）"
            }
        ));
        // 装完依赖才挂 watch，否则 `bun install` 的写文件会被误判成新增脚本。
        // 只发现模式本来就没起进程，也不必 watch。
        if spawn {
            host.watch(Lang::Js);
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

define_plugin!(JsHost, "fuyubot.js-host");
