"""fuyubot —— 脚本插件作者用的薄 SDK（Python 版）。

它只是 ``fuyubot-sdk-native`` 原生桥的一层 **ctypes 封装**：加载
``fuyubot_sdk_native.dll``（或 .so / .dylib，见 :func:`_load`），把 ``fuyubot_bridge_*``
那组 C 函数转成作者直接能用的 Python 调用。

作者看不到任何协议细节——没有 ``"op"``、没有 ``tag``/``handle``、没有手拼 JSON：

    from fuyubot import on, invoke, log, run

    def on_message(update):
        log(f"新消息 {update['message']['id']}")

    on("updateNewMessage", on_message)
    run()  # 最后一行，永远如此

协议本身（「一行一个 JSON」、订阅号配对、ready 握手）在原生桥里完成，这里只做
「把 Python 函数转成 C 函数指针」这一件事。
"""

from __future__ import annotations

import json
import sys
from typing import Any, Callable, Dict, Optional

import ctypes
from ctypes import CFUNCTYPE, c_char_p, c_int32, c_uint32, c_uint64, c_void_p

# ---------------------------------------------------------------------------
# 加载原生桥
# ---------------------------------------------------------------------------

# 平台 → dll 文件名（cdylib 后缀 / 库名前缀）。
_IMPL = {
    "win32": ("fuyubot_sdk_native.dll",),
    "cygwin": ("fuyubot_sdk_native.dll",),
    "linux": ("libfuyubot_sdk_native.so",),
    "darwin": ("libfuyubot_sdk_native.dylib",),
}


def _load():
    """按平台加载原生桥 dll。

    查找顺序：脚本同目录 → 环境变量 ``FUYUBOT_NATIVE_LIB``（可指向绝对路径）→
    当前工作目录。找不到就报一个能看懂的错。
    """
    import os

    env = os.environ.get("FUYUBOT_NATIVE_LIB")
    candidates = []
    if env:
        candidates.append(env)
    here = os.path.dirname(os.path.abspath(__file__))
    for name in _IMPL.get(sys.platform, _IMPL["win32"]):
        candidates.append(os.path.join(here, name))
        candidates.append(name)  # 相对 cwd

    last_err = None
    for path in candidates:
        if not os.path.isabs(path):
            # 相对路径交给 ctypes 自己按系统库搜索路径找。
            pass
        try:
            return ctypes.CDLL(path)
        except OSError as exc:  # 找不到 / 位数不符
            last_err = exc
            continue

    raise RuntimeError(
        "找不到 fuyubot 原生桥库（fuyubot_sdk_native.{dll,so,dylib}）。"
        "请确认插件目录里有它，或用环境变量 FUYUBOT_NATIVE_LIB 指定路径。"
        f"最后错误：{last_err}"
    )


_lib = _load()

# ---------------------------------------------------------------------------
# C 函数签名（与 fuyubot-sdk-native 冻结的 ABI 一一对应）
# ---------------------------------------------------------------------------

# 事件 / 响应回调：void (*)(void *user_data, const char *payload_json)
CALLBACK = CFUNCTYPE(None, c_void_p, c_char_p)
# 中间件回调：void (*)(void *user_data, uint32_t stage, const char *payload_json)
MIDDLEWARE_CALLBACK = CFUNCTYPE(None, c_void_p, c_uint32, c_char_p)

_lib.fuyubot_bridge_connect.restype = c_int32

_lib.fuyubot_bridge_on.argtypes = [c_char_p, c_char_p, CALLBACK, c_void_p]
_lib.fuyubot_bridge_on.restype = c_uint64

_lib.fuyubot_bridge_off.argtypes = [CALLBACK, c_void_p]

_lib.fuyubot_bridge_invoke.argtypes = [c_char_p, c_char_p, CALLBACK, c_void_p]
_lib.fuyubot_bridge_invoke.restype = c_uint64

_lib.fuyubot_bridge_emit.argtypes = [c_char_p, c_char_p]
_lib.fuyubot_bridge_log.argtypes = [c_uint32, c_char_p]
_lib.fuyubot_bridge_register_tool.argtypes = [c_char_p]

_lib.fuyubot_bridge_use_middleware.argtypes = [
    c_uint32,
    MIDDLEWARE_CALLBACK,
    c_void_p,
]
_lib.fuyubot_bridge_use_middleware.restype = c_uint64


# ---------------------------------------------------------------------------
# 回调生命周期：ctypes 回调对象必须被持有，否则被 GC 后 C 侧调用即悬空。
# 这里把每个回调对象都记下来，让它们活到进程结束。用 list 而非按 handler 去重：
# 同一个 Python 函数可用于多处订阅，每一处都产生一个独立的 C 回调对象。
# ---------------------------------------------------------------------------

_keepalive: list = []


def _keep(callback: CALLBACK) -> None:
    """持有一个 ctypes 回调对象，防 GC。"""
    _keepalive.append(callback)


def _decode(payload: bytes) -> Any:
    """把 C 传回的 JSON 字节解码成 Python 对象（dict / list / 标量）。"""
    if not payload:
        return None
    text = payload.decode("utf-8")
    try:
        return json.loads(text)
    except json.JSONDecodeError:
        return text  # 兜底：非 JSON 就原样给字符串


def _encode(value: Any) -> Optional[bytes]:
    """把 Python 对象编码成 JSON 字节（None 表示「省略该字段」）。"""
    if value is None:
        return None
    return json.dumps(value).encode("utf-8")


# ---------------------------------------------------------------------------
# 作者 API
# ---------------------------------------------------------------------------


def on(event: str, handler: Callable[[Any], None], filter: Optional[Dict] = None) -> None:
    """订阅一条事件。收到时 ``handler(payload)``，``payload`` 是解析好的 dict。

    ``filter`` 可选，做字段匹配（``{"chat_id": 123}``）。
    """
    def trampoline(user_data, payload: bytes):
        # user_data 没用到：Python 侧 handler 已由闭包捕获，不需要回传。
        handler(_decode(payload))

    cb = CALLBACK(trampoline)
    _keep(cb)
    _lib.fuyubot_bridge_on(
        event.encode("utf-8"),
        _encode(filter),
        cb,
        None,  # user_data：不用，闭包已捕获 handler
    )


def invoke(
    method: str,
    params: Optional[Dict] = None,
    on_response: Optional[Callable[[Any], None]] = None,
) -> None:
    """发一条 TDLib 请求。给 ``on_response`` 就按请求配对回调（收到响应时调用）。

    ``params`` 是除 ``@type`` 之外的参数 dict，可省略。
    """
    cb = None
    if on_response is not None:
        def trampoline(user_data, payload: bytes):
            on_response(_decode(payload))

        cb = CALLBACK(trampoline)
        _keep(cb)

    _lib.fuyubot_bridge_invoke(
        method.encode("utf-8"),
        _encode(params),
        cb,
        None,
    )


def emit(event: str, payload: Any = None) -> None:
    """发一条自定义事件。"""
    _lib.fuyubot_bridge_emit(event.encode("utf-8"), _encode(payload))


def log(message: str, level: str = "info") -> None:
    """打日志。``level``：error / warn / info / debug / trace。"""
    # 数值必须与 fuyubot-abi 的 LogLevel 一致（Error=1..Trace=5）。
    levels = {"error": 1, "warn": 2, "info": 3, "debug": 4, "trace": 5}
    _lib.fuyubot_bridge_log(levels.get(level, 3), message.encode("utf-8"))


def register_tool(name: str, description: str = "", input_schema: Optional[Dict] = None) -> None:
    """登记一个工具的元数据。"""
    descriptor = {"name": name, "description": description}
    if input_schema is not None:
        descriptor["input_schema"] = input_schema
    _lib.fuyubot_bridge_register_tool(json.dumps(descriptor).encode("utf-8"))


def use_middleware(stage: int, handler: Callable[[int, Any], None]) -> None:
    """挂一个中间件。``handler(stage, payload)``，``stage`` 是命中的阶段。"""
    def trampoline(user_data, stage: int, payload: bytes):
        handler(stage, _decode(payload))

    cb = MIDDLEWARE_CALLBACK(trampoline)
    _keep(cb)
    _lib.fuyubot_bridge_use_middleware(stage, cb, None)


def run() -> None:
    """发 ready 握手，然后阻塞读 stdin 直到进程结束。永远是脚本最后一行。"""
    _lib.fuyubot_bridge_connect()
    _lib.fuyubot_bridge_run()


# ---------------------------------------------------------------------------
# 自检：直接运行时验证 API 形状（不碰真宿主，不调 run）。
# ---------------------------------------------------------------------------

def _selftest() -> None:
    seen: list = []

    def handler(payload):
        seen.append(payload)

    on("updateNewMessage", handler)
    invoke("getMe", {}, handler)
    emit("custom", {"x": 1})
    log("hi", "info")
    register_tool("demo", "desc", {"type": "object"})
    use_middleware(0, lambda stage, payload: seen.append((stage, payload)))

    # 不调 run（会阻塞）。这里只验证「调用不抛异常、回调对象被正确持有」。
    # 有回调的是三处：on、invoke 的 on_response、use_middleware。
    assert len(_keepalive) == 3, f"回调没被正确持有：{len(_keepalive)}"
    print("fuyubot: self-test ok")


if __name__ == "__main__":
    _selftest()
