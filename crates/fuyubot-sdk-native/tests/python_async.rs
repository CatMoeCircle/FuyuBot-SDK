//! `fuyubot.py`（uv 托管路径）的 asyncio 路径真跑一遍。
//!
//! 用 stdin/stdout 扮演 `fuyubot-python-host`，起一个真的 Python 进程跑
//! `fuyubot.run_async`，断言 `invoke_async` / `wait_async` 都能从 await 里拿到值。
//! 没有 Python 解释器时跳过。

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

const SCRIPT: &str = r#"
import asyncio
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from fuyubot import invoke_async, wait_async, run_async

async def main():
    me = await invoke_async("getMe", {})
    print("INVOKE:" + str(me.get("@type")), flush=True)
    upd = await wait_async({"@type": "updateNewMessage"}, 1000)
    print("WAIT:" + str((upd or {}).get("@type")), flush=True)
    asyncio.get_running_loop().stop()

run_async(main)
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
fn native_python_run_async_awaits_request_and_update() {
    let Some(python) = interpreter() else {
        eprintln!("跳过：找不到 python / python3 / py");
        return;
    };
    let Some(lib) = native_library() else {
        eprintln!("跳过：找不到 fuyubot_sdk_native 动态库（先 cargo build -p fuyubot-sdk-native）");
        return;
    };

    let dir = std::env::temp_dir().join(format!("fuyubot-native-py-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("建临时目录");
    std::fs::copy(
        concat!(env!("CARGO_MANIFEST_DIR"), "/fuyubot.py"),
        dir.join("fuyubot.py"),
    )
    .expect("拷 fuyubot.py");
    std::fs::write(dir.join("plugin.py"), SCRIPT).expect("写 plugin.py");

    let mut child = Command::new(python)
        .arg("plugin.py")
        .current_dir(&dir)
        .env("FUYUBOT_NATIVE_LIB", &lib)
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

    let deadline = Instant::now() + Duration::from_secs(15);
    let mut saw_invoke = false;
    let mut saw_wait = false;
    while !(saw_invoke && saw_wait) {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(!remaining.is_zero(), "python 脚本超时");
        let Ok(line) = rx.recv_timeout(remaining) else {
            break;
        };
        let Ok(message) = serde_json::from_str::<serde_json::Value>(&line) else {
            if line.starts_with("INVOKE:user") {
                saw_invoke = true;
            } else if line.starts_with("WAIT:updateNewMessage") {
                saw_wait = true;
            }
            continue;
        };
        let op = message.get("op").and_then(|v| v.as_str()).unwrap_or("");
        let tag = message.get("tag").and_then(|v| v.as_u64()).unwrap_or(0);
        match op {
            "on" => {
                let handle = 10 + tag;
                writeln!(stdin, r#"{{"op":"on","tag":{tag},"handle":{handle}}}"#).unwrap();
                stdin.flush().unwrap();
                if tag == 3 {
                    // wait_async 的 update 订阅建立后，再回一条匹配的更新。
                    writeln!(
                        stdin,
                        r#"{{"op":"event","handle":{handle},"payload":{{"@type":"updateNewMessage"}}}}"#
                    )
                    .unwrap();
                    stdin.flush().unwrap();
                }
            }
            "invoke" => {
                writeln!(stdin, r#"{{"op":"invoked","tag":{tag},"request_id":7}}"#).unwrap();
                // 响应事件在原生桥里是特殊通道：带 event="response"，按 request_id 配对，
                // 不按 handle 分发，所以这里不带 handle。
                writeln!(
                    stdin,
                    r#"{{"op":"event","event":"response","payload":{{"request_id":7,"@type":"user"}}}}"#
                )
                .unwrap();
                stdin.flush().unwrap();
            }
            _ => {}
        }
    }

    drop(stdin);
    let status = child.wait().expect("等 python 退出");
    let _ = std::fs::remove_dir_all(&dir);
    assert!(saw_invoke, "invoke_async 没等到响应");
    assert!(saw_wait, "wait_async 没等到更新");
    assert!(status.success(), "python 退出码 {:?}", status.code());
}

/// 找 cargo 编出来的原生桥动态库（测试二进制所在 target 目录的上一级）。
fn native_library() -> Option<std::path::PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let profile = exe.parent()?.parent()?;
    let name = format!(
        "{}fuyubot_sdk_native{}",
        std::env::consts::DLL_PREFIX,
        std::env::consts::DLL_SUFFIX
    );
    let path = profile.join(name);
    path.is_file().then_some(path)
}
