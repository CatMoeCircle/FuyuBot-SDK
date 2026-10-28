# fuyubot-sdk-native

脚本插件的**原生桥**：编成 cdylib，在**脚本进程**里被 ctypes（Python）/ bun:ffi（JS）
加载，导出 `fuyubot_bridge_*` 一组 C 函数，读写脚本进程自己的 stdio 与宿主通信。

一句话职责：把「一行一个 JSON」的宿主协议沉进一份 Rust 实现，让 Python 与 JS 共用、
对协议的理解不可能漂移。

## 文件

- `src/lib.rs` — 原生桥本体（协议状态机 + `fuyubot_bridge_*` C 导出）。
  冻结的 C ABI 见文件头注释。
- `fuyubot.py` — Python 薄 SDK（ctypes 封装）。作者 `from fuyubot import on, run`；
  另有 `run_async` / `invoke_async` / `wait_async` 的 asyncio 版。
- `fuyubot.pyi` — Python 薄 SDK 的类型存根。
- `fuyubot.ts` — TS/JS 薄 SDK（bun:ffi 封装）。作者 `import { on, run } from "./fuyubot"`；
  另有 `runAsync` / `invokeAsync` / `waitAsync` 的 async 版。
- `fuyubot.d.ts` — TS 薄 SDK 的类型存根。

薄 SDK 只做一件事：把语言回调转成 C 函数指针，把 C 传回的 JSON 解码成 dict / object。
作者看不到任何 `"op"` / `tag` / `handle`。

## 薄 SDK 的两个 bun:ffi 坑（写进代码注释，别重蹈）

bun 1.3.1 的 bun:ffi 实测：

1. **`cstring` 参数不接受 JS string**——传给 C 的字符串必须 `Buffer.from(s + "\0")`。
2. **回调的 `cstring` 参数不可靠**——用 `ptr` + `new CString(p)` 手动读到 NUL 为止。

Python 的 ctypes 则没有这些坑（`CFUNCTYPE` + `c_char_p` 都自动处理）。

## 测试

`cargo test -p fuyubot-sdk-native` 用假 stdio 离线单测协议状态机；薄 SDK 各自带
`_selftest`（直接跑 `python fuyubot.py` / `bun fuyubot.ts`，需设 `FUYUBOT_NATIVE_LIB`
指向编好的 dll）。

`tests/python_async.rs` 会起一个**真的 Python 进程**跑 `run_async`，用 stdin/stdout
扮演 `fuyubot-python-host`，断言 `invoke_async` / `wait_async` 都能从 await 里返回；
没有 Python 解释器时跳过。`tests/js_async.rs` 对 bun 做同样的事（没有 bun 时跳过）。
