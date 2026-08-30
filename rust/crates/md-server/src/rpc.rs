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
    AppStartClosingTime,
    AppCancelClosingTime,
    HiveTextSearch,
    HistoryAdd,
    HistoryList,
    HistorySearch,
    SessionResolveCwd,
    PtyRedraw,
    FsReadBinary,
    ConfigEnsureHome,
    ConfigSetAgentTokenCap,
    AnalyticsMessageSent,
    KgList,
    KgGet,
    KgSearch,
    KgRemove,
    KgStatus,
    KgIngestFiles,
    RosterWrite,
    MemoryReflectNow,
    IntegrationsList,
    IntegrationsTemplates,
    IntegrationsUpsert,
    IntegrationsSetSecret,
    IntegrationsRemove,
    IntegrationsTest,
    ProviderKeySet,
    ProviderKeyHas,
    ProviderKeyClear,
    TriggersGetContext,
    TriggersSetContext,
}

/// What this server intends to do about a channel.
///
/// Without this, `unported()` counts channels that will NEVER be ported —
/// clipboard access, the desktop auto-updater, the app's own window — so the
/// coverage number can never reach 100% and nobody can tell how much of the
/// remainder is real work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Plan {
    /// Implemented here.
    Server(Op),
    /// The browser's job. The server has no part in it, so a client that calls
    /// this has a bug: it should be using the platform API directly.
    Client(&'static str),
    /// Electron-only capability with no meaning for a remote tenant.
    Dropped(&'static str),
    /// Real work, still to do.
    Todo,
}

/// The single channel-to-plan mapping. Everything else — dispatch, the coverage
/// report, the error a client sees — reads this one function.
pub const fn plan(rpc: Rpc) -> Plan {
    use Plan::{Client, Dropped};
    match rpc {
        // ── The browser's job ────────────────────────────────────────────────
        // The async Clipboard API covers these. `readClipboardSync` cannot be
        // ported at all — a web client cannot block on a round trip — but the
        // browser's own paste event carries the text synchronously, which is
        // actually better than what it replaces.
        Rpc::AppCopyToClipboard | Rpc::AppReadClipboard | Rpc::AppReadClipboardSync => {
            Client("use the async Clipboard API; paste events carry text synchronously")
        }
        Rpc::ClipboardSaveImage => Client("write the image with the Clipboard API"),
        Rpc::AppOpenExternal => Client("a plain link opens it"),
        Rpc::AppSetNotifications => Client("the Web Notifications permission is the browser's"),
        // A native picker cannot reach the server's filesystem, and a browser
        // never learns a dropped file's real path. Choosing a server directory
        // needs a server-side browser built on `fs:listDir`; attaching a local
        // file becomes an upload.
        Rpc::DialogChooseFolder => Client("browse the server with fs:listDir"),
        Rpc::DialogAttachFiles => Client("upload the file instead — a browser has no real path"),
        // Opens a native picker and then ingests. The picker half cannot cross a
        // network; the ingest half is `kg:ingestFiles`, which is ported.
        Rpc::KgAddFiles => Client("pick files client-side, then call kg:ingestFiles"),

        // ── No meaning for a remote tenant ───────────────────────────────────
        // There is no window to close, and closing a tab must not stop the
        // tenant's agents. The graceful path is closing time, which is ported.
        Rpc::AppConfirmClose | Rpc::AppCancelClose => {
            Dropped("no window to close; use closing time to wind the floor down")
        }
        Rpc::AppSetLoginItem => Dropped("no desktop app to launch at login"),
        // These act on the SERVER's machine, which is not the tenant's.
        Rpc::FsRevealPath => Dropped("reveal-in-file-manager would act on the server"),
        Rpc::TerminalOpenAtFolder => Dropped("open-in-terminal would act on the server"),
        // electron-updater is gone. The server updates out of band; the client
        // needs at most a "reload, the server changed" signal.
        Rpc::UpdateCheckNow
        | Rpc::UpdateCurrent
        | Rpc::UpdateDownload
        | Rpc::UpdateOpenRelease
        | Rpc::UpdateRestartAndInstall
        | Rpc::UpdateSimulate => Dropped("the server updates out of band"),

        other => match handler_for(other) {
            Some(op) => Plan::Server(op),
            None => Plan::Todo,
        },
    }
}

/// The channel-to-implementation mapping. `None` means "no handler", which
/// `plan` refines into "todo" versus "never".
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
        Rpc::AppStartClosingTime => Op::AppStartClosingTime,
        Rpc::AppCancelClosingTime => Op::AppCancelClosingTime,
        Rpc::HiveTextSearch => Op::HiveTextSearch,
        Rpc::HistoryAdd => Op::HistoryAdd,
        Rpc::HistoryList => Op::HistoryList,
        Rpc::HistorySearch => Op::HistorySearch,
        Rpc::SessionResolveCwd => Op::SessionResolveCwd,
        Rpc::PtyRedraw => Op::PtyRedraw,
        Rpc::FsReadBinary => Op::FsReadBinary,
        Rpc::ConfigEnsureHome => Op::ConfigEnsureHome,
        Rpc::ConfigSetAgentTokenCap => Op::ConfigSetAgentTokenCap,
        Rpc::AnalyticsMessageSent => Op::AnalyticsMessageSent,
        Rpc::KgList => Op::KgList,
        Rpc::KgGet => Op::KgGet,
        Rpc::KgSearch => Op::KgSearch,
        Rpc::KgRemove => Op::KgRemove,
        Rpc::KgStatus => Op::KgStatus,
        Rpc::KgIngestFiles => Op::KgIngestFiles,
        Rpc::RosterWrite => Op::RosterWrite,
        Rpc::MemoryReflectNow => Op::MemoryReflectNow,
        Rpc::IntegrationsList => Op::IntegrationsList,
        Rpc::IntegrationsTemplates => Op::IntegrationsTemplates,
        Rpc::IntegrationsUpsert => Op::IntegrationsUpsert,
        Rpc::IntegrationsSetSecret => Op::IntegrationsSetSecret,
        Rpc::IntegrationsRemove => Op::IntegrationsRemove,
        Rpc::IntegrationsTest => Op::IntegrationsTest,
        Rpc::ProviderKeySet => Op::ProviderKeySet,
        Rpc::ProviderKeyHas => Op::ProviderKeyHas,
        Rpc::ProviderKeyClear => Op::ProviderKeyClear,
        Rpc::TriggersGetContext => Op::TriggersGetContext,
        Rpc::TriggersSetContext => Op::TriggersSetContext,
        _ => return None,
    })
}

/// Channels that are real remaining work. Excludes the ones that will never be
/// ported, so this number can actually reach zero.
pub fn unported() -> Vec<Rpc> {
    Rpc::ALL.iter().copied().filter(|r| plan(*r) == Plan::Todo).collect()
}

/// Channels deliberately not ported, with the reason. Reported by `/api/health`
/// so the remainder is legible without reading this file.
pub fn not_applicable() -> Vec<(Rpc, &'static str)> {
    Rpc::ALL
        .iter()
        .copied()
        .filter_map(|r| match plan(r) {
            Plan::Client(why) | Plan::Dropped(why) => Some((r, why)),
            _ => None,
        })
        .collect()
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

    let op = match plan(rpc) {
        Plan::Server(op) => op,
        Plan::Todo => {
            return Json(RpcResponse::err(
                ErrorCode::NotImplemented,
                format!("{channel} is not ported yet (bridge method `{}`)", rpc.bridge_method()),
            ))
        }
        // A client calling one of these has a bug — the answer tells it what to
        // do instead, rather than looking like a gap that will close later.
        Plan::Client(why) => {
            return Json(RpcResponse::err(
                ErrorCode::NotApplicable,
                format!("{channel} is the client's job: {why}"),
            ))
        }
        Plan::Dropped(why) => {
            return Json(RpcResponse::err(
                ErrorCode::NotApplicable,
                format!("{channel} has no server-side meaning: {why}"),
            ))
        }
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
        let na = not_applicable().len();
        let todo = unported().len();
        let done = total - todo - na;
        println!("ported {done}/{total}  |  todo {todo}  |  never {na}");
        for r in unported() {
            println!("  todo {} ({})", r.as_str(), r.bridge_method());
        }
        for (r, why) in not_applicable() {
            println!("  never {} — {}", r.as_str(), why);
        }
        assert_eq!(done, 78, "handler count changed; update this number intentionally");
        assert_eq!(done + todo + na, total, "every channel must have exactly one plan");
    }

    /// A channel classified as never-ported must not also have a handler: that
    /// would mean the dispatcher refuses a channel this server can actually
    /// serve, and the refusal would be invisible until someone called it.
    #[test]
    fn nothing_is_both_implemented_and_written_off() {
        for (r, _) in not_applicable() {
            assert!(
                handler_for(r).is_none(),
                "{} has a handler but is classified as not-applicable",
                r.as_str()
            );
        }
    }

    /// The reasons are shown to clients, so an empty one is a broken message.
    #[test]
    fn every_written_off_channel_says_why() {
        for (r, why) in not_applicable() {
            assert!(!why.is_empty(), "{} has no reason", r.as_str());
        }
        assert!(!not_applicable().is_empty(), "the classification is wired up");
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
            Op::AppStartClosingTime, Op::AppCancelClosingTime,
            Op::HiveTextSearch, Op::HistoryAdd, Op::HistoryList, Op::HistorySearch, Op::SessionResolveCwd, Op::PtyRedraw, Op::FsReadBinary, Op::ConfigEnsureHome, Op::ConfigSetAgentTokenCap, Op::AnalyticsMessageSent,
            Op::KgList, Op::KgGet, Op::KgSearch, Op::KgRemove, Op::KgStatus, Op::KgIngestFiles, Op::RosterWrite, Op::MemoryReflectNow,
            Op::IntegrationsList, Op::IntegrationsTemplates, Op::IntegrationsUpsert, Op::IntegrationsSetSecret, Op::IntegrationsRemove, Op::IntegrationsTest, Op::ProviderKeySet, Op::ProviderKeyHas, Op::ProviderKeyClear, Op::TriggersGetContext, Op::TriggersSetContext,
        ];
        for op in all {
            assert!(ops.contains(&op), "{op:?} is not reachable from any channel");
        }
        assert_eq!(ops.len(), all.len(), "a channel maps to a duplicate op");
    }
}
