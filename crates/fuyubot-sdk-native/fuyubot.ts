/**
 * fuyubot —— 脚本插件作者用的薄 SDK（TypeScript / bun 版）。
 *
 * 它只是 `fuyubot-sdk-native` 原生桥的一层 **bun:ffi 封装**：用
 * `Bun.dlopen` 加载 `fuyubot_sdk_native.dll`（或 .so / .dylib），把
 * `fuyubot_bridge_*` 那组 C 函数转成作者直接能用的调用。
 *
 * 作者看不到任何协议细节——没有 `"op"`、没有 `tag`/`handle`、没有手拼 JSON：
 *
 * ```ts
 * import { on, invoke, log, run } from "./fuyubot";
 *
 * on("updateNewMessage", (update) => log(`新消息 ${update.message.id}`));
 * run(); // 最后一行，永远如此
 * ```
 *
 * 协议本身（「一行一个 JSON」、订阅号配对、ready 握手）在原生桥里完成，这里只做
 * 「把 JS 函数转成 C 函数指针」这一件事。回调能跨线程触发（S0 已验证 JSCallback
 * 在 Rust 的非主线程被调用不崩）。
 */

import { CString, dlopen, JSCallback, ptr } from "bun:ffi";

type Json = any;

// ---------------------------------------------------------------------------
// 加载原生桥
// ---------------------------------------------------------------------------

// 平台 → 库文件名。
const IMPL: Record<string, string> = {
  win32: "fuyubot_sdk_native.dll",
  cygwin: "fuyubot_sdk_native.dll",
  linux: "libfuyubot_sdk_native.so",
  darwin: "libfuyubot_sdk_native.dylib",
};

function findLib(): string {
  const env = process.env.FUYUBOT_NATIVE_LIB;
  if (env) return env;
  return IMPL[process.platform] ?? IMPL.win32;
}

// C 回调签名。payload 用 `ptr`（裸指针），回调里用 `new CString(p)` 读——
// bun 1.3.1 的回调 `cstring` 参数不会可靠地自动解码成 string（实测给的是 number），
// 用裸指针 + CString 手动读最稳。
const CALLBACK_SIG = { args: ["ptr", "ptr"], returns: "void" } as const;
const MIDDLEWARE_SIG = { args: ["ptr", "u32", "ptr"], returns: "void" } as const;

interface Bridge {
  symbols: {
    fuyubot_bridge_connect(): number;
    fuyubot_bridge_on(
      event: Buffer,
      filter: Buffer | null,
      callback: JSCallback,
      user_data: ptr | null,
    ): number;
    fuyubot_bridge_off(callback: JSCallback, user_data: ptr | null): void;
    fuyubot_bridge_invoke(
      method: Buffer,
      params: Buffer | null,
      on_response: JSCallback | null,
      user_data: ptr | null,
    ): number;
    fuyubot_bridge_emit(event: Buffer, payload: Buffer | null): void;
    fuyubot_bridge_log(level: number, message: Buffer): void;
    fuyubot_bridge_register_tool(descriptor: Buffer): void;
    fuyubot_bridge_use_middleware(
      stage: number,
      callback: JSCallback,
      user_data: ptr | null,
    ): number;
    fuyubot_bridge_run(): void;
  };
  close(): void;
}

let _lib: Bridge | null = null;

function lib(): Bridge {
  if (!_lib) {
    _lib = dlopen(findLib(), {
      fuyubot_bridge_connect: { args: [], returns: "i32" },
      fuyubot_bridge_on: {
        args: ["cstring", "cstring", "function", "ptr"],
        returns: "u64",
      },
      fuyubot_bridge_off: { args: ["function", "ptr"], returns: "void" },
      fuyubot_bridge_invoke: {
        args: ["cstring", "cstring", "function", "ptr"],
        returns: "u64",
      },
      fuyubot_bridge_emit: { args: ["cstring", "cstring"], returns: "void" },
      fuyubot_bridge_log: { args: ["u32", "cstring"], returns: "void" },
      fuyubot_bridge_register_tool: { args: ["cstring"], returns: "void" },
      fuyubot_bridge_use_middleware: {
        args: ["u32", "function", "ptr"],
        returns: "u64",
      },
      fuyubot_bridge_run: { args: [], returns: "void" },
    }) as unknown as Bridge;
  }
  return _lib;
}

// ---------------------------------------------------------------------------
// 回调生命周期：JSCallback 对象只要还可能在 C 侧被调用就得一直持有。
// 这里用一个全局数组把每个回调都留住，活到进程结束。
// ---------------------------------------------------------------------------

const _keepalive: JSCallback[] = [];

function _keep(cb: JSCallback): JSCallback {
  _keepalive.push(cb);
  return cb;
}

// ---------------------------------------------------------------------------
// 作者 API
// ---------------------------------------------------------------------------

// bun:ffi 的 `cstring` 参数不接受 JS string，必须给 Buffer（NUL 结尾）。
// 这里统一把字符串编码成 Buffer 再交给 C。
function cstr(s: string): Buffer {
  return Buffer.from(s + "\0", "utf8");
}

/**
 * 把作者写的 TDLib 字面量规整成原生桥（再往下是 TDLib）认的形状：**`_` → `@type`**。
 *
 * TDLib 要求每个对象带判别字段，写成了要引号的 `"@type"` 就啰嗦；这里允许写 `_`，
 * 效果与 `"@type"` 完全等价，且**递归到任意深度**（`vector<T>` 里那串对象也生效）。
 * 同时写了两个时以 `@type` 为准；返回**新对象**，不改动作者传进来的那份。
 *
 * 为什么必须在这一侧做：原生桥把 params 当作字符串原样转发，不会替作者补全判别字段。
 * 与 `fuyubot-sdk-script/js/fuyubot_plugin.js` 里那份是同一个约定。
 */
function canonicalize(value: Json, path: Set<Json> = new Set()): Json {
  if (Array.isArray(value)) {
    if (path.has(value)) return value;
    path.add(value);
    const items = value.map((item) => canonicalize(item, path));
    path.delete(value);
    return items;
  }
  if (value === null || typeof value !== "object") return value;
  // `path` 是当前这条递归链：同一对象在两处复用照样被规整，真环则提前返回不拖死这里。
  if (path.has(value)) return value;
  path.add(value);
  const out: Record<string, Json> = {};
  const tag = (value as Record<string, Json>)["@type"] ?? (value as Record<string, Json>)["_"];
  if (typeof tag === "string") out["@type"] = tag;
  for (const key of Object.keys(value as Record<string, Json>)) {
    if (key === "@type" || key === "_") continue;
    out[key] = canonicalize((value as Record<string, Json>)[key], path);
  }
  path.delete(value);
  return out;
}

/** 订阅一条事件。收到时 `handler(payload)`，`payload` 是解析好的对象。 */
export function on(
  event: string,
  handler: (payload: Json) => void,
  filter?: Record<string, Json>,
): void {
  const cb = _keep(
    new JSCallback(
      (_userData: ptr, payload: ptr) => handler(_decode(payload)),
      CALLBACK_SIG,
    ),
  );
  lib().symbols.fuyubot_bridge_on(
    cstr(event),
    filter ? cstr(JSON.stringify(filter)) : null,
    cb,
    null,
  );
}

/** 发一条 TDLib 请求。给 `onResponse` 就按请求配对回调。
 *
 * `params` 里嵌的 TDLib 对象的判别字段可以写成 `_`（递归替换成 `@type`），
 * 见 {@link canonicalize}。
 */
export function invoke(
  method: string,
  params?: Record<string, Json>,
  onResponse?: (response: Json) => void,
): void {
  const encoded = params ? canonicalize(params) : null;
  const cb = onResponse
    ? _keep(
        new JSCallback(
          (_userData: ptr, payload: ptr) => onResponse(_decode(payload)),
          CALLBACK_SIG,
        ),
      )
    : null;
  lib().symbols.fuyubot_bridge_invoke(
    cstr(method),
    encoded ? cstr(JSON.stringify(encoded)) : null,
    cb,
    null,
  );
}

/** 发一条自定义事件。 */
export function emit(event: string, payload?: Json): void {
  lib().symbols.fuyubot_bridge_emit(
    cstr(event),
    payload !== undefined ? cstr(JSON.stringify(payload)) : null,
  );
}

/** 打日志。level：error / warn / info / debug / trace。 */
export function log(message: string, level: string = "info"): void {
  // 数值必须与 fuyubot-abi 的 LogLevel 一致（Error=1..Trace=5）。
  const levels: Record<string, number> = { error: 1, warn: 2, info: 3, debug: 4, trace: 5 };
  lib().symbols.fuyubot_bridge_log(levels[level] ?? 3, cstr(message));
}

/** 登记一个工具的元数据。 */
export function registerTool(
  name: string,
  description: string = "",
  inputSchema?: Record<string, Json>,
): void {
  const descriptor: Record<string, Json> = { name, description };
  if (inputSchema !== undefined) descriptor.input_schema = inputSchema;
  lib().symbols.fuyubot_bridge_register_tool(cstr(JSON.stringify(descriptor)));
}

/** 挂一个中间件。`handler(stage, payload)`。 */
export function useMiddleware(
  stage: number,
  handler: (stage: number, payload: Json) => void,
): void {
  const cb = _keep(
    new JSCallback(
      (_userData: ptr, stage: number, payload: ptr) => handler(stage, _decode(payload)),
      MIDDLEWARE_SIG,
    ),
  );
  lib().symbols.fuyubot_bridge_use_middleware(stage, cb, null);
}

/** 发 ready 握手，然后阻塞读 stdin 直到进程结束。永远是脚本最后一行。 */
export function run(): void {
  lib().symbols.fuyubot_bridge_connect();
  lib().symbols.fuyubot_bridge_run();
}

// ---------------------------------------------------------------------------
// 内部
// ---------------------------------------------------------------------------

function _decode(payload: ptr | null): Json {
  if (!payload) return null;
  // 裸指针 → C 字符串（到 NUL 为止）→ JSON 解析。
  const text = new CString(payload).toString();
  if (!text) return null;
  try {
    return JSON.parse(text);
  } catch {
    return text; // 兜底：非 JSON 就原样给字符串
  }
}

// ---------------------------------------------------------------------------
// 自检：直接运行时验证 API 形状（不碰真宿主，不调 run）。
// ---------------------------------------------------------------------------

function _selftest(): void {
  // `_` → `@type`（递归到任意深度）：纯值判断，不碰 C 侧。
  const shape = canonicalize({
    _: "inputMessageVideo",
    video: {
      _: "inputVideo",
      video: { _: "inputFileLocal", path: "./video.mp4" },
      duration: 120,
    },
  });
  const shapeText = JSON.stringify(shape);
  const wantText = JSON.stringify({
    "@type": "inputMessageVideo",
    video: {
      "@type": "inputVideo",
      video: { "@type": "inputFileLocal", path: "./video.mp4" },
      duration: 120,
    },
  });
  if (shapeText !== wantText) throw new Error(`_ 没换成 @type：${shapeText}`);

  const seen: Json[] = [];
  const handler = (p: Json) => seen.push(p);

  on("updateNewMessage", handler);
  invoke("getMe", {}, handler);
  emit("custom", { x: 1 });
  log("hi", "info");
  registerTool("demo", "desc", { type: "object" });
  useMiddleware(0, (stage, payload) => seen.push([stage, payload]));

  // 不调 run（会阻塞）。这里只验证「调用不抛异常、回调被持有」。
  if (_keepalive.length !== 3) throw new Error(`回调没被正确持有：${_keepalive.length}`);
  console.log("fuyubot: self-test ok");
}

if (import.meta.main) {
  _selftest();
}
