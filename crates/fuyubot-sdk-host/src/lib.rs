//! 中间插件的**宿主侧**共享逻辑。
//!
//! `fuyubot-python-host` / `fuyubot-js-host` 都链接这一份，只差语言参数：
//! 扫 `plugins/scripts/` 目录 → 探测解释器 → 给每个脚本装依赖 → spawn 一个进程 →
//! 读写它的 stdio（「一行一个 JSON」协议）→ 用子作用域隔离每个脚本 →
//! watch 目录增删 → 卸载即回收。
//!
//! ## 与 `fuyubot-sdk-script` 的关系
//!
//! 桥接逻辑（把脚本 stdout 的 `op` 翻译成 `HostApi` 调用）与
//! `fuyubot-sdk-script` 的 `Shared::handle_line` 是**同一份协议的两侧**，这里复制并
//! 改造成「一个中间插件管多个脚本」。两者最终会统一，当前兼容期各留一份。
//!
//! ## 子作用域
//!
//! 宿主预置了内置 service `fuyubot.host.scope`（见 fuyubot-loader）。每个脚本
//! `scope_create` 一个子作用域，pump 线程 `scope_enter` 之后再处理脚本的行——
//! 这样脚本注册的订阅/工具/中间件都落在它自己的 scope，卸载时 `scope_drop` 只撤它。
//!
//! 构造 [`ScriptHost`] 时会额外建一个**根作用域**，它是 watch 线程加载新脚本的立足
//! 点：`scope_create` 从 thread-local 栈继承 owner，而 watch 线程是全新线程、栈是空的，
//! 不先进根作用域的话热加载进来的脚本会挂到 owner = 0（宿主自己）名下，中间插件卸载
//! 时 `rollback_owner` 就回滚不到它们。
//!
//! ## watch 的范围：**只认目录增删**
//!
//! 多一个带合法 manifest 的目录 → 装依赖 → 起进程；少一个 → 杀进程树 → `scope_drop`。
//! **不做热更新**：改脚本文件不重载。一个脚本进程的完整生命周期是
//!「装依赖 + 起进程 + `ready` 握手」，改一行代码就整套重来既慢又常是白做。
//!
//! 轮询（`POLL_INTERVAL`）而非 `notify`：比对的是「顶层有哪些脚本目录」，
//! 于是 `bun install` / `uv sync` 在 `node_modules` / `.venv` 里掀起的文件风暴天然
//! 看不见——比按事件流去过滤依赖目录省心得多，代价是秒级延迟（对人工增删足够了）。
//!
//! ## 托管清单 service
//!
//! 每个中间插件把自己管着的脚本暴露成 service `fuyubot.host.scripts.<lang>`
//! （一个 `list` 方法，返回 `{"lang":…,"scripts":[…]}`）。**按语言分开起名**：
//! 两个中间插件共用一个名字的话，服务容器的「同名重复注册即报错」会让第二个
//! 什么都注册不上。宿主侧按前缀枚举 + `HostRuntime::call_service` 查询——
//! `fuyubot plugin list` 靠它把清单展平到脚本，否则只能看到两个 dll。
//!
//! 配置里 `"spawn": false` 进入**只发现模式**：不装依赖、不起进程、不 watch，
//! 只把扫到的脚本报进清单。列清单不该替用户跑 `bun install`，更不该把作者的
//! 脚本进程拉起来。

use std::collections::{BTreeMap, HashMap};
use std::ffi::{CStr, CString};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use fuyubot_plugin_api::{
    HostApi, HostExt, LogLevel, ServiceHandle, ServiceMethod, ServiceVTable, Stage, Subscription,
    parse_payload,
};
use serde_json::{Value, json};

/// `plugin_start` 最多等脚本报 `ready` 多久。
const STARTUP_TIMEOUT: Duration = Duration::from_secs(10);

/// watch 的轮询间隔。
///
/// 目录增删不需要毫秒级响应：脚本加载本身要过依赖安装 + 起进程 + `ready` 握手，
/// 量级在几百毫秒到几十秒。1s 的延迟感知不到，却换来了零依赖（不用 `notify`）
/// 和对 `node_modules` / `.venv` 内部文件风暴的天然免疫——轮询只比较「顶层有哪些
/// 脚本目录」，不论目录里多少文件在动。
const POLL_INTERVAL: Duration = Duration::from_millis(1000);

/// 内置 scope service 的名字。
const SCOPE_SERVICE_NAME: &str = "fuyubot.host.scope";

/// 脚本清单 service 的名字前缀：完整名字是 `fuyubot.host.scripts.<lang>`。
///
/// **按语言分开**而不是共用一个名字：两个中间插件（python / js）跑在同一个宿主里，
/// 服务容器对同名重复注册直接报错——共用一个名字就意味着只有一个的语言的脚本
/// 能被查到。带前缀的名字让宿主侧可以按前缀枚举（见 `HostRuntime::call_service`
/// 的调用方 `fuyubot plugin list`），将来多一门语言也不用改宿主编译单元。
const SCRIPTS_SERVICE_PREFIX: &str = "fuyubot.host.scripts";

/// 这门语言的清单 service 的完整名字。
fn scripts_service_name(lang: Lang) -> String {
    format!("{SCRIPTS_SERVICE_PREFIX}.{}", lang.label())
}

// ── 语言与脚本发现 ──────────────────────────────────────────────────

/// 从中间插件的配置 JSON 里取脚本目录，默认 `plugins/scripts`。
pub fn scripts_dir(config_json: &str) -> String {
    serde_json::from_str::<Value>(config_json)
        .ok()
        .and_then(|v| {
            v.get("scripts_dir")
                .and_then(|d| d.as_str())
                .map(str::to_owned)
        })
        .unwrap_or_else(|| "plugins/scripts".to_owned())
}

/// 从中间插件的配置 JSON 里读「要不要真的跑脚本」，默认 `true`。
///
/// `false` = **只发现不启动**：不探测解释器、不装依赖、不起进程、不挂 watch，
/// 只把扫到的脚本清单暴露成 service。给 `fuyubot plugin list` 用——列清单不该
/// 触发 `bun install` / `uv sync`，更不该把作者的脚本进程拉起来。
/// 缺省为 `true`：`fuyubot run` 不传这个字段，行为与从前完全一致。
pub fn spawn_scripts(config_json: &str) -> bool {
    serde_json::from_str::<Value>(config_json)
        .ok()
        .and_then(|v| v.get("spawn").and_then(Value::as_bool))
        .unwrap_or(true)
}

/// 中间插件管理的语言。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    /// 用 `uv run <entry>` 跑。
    Python,
    /// 用 `bun <entry>` 跑。
    Js,
}

impl Lang {
    /// 探测解释器：`uv` / `bun`。缺失时给出可执行的安装提示。
    pub fn probe(self) -> Result<(), String> {
        let (cmd, missing_hint) = match self {
            Self::Python => ("uv", "winget install astral-sh.uv"),
            Self::Js => ("bun", "curl -fsSL https://bun.sh/install | bash"),
        };
        match Command::new(cmd).arg("--version").output() {
            Ok(_) => Ok(()),
            Err(_) => Err(format!("缺 {cmd}，请安装（如 {missing_hint}）")),
        }
    }

    fn manifest_file(self) -> &'static str {
        match self {
            Self::Python => "pyproject.toml",
            Self::Js => "package.json",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Python => "python",
            Self::Js => "js",
        }
    }

    /// 跑脚本的命令，返回「程序 + 参数」。
    ///
    /// 与 [`Self::install_command`] 成对：一个负责开工前装依赖，一个负责把脚本
    /// 跑起来。**两者都必须在脚本自己的目录里执行**（`current_dir` 由调用方设）。
    fn run_command(self, entry: &str) -> (&'static str, Vec<&str>) {
        match self {
            Self::Python => ("uv", vec!["run", entry]),
            Self::Js => ("bun", vec![entry]),
        }
    }

    /// 装依赖的命令。**总在脚本自己的目录里跑**（`current_dir` 由调用方设）。
    ///
    /// 有 lock 文件时加 `--frozen-lockfile`：lock 与 manifest 不一致就**报错**，
    /// 而不是悄悄改 lock——宿主不该在用户不知情的情况下替他升级依赖树。
    fn install_command(self, dir: &Path) -> (&'static str, Vec<&'static str>) {
        match self {
            Self::Python => ("uv", vec!["sync"]),
            Self::Js => {
                if dir.join("bun.lock").exists() {
                    ("bun", vec!["install", "--frozen-lockfile"])
                } else {
                    ("bun", vec!["install"])
                }
            }
        }
    }
}

/// 一个脚本插件的身份，从 manifest（`package.json` / `pyproject.toml`）的
/// `fuyubot` / `[tool.fuyubot]` 段读出。
#[derive(Debug, Clone)]
pub struct ScriptManifest {
    /// 宿主里的插件名（日志前缀、错误归属）。
    pub name: String,
    /// 入口文件（相对脚本目录）。
    pub entry: String,
    /// 脚本目录。
    pub dir: PathBuf,
}

/// 扫目录，挑出属于 `lang` 的脚本（有 manifest 且带 fuyubot 段）。排序保证稳定。
pub fn discover(dir: &Path, lang: Lang) -> Vec<ScriptManifest> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return found;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let manifest_path = path.join(lang.manifest_file());
        let Ok(content) = std::fs::read_to_string(&manifest_path) else {
            continue;
        };
        if let Some((name, entry_name)) = parse_manifest(&content, lang) {
            found.push(ScriptManifest {
                name,
                entry: entry_name,
                dir: path,
            });
        }
    }
    found.sort_by(|a, b| a.name.cmp(&b.name));
    found
}

fn parse_manifest(content: &str, lang: Lang) -> Option<(String, String)> {
    match lang {
        Lang::Js => parse_package_json(content),
        Lang::Python => parse_pyproject(content),
    }
}

fn parse_package_json(content: &str) -> Option<(String, String)> {
    let json: Value = serde_json::from_str(content).ok()?;
    let fuyubot = json.get("fuyubot")?;
    let name = fuyubot.get("name")?.as_str()?;
    let entry = fuyubot
        .get("entry")
        .and_then(Value::as_str)
        .unwrap_or("index.ts");
    Some((name.to_owned(), entry.to_owned()))
}

/// 扫一遍目录得到「插件名 → manifest」快照，供 watch 比较。
///
/// 键是**插件名**（manifest 里声明的）而不是目录名：作者可以随手改目录名，
/// 插件身份跟着 manifest 走。
fn snapshot(dir: &Path, lang: Lang) -> BTreeMap<String, ScriptManifest> {
    discover(dir, lang)
        .into_iter()
        .map(|manifest| (manifest.name.clone(), manifest))
        .collect()
}

/// 轻量解析 `[tool.fuyubot]` 段（不引 TOML 库，够用即可）。
fn parse_pyproject(content: &str) -> Option<(String, String)> {
    let mut in_fuyubot = false;
    let mut name = None;
    let mut entry = None;
    for line in content.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_fuyubot = line == "[tool.fuyubot]";
            continue;
        }
        if !in_fuyubot {
            continue;
        }
        for (key, slot) in [("name", &mut name), ("entry", &mut entry)] {
            if let Some(rest) = line.strip_prefix(key) {
                if let Some(value) = rest.split_once('=').map(|(_, v)| v.trim()) {
                    *slot = Some(value.trim_matches('"').to_owned());
                }
            }
        }
    }
    name.map(|n| (n, entry.unwrap_or_else(|| "main.py".to_owned())))
}

// ── scope service 封装 ──────────────────────────────────────────────

/// 内置 scope service 的封装。`vtable` 存成整数，让它自动 `Send + Sync`（要跨到
/// 每个脚本的 pump 线程上用）。
#[derive(Clone)]
struct ScopeService {
    vtable: usize,
}

impl ScopeService {
    fn from(host: &HostApi) -> Option<Self> {
        let vtable = host.get_service(SCOPE_SERVICE_NAME);
        if vtable.is_null() {
            None
        } else {
            Some(Self {
                vtable: vtable as usize,
            })
        }
    }

    /// 按名字调一个方法。`None` = 成功无输出；`Some(text)` = 有输出（handle 或错误 JSON）。
    fn call(&self, method: &str, input: &str) -> Option<String> {
        // SAFETY: vtable 来自 get_service，指向宿主预置的内置 service，进程生命周期内有效。
        let service = unsafe { &*(self.vtable as *const ServiceVTable) };
        let input = CString::new(input).ok()?;
        for index in 0..service.method_count {
            // SAFETY: methods 指向 method_count 个连续 ServiceMethod。
            let method_entry = unsafe { &*service.methods.add(index) };
            if method_entry.name.is_null() {
                continue;
            }
            // SAFETY: 方法名是 NUL 结尾 UTF-8。
            if unsafe { CStr::from_ptr(method_entry.name) }.to_string_lossy() != method {
                continue;
            }
            let func = method_entry.func?;
            // SAFETY: func 是服务提供方给的函数指针，input 在调用期间存活。
            let raw = unsafe { func(service.context, input.as_ptr()) };
            if raw.is_null() {
                return None;
            }
            // SAFETY: 服务契约要求返回字符串由提供方持有、调用方读完即弃。
            return Some(
                unsafe { CStr::from_ptr(raw) }
                    .to_string_lossy()
                    .into_owned(),
            );
        }
        None
    }

    fn create(&self) -> Option<u64> {
        let out = self.call("scope_create", "{}")?;
        serde_json::from_str::<Value>(&out).ok()?["handle"].as_u64()
    }

    fn enter(&self, handle: u64) -> bool {
        self.call("scope_enter", &format!(r#"{{"handle":{handle}}}"#))
            .is_none()
    }

    fn leave(&self) {
        let _ = self.call("scope_leave", "{}");
    }

    fn drop(&self, handle: u64) -> bool {
        self.call("scope_drop", &format!(r#"{{"handle":{handle}}}"#))
            .is_none()
    }
}

// ── scripts service（托管清单查询）─────────────────────────────────

// 服务方法返回的字符串：**提供方持有**，调用方读完即弃（ABI 契约）。
// 线程局部缓冲，与宿主侧 `host_string` 同一套路。
thread_local! {
    static SERVICE_STRING: std::cell::RefCell<Option<CString>> =
        std::cell::RefCell::new(None);
}

fn service_string(text: &str) -> *const core::ffi::c_char {
    SERVICE_STRING.with(|slot| {
        *slot.borrow_mut() = Some(CString::new(text).unwrap_or_default());
        // SAFETY: slot 刚被赋值，borrow 期间指针有效。
        slot.borrow().as_ref().expect("刚放进去").as_ptr()
    })
}

/// 清单 service 的方法表。只有一个 `list`。
struct ScriptsMethods([ServiceMethod; 1]);

// SAFETY: `name` 指向 `'static` C 字符串字面量，`func` 是纯函数指针。
unsafe impl Sync for ScriptsMethods {}

static SCRIPTS_METHODS: ScriptsMethods = ScriptsMethods([ServiceMethod {
    name: c"list".as_ptr(),
    func: Some(api_scripts_list),
}]);

/// `list` 的入参被忽略；返回 `{"lang":…,"scripts":[…]}`。
///
/// `scripts` 每项：`name`（宿主里的插件名）/ `dir` / `entry` / `running` /
/// `pid` / `error`（起不来的脚本带原因，能起来的为 `null`）。
unsafe extern "C" fn api_scripts_list(
    context: *mut core::ffi::c_void,
    _input: *const core::ffi::c_char,
) -> *const core::ffi::c_char {
    if context.is_null() {
        return service_string(r#"{"lang":"?","scripts":[]}"#);
    }
    // SAFETY: context 来自 provide_scripts_service，指向活的 ScriptHost。
    let host = unsafe { &*(context as *const ScriptHost) };
    service_string(&host.scripts_json())
}

// ── 桥接（复制自 fuyubot-sdk-script，改造成多脚本）──────────────────

/// 一条通往脚本的**可写**通道（抽成 trait 便于测试顶替真实 stdin）。
trait LineSink: Send + Sync {
    fn write_line(&self, line: &str);
}

/// 真实子进程的 stdin。
struct ChildSink {
    stdin: Mutex<ChildStdin>,
    alive: AtomicBool,
}

impl LineSink for ChildSink {
    fn write_line(&self, line: &str) {
        if !self.alive.load(Ordering::Relaxed) {
            return;
        }
        let mut stdin = self.stdin.lock().unwrap_or_else(|e| e.into_inner());
        if writeln!(stdin, "{line}")
            .and_then(|()| stdin.flush())
            .is_err()
        {
            self.alive.store(false, Ordering::Relaxed);
        }
    }
}

/// 宿主与脚本之间的这条管道。
struct Chat {
    sink: Box<dyn LineSink>,
}

impl Chat {
    fn send(&self, value: &Value) {
        self.sink.write_line(&value.to_string());
    }
}

/// 一条订阅在宿主侧的私有数据（回调的 `user_data`）。
struct SubState {
    chat: Arc<Chat>,
    event: String,
    handle: AtomicU64,
}

impl SubState {
    fn forward(&self, payload: *const core::ffi::c_char) {
        // SAFETY: 宿主按 ABI 契约传的是临时 NUL 结尾字符串。
        let payload = unsafe { parse_payload(payload) };
        self.chat.send(&json!({
            "op": "event",
            "event": self.event,
            "handle": self.handle.load(Ordering::Relaxed),
            "payload": payload,
        }));
    }
}

/// 一条中间件在宿主侧的私有数据。
struct MiddlewareState {
    chat: Arc<Chat>,
    handle: AtomicU64,
}

impl MiddlewareState {
    fn forward(&self, stage: u32, payload: *const core::ffi::c_char) {
        // SAFETY: 同 SubState::forward。
        let payload = unsafe { parse_payload(payload) };
        self.chat.send(&json!({
            "op": "middleware",
            "handle": self.handle.load(Ordering::Relaxed),
            "stage": stage,
            "payload": payload,
        }));
    }
}

/// 脚本启动到哪一步。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Startup {
    Waiting,
    Ready,
    Dead,
}

/// 一个脚本进程的桥接状态：pump 线程读脚本 stdout，`handle_line` 翻译成 HostApi 调用。
struct Shared {
    host: HostApi,
    chat: Arc<Chat>,
    subs: Mutex<HashMap<u64, Subscription>>,
    states: Mutex<Vec<Arc<SubState>>>,
    middleware_states: Mutex<Vec<Arc<MiddlewareState>>>,
    startup: Mutex<Startup>,
    startup_signal: Condvar,
}

impl Shared {
    fn new(host: HostApi, chat: Arc<Chat>) -> Self {
        Self {
            host,
            chat,
            subs: Mutex::new(HashMap::new()),
            states: Mutex::new(Vec::new()),
            middleware_states: Mutex::new(Vec::new()),
            startup: Mutex::new(Startup::Waiting),
            startup_signal: Condvar::new(),
        }
    }

    fn mark_ready(&self) {
        *self.startup.lock().unwrap_or_else(|e| e.into_inner()) = Startup::Ready;
        self.startup_signal.notify_all();
    }

    fn mark_dead(&self) {
        let mut state = self.startup.lock().unwrap_or_else(|e| e.into_inner());
        if *state == Startup::Waiting {
            *state = Startup::Dead;
        }
        drop(state);
        self.startup_signal.notify_all();
    }

    fn wait_startup(&self, timeout: Duration) -> Startup {
        let deadline = Instant::now() + timeout;
        let mut state = self.startup.lock().unwrap_or_else(|e| e.into_inner());
        while *state == Startup::Waiting {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            let (guard, _) = self
                .startup_signal
                .wait_timeout(state, deadline - now)
                .unwrap_or_else(|e| e.into_inner());
            state = guard;
        }
        *state
    }

    fn reply(&self, tag: Option<u64>, op: &str, mut extra: Value) {
        if !extra.is_object() {
            extra = json!({});
        }
        let object = extra.as_object_mut().expect("上面保证是对象");
        object.insert("op".to_owned(), json!(op));
        if let Some(tag) = tag {
            object.insert("tag".to_owned(), json!(tag));
        }
        self.chat.send(&extra);
    }

    fn fail(&self, tag: Option<u64>, message: &str) {
        self.reply(tag, "error", json!({ "message": message }));
    }

    fn handle_line(&self, line: &str) {
        let value: Value = match serde_json::from_str(line) {
            Ok(value) => value,
            Err(e) => {
                self.fail(None, &format!("这一行不是合法 JSON ({e}): {line}"));
                return;
            }
        };
        let tag = value.get("tag").and_then(Value::as_u64);
        let Some(op) = value.get("op").and_then(Value::as_str) else {
            self.fail(tag, "缺 op 字段");
            return;
        };

        match op {
            "ready" => self.mark_ready(),

            "log" => {
                let text = value.get("message").and_then(Value::as_str).unwrap_or("");
                self.host.log(level_of(value.get("level")), text);
            }

            "on" => {
                let Some(event) = value.get("event").and_then(Value::as_str) else {
                    self.fail(tag, "on 缺 event 字段");
                    return;
                };
                let filter = match value.get("filter") {
                    None | Some(Value::Null) => None,
                    Some(filter) => Some(filter.to_string()),
                };
                let state = Arc::new(SubState {
                    chat: Arc::clone(&self.chat),
                    event: event.to_owned(),
                    handle: AtomicU64::new(0),
                });
                let pointer = Arc::as_ptr(&state) as *mut core::ffi::c_void;
                let handle = self.host.on(event, filter.as_deref(), forward, pointer);
                let Some(subscription) = Subscription::new(self.host, handle) else {
                    self.fail(tag, &format!("订阅 {event} 失败（过滤器不合法？）"));
                    return;
                };
                state.handle.store(handle.id, Ordering::Relaxed);
                self.states
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(state);
                self.subs
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(handle.id, subscription);
                self.reply(tag, "on", json!({ "handle": handle.id }));
            }

            "off" => {
                if let Some(id) = value.get("handle").and_then(Value::as_u64) {
                    self.subs
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .remove(&id);
                }
            }

            "invoke" => {
                let Some(method) = value.get("method").and_then(Value::as_str) else {
                    self.fail(tag, "invoke 缺 method 字段");
                    return;
                };
                let params = value.get("params").cloned().unwrap_or(Value::Null);
                match self.host.invoke_value(method, params) {
                    Ok(request) => self.reply(tag, "invoked", json!({ "request_id": request.id })),
                    Err(e) => self.fail(tag, &format!("invoke {method} 失败: {e}")),
                }
            }

            "emit" => {
                let event = value.get("event").and_then(Value::as_str).unwrap_or("");
                let payload = value
                    .get("payload")
                    .map(Value::to_string)
                    .unwrap_or_else(|| "null".to_owned());
                if let Err(e) = self.host.emit(event, &payload) {
                    self.fail(tag, &format!("emit {event} 失败: {e}"));
                }
            }

            "tool" => {
                let descriptor = value
                    .get("descriptor")
                    .map(Value::to_string)
                    .unwrap_or_else(|| "{}".to_owned());
                let handle = self.host.register_tool_meta(&descriptor);
                if handle.is_valid() {
                    self.reply(tag, "tool", json!({ "handle": handle.id }));
                } else {
                    self.fail(tag, "工具描述不合法");
                }
            }

            "callTool" => {
                let Some(name) = value.get("name").and_then(Value::as_str) else {
                    self.fail(tag, "callTool 缺 name 字段");
                    return;
                };
                let input = value
                    .get("input")
                    .map(Value::to_string)
                    .unwrap_or_else(|| "{}".to_owned());
                match self.host.call_tool_result(name, &input) {
                    Ok(text) => {
                        let output: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
                        self.reply(tag, "toolResult", json!({ "output": output }));
                    }
                    Err(e) => self.fail(tag, &format!("callTool {name} 失败: {e}")),
                }
            }

            "listTools" => {
                let tools = self.host.list_tools();
                let parsed = serde_json::from_str(&tools).unwrap_or(Value::Null);
                self.reply(tag, "listTools", json!({ "tools": parsed }));
            }

            "execute" => {
                let request = value
                    .get("request")
                    .map(Value::to_string)
                    .unwrap_or_else(|| "{}".to_owned());
                let result = self
                    .host
                    .td_execute(&request)
                    .and_then(|text| serde_json::from_str(&text).ok())
                    .unwrap_or(Value::Null);
                self.reply(tag, "execute", json!({ "result": result }));
            }

            "middleware" => {
                let Some(stage_num) = value.get("stage").and_then(Value::as_u64) else {
                    self.fail(tag, "middleware 缺 stage 字段");
                    return;
                };
                let Some(stage) = Stage::from_u32(stage_num as u32) else {
                    self.fail(tag, &format!("非法的中间件阶段: {stage_num}"));
                    return;
                };
                let state = Arc::new(MiddlewareState {
                    chat: Arc::clone(&self.chat),
                    handle: AtomicU64::new(0),
                });
                let pointer = Arc::as_ptr(&state) as *mut core::ffi::c_void;
                let handle = self.host.use_middleware(stage, forward_middleware, pointer);
                if !handle.is_valid() {
                    self.fail(tag, "中间件注册失败");
                    return;
                }
                state.handle.store(handle.id, Ordering::Relaxed);
                self.middleware_states
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(state);
                self.reply(tag, "middleware", json!({ "handle": handle.id }));
            }

            "wait" => {
                let filter = value
                    .get("filter")
                    .map(Value::to_string)
                    .unwrap_or_else(|| "{}".to_owned());
                let timeout_ms = value.get("timeout_ms").and_then(Value::as_u64).unwrap_or(0);
                let host = self.host;
                let chat = Arc::clone(&self.chat);
                let reply_tag = tag;
                std::thread::spawn(move || {
                    let handle = host.wait_for_update(&filter, timeout_ms);
                    if !handle.did_match() {
                        chat.send(&json!({
                            "op": "wait",
                            "tag": reply_tag,
                            "payload": Value::Null,
                        }));
                        return;
                    }
                    // SAFETY: 宿主保证 payload 在本线程下一次宿主调用前有效，这里立刻拷走。
                    let payload = unsafe { parse_payload(handle.payload_json) };
                    chat.send(&json!({
                        "op": "wait",
                        "tag": reply_tag,
                        "payload": payload,
                    }));
                });
            }

            "getService" => {
                let Some(name) = value.get("name").and_then(Value::as_str) else {
                    self.fail(tag, "getService 缺 name 字段");
                    return;
                };
                let vtable = self.host.get_service(name);
                if vtable.is_null() {
                    self.reply(tag, "getService", json!({ "service": Value::Null }));
                    return;
                }
                // SAFETY: 宿主返回的 vtable 在插件生命周期内有效；这里只读。
                let service = unsafe { describe_service(vtable) };
                self.reply(tag, "getService", json!({ "service": service }));
            }

            "callService" => {
                let (Some(name), Some(method)) = (
                    value.get("name").and_then(Value::as_str),
                    value.get("method").and_then(Value::as_str),
                ) else {
                    self.fail(tag, "callService 缺 name 或 method 字段");
                    return;
                };
                let input = value
                    .get("input")
                    .map(Value::to_string)
                    .unwrap_or_else(|| "{}".to_owned());
                // SAFETY: 宿主返回的 vtable 在插件生命周期内有效。
                let result = unsafe { call_service(self.host.get_service(name), method, &input) };
                match result {
                    Some(text) => {
                        let parsed = serde_json::from_str(&text).unwrap_or(Value::Null);
                        self.reply(tag, "callService", json!({ "result": parsed }));
                    }
                    None => self.fail(tag, &format!("服务 {name} 没有方法 {method} 或不存在")),
                }
            }

            other => self.fail(tag, &format!("不认识的 op: {other}")),
        }
    }

    fn shutdown(&self) {
        self.subs.lock().unwrap_or_else(|e| e.into_inner()).clear();
        self.states
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        self.middleware_states
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }
}

fn level_of(value: Option<&Value>) -> LogLevel {
    match value {
        Some(Value::Number(number)) => LogLevel::from_u32(number.as_u64().unwrap_or(3) as u32),
        Some(Value::String(name)) => match name.as_str() {
            "error" => LogLevel::Error,
            "warn" => LogLevel::Warn,
            "debug" => LogLevel::Debug,
            "trace" => LogLevel::Trace,
            _ => LogLevel::Info,
        },
        _ => LogLevel::Info,
    }
}

/// 订阅回调：把事件原样写给脚本。
unsafe extern "C-unwind" fn forward(
    user_data: *mut core::ffi::c_void,
    payload: *const core::ffi::c_char,
) -> i32 {
    // SAFETY: user_data 是 Shared::handle_line 里 Arc::as_ptr 出来的，活到 shutdown。
    let Some(state) = (unsafe { (user_data as *const SubState).as_ref() }) else {
        return -1;
    };
    state.forward(payload);
    0
}

/// 中间件回调。
unsafe extern "C-unwind" fn forward_middleware(
    user_data: *mut core::ffi::c_void,
    stage: u32,
    payload: *const core::ffi::c_char,
) -> i32 {
    // SAFETY: 同 forward。
    let Some(state) = (unsafe { (user_data as *const MiddlewareState).as_ref() }) else {
        return -1;
    };
    state.forward(stage, payload);
    0
}

/// 把一个 ServiceVTable 读成脚本看得懂的描述。
///
/// # Safety
/// `vtable` 必须指向活着的、宿主构造的服务描述。
unsafe fn describe_service(vtable: *const ServiceVTable) -> Value {
    // SAFETY: 调用方保证 vtable 有效。
    let service = unsafe { &*vtable };
    let name = if service.name.is_null() {
        String::new()
    } else {
        // SAFETY: ABI 契约要求 name 是 NUL 结尾 UTF-8。
        unsafe { CStr::from_ptr(service.name) }
            .to_string_lossy()
            .into_owned()
    };
    let mut methods = Vec::new();
    if !service.methods.is_null() {
        for index in 0..service.method_count {
            // SAFETY: methods 指向 method_count 个连续 ServiceMethod。
            let method = unsafe { &*service.methods.add(index) };
            if method.name.is_null() {
                continue;
            }
            // SAFETY: 同上。
            methods.push(
                unsafe { CStr::from_ptr(method.name) }
                    .to_string_lossy()
                    .into_owned(),
            );
        }
    }
    json!({ "name": name, "methods": methods })
}

/// 调用服务的一个方法。服务或方法不存在时返回 `None`。
///
/// # Safety
/// `vtable` 必须指向活着的服务描述，或为 null。
unsafe fn call_service(
    vtable: *const ServiceVTable,
    method_name: &str,
    input_json: &str,
) -> Option<String> {
    let service = unsafe { vtable.as_ref()? };
    let method = (0..service.method_count)
        .map(|index| unsafe { &*service.methods.add(index) })
        .find(|method| {
            !method.name.is_null()
                && unsafe { CStr::from_ptr(method.name) }.to_string_lossy() == method_name
        })?;
    let func = method.func?;
    let Ok(input) = CString::new(input_json) else {
        return None;
    };
    // SAFETY: func 是服务提供方给的函数指针，input 在调用期间存活。
    let raw = unsafe { func(service.context, input.as_ptr()) };
    if raw.is_null() {
        return Some("null".to_owned());
    }
    // SAFETY: 服务契约要求返回字符串由提供方持有、调用方读完即弃。
    Some(
        unsafe { CStr::from_ptr(raw) }
            .to_string_lossy()
            .into_owned(),
    )
}

// ── 脚本进程管理 ────────────────────────────────────────────────────

/// 一个跑起来的脚本：进程 + 桥接状态 + 子作用域。
struct ManagedScript {
    /// 脚本目录与入口：清单查询（`fuyubot.host.scripts`）要报，起进程时就知道。
    dir: PathBuf,
    entry: String,
    scope: u64,
    shared: Arc<Shared>,
    child: Mutex<Child>,
    pump: Option<std::thread::JoinHandle<()>>,
}

impl ManagedScript {
    fn stop(&mut self) {
        // 先杀**进程树**：脚本跑在 `uv run` / `bun` 的子进程里，只杀解释器会把脚本
        // 留成孤儿（设计文档 S3 的验收项）。`taskkill /T` 连子进程一起收。
        let pid = self.child.lock().ok().map(|child| child.id()).unwrap_or(0);
        if pid != 0 {
            kill_tree(pid);
        }
        // 兜底：taskkill 失败（沙箱里可能被拒）时，至少杀掉解释器本体。
        if let Ok(mut child) = self.child.lock() {
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(pump) = self.pump.take() {
            let _ = pump.join();
        }
        // 撤销桥接状态（订阅/中间件随 scope_drop 或 owner 回滚一并处理）。
        self.shared.shutdown();
    }
}

/// 杀进程树。Windows 用 `taskkill /T /F`，Unix 用 `kill -TERM`。
fn kill_tree(pid: u32) {
    #[cfg(windows)]
    {
        let _ = Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .status();
    }
    #[cfg(not(windows))]
    {
        let _ = Command::new("kill")
            .args(["-TERM", &pid.to_string()])
            .status();
    }
}

/// 中间插件的宿主：扫目录、为每个脚本 spawn 进程并桥接。
pub struct ScriptHost {
    host: HostApi,
    scripts_dir: PathBuf,
    /// `false` = 只发现不启动（`fuyubot plugin list` 用）：不装依赖、不起进程。
    spawn: bool,
    scope: Option<ScopeService>,
    /// 中间插件自己的根作用域，**必须在 plugin_scope 内创建**（也就是 `new` 被
    /// 调用的时候）。
    ///
    /// 它的存在是为了让后起的线程也能创建「归属于中间插件」的子作用域：
    /// `scope_create` 是从 thread-local 栈继承 owner 的，watch 线程是全新线程、
    /// 栈是空的，直接 create 会拿到 owner = 0（宿主自己），中间插件卸载时
    /// `rollback_owner` 就回滚不到它。watch 线程先 `enter(root)` 再 create 才对。
    root: Option<u64>,
    scripts: Mutex<HashMap<String, ManagedScript>>,
    /// 起不来的脚本 → 原因。`list` 要把它们也报出来，否则「装了 5 个只起来 3 个」
    /// 在清单里看不出来。
    failures: Mutex<BTreeMap<String, String>>,
    /// 只发现模式下的快照（`spawn = false`）：扫到但没启动的脚本。
    discovered: Mutex<BTreeMap<String, ScriptManifest>>,
    /// 置 true 让 watch 线程退出。
    watching_stopped: AtomicBool,
    /// watch 线程句柄 + 它的线程 id。
    ///
    /// `Mutex` 是为了让整个 struct 保持 `Sync`（`JoinHandle` 本身只 `Send`）——
    /// `start_all` 的并行装依赖要跨线程借用 `&self`。存 id 是为了 `stop_watch`
    /// 能避开 self-join：最后一个强引用消失时，drop 很可能正跑在 watch 线程里。
    watcher: Mutex<Option<(std::thread::ThreadId, std::thread::JoinHandle<()>)>>,
}

impl ScriptHost {
    /// 构造。`scripts_dir` 是脚本目录（通常是 `<bot>/plugins/scripts/`），
    /// `spawn` 为 `false` 时只发现不启动（见字段文档）。
    ///
    /// **必须在插件自己的作用域内调用**（宿主调 `plugin_start` 的那一刻正合适）：
    /// 这里创建的根作用域会继承当前 owner，之后热加载进来的脚本才认得自己属于谁。
    pub fn new(host: HostApi, scripts_dir: impl Into<PathBuf>, spawn: bool) -> Self {
        let scope = ScopeService::from(&host);
        let root = scope.as_ref().and_then(ScopeService::create);
        Self {
            host,
            scripts_dir: scripts_dir.into(),
            spawn,
            scope,
            root,
            scripts: Mutex::new(HashMap::new()),
            failures: Mutex::new(BTreeMap::new()),
            discovered: Mutex::new(BTreeMap::new()),
            watching_stopped: AtomicBool::new(false),
            watcher: Mutex::new(None),
        }
    }

    /// 托管清单的 JSON：`{"lang":…,"scripts":[…]}`。给 service 的 `list` 用。
    ///
    /// 三个来源合并：**跑着的**（`scripts`，带 pid）、**起不来的**（`failures`，
    /// 带原因）、**只发现没启动的**（`discovered`，`spawn = false` 时的快照）。
    /// 三者名字不重叠（一个脚本同一时刻只处于一种状态），按名字排序输出。
    fn scripts_json(&self) -> String {
        let mut entries: BTreeMap<String, Value> = BTreeMap::new();

        for (name, script) in self
            .scripts
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
        {
            let pid = script
                .child
                .lock()
                .ok()
                .map(|child| child.id())
                .unwrap_or(0);
            entries.insert(
                name.clone(),
                json!({
                    "name": name,
                    "dir": script.dir,
                    "entry": script.entry,
                    "running": true,
                    "pid": pid,
                    "error": Value::Null,
                }),
            );
        }
        for (name, error) in self
            .failures
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
        {
            entries.insert(
                name.clone(),
                json!({
                    "name": name,
                    "dir": Value::Null,
                    "entry": Value::Null,
                    "running": false,
                    "pid": 0,
                    "error": error,
                }),
            );
        }
        for (name, manifest) in self
            .discovered
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
        {
            entries.insert(
                name.clone(),
                json!({
                    "name": name,
                    "dir": manifest.dir,
                    "entry": manifest.entry,
                    "running": false,
                    "pid": 0,
                    "error": Value::Null,
                }),
            );
        }

        json!({ "scripts": entries.into_values().collect::<Vec<_>>() }).to_string()
    }

    /// 把清单 service 注册进宿主（`fuyubot.host.scripts.<lang>`）。
    ///
    /// vtable 与名字各泄漏一份：ABI 要求提供方保证它们活到注销为止，而插件整个
    /// 进程生命周期只 start 一次，两份小结构的代价可以忽略；换来的是 `context`
    /// 能直接指向 `self`（否则得给 `ScriptHost` 套 `Arc` 再存 `Weak`，更绕）。
    fn provide_scripts_service(&self, lang: Lang) {
        let name = scripts_service_name(lang);
        let Ok(leaked_name) = CString::new(name.clone()) else {
            return;
        };
        let leaked_name: &'static CString = Box::leak(Box::new(leaked_name));
        let vtable: &'static ServiceVTable = Box::leak(Box::new(ServiceVTable {
            name: leaked_name.as_ptr(),
            version: 1,
            size: core::mem::size_of::<ServiceVTable>() as u32,
            context: self as *const ScriptHost as *mut core::ffi::c_void,
            methods: SCRIPTS_METHODS.0.as_ptr(),
            method_count: SCRIPTS_METHODS.0.len(),
        }));
        // SAFETY: vtable 刚泄漏，活到进程结束；ABI 契约要的就是这个。
        let handle = unsafe { self.host.provide_service(&name, vtable) };
        if handle == ServiceHandle::INVALID {
            self.host
                .warn(&format!("注册 {name} service 失败（同名已存在？）"));
        }
    }

    /// 扫目录并启动所有脚本。返回每脚本的启动结果（`Err` = 该脚本失败，不影响别的）。
    pub fn start_all(&self, lang: Lang) -> Vec<(String, Result<(), String>)> {
        let mut outcomes = Vec::new();

        // 清单 service 先注册：哪怕后面解释器缺失、一个脚本都起不来，
        // 宿主侧（`fuyubot plugin list`）也查得到「这个中间插件管着哪些脚本」。
        self.provide_scripts_service(lang);

        // 只发现模式（`spawn = false`，`fuyubot plugin list` 用）：扫一眼存进
        // `discovered` 就收工。不探测解释器——列清单不该因为没装 bun 就什么都看不到；
        // 不装依赖、不起进程、不挂 watch。
        if !self.spawn {
            let manifests = discover(&self.scripts_dir, lang);
            let mut discovered = self.discovered.lock().unwrap_or_else(|e| e.into_inner());
            for manifest in manifests {
                discovered.insert(manifest.name.clone(), manifest);
            }
            return outcomes;
        }

        // 解释器缺失：整体降级，打一条清晰的日志，不阻止宿主启动。
        if let Err(e) = lang.probe() {
            self.host
                .warn(&format!("[fuyubot-{}-host] {}", lang.label(), e));
            return outcomes;
        }

        let Some(scope) = &self.scope else {
            self.host
                .warn("宿主没有内置 fuyubot.host.scope service，脚本子作用域不可用，跳过全部脚本");
            return outcomes;
        };

        let manifests = discover(&self.scripts_dir, lang);

        // 装依赖**并行**（冷启动最贵的一段，串行会把 N 个脚本的等待叠起来），
        // 起进程**串行**（落到日志里才读得出谁是谁、出错在第几个）。
        let installed = Self::install_all(&manifests, |manifest| self.install_deps(manifest, lang));

        for manifest in &manifests {
            let name = manifest.name.clone();
            // 依赖没装好就别起进程了——`uv run` / `bun` 也得去现装，报错只会更晦涩。
            let outcome = match installed.get(&name) {
                Some(Err(e)) => Err(e.clone()),
                _ => self.start_script(manifest, lang, scope),
            };
            if let Err(e) = &outcome {
                // 记进失败清单：`list` 要能看出「这个脚本在，但起不来」。
                self.failures
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(name.clone(), e.clone());
            }
            outcomes.push((name, outcome));
        }
        outcomes
    }

    /// 并行给一批脚本装依赖，返回「插件名 → 结果」。
    ///
    /// 抽成接受 installer 的纯函数是为了能在测试里换掉真实包管理器。失败被**逐个
    /// 隔离**：一个脚本的依赖装不上，其余脚本照样拿到自己的结果。
    fn install_all(
        manifests: &[ScriptManifest],
        install: impl Fn(&ScriptManifest) -> Result<(), String> + Sync,
    ) -> HashMap<String, Result<(), String>> {
        std::thread::scope(|scope| {
            let handles: Vec<_> = manifests
                .iter()
                .map(|manifest| scope.spawn(|| install(manifest)))
                .collect();
            // 名字从 `manifests` 取，不从线程返回值里取：install 万一 panic，
            // join 只拿得到 Err，名字得有个不依赖它的来源。
            handles
                .into_iter()
                .zip(manifests.iter())
                .map(|(handle, manifest)| {
                    let outcome = handle
                        .join()
                        .unwrap_or_else(|_| Err("依赖安装线程崩溃".to_owned()));
                    (manifest.name.clone(), outcome)
                })
                .collect()
        })
    }

    /// 给一个脚本装依赖。失败只影响这一个脚本。
    fn install_deps(&self, manifest: &ScriptManifest, lang: Lang) -> Result<(), String> {
        let (program, args) = lang.install_command(&manifest.dir);
        let outcome = Command::new(program)
            .args(&args)
            .current_dir(&manifest.dir)
            // stdout 丢掉：包管理器的进度条只会污染日志；
            // stderr 继承：真出错时要能看见原因。注意这里的 stdout **不是**脚本的
            // stdio——脚本进程还没起来，协议通道不受影响。
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .status();
        match outcome {
            Ok(status) if status.success() => Ok(()),
            Ok(status) => Err(format!(
                "{program} {} 失败（退出码 {:?}）",
                args.join(" "),
                status.code()
            )),
            Err(e) => Err(format!("跑 {program} {} 失败: {e}", args.join(" "))),
        }
    }

    fn start_script(
        &self,
        manifest: &ScriptManifest,
        lang: Lang,
        scope: &ScopeService,
    ) -> Result<(), String> {
        let name = manifest.name.clone();

        // 每个脚本一个子作用域（继承中间插件的 owner）。
        let Some(script_scope) = scope.create() else {
            return Err("scope_create 失败".to_owned());
        };

        // spawn 解释器。`install_command` / `run_command` 必须配着用：先装依赖再跑。
        let (program, args) = lang.run_command(&manifest.entry);
        let mut command = Command::new(program);
        command
            .args(&args)
            .current_dir(&manifest.dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        let mut child = command
            .spawn()
            .map_err(|e| format!("起进程 {program} 失败: {e}"))?;

        let stdin = child.stdin.take().ok_or("拿不到脚本 stdin")?;
        let stdout = child.stdout.take().ok_or("拿不到脚本 stdout")?;

        let sink = ChildSink {
            stdin: Mutex::new(stdin),
            alive: AtomicBool::new(true),
        };
        let chat = Arc::new(Chat {
            sink: Box::new(sink),
        });
        let shared = Arc::new(Shared::new(self.host, chat));

        // pump 线程：enter 脚本自己的作用域，再读它的行。这样脚本的注册都落到这个 scope。
        let pump_scope = scope.clone();
        let pump_shared = Arc::clone(&shared);
        let reader = BufReader::new(stdout);
        let pump = std::thread::spawn(move || {
            let entered = pump_scope.enter(script_scope);
            for line in reader.lines() {
                let Ok(line) = line else {
                    break; // stdout 关闭 = 脚本退出
                };
                if !entered {
                    break;
                }
                pump_shared.handle_line(&line);
            }
            pump_shared.mark_dead();
            if entered {
                pump_scope.leave();
            }
        });

        // 等 ready。
        let state = shared.wait_startup(STARTUP_TIMEOUT);
        if state != Startup::Ready {
            // 脚本没起来：回收进程和 scope，别留半初始化的状态。
            let _ = child.kill();
            let _ = child.wait();
            let _ = pump.join();
            scope.drop(script_scope);
            return Err(match state {
                Startup::Dead => {
                    "脚本在报 ready 前就退出了（语法错 / import 失败 / 缺依赖？）".to_owned()
                }
                _ => format!("等 ready 超时（>{STARTUP_TIMEOUT:?}），脚本可能忘了在最后调 run()"),
            });
        }

        self.scripts
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                name,
                ManagedScript {
                    dir: manifest.dir.clone(),
                    entry: manifest.entry.clone(),
                    scope: script_scope,
                    shared,
                    child: Mutex::new(child),
                    pump: Some(pump),
                },
            );
        Ok(())
    }

    /// 停掉并卸载一个脚本（撤销它的子作用域）。
    pub fn stop_script(&self, name: &str) {
        let scope = self.scope.clone();
        let mut script = self
            .scripts
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(name);
        if let Some(script) = script.as_mut() {
            script.stop();
            if let Some(scope) = &scope {
                scope.drop(script.scope);
            }
        }
        // 目录被删掉时，失败记录也跟着撤：留着就是一条永远对不上的陈旧项。
        self.failures
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(name);
        self.discovered
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(name);
    }

    /// 停掉所有脚本。
    pub fn stop_all(&self) {
        let names: Vec<String> = self
            .scripts
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .cloned()
            .collect();
        for name in names {
            self.stop_script(&name);
        }
    }

    /// 加载一个新出现的脚本：装依赖 → 起进程。watch 发现新目录时走这条。
    fn load_script(
        &self,
        manifest: &ScriptManifest,
        lang: Lang,
        scope: &ScopeService,
    ) -> Result<(), String> {
        let name = manifest.name.clone();
        self.install_deps(manifest, lang)?;
        let outcome = self.start_script(manifest, lang, scope);
        match &outcome {
            Ok(()) => {
                // 之前起不来、现在起来了：把失败记录撤掉，清单里它就是「运行中」。
                self.failures
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&name);
                self.host.info(&format!("检测到新脚本 {name}，已加载"))
            }
            Err(e) => {
                self.failures
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(name.clone(), e.clone());
                self.host.warn(&format!("新脚本 {name} 加载失败: {e}"))
            }
        }
        outcome
    }

    /// 开始 watch 脚本目录：**只认增删**——多一个脚本目录即加载，少一个即卸载。
    ///
    /// 刻意**不做热更新**（改 `index.ts` / `main.py` 不重载）：一个脚本进程的完整
    /// 生命周期是「装依赖 + 起进程 + `ready` 握手」，改一行代码就整套重来既慢，
    /// 又容易在依赖没动的时候白做一遍。作者重启 bot 就够了。
    ///
    /// 必须在 `start_all` **之后**调用：设计文档 S3 第一条细节就说，
    /// `bun install` / `uv sync` 正在写文件时 watch 若已在线，一次安装会抖成一串
    /// 加载事件。先装后挂，且本实现的轮询只比较顶层脚本目录清单，`node_modules` /
    /// `.venv` 里怎么翻涌都看不见。
    pub fn watch(self: &Arc<Self>, lang: Lang) {
        let Some(scope) = self.scope.clone() else {
            self.host
                .warn("宿主没有内置 fuyubot.host.scope service，目录 watch 未启用");
            return;
        };
        let Some(root) = self.root else {
            self.host.warn("没有可用的根作用域，目录 watch 未启用");
            return;
        };
        // Weak，不能是强引用：本线程若也握一条 `Arc<ScriptHost>`，引用计数永不归零，
        // `ScriptHost::drop` 就永不触发——而 drop 正是通知本线程退出的地方。
        let weak = Arc::downgrade(self);
        let dir = self.scripts_dir.clone();
        let handle = std::thread::spawn(move || {
            // 进入根作用域，且只进一次（本线程固定）：watch 线程的 thread-local
            // 作用域栈原本是空的，不 enter 的话它建出来的脚本子作用域会挂到
            // owner = 0（宿主自己）名下，中间插件卸载时 `rollback_owner` 回滚不到。
            let Some(first) = weak.upgrade() else {
                return;
            };
            if !scope.enter(root) {
                first
                    .host
                    .warn("watch 线程进不去中间插件的根作用域，目录 watch 未启用");
                return;
            }
            drop(first);

            let mut seen = snapshot(&dir, lang);
            loop {
                let Some(host) = weak.upgrade() else {
                    break;
                };
                if host.watching_stopped.load(Ordering::Relaxed) {
                    break;
                }
                // 先放掉强引用再去睡：攥着它睡会让 `ScriptHost` 永远等不到
                // 引用计数归零，也就永远不会被释放。
                drop(host);
                std::thread::sleep(POLL_INTERVAL);

                let Some(host) = weak.upgrade() else {
                    break;
                };
                let current = snapshot(&dir, lang);
                for (name, manifest) in &current {
                    if !seen.contains_key(name) {
                        let _ = host.load_script(manifest, lang, &scope);
                    }
                }
                for name in seen.keys() {
                    if !current.contains_key(name) {
                        host.stop_script(name);
                        host.host.info(&format!("脚本 {name} 的目录已移除，已卸载"));
                    }
                }
                seen = current;
                drop(host);
            }
            scope.leave();
        });
        *self.watcher.lock().unwrap_or_else(|e| e.into_inner()) =
            Some((handle.thread().id(), handle));
    }

    /// 让 watch 线程退出并等它收尾。幂等。
    ///
    /// 插件 `stop` 时要**显式**调用：dll 卸载之后若还有线程在它的代码段里跑，
    /// 就是段错误，所以这里一定要把线程收干净。
    pub fn stop_watch(&self) {
        self.watching_stopped.store(true, Ordering::Relaxed);
        let Some((owner, handle)) = self
            .watcher
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        else {
            return;
        };
        // 别 join 自己：`ScriptHost::drop` 是在最后一个强引用消失时跑的，而那个引用
        // 很可能正握在 watch 线程手里（它刚 `drop(host)`）。这里 join 会死锁。
        // 那种情况下拿出来的 handle 直接丢弃 = detach，线程下一轮 upgrade 失败即退出。
        if owner != std::thread::current().id() {
            let _ = handle.join();
        }
    }

    /// 当前管着的脚本数。
    pub fn script_count(&self) -> usize {
        self.scripts.lock().unwrap_or_else(|e| e.into_inner()).len()
    }
}

impl Drop for ScriptHost {
    fn drop(&mut self) {
        // 兜底路径：插件作者忘了显式 `stop_watch`（正常该在 `Plugin::stop` 里调），
        // 走到这里至少别让线程变成孤儿。注意此时可能正身处 watch 线程内部，
        // `stop_watch` 自己会避开 self-join。
        self.stop_watch();
        self.stop_all();
        // 撤销根作用域：眼下里面没注册任何东西，留这一步是因为「scope 属于谁」的
        // 语义要对称——有 create 就得有 drop，将来中间插件往根作用域里挂全局状态
        // 时不用再回头补。
        if let (Some(scope), Some(root)) = (&self.scope, self.root) {
            scope.drop(root);
        }
    }
}

// ── 钉住原生桥的导出符号 ────────────────────────────────────────────
//
// `fuyubot-sdk-native` 作为 rlib 被链接进中间插件的 cdylib 后，`fuyubot_bridge_*`
// 这些 `#[no_mangle]` 函数若没人引用，会被链接器当死代码裁掉——脚本进程加载同一个
// dll 时就会找不到原生后端。这个 `#[used]` 数组把它们钉住（设计文档 4.1 的
// 「同一份 dll 两个进程各加载一次」就靠它成立）。
/// 空指针 newtype：让 `#[used]` 锚点数组能 `Sync`。
#[allow(dead_code)] // 字段只当锚点存着，从不解引用
struct FnPtr(*const ());

// SAFETY: 只把函数指针地址当锚点存着，从不解引用。
unsafe impl Sync for FnPtr {}

#[used]
static KEEP_NATIVE_BRIDGE_SYMBOLS: [FnPtr; 9] = [
    FnPtr(fuyubot_sdk_native::fuyubot_bridge_connect as *const ()),
    FnPtr(fuyubot_sdk_native::fuyubot_bridge_on as *const ()),
    FnPtr(fuyubot_sdk_native::fuyubot_bridge_off as *const ()),
    FnPtr(fuyubot_sdk_native::fuyubot_bridge_invoke as *const ()),
    FnPtr(fuyubot_sdk_native::fuyubot_bridge_emit as *const ()),
    FnPtr(fuyubot_sdk_native::fuyubot_bridge_log as *const ()),
    FnPtr(fuyubot_sdk_native::fuyubot_bridge_register_tool as *const ()),
    FnPtr(fuyubot_sdk_native::fuyubot_bridge_use_middleware as *const ()),
    FnPtr(fuyubot_sdk_native::fuyubot_bridge_run as *const ()),
];

#[cfg(test)]
mod tests {
    use super::*;
    use fuyubot_plugin_api::{ABI_VERSION, HostApiVTable};

    /// 造一个干净的临时目录（跑完由测试自己清掉）。
    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/test-tmp/fuyubot-sdk-host")
            .join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn discovers_js_scripts_from_package_json_and_skips_the_rest() {
        let dir = temp_dir("discover-js");

        let weather = dir.join("weather-bot");
        std::fs::create_dir_all(&weather).unwrap();
        std::fs::write(
            weather.join("package.json"),
            r#"{"name":"weather-bot","fuyubot":{"name":"fuyubot.example.weather","entry":"index.ts"}}"#,
        )
        .unwrap();

        // 没有 fuyubot 段 → 跳过。
        let not_plugin = dir.join("not-plugin");
        std::fs::create_dir_all(&not_plugin).unwrap();
        std::fs::write(not_plugin.join("package.json"), r#"{"name":"x"}"#).unwrap();

        // 没 manifest → 跳过。
        std::fs::create_dir_all(dir.join("empty")).unwrap();

        let found = discover(&dir, Lang::Js);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].name, "fuyubot.example.weather");
        assert_eq!(found[0].entry, "index.ts");
        assert!(found[0].dir.ends_with("weather-bot"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn js_entry_defaults_to_index_ts() {
        let dir = temp_dir("discover-js-default");
        let bot = dir.join("bot");
        std::fs::create_dir_all(&bot).unwrap();
        std::fs::write(bot.join("package.json"), r#"{"fuyubot":{"name":"a.b"}}"#).unwrap();
        let found = discover(&dir, Lang::Js);
        assert_eq!(found[0].entry, "index.ts");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn discovers_python_scripts_from_pyproject_and_defaults_entry() {
        let dir = temp_dir("discover-py");

        let weather = dir.join("weather-bot");
        std::fs::create_dir_all(&weather).unwrap();
        std::fs::write(
            weather.join("pyproject.toml"),
            "[project]\nname = \"weather-bot\"\n\n[tool.fuyubot]\nname = \"fuyubot.example.weather\"\nentry = \"main.py\"\n",
        )
        .unwrap();

        // 没写 entry → 默认 main.py。
        let default = dir.join("default-bot");
        std::fs::create_dir_all(&default).unwrap();
        std::fs::write(
            default.join("pyproject.toml"),
            "[tool.fuyubot]\nname = \"a.b\"\n",
        )
        .unwrap();

        let found = discover(&dir, Lang::Python);
        assert_eq!(found.len(), 2, "{found:?}");
        // 按 name 排序："a.b" < "fuyubot.example.weather"。
        assert_eq!(found[0].name, "a.b");
        assert_eq!(found[0].entry, "main.py", "没写 entry 默认 main.py");
        assert_eq!(found[1].name, "fuyubot.example.weather");
        assert_eq!(found[1].entry, "main.py");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn scripts_dir_defaults_when_missing_or_invalid() {
        assert_eq!(scripts_dir(""), "plugins/scripts");
        assert_eq!(scripts_dir("{}"), "plugins/scripts");
        assert_eq!(scripts_dir("not json"), "plugins/scripts");
        assert_eq!(scripts_dir(r#"{"scripts_dir":"custom/dir"}"#), "custom/dir");
    }

    #[test]
    fn run_command_passes_the_entry_to_the_interpreter() {
        // 这条要是漏了参数，`bun` 会被无参启动 → 直接 REPL/退出 → 脚本永远
        // 报不出 ready。曾经真的就是这样（当时的 `if matches!(lang, Lang::Python)`
        // 只在 Python 分支加参数），端到端才暴露出来。
        let (program, args) = Lang::Js.run_command("index.ts");
        assert_eq!(program, "bun");
        assert_eq!(args, ["index.ts"]);

        let (program, args) = Lang::Python.run_command("main.py");
        assert_eq!(program, "uv");
        assert_eq!(args, ["run", "main.py"]);
    }

    #[test]
    fn install_command_freezes_when_lock_file_exists() {
        let dir = temp_dir("install-command");

        // 有 lock：冻结，lock 与 manifest 不一致时报错而不是偷偷改 lock。
        let locked = dir.join("locked");
        std::fs::create_dir_all(&locked).unwrap();
        std::fs::write(locked.join("bun.lock"), "").unwrap();
        let (program, args) = Lang::Js.install_command(&locked);
        assert_eq!(program, "bun");
        assert_eq!(args, ["install", "--frozen-lockfile"]);

        // 没 lock：允许生成 / 更新它。
        let unlocked = dir.join("unlocked");
        std::fs::create_dir_all(&unlocked).unwrap();
        let (program, args) = Lang::Js.install_command(&unlocked);
        assert_eq!(program, "bun");
        assert_eq!(args, ["install"]);

        let (program, args) = Lang::Python.install_command(&unlocked);
        assert_eq!(program, "uv");
        assert_eq!(args, ["sync"]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 验收（设计文档 S3）：**装依赖并行**（冷启动最贵的一段不该串行叠加），
    /// 且一个脚本装不上不影响其余脚本。
    #[test]
    fn install_all_runs_dependency_installs_in_parallel_and_isolates_failures() {
        let manifests: Vec<ScriptManifest> = ["a", "b", "c"]
            .iter()
            .map(|name| ScriptManifest {
                name: (*name).to_owned(),
                entry: "main.py".to_owned(),
                dir: PathBuf::from(name),
            })
            .collect();

        // 同一时刻有几个 installer 在跑：> 1 就说明确实并行了。
        let concurrent = AtomicU64::new(0);
        let peak = AtomicU64::new(0);
        let outcomes = ScriptHost::install_all(&manifests, |manifest| {
            let now = concurrent.fetch_add(1, Ordering::SeqCst) + 1;
            peak.fetch_max(now, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(150));
            concurrent.fetch_sub(1, Ordering::SeqCst);
            if manifest.name == "b" {
                Err("装不上".to_owned())
            } else {
                Ok(())
            }
        });

        assert_eq!(outcomes.len(), 3, "{outcomes:?}");
        assert!(outcomes["a"].is_ok());
        assert!(outcomes["c"].is_ok());
        assert_eq!(
            outcomes["b"].as_ref().unwrap_err(),
            "装不上",
            "b 的失败要原样带出来，好让 start_all 跳过起进程"
        );
        assert!(
            peak.load(Ordering::SeqCst) > 1,
            "装依赖必须是并行的，实测并发峰值 {}",
            peak.load(Ordering::SeqCst)
        );
    }

    /// watch 的增删判定完全建立在 `snapshot` 上，所以这一份行为就是 watch 的行为：
    /// **只有带合法 manifest 的目录才算脚本**，manifest 消失 = 脚本消失（触发卸载）。
    #[test]
    fn snapshot_counts_only_manifests_and_notices_their_disappearance() {
        let dir = temp_dir("snapshot");

        let plugin = dir.join("plugin-dir");
        std::fs::create_dir_all(&plugin).unwrap();
        std::fs::write(plugin.join("package.json"), r#"{"fuyubot":{"name":"a.b"}}"#).unwrap();

        // 空目录与没有 fuyubot 段的 package.json 都不算脚本。
        std::fs::create_dir_all(dir.join("empty")).unwrap();
        std::fs::create_dir_all(dir.join("plain-package")).unwrap();
        std::fs::write(
            dir.join("plain-package").join("package.json"),
            r#"{"name":"x"}"#,
        )
        .unwrap();

        let before = snapshot(&dir, Lang::Js);
        assert_eq!(before.len(), 1, "{before:?}");
        assert!(before.contains_key("a.b"));

        // 拿掉 manifest：watch 下一轮就会把它当作「目录被删」来卸载。
        std::fs::remove_file(plugin.join("package.json")).unwrap();
        assert!(
            snapshot(&dir, Lang::Js).is_empty(),
            "manifest 没了就该从快照里消失"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn native_bridge_anchor_keeps_all_nine_symbols() {
        // 锚点数组钉住 9 个 fuyubot_bridge_*，少一个中间插件 dll 就缺原生后端。
        assert_eq!(KEEP_NATIVE_BRIDGE_SYMBOLS.len(), 9);
    }

    /// `spawn` 开关：缺省 `true`（`run` 不传这个字段，行为与从前一致），
    /// 显式 `false` 进只发现模式；坏 JSON / 错类型都退回默认，不能让
    /// `plugin list` 因为配置写错就真的去起进程。
    #[test]
    fn spawn_scripts_defaults_to_true_and_reads_the_flag() {
        assert!(spawn_scripts(""));
        assert!(spawn_scripts("{}"));
        assert!(spawn_scripts("not json"));
        assert!(spawn_scripts(r#"{"spawn":"false"}"#));
        assert!(!spawn_scripts(r#"{"spawn":false}"#));
    }

    /// 清单 service 按语言分开起名：两个中间插件在同一个宿主里，
    /// 共用一个名字的话第二个会撞「同名重复注册」。
    #[test]
    fn scripts_service_names_are_per_language() {
        let python = scripts_service_name(Lang::Python);
        let js = scripts_service_name(Lang::Js);
        assert_eq!(python, "fuyubot.host.scripts.python");
        assert_eq!(js, "fuyubot.host.scripts.js");
        assert_ne!(python, js);
        assert!(
            python.starts_with(SCRIPTS_SERVICE_PREFIX) && js.starts_with(SCRIPTS_SERVICE_PREFIX),
            "两者要能按同一个前缀枚举出来"
        );
    }

    // ── 假宿主：只要 get_service 返回 null（没有内置 scope service）───────
    //
    // `HostApi::from_raw` 会逐个校验 17 个函数非空（缺一个就是宿主实现不完整），
    // 所以这里每个都补一个空桩；本测试只走 `new` 里的 get_service 一条路。

    unsafe extern "C" fn fake_get_service(
        _host: *mut core::ffi::c_void,
        _name: *const core::ffi::c_char,
    ) -> *const ServiceVTable {
        core::ptr::null()
    }

    unsafe extern "C" fn stub_sub(
        _h: *mut core::ffi::c_void,
        _e: *const core::ffi::c_char,
        _f: *const core::ffi::c_char,
        _c: Option<fuyubot_plugin_api::EventCallbackFn>,
        _u: *mut core::ffi::c_void,
    ) -> fuyubot_plugin_api::SubscriptionHandle {
        fuyubot_plugin_api::SubscriptionHandle::INVALID
    }
    unsafe extern "C" fn stub_off(
        _h: *mut core::ffi::c_void,
        _s: fuyubot_plugin_api::SubscriptionHandle,
    ) {
    }
    unsafe extern "C" fn stub_emit(
        _h: *mut core::ffi::c_void,
        _t: *const core::ffi::c_char,
        _p: *const core::ffi::c_char,
    ) -> i32 {
        0
    }
    unsafe extern "C" fn stub_tool(
        _h: *mut core::ffi::c_void,
        _d: *const core::ffi::c_char,
        _handler: Option<fuyubot_plugin_api::ToolHandlerFn>,
        _u: *mut core::ffi::c_void,
    ) -> fuyubot_plugin_api::ToolHandle {
        fuyubot_plugin_api::ToolHandle::INVALID
    }
    unsafe extern "C" fn stub_call_tool(
        _h: *mut core::ffi::c_void,
        _n: *const core::ffi::c_char,
        _i: *const core::ffi::c_char,
    ) -> fuyubot_plugin_api::RequestId {
        fuyubot_plugin_api::RequestId::INVALID
    }
    unsafe extern "C" fn stub_list_tools(_h: *mut core::ffi::c_void) -> *const core::ffi::c_char {
        core::ptr::null()
    }
    unsafe extern "C" fn stub_call_tool_result(
        _h: *mut core::ffi::c_void,
        _n: *const core::ffi::c_char,
        _i: *const core::ffi::c_char,
    ) -> *const core::ffi::c_char {
        core::ptr::null()
    }
    unsafe extern "C" fn stub_list_plugins(_h: *mut core::ffi::c_void) -> *const core::ffi::c_char {
        core::ptr::null()
    }
    unsafe extern "C" fn stub_provide(
        _h: *mut core::ffi::c_void,
        _n: *const core::ffi::c_char,
        _v: *const ServiceVTable,
    ) -> ServiceHandle {
        ServiceHandle::INVALID
    }
    unsafe extern "C" fn stub_td(
        _h: *mut core::ffi::c_void,
        _m: *const core::ffi::c_char,
    ) -> fuyubot_plugin_api::RequestId {
        fuyubot_plugin_api::RequestId::INVALID
    }
    unsafe extern "C" fn stub_td2(
        _h: *mut core::ffi::c_void,
        _n: *const core::ffi::c_char,
        _p: *const core::ffi::c_char,
    ) -> fuyubot_plugin_api::RequestId {
        fuyubot_plugin_api::RequestId::INVALID
    }
    unsafe extern "C" fn stub_execute(
        _h: *mut core::ffi::c_void,
        _r: *const core::ffi::c_char,
    ) -> *const core::ffi::c_char {
        core::ptr::null()
    }
    unsafe extern "C" fn stub_middleware(
        _h: *mut core::ffi::c_void,
        _s: u32,
        _c: Option<fuyubot_plugin_api::MiddlewareCallbackFn>,
        _u: *mut core::ffi::c_void,
    ) -> fuyubot_plugin_api::MiddlewareHandle {
        fuyubot_plugin_api::MiddlewareHandle::INVALID
    }
    unsafe extern "C" fn stub_wait(
        _h: *mut core::ffi::c_void,
        _f: *const core::ffi::c_char,
        _t: u64,
    ) -> fuyubot_plugin_api::WaitHandle {
        fuyubot_plugin_api::WaitHandle::INVALID
    }
    unsafe extern "C" fn stub_cancel(
        _h: *mut core::ffi::c_void,
        _w: fuyubot_plugin_api::WaitHandle,
    ) {
    }
    unsafe extern "C" fn stub_log(
        _h: *mut core::ffi::c_void,
        _l: u32,
        _m: *const core::ffi::c_char,
    ) {
    }
    unsafe extern "C" fn stub_get_config(
        _h: *mut core::ffi::c_void,
        _s: *const core::ffi::c_char,
        _k: *const core::ffi::c_char,
    ) -> *const core::ffi::c_char {
        core::ptr::null()
    }
    unsafe extern "C" fn stub_set_config(
        _h: *mut core::ffi::c_void,
        _k: *const core::ffi::c_char,
        _v: *const core::ffi::c_char,
    ) -> i32 {
        0
    }

    fn fake_api() -> HostApi {
        let vtable: &'static HostApiVTable = Box::leak(Box::new(HostApiVTable {
            version: ABI_VERSION,
            size: core::mem::size_of::<HostApiVTable>() as u32,
            host_ctx: core::ptr::null_mut(),
            on: Some(stub_sub),
            off: Some(stub_off),
            emit: Some(stub_emit),
            register_tool: Some(stub_tool),
            call_tool: Some(stub_call_tool),
            call_tool_result: Some(stub_call_tool_result),
            list_tools: Some(stub_list_tools),
            list_plugins: Some(stub_list_plugins),
            provide_service: Some(stub_provide),
            get_service: Some(fake_get_service),
            td_invoke: Some(stub_td),
            td_invoke_simplified: Some(stub_td2),
            td_execute: Some(stub_execute),
            use_middleware: Some(stub_middleware),
            wait_for_update: Some(stub_wait),
            cancel_wait: Some(stub_cancel),
            log: Some(stub_log),
            get_config: Some(stub_get_config),
            set_config: Some(stub_set_config),
            // ABI 0.1.1 的尾部字段：测试里和 provide_service 用同一个桩。
            provide_service_shared: Some(stub_provide),
        }));
        // SAFETY: vtable 刚泄漏，活到进程结束。
        unsafe { HostApi::from_raw(vtable) }.expect("假宿主必须能构造")
    }

    /// 清单 JSON 合并三个来源：跑着的 / 起不来的（带原因）/ 只发现没启动的。
    /// 三者同一时刻名字不重叠，输出按名字排序。
    #[test]
    fn scripts_json_merges_discovered_and_failures() {
        let host = ScriptHost::new(fake_api(), "plugins/scripts", false);

        host.discovered.lock().unwrap().insert(
            "fuyubot.example.alpha".to_owned(),
            ScriptManifest {
                name: "fuyubot.example.alpha".to_owned(),
                entry: "index.ts".to_owned(),
                dir: PathBuf::from("/x/alpha"),
            },
        );
        host.failures
            .lock()
            .unwrap()
            .insert("fuyubot.example.beta".to_owned(), "缺 bun".to_owned());

        let json: Value = serde_json::from_str(&host.scripts_json()).unwrap();
        let scripts = json["scripts"].as_array().expect("scripts 要是数组");
        assert_eq!(scripts.len(), 2, "{scripts:?}");

        // 排序稳定：alpha 在前。
        assert_eq!(scripts[0]["name"], "fuyubot.example.alpha");
        assert_eq!(scripts[0]["running"], false);
        assert_eq!(scripts[0]["dir"], "/x/alpha");
        assert_eq!(scripts[0]["entry"], "index.ts");
        assert!(scripts[0]["error"].is_null());

        // 起不来的也要在清单里，且带上原因——否则「装了 5 个只起来 3 个」看不出来。
        assert_eq!(scripts[1]["name"], "fuyubot.example.beta");
        assert_eq!(scripts[1]["running"], false);
        assert_eq!(scripts[1]["error"], "缺 bun");

        // 卸载要把失败记录一并撤掉：删掉目录还留着失败项就是陈旧数据。
        host.stop_script("fuyubot.example.beta");
        let json: Value = serde_json::from_str(&host.scripts_json()).unwrap();
        assert_eq!(json["scripts"].as_array().unwrap().len(), 1);
    }
}
