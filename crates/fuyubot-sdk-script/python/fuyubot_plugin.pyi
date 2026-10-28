"""`fuyubot_plugin` 的类型声明（Python 插件作者对着它写代码）。

每个函数对应宿主 `HostApiVTable` 上的哪一项写在各自的 docstring 里——
**这就是「Python SDK 声明的 ABI 契约」**，宿主侧的对应实现见
`crates/fuyubot-loader/src/runtime.rs`。
"""

from asyncio import Future
from typing import Any, Awaitable, Callable, Dict, List, Optional, Union

#: 负载/回调里能看到的 JSON 值。
Json = Union[None, bool, int, float, str, List[Any], Dict[str, Any]]
#: 事件回调：收到一条事件的负载。
Handler = Callable[[Json], None]

def log(message: str, level: str = "info") -> None:
    """宿主 `HostApiVTable::log`。`level`：error / warn / info / debug / trace。"""
    ...

def on(
    event: str,
    handler: Optional[Handler] = None,
    filter: Optional[Dict[str, Any]] = None,
) -> Any:
    """宿主 `HostApiVTable::on`。可当装饰器用。

    `filter` 里的 `event_type` 键选择通道（`"update"` / `"response"`），
    其余键做负载字段匹配；不参与匹配的键不会传给宿主。
    """
    ...

def off(event: str, handler: Handler) -> None:
    """host `HostApiVTable::off` 的本地一半：摘掉回调分发表里的一项。"""
    ...

def invoke(
    method: str,
    params: Optional[Dict[str, Any]] = None,
    on_response: Optional[Handler] = None,
) -> int:
    """宿主 `HostApiVTable::td_invoke_simplified`（回调版）。

    `method` 是 TDLib 的方法名（如 `"sendMessage"`），`params` 是除 `@type`
    之外的参数。返回脚本侧的 tag；给了 `on_response` 就按 `request_id` 配对回调。
    """
    ...

def invoke_async(
    method: str,
    params: Optional[Dict[str, Any]] = None,
) -> Future:
    """`invoke` 的 `await` 版：只能在 `run_async(...)` 的 loop 里用。

    TDLib 的业务错误（`@type: "error"`）也会 resolve，自己判 `response["@type"]`；
    宿主拒绝请求才抛异常。
    """
    ...

def emit(event: str, payload: Optional[Json] = None) -> None:
    """宿主 `HostApiVTable::emit`。"""
    ...

def register_tool(
    name: str,
    description: str = "",
    input_schema: Optional[Dict[str, Any]] = None,
) -> None:
    """宿主 `HostApiVTable::register_tool`（只登记元数据，没有处理器指针）。"""
    ...

def call_tool(
    name: str,
    input: Optional[Dict[str, Any]] = None,
    on_response: Optional[Handler] = None,
) -> int:
    """宿主 `HostApiVTable::call_tool`：异步调用一个工具。"""
    ...

def list_tools(on_tools: Optional[Callable[[List[Any]], None]] = None) -> int:
    """宿主 `HostApiVTable::list_tools`：同步列出全部工具元数据。"""
    ...

def td_execute(
    request: Dict[str, Any],
    on_result: Optional[Callable[[Json], None]] = None,
) -> int:
    """宿主 `HostApiVTable::td_execute`：同步执行不碰网络的 TDLib 方法。"""
    ...

def use_middleware(
    stage: int,
    handler: Callable[[int, Json], None],
) -> Callable[[int, Json], None]:
    """宿主 `HostApiVTable::use_middleware`：挂中间件。`stage` 是 0..5 的整数。"""
    ...

def wait_for_update(
    filter: Dict[str, Any],
    timeout_ms: int = 0,
    on_match: Optional[Callable[[Json], None]] = None,
) -> int:
    """宿主 `HostApiVTable::wait_for_update`：阻塞等一条匹配的更新。"""
    ...

def wait_async(filter: Dict[str, Any], timeout_ms: int = 0) -> Future:
    """`wait_for_update` 的 `await` 版：只能在 `run_async(...)` 里用；超时返回 `None`。"""
    ...

def get_service(
    name: str,
    on_service: Optional[Callable[[Optional[Dict[str, Any]]], None]] = None,
) -> int:
    """宿主 `HostApiVTable::get_service`：读服务的元数据（名字 + 方法列表）。"""
    ...

def call_service(
    name: str,
    method: str,
    input: Optional[Dict[str, Any]] = None,
    on_result: Optional[Callable[[Json], None]] = None,
) -> int:
    """同步调用宿主服务的一个方法。"""
    ...

def config() -> Dict[str, Any]:
    """宿主传给插件的配置（`plugin_start` 的 config JSON，含 `plugin_config` 键）。"""
    ...

def plugin_config() -> Dict[str, Any]:
    """**自己的**配置段（`[plugin.<插件名>]`）。同步、`run()` 之前也能读；没配就是空字典。"""
    ...

def get_config(
    section: str,
    key: str = "",
    on_config: Optional[Callable[[Optional[Json]], None]] = None,
) -> int:
    """读任意插件段（异步，回调在 `run()` 之后）。`section` 空串读顶层，`key` 空串读整段。"""
    ...

def set_config(
    key: str,
    value: Json,
    on_done: Optional[Callable[[bool], None]] = None,
) -> int:
    """写**自己的**配置段（成功后回写 `fuyubot.toml`）。`key` 空串覆盖整段。"""
    ...

def run() -> None:
    """开始读宿主的行并分发。插件的最后一行永远是这个。"""
    ...

def run_async(main: Optional[Callable[[], Awaitable[None]]] = None) -> None:
    """asyncio 版 `run()`：事件循环 + 后台 stdin 读取，支持 `await`。

    `main` 是 async 函数（ready 之后作为任务启动）；不传就只跑事件循环。
    """
    ...
