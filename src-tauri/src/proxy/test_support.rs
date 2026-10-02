//! 代理模块的测试专用辅助。
//!
//! 本模块的测试会绑定本地 mock upstream（`127.0.0.1:0`），HTTP 客户端必须
//! **直连回环**。而 `reqwest::Client::new()` 会读取 `HTTP_PROXY` / `HTTPS_PROXY`
//! / `ALL_PROXY` 等环境变量：在设置了全局代理的机器上，回环请求会被发往该代理，
//! mock upstream 不可达，测试表现为 502（而非断言失败），极易被误判为业务回归。
//!
//! 生产侧的转发客户端（`proxy::http_client::build_client`）已自行决策代理策略，
//! 不继承环境变量，因此这里只需修正测试客户端。
//!
//! 用单点辅助函数替代各处裸 `reqwest::Client::new()`，避免同类缺陷再次遗漏。

/// 构造不继承环境代理的测试客户端：所有请求（含回环）始终直连。
pub fn local_client() -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("build local test client")
}
