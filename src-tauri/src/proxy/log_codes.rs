//! 代理模块日志错误码定义
//!
//! 格式: [模块-编号] 消息
//! - CB: Circuit Breaker (熔断器)
//! - SRV: Server (服务器)
//! - FWD: Forwarder (转发器)
//! - FO: Failover (故障转移)
//! - RSP: Response (响应处理)
//! - USG: Usage (使用量)
//!
//! **这是一份错误码字典，不是死代码**：27 个码位构成完整的分类体系，
//! 其中一部分尚未被调用点引用（新增错误时按模块取下一个未用码位即可，
//! 不需要重排已有码）。因此这里用模块级 `allow(dead_code)` 而不是逐个标注，
//! 也**不要**因为"暂时没引用"就删——那会让码位出现空洞，日志检索时对不上。

#![allow(dead_code)]

/// 熔断器日志码
pub mod cb {
    pub const OPEN_TO_HALF_OPEN: &str = "CB-001";
    pub const HALF_OPEN_TO_CLOSED: &str = "CB-002";
    pub const HALF_OPEN_PROBE_FAILED: &str = "CB-003";
    pub const TRIGGERED_FAILURES: &str = "CB-004";
    pub const TRIGGERED_ERROR_RATE: &str = "CB-005";
    pub const MANUAL_RESET: &str = "CB-006";
}

/// 服务器日志码
pub mod srv {
    pub const STARTED: &str = "SRV-001";
    pub const STOPPED: &str = "SRV-002";
    pub const STOP_TIMEOUT: &str = "SRV-003";
    pub const TASK_ERROR: &str = "SRV-004";
    pub const ACCEPT_ERR: &str = "SRV-005";
    pub const CONN_ERR: &str = "SRV-006";
}

/// 转发器日志码
pub mod fwd {
    pub const PROVIDER_FAILED_RETRY: &str = "FWD-001";
    pub const ALL_PROVIDERS_FAILED: &str = "FWD-002";
    pub const SINGLE_PROVIDER_FAILED: &str = "FWD-003";
}

/// 故障转移日志码
pub mod fo {
    pub const SWITCH_SUCCESS: &str = "FO-001";
    pub const CONFIG_READ_ERROR: &str = "FO-002";
    pub const LIVE_BACKUP_ERROR: &str = "FO-003";
    pub const ALL_CIRCUIT_OPEN: &str = "FO-004";
    pub const NO_PROVIDERS: &str = "FO-005";
}

/// 响应处理日志码
pub mod rsp {
    pub const BUILD_STREAM_ERROR: &str = "RSP-001";
    pub const READ_BODY_ERROR: &str = "RSP-002";
    pub const BUILD_RESPONSE_ERROR: &str = "RSP-003";
    pub const STREAM_TIMEOUT: &str = "RSP-004";
    pub const STREAM_ERROR: &str = "RSP-005";
}

/// 使用量日志码
pub mod usg {
    pub const LOG_FAILED: &str = "USG-001";
    pub const PRICING_NOT_FOUND: &str = "USG-002";
}
