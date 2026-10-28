// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! API 服务模块（应用层）
//!
//! HTTP API + 认证 + 会话 HTTP 路由
//!
//! H6 架构合规整改：从 evorule-governance/src/api/ 迁移到应用层。
//! SessionManager 本身保留在 evorule_governance_host::session（机制层），
//! 此处仅包含 HTTP 路由处理（应用层策略）。

pub mod app_quota;
pub mod audit_archive;
pub mod bundles;
pub mod caller_role_resolver;
pub mod hit_stats;
pub mod io_contract;
pub mod io_guard;
pub mod knowledge;
pub mod marketplace;
pub mod openapi;
pub mod pdf_export;
pub mod permissions;
pub mod platform_auth;
pub mod plugin_packs;
pub mod plugin_probe;
pub mod route_policy_loader;
pub mod server;
pub mod template_render;
pub mod writeback_forward;

// H6: main.rs 直接从 `api::server::{...}` 导入所需类型,
// 此处不再 `pub use` 重导出（mod api 是 private 的,外部 crate 无法访问）。
// 如未来需要从 crate root 重导出,改为 `pub use server::{...};` 即可。

/// （仅测试）sandbox:-only 证据闸公共夹具：seed 真实沙盒引用供 import
/// 正路径测试通过深度校验（存在性 + closed + 报告 failed==0）。
///
/// 可见性注意：mod 不带 pub(crate)——私有 mod 对父模块（api）及其后代
/// （knowledge/bundles/server 测试模块）天然可见；build.rs 编译期门禁的
/// strip_test_mod 仅识别 `#[cfg(test)]` 紧跟 `mod` 的形态（带可见性修饰
/// 会导致模块体被当生产代码扫描，unwrap 即拦）。
#[cfg(test)]
mod evidence_fixture {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;

    /// 进程内 seed 序号（报告文件名唯一化，多测试并行互不覆盖）
    static SEED_SEQ: AtomicU64 = AtomicU64::new(0);

    /// 在给定 workspace 元数据库中 seed 一个 closed+PASS 沙盒会话并落盘
    /// 报告文件，返回沙盒 ID。in-memory 库自增 ID 从 1 起，空库首个 seed
    /// 即得 `sandbox:1`，与测试夹具包的引用约定一致。
    ///
    /// 报告落盘与 close_sandbox 生产落盘同目录（仓库忽略区
    /// ./data/sandbox_reports），文件名带时间戳+序号+pid 保证唯一。
    pub(super) fn seed_pass_sandbox(ws_db: &Arc<evorule_workspace::WorkspaceDb>) -> i64 {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let seq = SEED_SEQ.fetch_add(1, Ordering::Relaxed);
        let now_dt = ws_db.get_production_state().unwrap().updated_at;
        let ws_id = format!("ws-evidence-fixture-{ts}-{seq}-{}", std::process::id());
        ws_db
            .insert_workspace(&evorule_workspace::models::WorkspaceRecord {
                id: ws_id.clone(),
                name: "evidence-fixture".into(),
                owner_id: "test:evidence-fixture".into(),
                created_at: now_dt,
                updated_at: now_dt,
                archived_at: None,
                state: evorule_workspace::models::WorkspaceState::Active,
                description: None,
            })
            .unwrap();
        let report_dir = evorule_workspace::sandbox_report_dir()
            .to_string_lossy()
            .to_string();
        std::fs::create_dir_all(&report_dir).unwrap();
        let facts = format!(
            "audit_evidence_fixture_{ts}_{seq}_{}.json",
            std::process::id()
        );
        let sid = ws_db
            .insert_sandbox_session(
                None,
                &ws_id,
                1,
                Some("hash-evidence-fixture"),
                1,
                "test:evidence-fixture",
            )
            .unwrap();
        // 关闭态 + export_path 指向报告目录；报告文件与 generate_test_report
        // 落盘同构（report_<facts basename>.json），failed=0 使引用可作 pass 证据
        let export_path = format!("{report_dir}/{facts}");
        ws_db.close_sandbox_session(sid, &export_path).unwrap();
        std::fs::write(
            format!("{report_dir}/report_{facts}"),
            r#"{"summary": {"total_cases": 1, "passed": 1, "failed": 0, "skipped": 0}}"#,
        )
        .unwrap();
        sid
    }
}
