// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 事件流审计原生服务（目标保真度/行为熵/审批密度/帧对账/分代切分/时代混杂哨兵/范围外写入）。
//!
//! # 定位
//! - run 事件流（CLI `run --json` / WS 帧同构面 / journal NDJSON 行）的
//!   确定性判据计算：goal 关键词覆盖率（M1 面）、行为熵序列与半程分布散度
//!   （M2 面）、审批密度（M3 面）、帧对账未闭合清单（M3 对账面）、分代
//!   切分索引与时代混杂哨兵（混档判读无效化门）、写类路径范围外检出
//!   （M4 面）。判据公式与离线分析器工具同源（同输入同输出
//!   对账口径），服务面供治理侧规则与宿主工具直接调用；
//! - 独立 crate，仅经 `IoDispatcher` 挂载；纯计算，无浮点出参
//!   （小数一律 4 位定点字符串）、无时钟、无随机；同输入双跑位元一致。
//!
//! # 路由设计
//! - `TraceAuditServiceRouter` 实现 [`IoHandler`]，按 `params.service_name` 分发：
//!   - 命中原生服务名 → 调用对应原生实现（进程内，确定性）
//!   - 未命中 → 回落 `ServiceRegistryHandler`（HTTP，兼容其他外部服务）
//!
//! # 统一信封 v1
//! - 全部 7 服务成功出参含保留键 `status`（恒 `determined`）+
//!   `determinism`（恒 `bitwise_replayable`）；无 scale/rounding 入参——
//!   舍入语义内建固定（4 位小数、与判据正本 round(x,4) 语义一致），
//!   契约按适用条件诚实省略 scale 入参；error 走框架 `Err(String)`
//!   通道并带自诊断指引。
//!
//! # 诚实边界
//! - 输入事件为任意字段对象，判据只读取约定字段（type/name/approved/
//!   auto_rejected/error/ts/purpose/model/seq/链哈希三键/payload 同名键）；
//!   字段按字符串口径读取，非字符串数值字段按缺省口径处理（如 name
//!   缺失计 "?"、error 非字符串计 null），与分析器工具的字段口径一致；
//! - 熵/散度求和序 = 事件首现序（与判据正本 Counter 迭代序一致）；散度
//!   并集键序 = 前半程首现序接后半程新键，与正本的集合并序在求和末位
//!   理论上可有 1ULP 差，4 位定点舍入后逐值一致（对账样本验证）；
//! - 事件数组上限 200000、窗宽上限 10000（资源守卫，超限 fail-visible
//!   拒解，不静默降级）；
//! - 分代切分只读零改写：返回索引与摘要，不产出切片文件（落盘归宿主）。
//!
//! # 服务清单（SSOT = official_native_services.json，双侧守卫锁定）
//! 1. `trace_behavior_entropy` — 行为熵窗口序列 + 前后半程 JS 散度
//! 2. `trace_approval_density` — 审批密度/结论分布/流级错误
//! 3. `trace_era_split` — 分代切分索引（J1∨J2 边界，J3 旁证）
//! 4. `trace_era_mixing_sentinel` — 时代混杂哨兵（ts 跨代 + 模型混桶）
//! 5. `trace_out_of_scope_writes` — 写类工具路径集合 + 范围外写入检出（M4 面）
//! 6. `trace_frame_pairing` — 帧对账未闭合四清单（M3 对账面）
//! 7. `trace_target_fidelity` — goal 关键词覆盖率（M1 面）

#![forbid(unsafe_code)]

use std::sync::Arc;

use async_trait::async_trait;
use evorule_plugin_kit::NativeServiceDef;
use evorule_reactor::{IoHandler, IoResult};
use evorule_tcb::JsonValue;

pub mod criteria;
pub mod era;
pub mod scope;
pub mod services;

use services::{
    mk_approval_density, mk_behavior_entropy, mk_era_mixing_sentinel, mk_era_split,
    mk_frame_pairing, mk_out_of_scope_writes, mk_target_fidelity,
};

// ============================================================================
// 声明表（SSOT 配对文件: official_native_services.json; 顺序即声明序,
// name/sensitive/description 三字段与顺序均被双侧守卫锁定）
// ============================================================================

/// 原生服务声明表（新增原生能力 = 本表追加一项 + SSOT 文件追加一项 +
/// 运行 sync 脚本同步治理侧嵌入副本）
pub const NATIVE_SERVICES: &[NativeServiceDef] = &[
    NativeServiceDef {
        name: "trace_behavior_entropy",
        sensitive: false,
        description: "run 事件流行为熵判据(ToolCall 到达序滑动窗口工具名香农熵 H=-Σp·log₂p,缺省窗宽10步长窗半;前后半程工具分布 JS 散度;熵与散度按 4 位小数定点字符串输出,确定性)",
        parameters: Some(
            r#"{"type":"object","properties":{"events":{"type":"array","items":{"type":"object"},"maxItems":200000,"description":"NDJSON 事件对象数组(CLI run --json / WS 帧同构面/journal 行皆可)"},"window":{"type":"integer","minimum":1,"maximum":10000,"description":"可选滑动窗宽,缺省 10"}},"required":["events"]}"#,
        ),
        make: mk_behavior_entropy,
    },
    NativeServiceDef {
        name: "trace_approval_density",
        sensitive: false,
        description: "run 事件流审批密度判据(ApprovalRequired/Result 密度与结论分布、auto_rejected 计数、流级 Error 计数与消息样本,确定性)",
        parameters: Some(
            r#"{"type":"object","properties":{"events":{"type":"array","items":{"type":"object"},"maxItems":200000,"description":"NDJSON 事件对象数组"}},"required":["events"]}"#,
        ),
        make: mk_approval_density,
    },
    NativeServiceDef {
        name: "trace_era_split",
        sensitive: false,
        description: "事件流分代切分索引(三重正交判据:J1 链哈希字段组形态翻转/J2 相邻行 ts 间隔超 24h/J3 react 主循环模型翻转旁证;边界取 J1∨J2 命中点,逐代行号/seq 区间/判据命中快照/摘要,只读零改写输入,确定性)",
        parameters: Some(
            r#"{"type":"object","properties":{"events":{"type":"array","items":{"type":"object"},"maxItems":200000,"description":"NDJSON journal 行对象数组"}},"required":["events"]}"#,
        ),
        make: mk_era_split,
    },
    NativeServiceDef {
        name: "trace_era_mixing_sentinel",
        sensitive: false,
        description: "时代混杂哨兵(检测单账面内多时代事件拼接:相邻 ts 间隔超 24h 与 react 主循环模型混桶两条证据面;混档判读无效化门前置,确定性)",
        parameters: Some(
            r#"{"type":"object","properties":{"events":{"type":"array","items":{"type":"object"},"maxItems":200000,"description":"NDJSON 事件对象数组"}},"required":["events"]}"#,
        ),
        make: mk_era_mixing_sentinel,
    },
    NativeServiceDef {
        name: "trace_out_of_scope_writes",
        sensitive: false,
        description: "写类工具路径集合与范围外写入检出(ToolCall 写类工具的 path/file/target 三键按序取首个非空字符串;scope 根区间判定=词典法零 FS 零 CWD,相对路径经 base_dir 入参解析,缺省=scope 根;写型工具值域可经 write_tools 扩展;归一失败判范围外,确定性)",
        parameters: Some(
            r#"{"type":"object","properties":{"events":{"type":"array","items":{"type":"object"},"maxItems":200000,"description":"NDJSON 事件对象数组(CLI run --json / WS 帧同构面/journal 行皆可)"},"scope":{"type":"string","description":"可选任务范围根目录(绝对路径);缺省不检出只收集写路径"},"base_dir":{"type":"string","description":"可选相对写路径解析基准(绝对路径);缺省=scope 根"},"write_tools":{"type":"array","items":{"type":"string"},"description":"可选写型工具名值域扩展;缺省五件(写型工具名值域可按 agent 定义扩展)"}},"required":["events"]}"#,
        ),
        make: mk_out_of_scope_writes,
    },
    NativeServiceDef {
        name: "trace_frame_pairing",
        sensitive: false,
        description: "run 事件流帧对账判据(ToolCall/ToolResult 与 ApprovalRequired/ApprovalResult 逐帧 FIFO 配对不校验 id 一致:ToolResult 出队否则 result_without_call,ApprovalResult 出队否则 result_without_request;审批帧标识取 proposal_id 真值字符串否则回落 tool_name 缺省 ?;出参未闭合四清单,确定性)",
        parameters: Some(
            r#"{"type":"object","properties":{"events":{"type":"array","items":{"type":"object"},"maxItems":200000,"description":"NDJSON 事件对象数组"}},"required":["events"]}"#,
        ),
        make: mk_frame_pairing,
    },
    NativeServiceDef {
        name: "trace_target_fidelity",
        sensitive: false,
        description: "run 事件流目标保真度判据(逐 LlmDone content 对 goal 关键词覆盖率:关键词集=goal 小写后连续字母/数字/下划线词块去 <=2 字符短词,覆盖率=命中词数/词总数 4 位舍入整值出整数带小数出字符串;空词集出 coverage 空数组与 note 提示,确定性)",
        parameters: Some(
            r#"{"type":"object","properties":{"events":{"type":"array","items":{"type":"object"},"maxItems":200000,"description":"NDJSON 事件对象数组"},"goal":{"type":"string","description":"任务目标文本(关键词集=连续字母/数字/下划线词块,去 <=2 字符短词)"}},"required":["events","goal"]}"#,
        ),
        make: mk_target_fidelity,
    },
];

/// 复合 IoHandler：`service_name` 命中原生服务名 → 原生执行；否则回落 HTTP。
///
/// 薄壳具名路由器：机制已上提 plugin-kit，本插件仅自持声明表并保持具名 API。
pub struct TraceAuditServiceRouter(evorule_plugin_kit::NativeServiceRouter);

impl TraceAuditServiceRouter {
    /// 构造复合路由。`fallback` 为未命中原生服务名时的 HTTP 处理器。
    pub fn new(fallback: Arc<dyn IoHandler>) -> Self {
        Self(evorule_plugin_kit::NativeServiceRouter::new(
            NATIVE_SERVICES,
            fallback,
        ))
    }

    /// 部署期启用子集构造。
    pub fn with_enabled(fallback: Arc<dyn IoHandler>, enabled: &[&str]) -> Result<Self, String> {
        evorule_plugin_kit::NativeServiceRouter::with_enabled(NATIVE_SERVICES, fallback, enabled)
            .map(Self)
    }

    /// 当前实例已启用的原生服务名列表(健康可见性/能力对账用)
    pub fn enabled_service_names(&self) -> Vec<&'static str> {
        self.0.enabled_service_names()
    }

    /// 原生服务名列表（server 侧服务绑定核对/能力对账用；自声明表派生）
    pub fn native_service_names() -> Vec<&'static str> {
        evorule_plugin_kit::NativeServiceRouter::native_service_names(NATIVE_SERVICES)
    }
}

#[async_trait]
impl IoHandler for TraceAuditServiceRouter {
    async fn execute(&self, params: &JsonValue) -> IoResult {
        self.0.execute(params).await
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn test_native_service_table_invariants() {
        // 声明表不变量:服务名唯一;每项构造子可用
        let mut names: Vec<&str> = NATIVE_SERVICES.iter().map(|d| d.name).collect();
        let total = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), total, "声明表服务名必须唯一");
        assert_eq!(total, 7, "本插件声明表应为 7 项");
        for d in NATIVE_SERVICES {
            let _ = (d.make)();
        }
    }

    #[test]
    fn test_declaration_ssot_matches_native_services() {
        // 同步守卫:SSOT 声明文件与本表 name/sensitive/description 三字段+顺序全量比对
        let raw = include_str!("../official_native_services.json");
        let doc: serde_json::Value = serde_json::from_str(raw).expect("SSOT 文件须为合法 JSON");
        let items = doc
            .get("services")
            .and_then(|v| v.as_array())
            .expect("SSOT 须含 services 数组");
        assert_eq!(
            items.len(),
            NATIVE_SERVICES.len(),
            "SSOT 条目数与声明表不一致"
        );
        for (i, (item, def)) in items.iter().zip(NATIVE_SERVICES).enumerate() {
            let name = item.get("name").and_then(|v| v.as_str()).expect("name");
            let sensitive = item
                .get("sensitive")
                .and_then(|v| v.as_bool())
                .expect("sensitive");
            let description = item
                .get("description")
                .and_then(|v| v.as_str())
                .expect("description");
            assert_eq!(name, def.name, "第 {i} 条服务名漂移");
            assert_eq!(sensitive, def.sensitive, "服务 {name} sensitive 漂移");
            assert_eq!(description, def.description, "服务 {name} description 漂移");
        }
    }

    #[test]
    fn test_parameters_contract_parses_and_covers_all() {
        for d in NATIVE_SERVICES {
            let raw = d.parameters.expect("每项声明须带 parameters 契约");
            let parsed: serde_json::Value =
                serde_json::from_str(raw).expect("parameters 须为合法 JSON");
            assert_eq!(
                parsed.get("type").and_then(|v| v.as_str()),
                Some("object"),
                "服务 {} 契约 type 须为 object",
                d.name
            );
        }
    }

    #[tokio::test]
    async fn test_all_native_services_route_natively() {
        // 声明表每一项都必须命中原生执行(而非 HTTP 回落)——
        // fallback 用哨兵报错,任何一项落到 fallback 即失败
        struct SentinelHandler;
        #[async_trait]
        impl IoHandler for SentinelHandler {
            async fn execute(&self, _params: &JsonValue) -> IoResult {
                Err("fallback-called".to_string())
            }
        }
        let router = TraceAuditServiceRouter::new(Arc::new(SentinelHandler));
        for d in NATIVE_SERVICES {
            let params = JsonValue::object_from_pairs(&[
                ("service_name", JsonValue::string(d.name)),
                ("args", JsonValue::object_from_pairs(&[])),
            ]);
            // 空参数下业务结果可成功可失败,但绝不能是 fallback 哨兵
            if let Err(e) = router.execute(&params).await {
                assert_ne!(e, "fallback-called", "服务 {} 落到了 HTTP 回落", d.name);
            }
        }
    }
}
