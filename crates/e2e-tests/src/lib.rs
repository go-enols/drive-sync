//! derive-sync 端到端测试与基准的共享库。
//!
//! 只放"测试基础设施"，不含产品逻辑：
//! - [`harness`]：进程内中继启动、真实 WebSocket 客户端（Hello 认证）、E2E 握手、
//!   延迟统计与进程内存采样（阶段 1 基准用）。
//! - [`client_harness`]：`derive_client::Runtime` 双/多设备节点封装、事件等待、
//!   配对与剪切板/目录/vault 断言辅助（阶段 2 端到端测试用）。
//!
//! `derive-bench`（`src/bin/bench.rs`）与 `tests/*.rs` 共用这里的代码，避免重复实现
//! 协议握手细节。

#![forbid(unsafe_code)]

pub mod client_harness;
pub mod harness;

