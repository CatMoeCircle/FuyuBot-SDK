# examples：五个最小插件

结论先说：**这些示例从没在真实 Telegram 环境里验证过**——没有 api_id/api_hash。
可复现的验证是回放：用运行时的 `fuyubot run --replay` 把一批更新 JSON 灌进管线再退出，
不接 TDLib、不用账号。

| 例子 | 语言 | 产物 | 做什么 |
|---|---|---|---|
| `rust-echo` | Rust | `fuyubot_example_echo.{dll,so,dylib}` | 订阅 `updateNewMessage`，把正文原样回过去；跳过 `is_outgoing` |
| `c-ping` | C | `c_ping.dll`（**不是** workspace 成员） | 正文等于 `/ping` 就回 `pong`；C 侧最短闭环 |
| `go-ping` | Go（c-shared） | `ping.dll`（**不是** workspace 成员） | 和 `c-ping` 同行为，逻辑在 Go；Go 的库不会被卸载 |
| `python-weather` | Python + Rust 胶水 | `weather.dll` + `weather.py` | `/weather <城市>` 查 wttr.in |
| `ts-counter` | TypeScript + Rust 胶水 | `counter.dll` + `counter.ts` | 每聊天计数；证明 `fuyubot-sdk codegen` 的 `.d.ts` 可直接用（Node ≥ 23.6） |

脚本插件的 `[lib] name`（`weather` / `counter`）**不能省**：宿主按动态库文件名找同名脚本。

`examples/replay/` 里是回放样例：`new_message.json`（ping / 中文 / outgoing）、
`weather.json`（要联网）、`counter.json`（两个聊天各自计数）。

## 编译（Windows，实际用过的命令）

```powershell
# 运行时二进制从 https://github.com/CatMoeCircle/Fuyu-Bot/releases 取，或本地编。
cargo build -p fuyubot-example-echo    --target-dir examples\rust-echo\target
cargo build -p fuyubot-example-weather --target-dir examples\python-weather\target
cargo build -p fuyubot-example-counter --target-dir examples\ts-counter\target

# 装进一个 bot 项目的 plugins/（fuyubot 是运行时 CLI）
.\fuyubot.exe plugin install examples\rust-echo
.\fuyubot.exe plugin install examples\python-weather   # 连 weather.py / fuyubot_plugin.py 一起拷
.\fuyubot.exe plugin install examples\ts-counter       # 连 counter.ts / fuyubot_plugin.js 一起拷
```

C / Go 的示例先刷头文件（头文件是产物，不入库）：

```powershell
fuyubot-sdk codegen --lang c --dir examples\c-ping
# 然后用自己的 cl / gcc 编（见旧仓库的 examples/README 历史版本）
```

离线回放：

```powershell
.\fuyubot.exe run --replay examples\replay\new_message.json --plugin-dir <插件目录>
```

## 如实说明

- **没在真实 Telegram 里验过**：发送参数形状按 TDLib 文档写，回放只证明「收到更新 → 做对判断 → 发出对请求」。
- **`python-weather` 的回放也要联网**（wttr.in）。
- 要跑真环境：`fuyubot.toml` 填真实 api_id，环境变量给 `TG_API_HASH` / `TG_BOT_TOKEN`
  （凭据不落盘），装好 `tdjson`，然后 `fuyubot run`。这一步还没有真账号证据。
