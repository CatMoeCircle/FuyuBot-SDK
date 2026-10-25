# FuyuBot SDK

插件作者的工具箱：插件 ABI 的安全包装、Python / JS / TS 脚本桥、以及开发 CLI `fuyubot-sdk`。
运行时本体在 [Fuyu-Bot](https://github.com/xiaoqvan/Fuyu-Bot)，文档站在
[FuyuBot-Docs](https://github.com/xiaoqvan/FuyuBot-Docs)。

## crate

| crate | 干什么 | 发布 |
| --- | --- | --- |
| `fuyubot-plugin-api` | 插件作者唯一要依赖的 Rust crate：重导出 ABI + `Subscription`（drop 即退订）+ 版本协商 | crates.io |
| `fuyubot-sdk-script` | Python / JS / TS 脚本桥：cdylib 起解释器、行 JSON 协议、`script_plugin!` 宏 | crates.io |
| `fuyubot-sdk-cli` | 二进制 `fuyubot-sdk`：脚手架 / ABI 绑定 / `td-methods` / 静态 `check` | Release 里的 zip |

`fuyubot-abi` 与 `fuyubot-tdlib-sys` 是**运行时仓库**的 crate（`fuyubot-sdk` 只用它们做
`td-methods` 查询），从 crates.io 拉；所以本仓库构建前，bot 仓库得先把它们发上去。

## 构建与自检

```bash
cargo build --workspace
cargo test  --workspace
cargo fmt --all --check
```

`td-methods` / `td-method` 需要 `td_api.tl`：`fuyubot-tdlib-sys/build.rs` 会从缓存或 Releases 取。
本机离线开发可以把缓存指向运行时仓库那份：

```bash
export FUYUBOT_TDLIB_VENDORED_CACHE=/path/to/Fuyu-Bot/.tdlib-cache
```

## 插件开发

```bash
fuyubot-sdk init mybot --lang rust      # 生成 <插件工程>/
fuyubot-sdk codegen --lang c --dir <工程> # 刷 C 头文件
fuyubot-sdk check path/to/plugin.dll    # 不加载、不运行，只读导出符号
```

模板里依赖写的是 crates.io 版本；想在本地对着 SDK 源码开发就设 `FUYUBOT_SDK_PATH`
指向本仓库的 `crates/`（生成器会写成 `path = …`）。

示例在 [`examples/`](examples/)（Rust / C / Go / Python / TS）。它们和运行时本体一样
**从没在真实 Telegram 上验过**；可复现的验证是回放（`examples/replay/`）。

## 发布

- **crate**：`.github/workflows/publish-crates.yml` 手动触发（需要 `CARGO_REGISTRY_TOKEN`）。
  顺序：bot 仓库先发 `fuyubot-abi` / `fuyubot-tdlib-sys` → 这里发 `fuyubot-plugin-api` → 发 `fuyubot-sdk-script`。
- **`fuyubot-sdk` 二进制**：`.github/workflows/release-sdk.yml` 在 `v*` tag 上产
  `fuyubot-sdk-<os>-<arch>.zip`（Windows / Linux 各 x86_64 + aarch64，macOS aarch64）。
  它不需要 `tdjson`。

## 与运行时的版本关系

`fuyubot-plugin-api` 的 `SDK_ABI_VERSION` 必须与运行时 `fuyubot-abi` 的 `ABI_VERSION` 一致；
两边都从同一份 ABI 定义演进。改 ABI 时先在 bot 仓库改、发版，再升这里的依赖版本。
