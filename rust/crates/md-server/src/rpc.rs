//! RPC dispatch.
//!
//! Every channel routes through [`handler_for`]. That one function is the only
//! place a channel is bound to an implementation, so port coverage is a query
//! over it rather than a spreadsheet: `cargo test -p md-server coverage` prints
//! exactly what is left.

use axum::extract::{Path, State};
use axum::Json;
use md_contract::{ErrorCode, Rpc, RpcResponse};
use serde_json::Value;

use crate::auth::Tenant;
use crate::handlers;
use crate::state::AppState;

/// The operations this build actually implements.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    AppInfo,
    ConfigGet,
    ConfigUpdate,
    FsListDir,
    FsStatAbs,
    FsReadFile,
    FsWriteFile,
    PtySpawn,
    PtyWrite,
    PtyResize,
    PtyKill,
    PtyList,
    GitIsRepo,
    GitMainRepo,
    GitBranch,
    GitStatus,
    GitLog,
    GitLogGraph,
    GitBranches,
    GitAheadBehind,
    GitDiff,
    GitCommitFiles,
    GitShowFile,
    GitCompareRefs,
    GitWorktrees,
    GitCheckout,
    HiveRegistry,
    HiveBoard,
    HiveTasks,
    HiveAddTask,
    HivePatchTask,
    HiveDeleteTask,
    HiveLog,
    HiveMemory,
    HiveInbox,
    HiveRenameAgent,
    HivePatchAgentRole,
    HiveSetArchived,
    HiveSetAgentHold,
    HiveSend,
    ControlPause,
    ControlResume,
    ControlHalt,
    ControlSteer,
    ControlSnapshot,
    ControlAutoDelivery,
    ControlGateTool,
}

/// The single channel-to-implementation mapping. `None` means "not ported yet",
/// which is deliberately distinct from "no such channel": the client can tell a
/// typo from a gap, and so can the coverage report.
pub const fn handler_for(rpc: Rpc) -> Option<Op> {
    Some(match rpc {
        Rpc::AppInfo => Op::AppInfo,
        Rpc::ConfigGet => Op::ConfigGet,
        Rpc::ConfigUpdate => Op::ConfigUpdate,
        Rpc::FsListDir => Op::FsListDir,
        Rpc::FsStatAbs => Op::FsStatAbs,
        Rpc::FsReadFile => Op::FsReadFile,
        Rpc::FsWriteFile => Op::FsWriteFile,
        Rpc::PtySpawn => Op::PtySpawn,
        Rpc::PtyWrite => Op::PtyWrite,
        Rpc::PtyResize => Op::PtyResize,
        Rpc::PtyKill => Op::PtyKill,
        Rpc::PtyList => Op::PtyList,
        Rpc::GitIsRepo => Op::GitIsRepo,
        Rpc::GitMainRepo => Op::GitMainRepo,
        Rpc::GitBranch => Op::GitBranch,
        Rpc::GitStatus => Op::GitStatus,
        Rpc::GitLog => Op::GitLog,
        Rpc::GitLogGraph => Op::GitLogGraph,
        Rpc::GitBranches => Op::GitBranches,
        Rpc::GitAheadBehind => Op::GitAheadBehind,
        Rpc::GitDiff => Op::GitDiff,
        Rpc::GitCommitFiles => Op::GitCommitFiles,
        Rpc::GitShowFile => Op::GitShowFile,
        Rpc::GitCompareRefs => Op::GitCompareRefs,
        Rpc::GitWorktrees => Op::GitWorktrees,
        Rpc::GitCheckout => Op::GitCheckout,
        Rpc::HiveRegistry => Op::HiveRegistry,
        Rpc::HiveBoard => Op::HiveBoard,
        Rpc::HiveTasks => Op::HiveTasks,
        Rpc::HiveAddTask => Op::HiveAddTask,
        Rpc::HivePatchTask => Op::HivePatchTask,
        Rpc::HiveDeleteTask => Op::HiveDeleteTask,
        Rpc::HiveLog => Op::HiveLog,
        Rpc::HiveMemory => Op::HiveMemory,
        Rpc::HiveInbox => Op::HiveInbox,
        Rpc::HiveRenameAgent => Op::HiveRenameAgent,
        Rpc::HivePatchAgentRole => Op::HivePatchAgentRole,
        Rpc::HiveSetArchived => Op::HiveSetArchived,
        Rpc::HiveSetAgentHold => Op::HiveSetAgentHold,
        Rpc::HiveSend => Op::HiveSend,
        Rpc::ControlPause => Op::ControlPause,
        Rpc::ControlResume => Op::ControlResume,
        Rpc::ControlHalt => Op::ControlHalt,
        Rpc::ControlSteer => Op::ControlSteer,
        Rpc::ControlSnapshot => Op::ControlSnapshot,
        Rpc::ControlAutoDelivery => Op::ControlAutoDelivery,
        Rpc::ControlGateTool => Op::ControlGateTool,
        _ => return None,
    })
}

/// Channels with no handler yet — the remaining port work, in one list.
pub fn unported() -> Vec<Rpc> {
    Rpc::ALL.iter().copied().filter(|r| handler_for(*r).is_none()).collect()
}

pub async fn rpc_handler(
    Path(channel): Path<String>,
    Tenant(tenant): Tenant,
    State(state): State<AppState>,
    Json(args): Json<Vec<Value>>,
) -> Json<RpcResponse> {
    let Some(rpc) = Rpc::parse(&channel) else {
        return Json(RpcResponse::err(
            ErrorCode::UnknownChannel,
            format!("no such channel: {channel}"),
        ));
    };

    let Some(op) = handler_for(rpc) else {
        return Json(RpcResponse::err(
            ErrorCode::NotImplemented,
            format!("{channel} is not ported yet (bridge method `{}`)", rpc.bridge_method()),
        ));
    };

    let Some(paths) = state.paths(&tenant).cloned() else {
        // Authenticated against a tenant with no provisioned home. A
        // configuration fault, not a client error, so it is not `Forbidden`.
        return Json(RpcResponse::err(
            ErrorCode::Internal,
            format!("tenant {tenant} has no provisioned home"),
        ));
    };

    let ctx = handlers::Ctx { state: state.clone(), tenant, paths, args };
    Json(handlers::run(op, ctx).await)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fails loudly when a handler is removed, and prints the remaining work.
    /// The expected number is updated deliberately as planes land, so a
    /// regression cannot hide behind a passing test.
    #[test]
    fn port_coverage() {
        let total = Rpc::ALL.len();
        let done = total - unported().len();
        println!("ported {done}/{total} RPC channels");
        for r in unported() {
            println!("  todo {} ({})", r.as_str(), r.bridge_method());
        }
        assert_eq!(done, 47, "handler count changed; update this number intentionally");
    }

    /// A channel must never map to two operations, and every op must be
    /// reachable — an unreachable op is dead code pretending to be coverage.
    #[test]
    fn every_op_is_reachable_from_some_channel() {
        let ops: Vec<Op> = Rpc::ALL.iter().filter_map(|r| handler_for(*r)).collect();
        let all = [
            Op::AppInfo, Op::ConfigGet, Op::ConfigUpdate, Op::FsListDir, Op::FsStatAbs,
            Op::FsReadFile, Op::FsWriteFile, Op::PtySpawn, Op::PtyWrite, Op::PtyResize,
            Op::PtyKill, Op::PtyList,
            Op::GitIsRepo, Op::GitMainRepo, Op::GitBranch, Op::GitStatus, Op::GitLog,
            Op::GitLogGraph, Op::GitBranches, Op::GitAheadBehind, Op::GitDiff,
            Op::GitCommitFiles, Op::GitShowFile, Op::GitCompareRefs, Op::GitWorktrees,
            Op::GitCheckout,
            Op::HiveRegistry, Op::HiveBoard, Op::HiveTasks, Op::HiveAddTask,
            Op::HivePatchTask, Op::HiveDeleteTask, Op::HiveLog, Op::HiveMemory,
            Op::HiveInbox, Op::HiveRenameAgent, Op::HivePatchAgentRole,
            Op::HiveSetArchived, Op::HiveSetAgentHold, Op::HiveSend,
            Op::ControlPause, Op::ControlResume, Op::ControlHalt, Op::ControlSteer,
            Op::ControlSnapshot, Op::ControlAutoDelivery, Op::ControlGateTool,
        ];
        for op in all {
            assert!(ops.contains(&op), "{op:?} is not reachable from any channel");
        }
        assert_eq!(ops.len(), all.len(), "a channel maps to a duplicate op");
    }
}
