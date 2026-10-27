"""fuyubot 薄 SDK 的类型存根（脚本插件作者对着它写代码）。

每个函数对应 `fuyubot-sdk-native` 冻结 C ABI 里的一个 `fuyubot_bridge_*` 符号。
实现是同目录的 `fuyubot.py`。
"""

from typing import Any, Callable, Dict, Optional

# 负载 / 回调里能看到的 JSON 值。
Json = Any


def on(
    event: str,
    handler: Callable[[Json], None],
    filter: Optional[Dict[str, Json]] = None,
) -> None:
    """订阅一条事件。收到时 ``handler(payload)``，``payload`` 是解析好的 dict。

    对应 C ABI 的 ``fuyubot_bridge_on``。
    """
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
