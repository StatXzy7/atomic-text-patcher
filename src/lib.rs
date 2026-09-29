//! # atomic-text-patcher
//!
//! 一个零第三方依赖的 Rust 文本补丁引擎，用于把一批互相关联的修改
//! （新增 / 删除 / 精确替换）以事务方式应用到项目目录：
//!
//! - [`plan::Plan::build`] 仅读取磁盘，生成可预览的计划；
//! - [`engine::commit`] 重新核对字节、写入 journal、逐文件原子替换；
//! - [`engine::recover`] 是显式恢复入口，提交中断后整批回到/进入一致状态；
//! - 预览回执位于项目目录之外，预览对目标目录零写入；
//! - 写入会话通过 `flock` 互斥；路径越界、符号链接逃逸一律拒绝。
//!
//! 典型用法见仓库中的 `examples/` 与 `demo/`。

pub mod engine;
pub mod error;
pub mod hash;
pub mod json;
pub mod lock;
pub mod pathsafe;
pub mod plan;

pub use engine::{commit, recover, CommitOutcome, CrashAction, CrashAt, CrashHook, NoCrash, RecoveryReport};
pub use error::{Error, ErrorKind};
pub use plan::{parse_ops, FileAction, Op, Plan};

/// Convenience wrapper: build a plan from a JSON operations document.
pub fn build_plan_from_json(root: &std::path::Path, json_text: &str) -> error::Result<Plan> {
    let ops = parse_ops(json_text)?;
    Plan::build(root, ops)
}
