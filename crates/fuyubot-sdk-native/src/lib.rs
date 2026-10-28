//! 脚本插件的**原生桥**：在脚本进程里被 ctypes（Python）/ bun:ffi（JS）加载，
//! 导出 `fuyubot_bridge_*` 一组 C 函数，读写脚本进程自己的 stdio 与宿主通信。
//!
//! ## 它站在协议链的哪一端
//!
//! 宿主侧是 `fuyubot-sdk-script`（或 v2 的中间插件），脚本侧是这里。两边说同一种
//! 「一行一个 JSON」的协议，只是**视角镜像**：
//!
//! - 脚本 → 宿主（写 stdout）：`ready` / `on` / `invoke` / `emit` / `tool` /
//!   `middleware` / `log`；
//! - 宿主 → 脚本（读 stdin）：`event` / `middleware` / 各种回执（按 `tag` 配对）。
//!
//! 协议的完整表格见 `fuyubot-sdk-script` 的 crate 文档——**那一份是权威**，这里只做
//! 镜像那半。
//!
//! ## 为什么要有这一层
//!
//! 现在 `fuyubot_plugin.js` / `fuyubot_plugin.py` 各自**用语言手写**这份协议客户端
//! （`tag`/`handle` 配对、`ready` 握手、按订阅号分发）。两份语言实现会漂移。把协议
//! 沉进这一份 Rust，Python 与 JS 的薄 SDK 就只剩「把回调转成 C 函数指针」这一件事，
//! 对协议的理解不可能再各说各话。
//!
//! ## 冻结的 C ABI（改动要 bump 版本）
//!
//! ```c
//! typedef void (*FuyubotCallback)(void *user_data, const char *payload_json);
//! typedef void (*FuyubotMiddlewareCallback)(void *user_data, uint32_t stage,
//!                                           const char *payload_json);
//!
//! int32_t  fuyubot_bridge_connect(void);
//! uint64_t fuyubot_bridge_on(const char *event, const char *filter_json,
//!                            FuyubotCallback callback, void *user_data);
//! void     fuyubot_bridge_off(FuyubotCallback callback, void *user_data);
//! uint64_t fuyubot_bridge_invoke(const char *method, const char *params_json,
//!                                FuyubotCallback on_response, void *user_data);
//! void     fuyubot_bridge_emit(const char *event, const char *payload_json);
//! void     fuyubot_bridge_log(uint32_t level, const char *message);
//! void     fuyubot_bridge_register_tool(const char *descriptor_json);
//! uint64_t fuyubot_bridge_use_middleware(uint32_t stage,
//!                                        FuyubotMiddlewareCallback callback,
//!                                        void *user_data);
//! void     fuyubot_bridge_run(void);
//!
//! // async 版（JS runAsync）：后台线程读 stdin 入队，主线程 step 分发
//! int32_t  fuyubot_bridge_start(void);
//! int32_t  fuyubot_bridge_step(void);    // 1 处理一行 / 0 暂时没数据 / -1 EOF
//! int32_t  fuyubot_bridge_running(void);
//! ```
//!
//! 调用顺序（脚本作者视角，由薄 SDK 转成这些调用）：
//! `on` / `invoke` / …（注册）→ `connect`（发 `ready` 握手）→ `run`（阻塞读 stdin）。
//! `runAsync` 走 `start` + `step`：回调在**调用 step 的线程**上触发，不会跨线程碰 JS。
//!
//! ## 线程模型
//!
//! `run()` **同步阻塞**读 stdin，事件回调在 `run()` 的线程上触发——与现有
//! `fuyubot_plugin.js` 的 `run()` 一致。S0 已验证 ctypes 与 bun:ffi 的回调都能在
//! 非主线程被调用。

use std::collections::{HashMap, VecDeque};
use std::ffi::{CStr, c_char, c_void};
use std::io::{BufRead, BufReader, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use serde_json::{Value, json};

/// 事件 / 响应回调（C ABI 里的 `FuyubotCallback`）。
type EventCallback = unsafe extern "C" fn(user_data: *mut c_void, payload_json: *const c_char);

/// 中间件回调（C ABI 里的 `FuyubotMiddlewareCallback`，多一个 `stage`）。
type MiddlewareCallback =
    unsafe extern "C" fn(user_data: *mut c_void, stage: u32, payload_json: *const c_char);

/// 一条通往宿主的**可写**通道。
///
/// 抽成 trait 而不是直接抱 stdout：测试要用内存缓冲顶替真实 stdout，否则这一层
/// 只能靠「真的读进程 stdout」来验。
trait LineSink: Send + Sync {
    fn write_line(&self, line: &str);
}

/// 真实 stdout：写一行 JSON，flush。
struct StdoutSink;

impl LineSink for StdoutSink {
    fn write_line(&self, line: &str) {
        // stdout 的锁在 `lock()` 里，写完即释放，保证「一行一个 JSON」不被打断。
        let stdout = std::io::stdout();
        let mut handle = stdout.lock();
        let _ = writeln!(handle, "{line}");
        let _ = handle.flush();
    }
}

/// 一个事件回调 + 它的 user_data。user_data 存成 `usize`，让结构体自动 `Send`。
#[derive(Clone, Copy)]
struct EventHandler {
    callback: EventCallback,
    user_data: usize,
}

/// response 通道的占位回调：什么都不做。真正的配对在 `handle_line` 的 `event`
/// 分支里按 `payload.request_id` 做，不经过 handlers 表。
extern "C" fn noop_callback(_user_data: *mut c_void, _payload: *const c_char) {}

// 函数指针本身不是 `Sync`，但这里只是把薄 SDK 给进来的指针原样存/取，
// 跨线程仅做「读出来再调用」，由调用点保证安全（S0 已验证 ctypes/bun:ffi 可跨线程）。
unsafe impl Sync for EventHandler {}

impl EventHandler {
    /// 调用回调，把 payload 交出去。
    ///
    /// # Safety
    /// `payload` 必须是 NUL 结尾的合法 UTF-8（宿主给的 JSON）。
    unsafe fn fire(self, payload: *const c_char) {
        // SAFETY: 回调由薄 SDK 登记，user_data 原样回传。
        unsafe { (self.callback)(self.user_data as *mut c_void, payload) };
    }
}

/// 一个中间件回调 + 它的 user_data。
#[derive(Clone, Copy)]
struct MiddlewareHandler {
    callback: MiddlewareCallback,
    user_data: usize,
}

unsafe impl Sync for MiddlewareHandler {}

impl MiddlewareHandler {
    /// 调用回调，带上 stage 与 payload。
    ///
    /// # Safety
    /// `payload` 必须是 NUL 结尾的合法 UTF-8。
    unsafe fn fire(self, stage: u32, payload: *const c_char) {
        // SAFETY: 回调由薄 SDK 登记，user_data 原样回传。
        unsafe { (self.callback)(self.user_data as *mut c_void, stage, payload) };
    }
}

/// 原生桥的共享状态：`run()` 的读线程与各 C 函数都靠它。
struct Bridge {
    sink: Arc<dyn LineSink>,
    /// 生效的订阅：宿主订阅号 → 回调列表。
    handlers: Mutex<HashMap<u64, Vec<EventHandler>>>,
    /// 生效的中间件：宿主中间件号 → 回调列表。
    middleware_handlers: Mutex<HashMap<u64, Vec<MiddlewareHandler>>>,
    /// tag → 回调（等宿主回执订阅号）。
    pending_ons: Mutex<HashMap<u64, EventHandler>>,
    /// tag → 回调（等宿主回执请求号）。
    pending_invokes: Mutex<HashMap<u64, EventHandler>>,
    /// tag → 回调（等宿主回执中间件号）。
    pending_middlewares: Mutex<HashMap<u64, MiddlewareHandler>>,
    /// request_id → 回调（invoke 的响应按它配对）。
    response_handlers: Mutex<HashMap<u64, EventHandler>>,
    /// 是否已经订阅过 response 通道（invoke 给 on_response 时才需要）。
    response_subscribed: AtomicBool,
    next_tag: AtomicU64,
    /// 是否已经发过 `ready`（幂等）。
    connected: AtomicBool,
}

impl Bridge {
    fn new(sink: Arc<dyn LineSink>) -> Self {
        Self {
            sink,
            handlers: Mutex::new(HashMap::new()),
            middleware_handlers: Mutex::new(HashMap::new()),
            pending_ons: Mutex::new(HashMap::new()),
            pending_invokes: Mutex::new(HashMap::new()),
            pending_middlewares: Mutex::new(HashMap::new()),
            response_handlers: Mutex::new(HashMap::new()),
            response_subscribed: AtomicBool::new(false),
            next_tag: AtomicU64::new(1),
            connected: AtomicBool::new(false),
        }
    }

    fn next_tag(&self) -> u64 {
        self.next_tag.fetch_add(1, Ordering::Relaxed)
    }

    fn send(&self, value: &Value) {
        self.sink.write_line(&value.to_string());
    }

    /// 发 `ready` 握手。幂等：重复调用只发一次。
    fn connect(&self) {
        if self.connected.swap(true, Ordering::SeqCst) {
            return;
        }
        self.send(&json!({ "op": "ready" }));
    }

    /// 订阅事件。返回 tag。
    fn on(&self, event: &str, filter_json: Option<&str>, handler: EventHandler) -> u64 {
        let tag = self.next_tag();
        self.pending_ons
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(tag, handler);
        let mut message = json!({ "op": "on", "tag": tag, "event": event });
        if let Some(Ok(value)) = filter_json.map(serde_json::from_str::<Value>) {
            message["filter"] = value;
        }
        self.send(&message);
        tag
    }

    /// 退订：从本地分发表里摘掉这个回调。
    fn off(&self, callback: EventCallback, user_data: usize) {
        for list in self
            .handlers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values_mut()
        {
            list.retain(|handler| {
                !std::ptr::fn_addr_eq(handler.callback, callback) || handler.user_data != user_data
            });
        }
    }

    /// 发一条 TDLib 请求。返回 tag。`on_response` 可空。
    fn invoke(
        &self,
        method: &str,
        params_json: Option<&str>,
        on_response: Option<EventHandler>,
    ) -> u64 {
        // 响应按 request_id 配对，得先收得到响应——订阅 response 通道（只一次）。
        if on_response.is_some() {
            self.ensure_response_channel();
        }
        let tag = self.next_tag();
        if let Some(handler) = on_response {
            self.pending_invokes
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(tag, handler);
        }
        let mut message = json!({ "op": "invoke", "tag": tag, "method": method });
        message["params"] = params_json
            .and_then(|text| serde_json::from_str::<Value>(text).ok())
            .unwrap_or_else(|| json!({}));
        self.send(&message);
        tag
    }

    /// 订阅 response 通道（幂等）。占位 handler 让宿主愿意把响应发过来，
    /// 真正的配对在 [`Bridge::handle_line`] 的 `event` 分支里按 `request_id` 做。
    fn ensure_response_channel(&self) {
        if self.response_subscribed.swap(true, Ordering::SeqCst) {
            return;
        }
        let tag = self.next_tag();
        self.pending_ons
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                tag,
                EventHandler {
                    callback: noop_callback,
                    user_data: 0,
                },
            );
        self.send(&json!({ "op": "on", "tag": tag, "event": "response" }));
    }

    /// 发一条自定义事件。
    fn emit(&self, event: &str, payload_json: Option<&str>) {
        let payload = payload_json
            .and_then(|text| serde_json::from_str::<Value>(text).ok())
            .unwrap_or(Value::Null);
        self.send(&json!({ "op": "emit", "event": event, "payload": payload }));
    }

    /// 打日志。`level` 0..4（error / warn / info / debug / trace）。
    fn log(&self, level: u32, message: &str) {
        self.send(&json!({ "op": "log", "level": level, "message": message }));
    }

    /// 登记工具元数据。
    fn register_tool(&self, descriptor_json: &str) {
        let descriptor =
            serde_json::from_str::<Value>(descriptor_json).unwrap_or_else(|_| json!({}));
        self.send(&json!({ "op": "tool", "descriptor": descriptor }));
    }

    /// 挂中间件。返回 tag。
    fn use_middleware(&self, stage: u32, handler: MiddlewareHandler) -> u64 {
        let tag = self.next_tag();
        self.pending_middlewares
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(tag, handler);
        self.send(&json!({ "op": "middleware", "tag": tag, "stage": stage }));
        tag
    }

    /// 处理宿主发来的一行。
    fn handle_line(&self, line: &str) {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            return;
        };
        let Some(op) = value.get("op").and_then(Value::as_str) else {
            return;
        };

        match op {
            "event" => {
                let event_type = value.get("event").and_then(Value::as_str);
                let payload = value.get("payload").cloned().unwrap_or(Value::Null);
                if event_type == Some("response") {
                    // 响应事件：按 payload 里的 request_id 配对 invoke 的 on_response。
                    if let Some(request_id) = payload.get("request_id").and_then(Value::as_u64) {
                        self.fire_response(request_id, &payload);
                    }
                } else {
                    // 普通事件：按订阅号分发。
                    let handle = value.get("handle").and_then(Value::as_u64);
                    self.fire_handlers(handle, &payload);
                }
            }
            "middleware" => {
                if let Some(stage) = value.get("stage").and_then(Value::as_u64) {
                    // 中间件**事件**：带 stage。
                    let handle = value.get("handle").and_then(Value::as_u64);
                    let payload = value.get("payload").cloned().unwrap_or(Value::Null);
                    self.fire_middleware(handle, stage as u32, &payload);
                } else if let (Some(tag), Some(handle)) = (
                    value.get("tag").and_then(Value::as_u64),
                    value.get("handle").and_then(Value::as_u64),
                ) {
                    // 中间件**注册回执**：把回调挂到回执里的 handle 上。
                    if let Some(handler) = self
                        .pending_middlewares
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .remove(&tag)
                    {
                        self.middleware_handlers
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .entry(handle)
                            .or_default()
                            .push(handler);
                    }
                }
            }
            "on" => {
                // 订阅回执：把回调从 pending 挂到 handle。
                let (Some(tag), Some(handle)) = (
                    value.get("tag").and_then(Value::as_u64),
                    value.get("handle").and_then(Value::as_u64),
                ) else {
                    return;
                };
                if let Some(handler) = self
                    .pending_ons
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&tag)
                {
                    self.handlers
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .entry(handle)
                        .or_default()
                        .push(handler);
                }
            }
            "invoked" => {
                // 请求回执：把回调从 pending 挂到 request_id 上（等响应）。
                let (Some(tag), Some(request_id)) = (
                    value.get("tag").and_then(Value::as_u64),
                    value.get("request_id").and_then(Value::as_u64),
                ) else {
                    return;
                };
                if let Some(handler) = self
                    .pending_invokes
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&tag)
                {
                    self.response_handlers
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .insert(request_id, handler);
                }
            }
            "error" => {
                let message = value
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("宿主报错");
                self.log(0, &format!("宿主报错：{message}"));
            }
            // 其余 op（含未来扩展的同步回执）：这一版静默忽略，不 panic。
            _ => {}
        }
    }

    /// 把一条事件分发给对应订阅号的回调。
    fn fire_handlers(&self, handle: Option<u64>, payload: &Value) {
        let Some(handle) = handle else {
            return;
        };
        let list = self
            .handlers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&handle)
            .cloned()
            .unwrap_or_default();
        self.fire_event_handlers(list, payload);
    }

    /// 把一次中间件调用分发给对应中间件号的回调。
    fn fire_middleware(&self, handle: Option<u64>, stage: u32, payload: &Value) {
        let Some(handle) = handle else {
            return;
        };
        let list = self
            .middleware_handlers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&handle)
            .cloned()
            .unwrap_or_default();
        let Ok(text) = serde_json::to_string(payload) else {
            return;
        };
        let Ok(cstr) = std::ffi::CString::new(text) else {
            return;
        };
        for handler in list {
            // SAFETY: cstr 在调用期间存活；回调由薄 SDK 登记。
            unsafe { handler.fire(stage, cstr.as_ptr()) };
        }
    }

    /// 触发一组事件回调。
    fn fire_event_handlers(&self, list: Vec<EventHandler>, payload: &Value) {
        let Ok(text) = serde_json::to_string(payload) else {
            return;
        };
        let Ok(cstr) = std::ffi::CString::new(text) else {
            return;
        };
        for handler in list {
            // SAFETY: cstr 在调用期间存活；回调由薄 SDK 登记。
            unsafe { handler.fire(cstr.as_ptr()) };
        }
    }

    /// 触发一条响应：按 request_id 找 invoke 的 on_response。
    fn fire_response(&self, request_id: u64, payload: &Value) {
        let handler = self
            .response_handlers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&request_id);
        if let Some(handler) = handler {
            self.fire_event_handlers(vec![handler], payload);
        }
    }

    /// 读 stdin 直到 EOF，逐行处理。**阻塞**。
    fn run_stdin(&self) {
        let stdin = std::io::stdin();
        for line in BufReader::new(stdin.lock()).lines() {
            let Ok(line) = line else {
                break;
            };
            let line = line.trim();
            if !line.is_empty() {
                self.handle_line(line);
            }
        }
    }
}

// ── C 导出层 ─────────────────────────────────────────────────────────

/// 全局单例：C 函数没有 self，状态只能放这里。
static BRIDGE: OnceLock<Bridge> = OnceLock::new();

fn bridge() -> &'static Bridge {
    BRIDGE.get_or_init(|| Bridge::new(Arc::new(StdoutSink)))
}

/// 从裸指针读一个 UTF-8 字符串。空指针 → `None`。
///
/// # Safety
/// `ptr` 必须是 NUL 结尾的合法 UTF-8，或 null。
unsafe fn read_cstr(ptr: *const c_char) -> Option<String> {
    if ptr.is_null() {
        return None;
    }
    // SAFETY: 调用方保证。
    Some(
        unsafe { CStr::from_ptr(ptr) }
            .to_string_lossy()
            .into_owned(),
    )
}

/// 发 `ready` 握手。幂等，返回 0。
#[unsafe(no_mangle)]
pub extern "C" fn fuyubot_bridge_connect() -> i32 {
    bridge().connect();
    0
}

/// 订阅事件。`filter_json` 可空。返回 tag。
///
/// # Safety
/// `event` / `filter_json` 必须是 NUL 结尾字符串或 null；`callback` 必须是合法函数指针。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fuyubot_bridge_on(
    event: *const c_char,
    filter_json: *const c_char,
    callback: Option<EventCallback>,
    user_data: *mut c_void,
) -> u64 {
    // SAFETY: 由调用方保证。
    let event = unsafe { read_cstr(event) }.unwrap_or_default();
    let filter = unsafe { read_cstr(filter_json) };
    let Some(callback) = callback else {
        return 0;
    };
    let handler = EventHandler {
        callback,
        user_data: user_data as usize,
    };
    bridge().on(&event, filter.as_deref(), handler)
}

/// 退订：从本地分发表摘掉这个回调。
///
/// # Safety
/// `callback` 必须是合法函数指针。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fuyubot_bridge_off(
    callback: Option<EventCallback>,
    user_data: *mut c_void,
) {
    let Some(callback) = callback else {
        return;
    };
    bridge().off(callback, user_data as usize);
}

/// 发一条 TDLib 请求。`params_json` 可空。`on_response` 可空。返回 tag。
///
/// # Safety
/// `method` / `params_json` 必须是 NUL 结尾字符串或 null；`on_response` 要么是
/// 合法函数指针，要么是 null。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fuyubot_bridge_invoke(
    method: *const c_char,
    params_json: *const c_char,
    on_response: Option<EventCallback>,
    user_data: *mut c_void,
) -> u64 {
    // SAFETY: 由调用方保证。
    let method = unsafe { read_cstr(method) }.unwrap_or_default();
    let params = unsafe { read_cstr(params_json) };
    let handler = on_response.map(|callback| EventHandler {
        callback,
        user_data: user_data as usize,
    });
    bridge().invoke(&method, params.as_deref(), handler)
}

/// 发一条自定义事件。
///
/// # Safety
/// `event` / `payload_json` 必须是 NUL 结尾字符串或 null。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fuyubot_bridge_emit(event: *const c_char, payload_json: *const c_char) {
    // SAFETY: 由调用方保证。
    let event = unsafe { read_cstr(event) }.unwrap_or_default();
    let payload = unsafe { read_cstr(payload_json) };
    bridge().emit(&event, payload.as_deref());
}

/// 打日志。`level` 0..4（error / warn / info / debug / trace）。
///
/// # Safety
/// `message` 必须是 NUL 结尾字符串或 null。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fuyubot_bridge_log(level: u32, message: *const c_char) {
    // SAFETY: 由调用方保证。
    let message = unsafe { read_cstr(message) }.unwrap_or_default();
    bridge().log(level, &message);
}

/// 登记工具元数据。
///
/// # Safety
/// `descriptor_json` 必须是 NUL 结尾字符串或 null。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fuyubot_bridge_register_tool(descriptor_json: *const c_char) {
    // SAFETY: 由调用方保证。
    let descriptor = unsafe { read_cstr(descriptor_json) }.unwrap_or_default();
    bridge().register_tool(&descriptor);
}

/// 挂中间件。`stage` 0..5。返回 tag。
///
/// # Safety
/// `callback` 必须是合法函数指针。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fuyubot_bridge_use_middleware(
    stage: u32,
    callback: Option<MiddlewareCallback>,
    user_data: *mut c_void,
) -> u64 {
    let Some(callback) = callback else {
        return 0;
    };
    let handler = MiddlewareHandler {
        callback,
        user_data: user_data as usize,
    };
    bridge().use_middleware(stage, handler)
}

/// 阻塞读 stdin 直到 EOF。脚本的最后一行永远是它。
#[unsafe(no_mangle)]
pub extern "C" fn fuyubot_bridge_run() {
    bridge().run_stdin();
}

/// `run` 的后台线程版本：起线程把 stdin 的行**排队**，由 [`fuyubot_bridge_step`]
/// 在调用者线程（JS 主线程）上逐行分发。
///
/// 为什么不直接在后台线程调回调：bun 的 JSCallback 不是线程安全的，threadsafe
/// 回调又会在入队后执行——payload 指针那时已经被复用。排队 + 主线程 step 最稳。
/// 重复调用是幂等的。
static INPUT_QUEUE: Mutex<VecDeque<String>> = Mutex::new(VecDeque::new());
static INPUT_EOF: AtomicBool = AtomicBool::new(false);
static READER_STARTED: AtomicBool = AtomicBool::new(false);

#[unsafe(no_mangle)]
pub extern "C" fn fuyubot_bridge_start() -> i32 {
    if READER_STARTED.swap(true, Ordering::SeqCst) {
        return 0;
    }
    std::thread::spawn(|| {
        let stdin = std::io::stdin();
        for line in BufReader::new(stdin.lock()).lines() {
            let Ok(line) = line else {
                break;
            };
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            INPUT_QUEUE
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push_back(line.to_owned());
        }
        INPUT_EOF.store(true, Ordering::SeqCst);
    });
    0
}

/// 在**调用者线程**上处理一行排队输入：1 = 处理了一行，0 = 暂时没数据，
/// -1 = stdin 已关闭且队列已空。
#[unsafe(no_mangle)]
pub extern "C" fn fuyubot_bridge_step() -> i32 {
    let line = INPUT_QUEUE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .pop_front();
    if let Some(line) = line {
        bridge().handle_line(&line);
        return 1;
    }
    if INPUT_EOF.load(Ordering::SeqCst) {
        -1
    } else {
        0
    }
}

/// 输入线程是否还在工作（队列非空或 stdin 尚未 EOF）。
#[unsafe(no_mangle)]
pub extern "C" fn fuyubot_bridge_running() -> i32 {
    let pending = !INPUT_QUEUE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .is_empty();
    (!INPUT_EOF.load(Ordering::SeqCst) || pending) as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 把写出去的行收在内存里，好断言。
    #[derive(Default)]
    struct VecSink(Mutex<Vec<String>>);

    impl LineSink for VecSink {
        fn write_line(&self, line: &str) {
            self.0.lock().unwrap().push(line.to_owned());
        }
    }

    /// 测试用回调：把收到的 payload 存进 `user_data` 指向的缓冲。
    ///
    /// 回调是 `extern "C"`，不能捕获环境，只能靠 user_data 传状态——这也正是
    /// 生产环境薄 SDK 的用法（把语言对象指针当 user_data）。
    extern "C" fn on_event(user_data: *mut c_void, payload: *const c_char) {
        // SAFETY: user_data 是 fixture 里 leak 出来的 `Mutex<Vec<String>>`。
        let sink = unsafe { &*(user_data as *const Mutex<Vec<String>>) };
        // SAFETY: payload 由宿主保证是 NUL 结尾字符串。
        let text = unsafe { CStr::from_ptr(payload) }
            .to_string_lossy()
            .into_owned();
        sink.lock().unwrap().push(text);
    }

    extern "C" fn on_middleware(user_data: *mut c_void, stage: u32, payload: *const c_char) {
        // SAFETY: 同上。
        let sink = unsafe { &*(user_data as *const Mutex<Vec<String>>) };
        // SAFETY: 同上。
        let text = unsafe { CStr::from_ptr(payload) }
            .to_string_lossy()
            .into_owned();
        sink.lock().unwrap().push(format!("{stage}:{text}"));
    }

    /// 造一个桥 + 记录写出行的 sink + 记录回调收到的 payload 的缓冲。
    ///
    /// 回调缓冲用 `Arc` 持有、`as_ptr` 当 user_data 传进去（生命周期覆盖整个测试）。
    fn fixture() -> (Arc<Bridge>, Arc<VecSink>, Arc<Mutex<Vec<String>>>) {
        let sink = Arc::new(VecSink::default());
        let bridge = Arc::new(Bridge::new(sink.clone() as Arc<dyn LineSink>));
        let events = Arc::new(Mutex::new(Vec::new()));
        (bridge, sink, events)
    }

    fn event_handler(events: &Arc<Mutex<Vec<String>>>) -> EventHandler {
        EventHandler {
            callback: on_event,
            user_data: Arc::as_ptr(events) as usize,
        }
    }

    /// 验收：connect 发 ready，且幂等。
    #[test]
    fn connect_sends_ready_once() {
        let (bridge, sink, _events) = fixture();
        bridge.connect();
        bridge.connect();
        let written = sink.0.lock().unwrap();
        assert_eq!(written.len(), 1, "{written:?}");
        assert_eq!(written[0], r#"{"op":"ready"}"#);
    }

    /// 验收：on 发订阅请求，宿主回执后事件按订阅号分发。
    #[test]
    fn on_subscribes_and_delivers_events_by_handle() {
        let (bridge, sink, events) = fixture();
        let tag = bridge.on(
            "updateNewMessage",
            Some(r#"{"chat_id":1}"#),
            event_handler(&events),
        );
        assert_eq!(tag, 1);

        let written = sink.0.lock().unwrap();
        assert_eq!(written.len(), 1, "{written:?}");
        let sent: Value = serde_json::from_str(&written[0]).unwrap();
        assert_eq!(sent["op"], "on");
        assert_eq!(sent["tag"], 1);
        assert_eq!(sent["event"], "updateNewMessage");
        assert_eq!(sent["filter"]["chat_id"], 1);
        drop(written);

        // 宿主回执订阅号 7，然后投递事件。
        bridge.handle_line(r#"{"op":"on","tag":1,"handle":7}"#);
        bridge.handle_line(r#"{"op":"event","handle":7,"payload":{"@type":"updateNewMessage"}}"#);

        let events = events.lock().unwrap();
        assert_eq!(events.len(), 1, "{events:?}");
        assert_eq!(events[0], r#"{"@type":"updateNewMessage"}"#);
    }

    /// 验收：invoke 发请求，响应按 request_id 配对。
    #[test]
    fn invoke_pairs_response_by_request_id() {
        let (bridge, sink, events) = fixture();
        let tag = bridge.invoke(
            "sendMessage",
            Some(r#"{"chat_id":1}"#),
            Some(event_handler(&events)),
        );
        assert_eq!(tag, 2, "第一个 tag 被 response 通道订阅占用了");

        // 发出去的两行：先订阅 response 通道，再发 invoke。
        let written = sink.0.lock().unwrap();
        assert_eq!(written.len(), 2, "{written:?}");
        let sub: Value = serde_json::from_str(&written[0]).unwrap();
        assert_eq!(sub["op"], "on");
        assert_eq!(sub["event"], "response");
        let sent: Value = serde_json::from_str(&written[1]).unwrap();
        assert_eq!(sent["op"], "invoke");
        assert_eq!(sent["method"], "sendMessage");
        assert_eq!(sent["params"]["chat_id"], 1);
        drop(written);

        // 宿主回执：response 通道订阅号 + invoke 的请求号。
        bridge.handle_line(r#"{"op":"on","tag":1,"handle":100}"#);
        bridge.handle_line(r#"{"op":"invoked","tag":2,"request_id":55}"#);
        // 响应作为 response 事件到达，payload 带 request_id。
        bridge.handle_line(
            r#"{"op":"event","event":"response","handle":100,"payload":{"@type":"message","request_id":55,"@extra":"55"}}"#,
        );

        let events = events.lock().unwrap();
        assert_eq!(events.len(), 1, "{events:?}");
        // payload 重新序列化后键序会变（serde_json 按字典序），解析后比字段。
        let got: Value = serde_json::from_str(&events[0]).unwrap();
        assert_eq!(got["@type"], "message");
        assert_eq!(got["request_id"], 55);
        assert_eq!(got["@extra"], "55");
    }

    /// 验收：emit / log / register_tool 正确序列化（解析后断言，不硬编码键序）。
    #[test]
    fn emit_log_register_tool_serialize() {
        let (bridge, sink, _events) = fixture();
        bridge.emit("custom", Some(r#"{"x":1}"#));
        bridge.log(1, "hello");
        bridge.register_tool(r#"{"name":"demo"}"#);

        let written = sink.0.lock().unwrap();
        assert_eq!(written.len(), 3, "{written:?}");

        let emit: Value = serde_json::from_str(&written[0]).unwrap();
        assert_eq!(emit["op"], "emit");
        assert_eq!(emit["event"], "custom");
        assert_eq!(emit["payload"]["x"], 1);

        let log: Value = serde_json::from_str(&written[1]).unwrap();
        assert_eq!(log["op"], "log");
        assert_eq!(log["level"], 1);
        assert_eq!(log["message"], "hello");

        let tool: Value = serde_json::from_str(&written[2]).unwrap();
        assert_eq!(tool["op"], "tool");
        assert_eq!(tool["descriptor"]["name"], "demo");
    }

    /// 验收：use_middleware 发请求，中间件事件带 stage。
    #[test]
    fn middleware_delivers_stage_and_payload() {
        let (bridge, sink, events) = fixture();
        bridge.use_middleware(
            0,
            MiddlewareHandler {
                callback: on_middleware,
                user_data: Arc::as_ptr(&events) as usize,
            },
        );

        let written = sink.0.lock().unwrap();
        let sent: Value = serde_json::from_str(&written[0]).unwrap();
        assert_eq!(sent["op"], "middleware");
        assert_eq!(sent["stage"], 0);
        drop(written);

        bridge.handle_line(r#"{"op":"middleware","tag":1,"handle":9}"#);
        bridge.handle_line(r#"{"op":"middleware","handle":9,"stage":0,"payload":{"t":"x"}}"#);

        let mw = events.lock().unwrap();
        assert_eq!(mw.len(), 1, "{mw:?}");
        assert_eq!(mw[0], r#"0:{"t":"x"}"#);
    }

    /// 验收：off 从本地分发表摘掉回调，之后不再投递。
    #[test]
    fn off_removes_the_handler() {
        let (bridge, _sink, events) = fixture();
        bridge.on("updateNewMessage", None, event_handler(&events));
        bridge.handle_line(r#"{"op":"on","tag":1,"handle":7}"#);

        bridge.off(on_event, Arc::as_ptr(&events) as usize);
        bridge.handle_line(r#"{"op":"event","handle":7,"payload":{}}"#);

        assert!(events.lock().unwrap().is_empty(), "off 之后不该再投递");
    }

    /// 验收：不认识的 op 静默忽略，不 panic。
    #[test]
    fn unknown_ops_are_ignored() {
        let (bridge, _sink, _events) = fixture();
        bridge.handle_line("not json");
        bridge.handle_line(r#"{"op":"wat"}"#);
        bridge.handle_line(r#"{"op":"event"}"#); // 没有 handle
        // 到这里没 panic 就算过。
    }
}
