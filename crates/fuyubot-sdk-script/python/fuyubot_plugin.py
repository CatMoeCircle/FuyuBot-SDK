"""fuyubot 脚本插件桥的 Python 侧。

这不是业务逻辑，只是**契约映射**：把宿主 `HostApiVTable` 上的动作包成普通函数，
底下走「一行一个 JSON」的协议（协议细节见 `fuyubot-sdk-script` 的 crate 文档）。

用法：把这个文件和你的 `plugin.py` 放在同一个目录，然后

    import fuyubot_plugin as host

    @host.on("updateNewMessage")
    def on_message(payload):
        host.log("收到一条更新")

    host.run()

类型声明见同目录的 `fuyubot_plugin.pyi`。
"""

import json
import sys
import threading

# 宿主按 **UTF-8** 收发（一行一个 JSON）。Windows 上 Python 默认用系统代码页读管道，
# 中文会直接变成乱码（`上海` → `涓婃捣`），所以这里把三个流显式钉成 UTF-8，
# 不指望 PYTHONUTF8 / PYTHONIOENCODING 一定被设上。
for _stream in (sys.stdin, sys.stdout, sys.stderr):
    try:
        _stream.reconfigure(encoding="utf-8")
    except (AttributeError, ValueError, OSError):
        # 被换成非文本流、或已被 detach：那就按原样用，不因为编码问题把插件搞挂。
        pass

_stdout_lock = threading.Lock()
_handlers = {}            # 宿主订阅号 -> [回调]
_middleware_handlers = {}  # 宿主中间件号 -> [回调]
_pending_ons = {}         # tag -> 回调（等宿主把订阅号回执给我们）
_pending_invokes = {}     # tag -> 回调（等宿主把请求号回执给我们）
_pending_middlewares = {}  # tag -> 回调（等宿主把中间件号回执给我们）
_pending_replies = {}     # tag -> 回调（等宿主的同步回执：listTools/execute/wait/getService/callService）
_response_handlers = {}   # request_id -> 回调
_response_channel = [False]
_next_tag = 1


def _load_config():
    """从 `argv[2]` 读宿主给的配置。**导入时就读**：这样 `plugin_config()` 在
    `run()` 之前（模块顶层）也能用——启动期读自己的配置是最常见的需求。"""
    if len(sys.argv) > 2 and sys.argv[2].strip():
        try:
            value = json.loads(sys.argv[2])
            if isinstance(value, dict):
                return value
        except ValueError:
            pass
    return {}


_config = _load_config()


def _send(message):
    with _stdout_lock:
        sys.stdout.write(json.dumps(message, separators=(",", ":")) + "\n")
        sys.stdout.flush()


def _new_tag():
    global _next_tag
    tag = _next_tag
    _next_tag += 1
    return tag


def log(message, level="info"):
    """往宿主日志里写一行。`level`：error / warn / info / debug / trace。"""
    _send({"op": "log", "level": level, "message": str(message)})


def on(event, handler=None, filter=None):
    """订阅事件。`handler` 给了就是普通调用，没给就能当装饰器用。

    `filter` 里的 `event_type` 键是**选通道**用的（"update" / "response"），
    其余键做负载匹配。

    **同一个事件可以订阅多次**，各带各的过滤器：宿主按订阅号把事件分回来，
    所以这里也是按订阅号分发的，不会出现「过滤器没命中的回调也被调到」。
    """
    if handler is None:
        def decorator(func):
            on(event, func, filter=filter)
            return func
        return decorator

    tag = _new_tag()
    _pending_ons[tag] = handler
    message = {"op": "on", "tag": tag, "event": event}
    if filter is not None:
        message["filter"] = filter
    _send(message)
    return handler


def off(event, handler):
    """退订（只影响本进程里的回调分发表；宿主的订阅随插件停止一起撤掉）。"""
    for handle in list(_handlers):
        handlers = _handlers[handle]
        if handler in handlers:
            handlers.remove(handler)


def invoke(method, params=None, on_response=None):
    """发一条 TDLib 请求。

    给了 `on_response` 就会在这条请求的响应回来时调它——响应按 `request_id`
    配对，脚本不用自己记请求号。
    """
    _ensure_response_channel()
    tag = _new_tag()
    if on_response is not None:
        _pending_invokes[tag] = on_response
    _send({"op": "invoke", "tag": tag, "method": method, "params": params or {}})
    return tag


def emit(event, payload=None):
    """发一条自定义事件（其他插件订阅得到）。"""
    _send({"op": "emit", "event": event, "payload": payload})


def register_tool(name, description="", input_schema=None):
    """登记一个工具的元数据。"""
    descriptor = {"name": name, "description": description}
    if input_schema is not None:
        descriptor["input_schema"] = input_schema
    _send({"op": "tool", "descriptor": descriptor})


def call_tool(name, input=None, on_response=None):
    """异步调用一个工具。响应按 `request_id` 配对，`on_response` 收到执行结果。"""
    tag = _new_tag()
    if on_response is not None:
        _pending_invokes[tag] = on_response
    _send({"op": "callTool", "tag": tag, "name": name, "input": input or {}})
    return tag


def list_tools(on_tools=None):
    """同步列出全部已登记工具的元数据。`on_tools` 收到工具列表。"""
    tag = _new_tag()
    if on_tools is not None:
        _pending_replies[tag] = lambda msg: on_tools(msg.get("tools"))
    _send({"op": "listTools", "tag": tag})
    return tag


def td_execute(request, on_result=None):
    """**同步**执行一个不碰网络的 TDLib 方法。发消息/编辑消息不能走它。"""
    tag = _new_tag()
    if on_result is not None:
        _pending_replies[tag] = lambda msg: on_result(msg.get("result"))
    _send({"op": "execute", "tag": tag, "request": request})
    return tag


def use_middleware(stage, handler):
    """挂中间件。`stage` 是 0..5 的整数；`handler(stage, payload)` 在更新经过该
    阶段时被调用。中间件无法单独注销（随插件停止一起撤掉）。"""
    tag = _new_tag()
    _pending_middlewares[tag] = handler
    _send({"op": "middleware", "tag": tag, "stage": stage})
    return handler


def wait_for_update(filter, timeout_ms=0, on_match=None):
    """阻塞等一条匹配 `filter` 的更新（在宿主侧独立线程里等）。`on_match` 收到
    匹配的负载，超时或被取消时收到 `None`。"""
    tag = _new_tag()
    if on_match is not None:
        _pending_replies[tag] = lambda msg: on_match(msg.get("payload"))
    _send({"op": "wait", "tag": tag, "filter": filter, "timeout_ms": timeout_ms})
    return tag


def get_service(name, on_service=None):
    """读取一个宿主服务的元数据。`on_service` 收到 `{"name": ..., "methods": [...]}`，
    服务不存在时收到 `None`。"""
    tag = _new_tag()
    if on_service is not None:
        _pending_replies[tag] = lambda msg: on_service(msg.get("service"))
    _send({"op": "getService", "tag": tag, "name": name})
    return tag


def call_service(name, method, input=None, on_result=None):
    """同步调用宿主服务的一个方法。`on_result` 收到返回 JSON，服务/方法不存在时 `None`。"""
    tag = _new_tag()
    if on_result is not None:
        _pending_replies[tag] = lambda msg: on_result(msg.get("result"))
    _send({"op": "callService", "tag": tag, "name": name, "method": method, "input": input or {}})
    return tag


def config():
    """宿主传给插件的配置（`plugin_start` 的 config JSON），没有就是空字典。

    里面额外带一个 `plugin_config` 键（= 你自己的 `[plugin.<插件名>]` 段）——
    通常用 [`plugin_config()`](#plugin_config) 取它，别自己翻这个字典。
    """
    return _config


def plugin_config():
    """**自己的**配置段（`fuyubot.toml` 里的 `[plugin.<你的插件名>]`）。

    同步、随时可读——**包括 `run()` 之前**（宿主在启动脚本时就把这段塞进了 `argv[2]`）。
    没有配这一段就是空字典。
    """
    value = _config.get("plugin_config")
    return value if isinstance(value, dict) else {}


def get_config(section, key="", on_config=None):
    """读**任意**插件段（自己的或别人的）——异步，回调要等 `run()` 之后才会来。

    `section` 是插件名（如 `"my.weather"`），空串读顶层（`[bot]` / `[telegram]` 等）；
    `key` 空串读整段、否则按点路径取（`"nested.k"`）。
    `on_config` 收到值；段 / 键不存在时收到 `None`。

    只想读**自己**那段就用 [`plugin_config()`](#plugin_config)——它是同步的，不需要
    等回执。
    """
    tag = _new_tag()
    if on_config is not None:
        _pending_replies[tag] = lambda msg: on_config(msg.get("config"))
    _send({"op": "getConfig", "tag": tag, "section": section, "key": key})
    return tag


def set_config(key, value, on_done=None):
    """写**自己的**配置段（`[plugin.<你的插件名>]`）。

    `key` 空串 = 覆盖整段，否则按点路径设键。成功后宿主会**回写 `fuyubot.toml`**
    （注意是全量重写，手写注释会丢）。`on_done` 收到 `True`。

    失败（越权 / 值不是合法 JSON / 写盘失败）时宿主发 `error`，只会打日志、
    **不会**调 `on_done`——所以别把「没收到回调」当成成功。
    """
    tag = _new_tag()
    if on_done is not None:
        _pending_replies[tag] = lambda _msg: on_done(True)
    _send({"op": "setConfig", "tag": tag, "key": key, "value": value})
    return tag


def _ensure_response_channel():
    """确保订阅了 response 通道：响应按 request_id 配对，得先收得到它。"""
    if not _response_channel[0]:
        _response_channel[0] = True
        on("response", _deliver_response)


def _dispatch(message):
    op = message.get("op")
    if op == "event":
        # 按订阅号分发：脚本可能给同一个事件注册了带不同过滤器的多条订阅。
        for handler in list(_handlers.get(message.get("handle"), [])):
            handler(message.get("payload"))
    elif op == "middleware" and "stage" in message:
        # 中间件**事件**：带 stage（注册回执带 tag、不带 stage）。
        for handler in list(_middleware_handlers.get(message.get("handle"), [])):
            handler(message.get("stage"), message.get("payload"))
    elif op == "on":
        handler = _pending_ons.pop(message.get("tag"), None)
        if handler is not None:
            _handlers.setdefault(message.get("handle"), []).append(handler)
    elif op == "middleware":
        # 中间件**注册回执**：把回调挂到回执里的那个 handle 上。
        handler = _pending_middlewares.pop(message.get("tag"), None)
        if handler is not None:
            _middleware_handlers.setdefault(message.get("handle"), []).append(handler)
    elif op == "invoked":
        handler = _pending_invokes.pop(message.get("tag"), None)
        if handler is not None:
            _response_handlers[message.get("request_id")] = handler
    elif op == "error":
        log("宿主报错：" + str(message.get("message")), level="error")
    elif "tag" in message:
        # 其余带 tag 的回执（listTools / execute / wait / getService / callService）。
        handler = _pending_replies.pop(message.get("tag"), None)
        if handler is not None:
            handler(message)


def _deliver_response(payload):
    if not isinstance(payload, dict):
        return
    handler = _response_handlers.pop(payload.get("request_id"), None)
    if handler is not None:
        handler(payload)


def run():
    """读宿主发来的行、分发给回调，直到宿主关掉管道。插件的最后一行就是它。

    这一行必须在**所有 `on(...)` 之后**：宿主会等收到 `{"op":"ready"}` 才认为
    插件加载完成，而 `plugin_start` 也要等它——脚本早退会被判成加载失败。
    """
    _send({"op": "ready"})
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            _dispatch(json.loads(line))
        except ValueError:
            log("收到一行不是 JSON 的内容：" + line, level="error")


def _selftest():
    """`python fuyubot_plugin.py` 就能跑。

    验的是这一层最容易写错的两件事：**事件按订阅号分发**（不是按事件名，
    否则带过滤器的第二条订阅会在不该响的时候响），以及**没见过的 handle/tag
    不能把脚本搞挂**。
    """
    global _handlers
    seen = []
    _handlers = {
        1: [lambda payload: seen.append(("a", payload))],
        2: [lambda payload: seen.append(("b", payload))],
    }
    _dispatch({"op": "event", "handle": 1, "payload": {"n": 1}})
    _dispatch({"op": "event", "handle": 2, "payload": {"n": 2}})
    # 没订阅过的 handle、没有 tag 的消息：静悄悄忽略，不许炸。
    _dispatch({"op": "event", "handle": 99, "payload": {"n": 3}})
    _dispatch({"op": "on", "handle": 5})
    assert seen == [("a", {"n": 1}), ("b", {"n": 2})], seen

    # 订阅回执要把回调挂到**回执里的那个 handle** 上。
    tag = _new_tag()
    _pending_ons[tag] = lambda payload: seen.append(("c", payload))
    _dispatch({"op": "on", "tag": tag, "handle": 42})
    _dispatch({"op": "event", "handle": 42, "payload": {"n": 4}})
    assert seen[-1] == ("c", {"n": 4}), seen

    # 响应按 request_id 配对。
    _response_handlers[7] = lambda payload: seen.append(("r", payload))
    _deliver_response({"request_id": 7, "ok": True})
    _deliver_response({"request_id": 7, "ok": True})  # 已经消费掉了，不该再来一次
    assert seen[-1] == ("r", {"request_id": 7, "ok": True}), seen
    assert len([item for item in seen if item[0] == "r"]) == 1, seen

    # 同步回执（_pending_replies）按 tag 配对，回调收到整个回执对象。
    rt = _new_tag()
    _pending_replies[rt] = lambda msg: seen.append(("tools", msg.get("tools")))
    _dispatch({"op": "listTools", "tag": rt, "tools": [{"name": "demo"}]})
    assert seen[-1] == ("tools", [{"name": "demo"}]), seen

    # 中间件：注册回执（带 tag）挂到 handle，事件（带 stage）按 handle 分发。
    mt = _new_tag()
    _pending_middlewares[mt] = lambda stage, payload: seen.append(("mw", stage, payload))
    _dispatch({"op": "middleware", "tag": mt, "handle": 9})
    _dispatch({"op": "middleware", "handle": 9, "stage": 0, "payload": {"t": "x"}})
    assert seen[-1] == ("mw", 0, {"t": "x"}), seen

    # 自己的配置段：同步可读（导入时就解析好了 argv[2]），缺省给空字典。
    global _config
    _config = {"plugin_config": {"greeting": "hi"}, "plugins_dir": "/p"}
    assert plugin_config() == {"greeting": "hi"}, plugin_config()
    _config = {}
    assert plugin_config() == {}, plugin_config()
    # `plugin_config` 不是对象时也不能返回怪东西。
    _config = {"plugin_config": None}
    assert plugin_config() == {}, plugin_config()

    # 读别人的段 / 写自己的段：都是 tag 回执。
    gt = _new_tag()
    _pending_replies[gt] = lambda msg: seen.append(("cfg", msg.get("config")))
    _dispatch({"op": "getConfig", "tag": gt, "config": {"a": 1}})
    assert seen[-1] == ("cfg", {"a": 1}), seen
    # 段不存在时回执里是 None。
    gt2 = _new_tag()
    _pending_replies[gt2] = lambda msg: seen.append(("cfg-none", msg.get("config")))
    _dispatch({"op": "getConfig", "tag": gt2, "config": None})
    assert seen[-1] == ("cfg-none", None), seen
    st = _new_tag()
    _pending_replies[st] = lambda msg: seen.append(("set", msg.get("op")))
    _dispatch({"op": "setConfig", "tag": st})
    assert seen[-1] == ("set", "setConfig"), seen

    print("fuyubot_plugin: self-test ok")


if __name__ == "__main__":
    _selftest()
