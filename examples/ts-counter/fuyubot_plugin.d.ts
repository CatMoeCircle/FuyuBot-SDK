/**
 * `fuyubot_plugin` 的类型声明（TS/JS 插件作者对着它写代码）。
 *
 * 每个函数对应宿主 `HostApiVTable` 上的哪一项写在各自的注释里——
 * **这就是「JS SDK 声明的 ABI 契约」**，宿主侧的对应实现见
 * `crates/fuyubot-loader/src/runtime.rs`。
 *
 * 对应的实现是同目录的 `fuyubot_plugin.js`（CommonJS）；TS 里这样接：
 * `import * as host from "./fuyubot_plugin";`
 *
 * `on` / `invoke` 用同目录的 `td_api.d.ts`（由 `fuyubot-sdk td-types` 从
 * `td_api.tl` 生成）做精确类型：写 `on("updateNewMessage", …)` 时回调参数
 * 是 `updateNewMessage`，写 `invoke("sendMessage", …)` 时参数是 `sendMessageParams`。
 */
/// <reference path="./td_api.d.ts" />
import type { TdUpdateMap, TdFunctionMap } from "./td_api";

/** 负载/回调里能看到的 JSON 值。 */
export type Json =
  | null
  | boolean
  | number
  | string
  | Json[]
  | { [key: string]: Json };

/** 事件回调：收到一条事件的负载。 */
export type Handler = (payload: Json) => void;

/**
 * **写** TDLib 对象时的宽松类型：判别字段 `@type` 与 `_` **二选一**，递归到任意深度。
 *
 * `td_api.d.ts` 里每个 interface 的 `"@type"` 是**必填**判别式——读负载时靠它把联合类型
 * 窄化（`if (content["@type"] === "messageText")`），所以那边不能松。但**手写**请求时
 * 每层都要抄一遍 `"@type"`（还带引号），嵌套越深越难读：
 *
 * ```ts
 * host.invoke("sendMessage", {
 *   chat_id: chatId,
 *   input_message_content: {
 *     _: "inputMessageVideo",                              // = "@type"
 *     video: { _: "inputFileLocal", path: "./video.mp4" },  // 递归到里面
 *     duration: 120,
 *   },
 * });
 * ```
 *
 * 桥在发往宿主之前把 `_` 递归换成 `@type`（同时写了两个时以 `@type` 为准）。
 * 代价是判别式在这一侧**可有可无**——漏了就是 TDLib 报错，由传输层而不是编译器发现。
 *
 * ⚠️ 只用在「作者写出去」的位置（`invoke` 的 params）。回调里读到的负载仍是
 * `td_api.d.ts` 里那个带必填 `@type` 的判别联合。
 */
export type TdInput<T> = T extends readonly (infer U)[]
  ? readonly TdInput<U>[]
  : T extends object
    ? { [K in keyof T as TdInputKey<K>]?: K extends "@type" ? T[K] : TdInput<T[K]> }
    : T;

/** `@type` 这条键额外接受 `_` 这个别名，其余照原样。 */
export type TdInputKey<K> = K extends "@type" ? "@type" | "_" : K;

/** 宿主 `HostApiVTable::log`。`level`：error / warn / info / debug / trace。 */
export declare function log(message: string, level?: string): void;

/**
 * 宿主 `HostApiVTable::on`。
 *
 * `filter` 里的 `event_type` 键选择通道（`"update"` / `"response"`），
 * 其余键做负载字段匹配。
 *
 * 事件名是 `TdUpdateMap` 的键时，回调参数推断为精确的负载类型。
 */
export declare function on<E extends string>(
  event: E,
  handler: E extends keyof TdUpdateMap
    ? (payload: TdUpdateMap[E]) => void
    : Handler,
  filter?: { [key: string]: Json },
): Handler;

/** 宿主 `HostApiVTable::off` 的本地一半：摘掉分发表里的一项。 */
export declare function off(event: string, handler: Handler): void;

/**
 * 宿主 `HostApiVTable::td_invoke_simplified`。
 *
 * `method` 是 TDLib 的方法名（如 `"sendMessage"`），`params` 是除 `@type`
 * 之外的参数。返回脚本侧的 tag；给了 `onResponse` 就按 `request_id` 配对回调。
 *
 * 方法名是 `TdFunctionMap` 的键时，`params` 与 `onResponse` 推断为精确类型
 * （参数字段都可省略——TDLib 会给缺省项填默认值，但传了就是精确类型）。
 *
 * `params` 里嵌的 TDLib 对象可以写 `_` 当判别字段，见 {@link TdInput}。
 */
export declare function invoke<M extends string>(
  method: M,
  params?: M extends keyof TdFunctionMap
    ? TdInput<TdFunctionMap[M]["params"]>
    : { [key: string]: Json },
  onResponse?: M extends keyof TdFunctionMap
    ? (response: TdFunctionMap[M]["result"]) => void
    : Handler,
): number;

/**
 * `invoke` 的 `await` 版：发请求并等它自己的响应。
 *
 * 插件作者首选这个。TDLib 的业务错误（`@type: "error"`）也会 resolve，
 * 自己判 `response["@type"]`；宿主拒绝请求时才 reject。
 */
export declare function invokeAsync<M extends string>(
  method: M,
  params?: M extends keyof TdFunctionMap
    ? TdInput<TdFunctionMap[M]["params"]>
    : { [key: string]: Json },
): Promise<M extends keyof TdFunctionMap ? TdFunctionMap[M]["result"] : Json>;

/** 宿主 `HostApiVTable::emit`。 */
export declare function emit(event: string, payload?: Json): void;

/** 宿主 `HostApiVTable::register_tool`（只登记元数据，没有处理器指针）。 */
export declare function registerTool(
  name: string,
  description?: string,
  inputSchema?: { [key: string]: Json },
): void;

/** 同步调用一个工具；`onResponse` 收到工具输出 JSON（失败时是 `{"error": …}`）。 */
export declare function callTool(
  name: string,
  input?: { [key: string]: Json },
  onResponse?: Handler,
): number;

/** 宿主 `HostApiVTable::list_tools`：同步列出全部工具元数据。 */
export declare function listTools(onTools?: (tools: Json[]) => void): number;

/**
 * 宿主 `HostApiVTable::td_execute`：**同步**执行不碰网络的 TDLib 方法。
 * 发消息/编辑消息不能走它。
 *
 * `request` 是完整的请求对象，它的判别字段同样可以写 `_`。
 */
export declare function tdExecute(
  request: { [key: string]: Json },
  onResult?: (result: Json) => void,
): number;

/**
 * 宿主 `HostApiVTable::use_middleware`：挂中间件。`stage` 是 0..5 的整数。
 * `handler(stage, payload)` 在更新经过该阶段时被调用；中间件无法单独注销。
 */
export declare function useMiddleware(
  stage: number,
  handler: (stage: number, payload: Json) => void,
): (stage: number, payload: Json) => void;

/** 宿主 `HostApiVTable::wait_for_update`：阻塞等一条匹配的更新。 */
export declare function waitForUpdate(
  filter: { [key: string]: Json },
  timeoutMs: number,
  onMatch?: (payload: Json) => void,
): number;

/**
 * `waitForUpdate` 的 `await` 版：等一条匹配的更新。
 *
 * `null` = 超时/取消。底层仍由宿主在独立线程上阻塞等待，脚本事件循环照常跑。
 */
export declare function waitAsync(
  filter: { [key: string]: Json },
  timeoutMs: number,
): Promise<Json | null>;

/** 宿主 `HostApiVTable::get_service`：读服务的元数据（名字 + 方法列表）。 */
export declare function getService(
  name: string,
  onService?: (service: { name: string; methods: string[] } | null) => void,
): number;

/** 同步调用宿主服务的一个方法。 */
export declare function callService(
  name: string,
  method: string,
  input?: { [key: string]: Json },
  onResult?: (result: Json) => void,
): number;

/** 宿主传给插件的配置（`plugin_start` 的 config JSON，含 `plugin_config` 键）。 */
export declare function config(): { [key: string]: Json };

/** **自己的**配置段（`[plugin.<插件名>]`）。同步、`run()` 之前也能读；没配就是空对象。 */
export declare function pluginConfig(): { [key: string]: Json };

/**
 * 读任意插件段（异步，回调在 `run()` 之后）。`section` 空串读顶层，`key` 空串读整段。
 * 段 / 键不存在时回调收到 `null`。
 */
export declare function getConfig(
  section: string,
  key?: string,
  onConfig?: (config: Json) => void,
): number;

/**
 * 写**自己的**配置段（成功后回写 `fuyubot.toml`）。`key` 空串覆盖整段。
 * 失败只会打日志，不会调 `onDone`。
 */
export declare function setConfig(
  key: string,
  value: Json,
  onDone?: (ok: boolean) => void,
): number;

/** 开始读宿主的行并分发。插件的最后一行永远是这个。 */
export declare function run(): void;
