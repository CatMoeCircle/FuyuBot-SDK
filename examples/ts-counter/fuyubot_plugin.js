"use strict";
/**
 * fuyubot 脚本插件桥的 JS/TS 侧。
 *
 * 这不是业务逻辑，只是**契约映射**：把宿主 `HostApiVTable` 上的动作包成普通函数，
 * 底下走「一行一个 JSON」的协议（协议细节见 `fuyubot-sdk-script` 的 crate 文档）。
 *
 * 用法：把这个文件和你的 `plugin.js` 放在同一个目录，然后
 *
 *     const host = require("./fuyubot_plugin.js");
 *
 *     host.on("updateNewMessage", (payload) => host.log("收到一条更新"));
 *
 *     host.run();
 *
 * 类型声明见同目录的 `fuyubot_plugin.d.ts`。
 * Node 与 Bun 都跑得起来（宿主会依次尝试 `node`、`bun`）。
 */

const readline = require("readline");

const handlers = new Map(); // 宿主订阅号 -> [回调]
const middlewareHandlers = new Map(); // 宿主中间件号 -> [回调]
const pendingOns = new Map(); // tag -> 回调（等宿主把订阅号回执回来）
const pendingInvokes = new Map(); // tag -> 回调（等宿主把请求号回执回来）
const pendingMiddlewares = new Map(); // tag -> 回调（等宿主把中间件号回执回来）
const pendingReplies = new Map(); // tag -> 回调（等宿主的同步回执：listTools/execute/wait/getService/callService）
const responseHandlers = new Map(); // request_id -> 回调
let responseChannelOpen = false;
let nextTag = 1;

/**
 * 从 `process.argv[2]` 读宿主给的配置。**模块加载时就读**：这样 `pluginConfig()`
 * 在 `run()` 之前（模块顶层）也能用——启动期读自己的配置是最常见的需求。
 */
function loadConfig() {
  if (process.argv[2] && process.argv[2].trim()) {
    try {
      const value = JSON.parse(process.argv[2]);
      if (value && typeof value === "object" && !Array.isArray(value)) {
        return value;
      }
    } catch (error) {
      // 非法 JSON：按空对象处理，别把插件搞挂。
    }
  }
  return {};
}

const config = loadConfig();

function send(message) {
  process.stdout.write(JSON.stringify(message) + "\n");
}

function newTag() {
  return nextTag++;
}

/** 往宿主日志里写一行。`level`：error / warn / info / debug / trace。 */
function log(message, level = "info") {
  send({ op: "log", level, message: String(message) });
}

/**
 * 订阅事件。
 *
 * `filter` 里的 `event_type` 键是**选通道**用的（`"update"` / `"response"`），
 * 其余键做负载匹配。
 *
 * **同一个事件可以订阅多次**，各带各的过滤器：宿主按订阅号把事件分回来，
 * 这里也按订阅号分发，所以不会出现「过滤器没命中的回调也被调到」。
 */
function on(event, handler, filter) {
  const tag = newTag();
  pendingOns.set(tag, handler);
  const message = { op: "on", tag, event };
  if (filter !== undefined && filter !== null) {
    message.filter = filter;
  }
  send(message);
  return handler;
}

/** 退订（只影响本进程的分发表；宿主的订阅随插件停止一起撤掉）。 */
function off(event, handler) {
  for (const list of handlers.values()) {
    const index = list.indexOf(handler);
    if (index >= 0) {
      list.splice(index, 1);
    }
  }
}

/**
 * 发一条 TDLib 请求。
 *
 * 给了 `onResponse` 就会在这条请求的响应回来时调它——响应按 `request_id`
 * 配对，脚本不用自己记请求号。
 */
function invoke(method, params, onResponse) {
  ensureResponseChannel();
  const tag = newTag();
  if (typeof onResponse === "function") {
    pendingInvokes.set(tag, onResponse);
  }
  send({ op: "invoke", tag, method, params: params || {} });
  return tag;
}

/** 发一条自定义事件（其他插件订阅得到）。 */
function emit(event, payload) {
  send({ op: "emit", event, payload });
}

/** 登记一个工具的元数据。 */
function registerTool(name, description = "", inputSchema) {
  const descriptor = { name, description };
  if (inputSchema !== undefined && inputSchema !== null) {
    descriptor.input_schema = inputSchema;
  }
  send({ op: "tool", descriptor });
}

/**
 * 调用一个工具（异步）。响应按 `request_id` 配对，`onResponse` 收到的是
 * 工具执行的最终结果（`ToolResponse` 的 JSON）。
 */
function callTool(name, input, onResponse) {
  const tag = newTag();
  if (typeof onResponse === "function") {
    pendingInvokes.set(tag, onResponse);
  }
  send({ op: "callTool", tag, name, input: input || {} });
  return tag;
}

/**
 * 列出全部已登记工具的元数据（同步回执）。`onTools` 收到工具数组。
 * 不传回调时返回 tag，结果会静默丢弃。
 */
function listTools(onTools) {
  const tag = newTag();
  if (typeof onTools === "function") {
    pendingReplies.set(tag, (msg) => onTools(msg.tools));
  }
  send({ op: "listTools", tag });
  return tag;
}

/**
 * **同步**执行一个不碰网络的 TDLib 方法（如 `parseTextEntities`、
 * `getTextEntities`）。`onResult` 收到结果 JSON；发消息/编辑消息**不能**走它。
 */
function tdExecute(request, onResult) {
  const tag = newTag();
  if (typeof onResult === "function") {
    pendingReplies.set(tag, (msg) => onResult(msg.result));
  }
  send({ op: "execute", tag, request });
  return tag;
}

/**
 * 挂一个中间件。`stage` 是 0..5 的整数（pre_update / pre_route / in_plugin /
 * post_response / pre_tool_call / post_tool_call）。`handler(stage, payload)` 在
 * 更新经过该阶段时被调用。中间件无法单独注销（随插件停止一起撤掉）。
 */
function useMiddleware(stage, handler) {
  const tag = newTag();
  pendingMiddlewares.set(tag, handler);
  send({ op: "middleware", tag, stage });
  return handler;
}

/**
 * 阻塞等一条匹配 `filter` 的更新（在宿主侧的独立线程里等，不占脚本进程）。
 * `onMatch` 收到匹配的负载；超时或被取消时收到 `null`。`timeoutMs` 为 0 表示
 * 只检查当前状态。
 */
function waitForUpdate(filter, timeoutMs, onMatch) {
  const tag = newTag();
  if (typeof onMatch === "function") {
    pendingReplies.set(tag, (msg) => onMatch(msg.payload));
  }
  send({ op: "wait", tag, filter, timeout_ms: timeoutMs || 0 });
  return tag;
}

/**
 * 读取一个宿主服务的元数据（名字 + 方法名列表）。`onService` 收到
 * `{ name, methods: [...] }`；服务不存在时收到 `null`。
 */
function getService(name, onService) {
  const tag = newTag();
  if (typeof onService === "function") {
    pendingReplies.set(tag, (msg) => onService(msg.service));
  }
  send({ op: "getService", tag, name });
  return tag;
}

/**
 * 同步调用宿主服务的一个方法。`onResult` 收到方法返回的 JSON；
 * 服务或方法不存在时收到 `null`。
 */
function callService(name, method, input, onResult) {
  const tag = newTag();
  if (typeof onResult === "function") {
    pendingReplies.set(tag, (msg) => onResult(msg.result));
  }
  send({ op: "callService", tag, name, method, input: input || {} });
  return tag;
}

/**
 * 宿主传给插件的配置（`plugin_start` 的 config JSON），没有就是空对象。
 *
 * 里面额外带一个 `pluginConfig` 键（= 你自己的 `[plugin.<插件名>]` 段）——
 * 通常用 {@link pluginConfig} 取它，别自己翻这个对象。
 */
function runtimeConfig() {
  return config;
}

/**
 * **自己的**配置段（`fuyubot.toml` 里的 `[plugin.<你的插件名>]`）。
 *
 * 同步、随时可读——**包括 `run()` 之前**（宿主在启动脚本时就把这段塞进了 `argv[2]`）。
 * 没有配这一段就是空对象。
 */
function pluginConfig() {
  const value = config.plugin_config;
  return value && typeof value === "object" && !Array.isArray(value) ? value : {};
}

/**
 * 读**任意**插件段（自己的或别人的）——异步，回调要等 `run()` 之后才会来。
 *
 * `section` 是插件名（如 `"my.weather"`），空串读顶层（`[bot]` / `[telegram]` 等）；
 * `key` 空串读整段、否则按点路径取（`"nested.k"`）。
 * `onConfig` 收到值；段 / 键不存在时收到 `null`。
 *
 * 只想读**自己**那段就用 {@link pluginConfig}——它是同步的，不需要等回执。
 */
function getConfig(section, key, onConfig) {
  const tag = newTag();
  if (typeof onConfig === "function") {
    pendingReplies.set(tag, (msg) => onConfig(msg.config === undefined ? null : msg.config));
  }
  send({ op: "getConfig", tag, section, key: key === undefined ? "" : key });
  return tag;
}

/**
 * 写**自己的**配置段（`[plugin.<你的插件名>]`）。
 *
 * `key` 空串 = 覆盖整段，否则按点路径设键。成功后宿主会**回写 `fuyubot.toml`**
 * （注意是全量重写，手写注释会丢）。`onDone` 收到 `true`。
 *
 * 失败（越权 / 值不是合法 JSON / 写盘失败）时宿主发 `error`，只会打日志、
 * **不会**调 `onDone`——所以别把「没收到回调」当成成功。
 */
function setConfig(key, value, onDone) {
  const tag = newTag();
  if (typeof onDone === "function") {
    pendingReplies.set(tag, () => onDone(true));
  }
  send({ op: "setConfig", tag, key: key === undefined ? "" : key, value });
  return tag;
}

function ensureResponseChannel() {
  // 响应按 request_id 配对，得先收得到它。
  if (!responseChannelOpen) {
    responseChannelOpen = true;
    on("response", deliverResponse);
  }
}

function dispatch(message) {
  if (message.op === "event") {
    // 按订阅号分发：同一个事件可能注册了带不同过滤器的多条订阅。
    const list = handlers.get(message.handle) || [];
    for (const handler of list.slice()) {
      handler(message.payload);
    }
  } else if (message.op === "middleware" && message.stage !== undefined) {
    // 中间件**事件**：带 stage（注册回执带 tag、不带 stage）。
    const list = middlewareHandlers.get(message.handle) || [];
    for (const handler of list.slice()) {
      handler(message.stage, message.payload);
    }
  } else if (message.op === "on") {
    const handler = pendingOns.get(message.tag);
    if (handler) {
      pendingOns.delete(message.tag);
      if (!handlers.has(message.handle)) {
        handlers.set(message.handle, []);
      }
      handlers.get(message.handle).push(handler);
    }
  } else if (message.op === "middleware") {
    // 中间件**注册回执**：把回调挂到回执里的那个 handle 上。
    const handler = pendingMiddlewares.get(message.tag);
    if (handler) {
      pendingMiddlewares.delete(message.tag);
      if (!middlewareHandlers.has(message.handle)) {
        middlewareHandlers.set(message.handle, []);
      }
      middlewareHandlers.get(message.handle).push(handler);
    }
  } else if (message.op === "invoked") {
    const handler = pendingInvokes.get(message.tag);
    if (handler) {
      pendingInvokes.delete(message.tag);
      responseHandlers.set(message.request_id, handler);
    }
  } else if (message.op === "error") {
    log("宿主报错：" + String(message.message), "error");
  } else if (message.tag !== undefined) {
    // 其余带 tag 的回执（listTools / execute / wait / getService / callService）。
    const handler = pendingReplies.get(message.tag);
    if (handler) {
      pendingReplies.delete(message.tag);
      handler(message);
    }
  }
}

function deliverResponse(payload) {
  if (payload === null || typeof payload !== "object") {
    return;
  }
  const handler = responseHandlers.get(payload.request_id);
  if (handler) {
    responseHandlers.delete(payload.request_id);
    handler(payload);
  }
}

/** 读宿主发来的行、分发给回调，直到宿主关掉管道。插件的最后一行就是它。
 *
 * 这一行必须在**所有 `on(...)` 之后**：宿主会等收到 `{"op":"ready"}` 才认为
 * 插件加载完成，而 `plugin_start` 也要等它——脚本早退会被判成加载失败。
 */
function run() {
  send({ op: "ready" });
  const lines = readline.createInterface({ input: process.stdin });
  lines.on("line", (line) => {
    const text = line.trim();
    if (!text) {
      return;
    }
    try {
      dispatch(JSON.parse(text));
    } catch (error) {
      log("收到一行不是 JSON 的内容：" + text, "error");
    }
  });
}

/** `node fuyubot_plugin.js` 就能跑。
 *
 * 验的是这一层最容易写错的两件事：**事件按订阅号分发**（不是按事件名，
 * 否则带过滤器的第二条订阅会在不该响的时候响），以及**没见过的 handle/tag
 * 不能把脚本搞挂**。
 */
function selfTest() {
  const seen = [];
  handlers.clear();
  handlers.set(1, [(payload) => seen.push(["a", payload])]);
  handlers.set(2, [(payload) => seen.push(["b", payload])]);
  dispatch({ op: "event", handle: 1, payload: { n: 1 } });
  dispatch({ op: "event", handle: 2, payload: { n: 2 } });
  // 没订阅过的 handle、没有 tag 的消息：静悄悄忽略，不许炸。
  dispatch({ op: "event", handle: 99, payload: { n: 3 } });
  dispatch({ op: "on", handle: 5 });
  assert(JSON.stringify(seen) === JSON.stringify([["a", { n: 1 }], ["b", { n: 2 }]]), seen);

  // 订阅回执要把回调挂到**回执里的那个 handle** 上。
  const tag = newTag();
  pendingOns.set(tag, (payload) => seen.push(["c", payload]));
  dispatch({ op: "on", tag, handle: 42 });
  dispatch({ op: "event", handle: 42, payload: { n: 4 } });
  assert(JSON.stringify(seen[seen.length - 1]) === JSON.stringify(["c", { n: 4 }]), seen);

  // 响应按 request_id 配对，且只喂一次。
  responseHandlers.set(7, (payload) => seen.push(["r", payload]));
  deliverResponse({ request_id: 7, ok: true });
  deliverResponse({ request_id: 7, ok: true });
  const responses = seen.filter((item) => item[0] === "r");
  assert(responses.length === 1, responses);

  // 同步回执（pendingReplies）按 tag 配对，回调收到整个回执对象。
  const rt = newTag();
  pendingReplies.set(rt, (msg) => seen.push(["tools", msg.tools]));
  dispatch({ op: "listTools", tag: rt, tools: [{ name: "demo" }] });
  assert(
    JSON.stringify(seen[seen.length - 1]) === JSON.stringify(["tools", [{ name: "demo" }]]),
    seen
  );

  // 中间件：注册回执（带 tag）挂到 handle，事件（带 stage）按 handle 分发。
  const mt = newTag();
  pendingMiddlewares.set(mt, (stage, payload) => seen.push(["mw", stage, payload]));
  dispatch({ op: "middleware", tag: mt, handle: 9 });
  dispatch({ op: "middleware", handle: 9, stage: 0, payload: { t: "x" } });
  assert(
    JSON.stringify(seen[seen.length - 1]) === JSON.stringify(["mw", 0, { t: "x" }]),
    seen
  );

  // 自己的配置段：同步可读（模块加载时就解析好了 argv[2]），缺省给空对象。
  config.plugin_config = { greeting: "hi" };
  assert(JSON.stringify(pluginConfig()) === JSON.stringify({ greeting: "hi" }), pluginConfig());
  config.plugin_config = null;
  assert(JSON.stringify(pluginConfig()) === "{}", pluginConfig());
  config.plugin_config = [1, 2];
  assert(JSON.stringify(pluginConfig()) === "{}", pluginConfig());
  delete config.plugin_config;

  // 读别人的段 / 写自己的段：都是 tag 回执。
  const gt = newTag();
  pendingReplies.set(gt, (msg) => seen.push(["cfg", msg.config]));
  dispatch({ op: "getConfig", tag: gt, config: { a: 1 } });
  assert(JSON.stringify(seen[seen.length - 1]) === JSON.stringify(["cfg", { a: 1 }]), seen);
  const st = newTag();
  pendingReplies.set(st, (msg) => seen.push(["set", msg.op]));
  dispatch({ op: "setConfig", tag: st });
  assert(JSON.stringify(seen[seen.length - 1]) === JSON.stringify(["set", "setConfig"]), seen);

  console.log("fuyubot_plugin: self-test ok");
}

function assert(condition, detail) {
  if (!condition) {
    console.error("fuyubot_plugin: self-test FAILED", detail);
    process.exit(1);
  }
}

module.exports = {
  log,
  on,
  off,
  invoke,
  emit,
  registerTool,
  callTool,
  listTools,
  tdExecute,
  useMiddleware,
  waitForUpdate,
  getService,
  callService,
  config: runtimeConfig,
  pluginConfig,
  getConfig,
  setConfig,
  run,
};

if (require.main === module) {
  selfTest();
}
