"""fuyubot 薄 SDK 的类型存根（脚本插件作者对着它写代码）。

每个函数对应 `fuyubot-sdk-native` 冻结 C ABI 里的一个 `fuyubot_bridge_*` 符号。
实现是同目录的 `fuyubot.py`。
"""

from asyncio import Future
from typing import Any, Awaitable, Callable, Dict, Optional

# 负载 / 回调里能看到的 JSON 值。
Json = Any


def on(
    event: str,
    handler: Callable[[Json], None],
    filter: Optional[Dict[str, Json]] = None,
) -> Any:
    """订阅一条事件。收到时 ``handler(payload)``，``payload`` 是解析好的 dict。

    对应 C ABI 的 ``fuyubot_bridge_on``。返回底层回调对象（可传给 ``off``）。
    """
    ...


def off(callback: Any) -> None:
    """退订 ``on`` 返回的回调对象。"""
    ...


def invoke(
    method: str,
    params: Optional[Dict[str, Json]] = None,
    on_response: Optional[Callable[[Json], None]] = None,
) -> None:
    """发一条 TDLib 请求。给 ``on_response`` 就按请求配对回调。

    对应 C ABI 的 ``fuyubot_bridge_invoke``。
    """
    ...


def invoke_async(
    method: str,
    params: Optional[Dict[str, Json]] = None,
) -> Future:
    """``invoke`` 的 ``await`` 版：只能在 ``run_async`` 的 loop 里用。"""
    ...


def wait_async(
    filter: Optional[Dict[str, Json]] = None,
    timeout_ms: int = 0,
) -> Future:
    """等一条匹配的更新；``timeout_ms == 0`` 表示一直等。超时/取消返回 ``None``。"""
    ...


def emit(event: str, payload: Json = None) -> None:
    """发一条自定义事件。对应 ``fuyubot_bridge_emit``。"""
    ...


def log(message: str, level: str = "info") -> None:
    """打日志。``level``：error / warn / info / debug / trace。

    对应 ``fuyubot_bridge_log``。
    """
    ...


def register_tool(
    name: str,
    description: str = "",
    input_schema: Optional[Dict[str, Json]] = None,
) -> None:
    """登记一个工具的元数据。对应 ``fuyubot_bridge_register_tool``。"""
    ...


def use_middleware(stage: int, handler: Callable[[int, Json], None]) -> None:
    """挂一个中间件。``handler(stage, payload)``。对应 ``fuyubot_bridge_use_middleware``。"""
    ...


def run() -> None:
    """发 ready 握手，然后阻塞读 stdin 直到进程结束。永远是脚本最后一行。"""
    ...


def run_async(main: Optional[Callable[[], Awaitable[None]]] = None) -> None:
    """asyncio 版 ``run()``：原生桥后台读 stdin，主线程跑事件循环。"""
    ...
