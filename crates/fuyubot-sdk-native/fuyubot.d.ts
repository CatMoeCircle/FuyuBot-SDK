/**
 * fuyubot 薄 SDK 的类型声明（TS/JS 脚本插件作者对着它写代码）。
 *
 * 每个函数对应 `fuyubot-sdk-native` 冻结 C ABI 里的一个 `fuyubot_bridge_*` 符号。
 * 实现是同目录的 `fuyubot.ts`（依赖 bun:ffi，只能在 bun 运行时跑）。
 */

/** 负载 / 回调里能看到的 JSON 值。 */
export type Json =
  | null
  | boolean
  | number
  | string
  | Json[]
  | { [key: string]: Json };

/** 事件 / 响应回调：收到一条事件（已解析成对象）的负载。 */
export type Handler = (payload: Json) => void;

/** 订阅一条事件。对应 `fuyubot_bridge_on`。 */
export declare function on(
  event: string,
  handler: Handler,
  filter?: { [key: string]: Json },
): void;

/** 发一条 TDLib 请求。给 `onResponse` 就按请求配对回调。对应 `fuyubot_bridge_invoke`。 */
export declare function invoke(
  method: string,
  params?: { [key: string]: Json },
  onResponse?: Handler,
): void;

/** 发一条自定义事件。对应 `fuyubot_bridge_emit`。 */
export declare function emit(event: string, payload?: Json): void;

/** 打日志。level：error / warn / info / debug / trace。对应 `fuyubot_bridge_log`。 */
export declare function log(message: string, level?: string): void;

/** 登记一个工具的元数据。对应 `fuyubot_bridge_register_tool`。 */
export declare function registerTool(
  name: string,
  description?: string,
  inputSchema?: { [key: string]: Json },
): void;

/** 挂一个中间件。`handler(stage, payload)`。对应 `fuyubot_bridge_use_middleware`。 */
export declare function useMiddleware(
  stage: number,
  handler: (stage: number, payload: Json) => void,
): void;

/** 发 ready 握手，然后阻塞读 stdin 直到进程结束。永远是脚本最后一行。 */
export declare function run(): void;
