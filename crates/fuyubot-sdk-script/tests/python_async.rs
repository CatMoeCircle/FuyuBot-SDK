//! `fuyubot_plugin.py` 的 asyncio 路径（`run_async` / `invoke_async` / `wait_async`）真跑一遍。
//!
//! 不依赖宿主：用 stdin/stdout 扮演 `fuyubot-python-host` 那一侧，喂协议行，断言脚本
//! 真的从 `await` 里拿到了响应和更新。没有 Python 解释器时跳过（和 C 工具链测试一个态度）。

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

const SCRIPT: &str = r#"
import asyncio
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import fuyubot_plugin as host

async def main():
    me = await host.invoke_async("getMe", {})
    print("INVOKE:" + str(me.get("@type")), flush=True)
    upd = await host.wait_async({"@type": "updateNewMessage"}, 1000)
    print("WAIT:" + str((upd or {}).get("@type")), flush=True)
    asyncio.get_running_loop().stop()

host.run_async(main)
"#;

fn interpreter() -> Option<&'static str> {
    for candidate in ["python", "python3", "py"] {
        let ok = Command::new(candidate)
            .arg("--version")
            .output()
            .map(|out| out.status.success())
            .unwrap_or(false);
        if ok {
            return Some(candidate);
        }
    }
    None
}

#[test]
fn python_run_async_awaits_request_and_update() {
    let Some(python) = interpreter() else {
        eprintln!("跳过：找不到 python / python3 / py");
        return;
    };

    let dir = std::env::temp_dir().join(format!("fuyubot-py-async-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("建临时目录");
    std::fs::copy(
        concat!(env!("CARGO_MANIFEST_DIR"), "/python/fuyubot_plugin.py"),
        dir.join("fuyubot_plugin.py"),
    )
    .expect("拷 fuyubot_plugin.py");
    std::fs::write(dir.join("plugin.py"), SCRIPT).expect("写 plugin.py");

    let mut child = Command::new(python)
        .arg("plugin.py")
        .current_dir(&dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("启动 python");
    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            let _ = tx.send(line);
        }
    });

    let deadline = Instant::now() + Duration::from_secs(10);
    let mut saw_invoke = false;
    let mut saw_wait = false;
    while !(saw_invoke && saw_wait) {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(!remaining.is_zero(), "python 脚本超时");
        let Ok(line) = rx.recv_timeout(remaining) else {
            break;
        };
        if line.contains("\"op\":\"on\"") {
            writeln!(stdin, r#"{{"op":"on","tag":1,"handle":10}}"#).unwrap();
            stdin.flush().unwrap();
        } else if line.contains("\"op\":\"invoke\"") {
            writeln!(stdin, r#"{{"op":"invoked","tag":2,"request_id":7}}"#).unwrap();
            writeln!(
                stdin,
                r#"{{"op":"event","handle":10,"payload":{{"request_id":7,"@type":"user"}}}}"#
            )
            .unwrap();
            stdin.flush().unwrap();
        } else if line.contains("\"op\":\"wait\"") {
            writeln!(
                stdin,
                r#"{{"op":"wait","tag":3,"payload":{{"@type":"updateNewMessage"}}}}"#
            )
            .unwrap();
            stdin.flush().unwrap();
        } else if line.starts_with("INVOKE:user") {
            saw_invoke = true;
        } else if line.starts_with("WAIT:updateNewMessage") {
            saw_wait = true;
        }
    }

    // 关 stdin 让 reader 线程看到 EOF，run_async 的 loop 停下来。
    drop(stdin);
    let status = child.wait().expect("等 python 退出");
    let _ = std::fs::remove_dir_all(&dir);
    assert!(saw_invoke, "invoke_async 没等到响应");
    assert!(saw_wait, "wait_async 没等到更新");
    assert!(status.success(), "python 退出码 {:?}", status.code());
}
