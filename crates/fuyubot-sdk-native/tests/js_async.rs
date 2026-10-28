//! `fuyubot.ts`（bun 托管路径）的 async 路径真跑一遍。
//!
//! 用 stdin/stdout 扮演 `fuyubot-js-host`，起一个真的 bun 进程跑 `runAsync`，
//! 断言 `invokeAsync` / `waitAsync` 都能从 await 里返回值。没有 bun 时跳过。

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

const SCRIPT: &str = r#"
import { invokeAsync, waitAsync, runAsync } from "./fuyubot";

async function main() {
  const me = await invokeAsync("getMe", {});
  console.log("INVOKE:" + JSON.stringify(me));
  const upd = await waitAsync({ "@type": "updateNewMessage" }, 1000);
  console.log("WAIT:" + ((upd as any)?.["@type"] ?? "null"));
}

await runAsync(main);
"#;

#[test]
fn native_js_run_async_awaits_request_and_update() {
    let bun = ["bun", "bun.exe"].into_iter().find(|candidate| {
        Command::new(candidate)
            .arg("--version")
            .output()
            .map(|out| out.status.success())
            .unwrap_or(false)
    });
    let Some(bun) = bun else {
        eprintln!("跳过：找不到 bun");
        return;
    };
    let Some(lib) = native_library() else {
        eprintln!("跳过：找不到 fuyubot_sdk_native 动态库（先 cargo build -p fuyubot-sdk-native）");
        return;
    };

    let dir = std::env::temp_dir().join(format!("fuyubot-native-js-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("建临时目录");
    std::fs::copy(
        concat!(env!("CARGO_MANIFEST_DIR"), "/fuyubot.ts"),
        dir.join("fuyubot.ts"),
    )
    .expect("拷 fuyubot.ts");
    std::fs::write(dir.join("main.ts"), SCRIPT).expect("写 main.ts");

    let mut child = Command::new(bun)
        .arg("main.ts")
        .current_dir(&dir)
        .env("FUYUBOT_NATIVE_LIB", &lib)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("启动 bun");
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
        assert!(!remaining.is_zero(), "bun 脚本超时");
        let Ok(line) = rx.recv_timeout(remaining) else {
            break;
        };
        let Ok(message) = serde_json::from_str::<serde_json::Value>(&line) else {
            if line.starts_with("INVOKE:") && line.contains("\"@type\":\"user\"") {
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
    let status = child.wait().expect("等 bun 退出");
    let _ = std::fs::remove_dir_all(&dir);
    assert!(saw_invoke, "invokeAsync 没等到响应");
    assert!(saw_wait, "waitAsync 没等到更新");
    assert!(status.success(), "bun 退出码 {:?}", status.code());
}

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
