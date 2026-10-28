/*
 * __NAME__：fuyubot 插件（C++）。
 *
 * 逻辑：收到 updateNewMessage，正文等于 `/ping` 就回 `pong`。
 * 由 `fuyubot-sdk init --lang cpp` 生成；用你自己的工具链编它（`g++ -shared` / `cl /LD`）。
 *
 * 导出符号必须放在 `extern "C"` 里——C++ 会改函数名，宿主就 dlsym 不到了。
 * ABI v4 起一共七个：v3 的 init / start / stop / free_string，加上
 * `plugin_manifest`（自描述，宿主在 init **之前**读它定依赖顺序）、
 * `fuyubot_abi_version`、`plugin_free_handle`。本模板给了 manifest。
 */
#include <cstdint>
#include <cstdlib> /* std::malloc / std::free / std::strtoll */
#include <cstring>
#include <string>

#include "fuyubot_plugin.h"

namespace {

const HostApiVTable *g_host = nullptr;
int g_state = 0;

/* 极朴素的 JSON 取值：够样例用，不处理转义。真要用请引一个 JSON 库。 */
std::string find_string(const std::string &json, const std::string &key) {
    const std::string pattern = "\"" + key + "\":\"";
    const std::size_t start = json.find(pattern);
    if (start == std::string::npos) {
        return {};
    }
    const std::size_t from = start + pattern.size();
    const std::size_t end = json.find('"', from);
    if (end == std::string::npos) {
        return {};
    }
    return json.substr(from, end - from);
}

long long find_int(const std::string &json, const std::string &key) {
    const std::string pattern = "\"" + key + "\":";
    const std::size_t start = json.find(pattern);
    if (start == std::string::npos) {
        return 0;
    }
    return std::strtoll(json.c_str() + start + pattern.size(), nullptr, 10);
}

int32_t on_update(void *user_data, const char *payload) {
    (void)user_data;
    const std::string json = payload == nullptr ? std::string() : std::string(payload);
    if (find_string(json, "text") != "/ping") {
        return 0;
    }
    const long long chat_id = find_int(json, "chat_id");
    if (chat_id == 0) {
        return 0;
    }
    const std::string params =
        "{\"chat_id\":" + std::to_string(chat_id) + ",\"text\":\"pong\"}";
    g_host->td_invoke_simplified(g_host->host_ctx, "sendMessage", params.c_str());
    g_host->log(g_host->host_ctx, 3, "ping 插件：回了 pong");
    return 0;
}

}  // namespace

extern "C" {

FUYUBOT_EXPORT PluginHandle plugin_init(const HostApiVTable *host) {
    PluginHandle failed{0, 0, nullptr, nullptr};
    if (host == nullptr || host->version != ABI_VERSION) {
        return failed;
    }
    g_host = host;
    PluginHandle handle{
        ABI_VERSION,
        static_cast<std::uint32_t>(sizeof(PluginHandle)),
        &g_state,
        "__NAME__",
    };
    return handle;
}

/* 版本：报给宿主的就是它。换成你自己的版本号。 */
#define PLUGIN_VERSION "0.1.0"

/*
 * 自描述（ABI v4）：宿主在 plugin_init **之前**读它，用来决定要不要激活。
 *
 * 两个列表就是依赖声明：`inject` 硬（缺了不激活），`soft_inject` 软（缺了照常
 * 加载，有了优先排前面）。留空 = 不声明，宿主退回读配置段 `[plugin.<名字>].inject`。
 *
 * **每次调用都新开一块堆内存**：宿主读完会调本库的 `plugin_free_string` 还回来，
 * 所以不能返回静态字符串（那边真的 free 就是未定义行为，见头文件「内存所有权」）。
 */
FUYUBOT_EXPORT const char *plugin_manifest() {
    static const char BODY[] =
        "{\"name\":\"__NAME__\",\"version\":\"" PLUGIN_VERSION "\","
        "\"inject\":[],\"soft_inject\":[]}";
    char *copy = static_cast<char *>(std::malloc(sizeof(BODY)));
    if (copy == nullptr) {
        return nullptr; /* 宿主把 nullptr 当成「没有元数据」，退回读配置。 */
    }
    std::memcpy(copy, BODY, sizeof(BODY)); /* sizeof 含结尾 NUL */
    return copy;
}

FUYUBOT_EXPORT int32_t plugin_start(PluginHandle handle, const char *config_json) {
    (void)config_json;
    if (handle.context == nullptr || g_host == nullptr) {
        return -1;
    }
    const SubscriptionHandle subscription =
        g_host->on(g_host->host_ctx, "updateNewMessage", nullptr, on_update, nullptr);
    return subscription.id == 0 ? -1 : 0;
}

FUYUBOT_EXPORT void plugin_stop(PluginHandle handle) {
    (void)handle;
    g_host = nullptr;
}

FUYUBOT_EXPORT void plugin_free_string(char *ptr) {
    /* 必须**真的**释放：manifest 是本插件 malloc 的，宿主按契约还回来。
       free(nullptr) 是合法空操作，不用额外判空。 */
    std::free(ptr);
}

}  // extern "C"
