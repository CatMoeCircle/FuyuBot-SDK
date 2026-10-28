# 插件 ABI 契约（ABI v4）

> **权威源只有一处**：`fuyubot-abi/src/lib.rs`（运行时仓库）。C 头文件
> `fuyubot_plugin.h` 是它的**产物**（cbindgen + `build.rs` 里三段手写契约），
> 本文件是叙述版。三者冲突时以头文件为准，并回改本文件。

结论：**一个插件 = 一个 cdylib**（`.dll` / `.so` / `.dylib`），宿主 `dlopen` 它、
按名字取符号、给它一张 `HostApiVTable`。没有注册表、没有 IDL、没有运行时代理。

---

## 1. 七个导出符号

| 符号 | C 签名 | 引进 | 必需 | 谁调 |
|---|---|---|---|---|
| `plugin_init` | `PluginHandle plugin_init(const HostApiVTable *host)` | v3 | **是** | 宿主，装载时一次 |
| `plugin_start` | `int32_t plugin_start(PluginHandle, const char *config_json)` | v3 | 否 | 宿主，激活时 |
| `plugin_stop` | `void plugin_stop(PluginHandle)` | v3 | 否 | 宿主，停用 / 卸载时 |
| `plugin_free_string` | `void plugin_free_string(char *ptr)` | v3 | 见下 | 宿主，归还插件给的字符串 |
| `plugin_manifest` | `const char *plugin_manifest(void)` | v4 | 否 | 宿主，**`plugin_init` 之前** |
| `fuyubot_abi_version` | `uint32_t fuyubot_abi_version(void)` | v4 | 否 | `fuyubot-sdk check` 静态读 |
| `plugin_free_handle` | `void plugin_free_handle(PluginHandle)` | v4 | 否 | 宿主，ABI 版本不匹配时（不导出就在那一刻漏掉插件侧状态） |

几条容易踩的：

- `plugin_init` 返回 `PluginHandle` **按值**，失败用 `abi_version = 0` 表示（没有 NULL
  可用）；`context` 必须非空，它是宿主透传给 `start` / `stop` 的那块状态。
- `plugin_free_string` 在 v3 里"必须有但可以是空实现"。**v4 起只要插件往外送过字符串
  （含 `plugin_manifest`），它就必须真的能释放**——空实现 = 每次加载漏一段。
- `plugin_free_handle` 不是给正常路径用的：它是"ABI 版本对不上、不能调 `plugin_stop`"
  时唯一安全的收尾方式。**签名永久冻结**——只接 `PluginHandle`、不碰 VTable，宿主才敢在
  一个"版本不认识的插件"上调用它；改签名等于让老宿主在未来的插件上乱调。
- 七个符号之外的一切都是实现细节，宿主不看也不会调。

## 2. 内存所有权（五条硬规矩）

1. **谁分配谁释放。** 插件经 `plugin_init` 交回的 `name`、以及插件自己分配的任何
   字符串，都归插件；宿主绝不会 free 它们。要让宿主接管，只能走
   `plugin_free_string` 那套约定。
2. **宿主返回的字符串是线程局部缓冲。** `list_tools` / `list_plugins` / `get_config`
   与 `WaitHandle` 的 payload 都指向宿主一块复用缓冲，**只在下一次调宿主函数之前有效**。
   要留着就当场拷走，并且**不要试图释放**——VTable 里没有 host 侧 free，这是设计。
3. **`get_service` 返回的不是缓冲**，而是提供方注册时给的那个 `ServiceVTable` 指针：
   服务注销之前一直有效，可以留着（但内存归提供方，不要写）。
4. **`ServiceMethodFn` 的返回值由提供方持有。** 调用方读完即弃。
5. **`plugin_manifest` 的返回值也归插件所有。** 它每次调用都新分配一条 NUL 结尾的
   JSON，宿主读完用 `plugin_free_string` 归还；返回 NULL 表示"我没有元数据"
   （不导出这个符号也行，那就是 v3 插件）。**不能返回静态字符串**——宿主会无条件 free。

## 3. 三个版本别混

| 名字 | 在哪 | 谁的 | 什么时候能看 |
|---|---|---|---|
| `ABI_VERSION`（= 4） | `HostApiVTable::version`（首字段） | 宿主编译时常量 | 插件在 `init` 里先比一次 |
| `PluginHandle::abi_version` | `plugin_init` 的返回值 | 插件编译时的 ABI 版本 | 装载时（运行期） |
| `plugin_manifest.version` | manifest JSON 的 `version` 字段 | 插件 **crate** 的版本 | 装载前（静态字符串） |

- 前两个是**ABI 世代**，用来判"这个插件能不能被这个宿主加载"；不匹配时宿主拒绝装载，
  并且不能调 `plugin_stop`（那是对不认识的 ABI 下手），只能调 `plugin_free_handle`。
- 第三个是**业务版本**，只用于诊断与兼容策略，与装载决策无关。
- `fuyubot-sdk check` 能静态看到"有没有 `fuyubot_abi_version` 这个符号"，
  但**看不到它的值**——那个值要 dlopen 才知道（这也是为什么 check 只报符号存在性）。

## 4. `plugin_manifest` 与依赖声明

形状（一行 JSON）：

```json
{"name":"fuyubot.demo","version":"0.1.0","inject":["storage.mongo"],"soft_inject":["media.ffmpeg"]}
```

| 字段 | 含义 |
|---|---|
| `name` | 插件名（点分）。与 `plugin_init` 给的不一致时宿主只告警，**以 `plugin_init` 为准** |
| `version` | 插件 crate 的版本（`CARGO_PKG_VERSION`） |
| `inject` | **硬依赖**：这些服务缺任何一个，插件不会被激活（停在 Pending） |
| `soft_inject` | **软依赖**：缺了照常加载；有了就优先排在它后面激活 |

**为什么必须在 `init` 之前读**：`inject` 决定"要不要激活"，而激活才轮到 `start`。
所以它不能依赖任何初始化过的状态——这就是脚本插件写不了依赖的原因：那时解释器
进程还没起，`.py` / `.js` 里的声明没有任何人能读到。

转义规则（插件侧生成时就要遵守）：

- `"` → `\"`，`\` → `\\`，换行 / 回车 / 制表 → `\n` / `\r` / `\t`；
- 其余 `< 0x20` 的控制字符（含 NUL）→ `\u00XX`；
- 非 ASCII（中文等）**原样写 UTF-8**，不要转成 `\uXXXX`。

**依赖的取值优先级**（宿主侧 `fuyubot_loader::loader::declared`）：

1. manifest 里那一侧**非空** → 用它；
2. 空数组 / 没有这个符号 / JSON 解析失败 → 回退配置段 `[plugin.<名>].inject` / `.soft_inject`。

⚠️ **这是覆盖，不是合并**，而且**逐侧判定**：只声明了 `inject` 时，`soft_inject` 那半边
仍然读配置。所以"开始声明依赖"对插件作者是**行为变更**——一旦声明，`fuyubot.toml`
里对应的那份就静默失效了。

宿主加载链路：`dlopen` → `read_manifest`（读符号、拷字符串、`plugin_free_string` 归还、
`serde_json` 解析）→ `plugin_init` → 内核按依赖决定激活顺序 → `plugin_start`。

## 5. `HostApiVTable` 一览

`{ version: u32, size: u32, host_ctx, ...19 个函数指针 }`。每个函数**首参都是
`host_ctx`**，每个字段都是 `Option<fn>`（C 侧是可能为空的函数指针，**必须判空**）。

| 组 | 字段 | 说明 |
|---|---|---|
| 事件 | `on` / `off` / `emit` | 订阅按 `@type` 或 `update` 兜底通道；`emit` 在插件侧被桥成 **scoped**：只送"自己 + 全局订阅者" |
| 工具 | `register_tool` / `call_tool` / `call_tool_result` / `list_tools` | `call_tool` 是请求号异步路径，`call_tool_result` 同步返回结果（失败是 `{"error":…}`） |
| 服务 | `provide_service` / `get_service` | 按名字注册 / 解析；`get_service` 返回 `ServiceVTable *` |
| TDLib | `td_invoke` / `td_invoke_simplified` / `td_execute` | 前两个发请求（拿 `RequestId`），`td_execute` 是同步的纯函数 |
| 管线 | `use_middleware` | 插到六阶段管线的某个 stage |
| 等待 | `wait_for_update` / `cancel_wait` | **阻塞**当前线程；异步形态见 `ResponseRouter`（Rust 侧） |
| 诊断 | `log` / `get_config` / `set_config` / `list_plugins` | `get_config` 能读任意段，`set_config` 只能写自己那段（宿主按 `owner_names` 判定，插件伪造不了） |

逐字段的签名以 `fuyubot_plugin.h` 为准（`cbindgen` 产出，带完整注释）。

## 6. 为什么没有可变参数函数指针

`extern "C" fn(.., ...)` 在 Rust 1.99 已稳定，但本 ABI 一个都没用：`...` 会把参数
类型和个数校验全部丢掉，而 VTable 里每个函数的参数都是固定的，用 `...` 只会让 C 侧
传错参数从**编译错误**降级成**运行期 UB**。

## 7. 三种语言的最小写法

- **C / C++**：`fuyubot-sdk init ping --lang c` 会写出源码 + `fuyubot_plugin.h`。
  头文件是从 `fuyubot-abi` 的 Rust 定义生成的，**不要手改**；刷新用
  `fuyubot-sdk codegen --lang c --dir <工程>`。`FUYUBOT_EXPORT` 就是那个可见性宏。
- **Rust**：`define_plugin!(MyPlugin, "name")` 展开全部七个符号（含 manifest，
  依赖来自 `impl Plugin` 的 `inject()` / `soft_inject()`）。一个 crate 只能展开一次。
- **脚本（Python / JS / TS）**：`script_plugin!("name", python, "x.py")`，
  依赖写成 `inject = [...]` / `soft_inject = [...]`（`inject` 必须在前）。

## 8. 变更记录

- **v3 → v4**：`register_tool` 加 `handler` + `user_data`（插件终于能真提供工具）；
  `list_tools` / `list_plugins` / `call_tool_result` 补齐；新增 `plugin_manifest`、
  `fuyubot_abi_version`、`plugin_free_handle` 三个静态导出。
- **追加规则**：VTable 字段只增不改，新字段一律加在**尾部**，老插件靠 `size` 判断
  宿主的表够不够大；因此 v4 宿主能加载 v3 插件（会告警但可用）。反过来不行。
