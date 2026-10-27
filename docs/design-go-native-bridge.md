# Go 插件改走「进程式」的设计方案（对齐 `fuyubot-sdk-script`）

> 状态：**只出方案，代码一行没动**。老形态（c-shared）按用户意见**暂时不动**，
> 本仓 loader 的 `Go buildinf` 识别与「不卸载」兜底继续留着。
> 归属：改动主要在本仓（FuyuBot-SDK），本仓之外的连带改动在 FuyuBot（运行时）。

## 1. 为什么改

`examples/go-ping` 今天的形态是 `go build -buildmode=c-shared` + cgo + 一个 C 薄壳
（`plugin.c` 负责构造按值返回的 `PluginHandle`）。宿主因此必须 `dlopen` 一个**含整套
Go runtime** 的库，而 Go 的 c-shared 库**不能卸载**（[golang/go#11100](https://go.dev/issue/11100)，
2015 至今 open；Linux 上 Go 链接器用 `-z nodelete` 把 `dlclose` 变成 no-op，
Windows 上没有等价物，`FreeLibrary` 会真的卸载 → `0xC0000005`）。

连带代价：

| 代价 | 表现 |
| --- | --- |
| 不可卸载 | 宿主 `LoadedPlugin::drop` 只能 `mem::forget` 句柄（FuyuBot `loader.rs:109-113`） |
| 文件被占用 | Windows 上 `.dll` 一直锁着，进程运行时删不掉/覆盖不了 |
| 不可热重载 | 同路径重复 `dlopen` 只是引用计数 +1，runtime 不重新初始化，而 `plugin_init` 会被再调 |
| 进程内多 runtime | 两个 Go 插件 = 两套调度器/GC，都去装信号处理器 |
| 工具链门槛 | 需要 cgo，也就是一台 C 编译器（Windows 上要 mingw-w64） |

## 2. 目标形态

```
fuyubot 宿主 ──dlopen──> 小 cdylib（Rust，可正常卸载）──spawn──> ping(.exe)（普通 Go 可执行文件）
       ▲                                                            │
       └──────────── HostApiVTable 调用 ◄── stdin/stdout 一行一个 JSON ─┘
```

与 `python-weather` / `ts-counter` **完全同构**：宿主眼里它就是一个脚本插件，
区别只是「子进程」从 `python x.py` 换成了直接跑一个编译好的 Go 可执行文件。
Go 侧因此**不再需要 cgo / `-buildmode=c-shared` / `plugin.c` / `fuyubot_plugin.h` / mingw**。

## 3. 方案 A：给 script 桥加 `native` 变体（推荐）

理由：`fuyubot-sdk-script` 存在的初衷就是「Python 与 JS **共用这一份实现**，
所以两个 SDK 不可能漂移」。让 Go 也进这一份实现，ABI 理解、订阅/工具/日志的语义、
启动超时与错误提示全都复用，不需要为 Go 单开一条维护线。

### A-1 本仓（FuyuBot-SDK）改动

| # | 位置 | 改什么 |
| --- | --- | --- |
| 1 | `crates/fuyubot-sdk-script/src/lib.rs` `Runtime` | 加 `Native` 变体。`candidates()` 语义从「依次试几个解释器名」变成「起这个可执行文件」：Windows 自动补 `.exe`，Unix 用原名；`label()` 返回 `"go"` |
| 2 | 同上 `spawn()` | native 分支：`Command::new(exe).arg(config_json)`，**不传** script 参数（可执行文件本身就是脚本）；不加 `PYTHONUNBUFFERED`（Go 的 `os.Stdout` 本来就不缓冲） |
| 3 | 同上 `script_plugin!` | 加一个 arm：`($name:literal, go, $script:literal)` → `Runtime::Native`。`$script` 在这条分支里语义是「可执行文件名」 |
| 4 | 新增 `crates/fuyubot-sdk-script/golang/fuyubot_plugin.go` | 协议客户端，对齐 `python/fuyubot_plugin.py`（220 行）/ `js/fuyubot_plugin.js`（237 行）：stdin/stdout 各一行一个 JSON，`ready` / `log` / `on` / `off` / `invoke` / `emit` / `tool`，`tag` 配对，goroutine 分发事件 |
| 5 | `crates/fuyubot-sdk-cli` | `Lang::Go` + `templates/go/{Cargo.toml, src/lib.rs, go.mod, main.go}`；生成时把 `fuyubot_plugin.go` 逐字拷进工程（与 `BRIDGE_PY` 同样走 `include_str!`，保证不漂移） |
| 6 | `examples/go-ping` | 重写：`main.go` 用 Go SDK、`go build`（普通可执行文件）、三行宏的 `Cargo.toml` + `src/lib.rs`；删掉 `plugin.c` 与 `fuyubot_plugin.h` |
| 7 | 桥的自检单测 | 加 native 分支：真 `go build` 一个可执行文件再跑一遍协议；**与 python/node 一样「找不到工具链就跳过」**，别让构建环境对 Go 有硬依赖（现有 `tests` 里 `("python", ...)` / `("node", ...)` 就是这么做的） |

`fuyubot-sdk codegen --lang go`：**不做**。Go 是静态类型，没有 `.pyi` / `.d.ts` 之类的
存根要落；收到就报错提示「Go 插件不需要 codegen，用 `fuyubot-sdk check`」。

### A-2 FuyuBot（运行时仓）改动

| # | 位置 | 改什么 |
| --- | --- | --- |
| 8 | `crates/fuyubot-cli/src/scaffold.rs::find_script` | 候选扩展名从 `["py","js","ts"]` 加上 `exe`（Windows）；Unix 上可执行文件常无扩展名，需要额外判一次 `is_file() + 可执行位`（可选，见待定 2） |
| 9 | 同上 `install()` 与 `companion_dirs()` | 自动跟着 cdylib 一起拷走同伴文件（Go 可执行文件），否则 `plugin install` 只拷了 cdylib，桥起来找不到可执行文件 |
| 10 | `crates/fuyubot-cli/src/main.rs:472` | 无改动，但语义变了：Go 插件会被算成 `scripted` → `--replay` 用脚本插件的等待窗口（正确，它确实是跨进程） |
| 11 | 文档站 / README | Go 插件的产物从「一个动态库」变成「cdylib + 可执行文件两件套」 |

**协议完全复用现有的 8 个 op，一个都不新增**（`ready` / `log` / `on` / `off` / `invoke` /
`emit` / `tool` + 宿主→脚本的 `event`），所以 A-1 里第 4 项是纯客户端实现，不碰宿主。

## 4. 方案 B：宿主侧「通用桥」cdylib（备选，改动更大）

让 SDK 预编译一份**通用桥**（`fuyubot_bridge_native.{dll,so}`），插件作者只交付
「一个可执行文件 + 一份描述（名字 / 可执行文件 / 参数）」，完全不碰 Rust/cargo。

- 好处：Go 作者不装 Rust 工具链；一个桥文件服务所有 Go 插件
- 代价：桥要在 `plugin_init` 就知道自己的插件名（现在名字是 `define_plugin!` 编译期烤进去的，
  而 config 要到 `plugin_start` 才到）→ 得改成从**自身文件名**或描述文件取名字；
  宿主侧产物的分发与内置插件机制也要跟着变
- 判断：B 的产品体验更好，但要动宿主的名字解析路径。**建议先做 A**，把「Go 走进程式」
  这条路走通并跑顺，再看要不要升到 B。

## 5. 收益 / 代价（诚实版）

**收益**

- 卸载与热重载恢复可用：`plugin_stop` 杀掉 Go 子进程、cdylib 正常 `dlclose`
- 崩溃隔离：Go panic 只死插件进程，不再带崩宿主
- 工具链门槛下降：不需要 cgo 与 C 编译器，`go build` 即可（Windows 上不再要 mingw）
- 进程内不再有第二套 Go runtime 抢信号处理器
- Windows 文件占用问题消失（被加载的是可卸载的小 cdylib）
- 与 Python / JS 同源，ABI 语义不可能漂移

**代价**

- **分发形态变重**：脚本是纯文本跨平台，Go 插件要按平台编**两件**（cdylib + 可执行文件）
- 每个 Go 插件一个进程（Go 二进制常驻内存比 python/node 小得多，这条其实比脚本插件轻）
- 一行一个 JSON 的跨进程往返，比进程内 cgo 调用慢；`/ping` 这类命令无感，高频事件要留意
- 多一份 Go SDK（~300 行 + 测试）要写、要维护
- Go 插件不再能「改一行源码就生效」，必须重新 `go build`

## 6. 兼容与迁移

- 老 c-shared Go 插件**继续能跑**（用户已选「暂时不动」）：FuyuBot 的 `looks_like_go_binary`
  识别与 `forget` 兜底保留，只是仍然不可卸载。**等新路径跑顺再决定要不要移除这条兜底**
  （移除的话别人拿 c-shared 插件来装会直接崩宿主，风险不小，建议留着并只在文档里标注「不推荐」）
- `fuyubot-sdk check <产物>`：对 Go 仍可静态读 cdylib 的导出符号（都是桥的符号），
  但**不检查**那个可执行文件是否与宿主 ABI 匹配——协议错配只能在运行时暴露，这点与脚本插件现状一致

## 7. 验收方式（真跑，不是纸面）

```powershell
# 插件工程里
go build -o ping.exe .                 # Windows；Linux/macOS 去掉 .exe
cargo build --release                  # 出 ping.dll（三行宏那个小 cdylib）

# 宿主侧
fuyubot run --replay ping.json --plugin-dir <放 ping.dll + ping.exe 的目录>
```

期望输出与现在的 `examples/go-ping/README.md` 一致（`start()` → `回了 pong` →
`sendMessage` → `stop()`），**外加两条新断言**：

1. `stop()` 之后 Go 子进程真的没了（进程列表 / 管道读到 EOF）
2. cdylib 被真正卸载：这条可以用「stop 之后能删掉/覆盖 `ping.dll`」来验证——
   现在的 c-shared 形态在 Windows 上做不到，正好是本次要消掉的痛点

## 8. 待定问题（要你拍板）

1. **Go SDK 怎么交付**：拷进工程（与 python/js 一致，`include_str!` 逐字拷贝，零网络依赖、
   不会版本漂移）还是发成一个 Go module（`go get github.com/CatMoeCircle/...`，能升级但要维护
   版本矩阵）。**倾向拷进工程**，理由是现有两门语言都这么做，且协议与宿主强耦合、本就该同版本。
2. **Unix 上无扩展名的可执行文件**：`find_script` 要不要为它加「无扩展名 + 可执行位」的匹配？
   加了更贴近 Go 的习惯产物名（`ping` 而不是 `ping.exe`），但要动 FuyuBot 的识别逻辑。
   不加的话就统一要求带 `.exe` 后缀（`fuyubot plugin install` 按扩展名识别，现在就是这样）。
3. **要不要一并给 Go 加 `--lang go` 模板**：这次可以先只做「桥 + SDK + 示例」（方案 A 的 1/2/3/4/6/7），
   模板（第 5 项）等跑通再补。

## 9. 工作量粗估

- 方案 A（不含模板）：桥 ~60 行改动 + Go SDK ~300 行 + 示例重写 + 本仓 ~15 行，半天到一天
- 加上 `--lang go` 模板与 CLI：再加半天
- 方案 B：另计，且要动宿主的名字解析
