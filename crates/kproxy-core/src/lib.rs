//! kiro-proxy 领域模型：账号、凭证、配置、路径解析。
//!
//! 本 crate 不做 IO，不依赖异步运行时。

pub mod account;
pub mod config;
pub mod ids;
pub mod model_policy;
pub mod paths;
pub mod provider;
