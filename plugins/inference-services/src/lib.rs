// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 推理分析原生服务（第一个确定性静态分析插件：量纲检查）。
//!
//! # 定位
//! - 对 SI 单位与物理公式做**确定性静态量纲分析**——纯计算、零 I/O、
//!   零状态、无墙钟，进程内原生执行（`io_request(call_service)`）。
//! - 独立 crate（`plugins/inference-services`），不修改核心 crate，
//!   仅经 `IoDispatcher` 挂载（机制同 physics-services 先例）。
//! - 算法对照上游参考实现逐函数移植，测试预言（期望值）同源。
//!
//! # 路由设计
//! - `InferenceServiceRouter` 实现 [`IoHandler`]，按 `params.service_name` 分发：
//!   - 命中原生服务名 → 调用对应原生实现（进程内，确定性）
//!   - 未命中 → 回落 `ServiceRegistryHandler`（HTTP，兼容其他外部服务）
//! - 原生实现接收的入参 = `params.args`。
//!
//! # 统一信封 v1（科学计算服务面规范）
//! - 全部 14 服务成功出参含保留键 `status` + `determinism`（bitwise_replayable：
//!   纯整数/逻辑域，同二进制同输入双跑位元一致）；`status` 恒 `determined`，
//!   唯一例外 = 收敛检测（既有 `undetermined` 字段与 `status` 恒等约束，
//!   undetermined=true 时携带 `reason`="unresolvable_by_static_analysis"）；
//! - 既有字段（verdict/note/reason/converges/undetermined 等）全部原位保留；
//!   error 维持框架 `Err(String)` 通道。
//!
//! # 服务清单（SSOT = official_native_services.json，双侧守卫锁定）
//! 1. `inference_dimension_from_unit` — SI 单位字符串 → 七分量量纲向量
//! 2. `inference_dimension_compare_units` — 两单位量纲等价性比较
//! 3. `inference_dimension_verify_formula` — 物理公式量纲一致性验证
//!    （1-3 = 阶段 1 量纲分析）
//! 4. `inference_rw_extract` — 规则读写集静态提取（阶段 2 共享底座）
//! 5. `inference_cycle_detect` — 规则集内环路检测（静态/保守分层）
//! 6. `inference_conflict_detect` — 规则集内冲突检测（六类型学）
//!    （4-6 = 规则静态分析批次；详见仓内设计文档：
//!    v4 仅算法底稿+测试预言，模型层零照搬）
//!
//! 推理服务批次（详见仓内设计文档；
//! sensitivity/causal/information 按裁定不移植）：`inference_forward_simulate`
//! （前向链仿真，合成文档迭代到不动点，enforce 前置门对齐，io 诚实断流）、
//! `inference_backward_plan`（后向链规划，目标反推生产者链，复用 rw_extract）、
//! `inference_effect_predict`（效果预测，rw_extract 之上影响分级薄层）、
//! `inference_convergence_check`（收敛检测，环复用 cycle_detect）。
//!
//! 差分等价反事实批次（详见仓内设计文档；
//! combination/six_way 聚合壳按裁定不移植）：`inference_counterfactual_analyze`
//! （反事实差异分析，双前向仿真 + 终态可观测面 diff，控制面单列）、
//! `inference_equivalence_probe`（等价探针，结构比较 + 有限探针行为证据，
//! probe_equivalent 为证据语义非数学证明）、`inference_differential_diff`
//! （规则集差分，读写集结构化 diff，保守方向声明）。

#![forbid(unsafe_code)]

pub mod backward;
pub mod conflict;
pub mod convergence;
pub mod counterfactual;
pub mod cycle;
pub mod differential;
pub mod dimension;
pub mod effect;
pub mod equivalence;
pub mod forward_sim;
pub mod rw_extract;
pub mod services;
pub mod sweep;

use std::sync::Arc;

use async_trait::async_trait;
use evorule_reactor::{IoHandler, IoResult};
use evorule_tcb::JsonValue;

// 机制件(trait/声明项/过滤路由器)在 plugin-kit,本 crate 自持声明表+薄壳具名路由器
pub use evorule_plugin_kit::{NativeService, NativeServiceDef};

use crate::services::{
    mk_backward_plan, mk_compare, mk_conflict_detect, mk_convergence_check,
    mk_counterfactual_analyze, mk_counterfactual_sweep, mk_cycle_detect, mk_differential_diff,
    mk_effect_predict, mk_equivalence_probe, mk_forward_simulate, mk_from_unit, mk_rw_extract,
    mk_verify_formula,
};

// ============================================================================
// 声明表（SSOT 配对文件: official_native_services.json; 顺序即声明序,
// name/sensitive/description 三字段与顺序均被双侧守卫锁定）
// ============================================================================

/// 原生服务声明表（新增原生能力 = 本表追加一项 + SSOT 文件追加一项 +
/// 运行 sync 脚本同步治理侧嵌入副本）
pub const NATIVE_SERVICES: &[NativeServiceDef] = &[
    NativeServiceDef {
        name: "inference_dimension_from_unit",
        sensitive: false,
        description: "SI 单位字符串解析为七分量量纲向量([L,M,T,I,Θ,N,J],确定性)",
        parameters: Some(r#"{"type":"object","properties":{"units":{"type":"array","items":{"type":"string"},"description":"SI 单位字符串数组,每项一个单位,如 m/s、kg·m/s²"}},"required":["units"]}"#),
        make: mk_from_unit,
    },
    NativeServiceDef {
        name: "inference_dimension_compare_units",
        sensitive: false,
        description: "两 SI 单位量纲等价性比较(七分量向量逐位相等判定,确定性)",
        parameters: Some(r#"{"type":"object","properties":{"pairs":{"type":"array","items":{"type":"array","items":{"type":"string"},"minItems":2,"maxItems":2},"description":"单位对数组,每项为二元字符串数组 [左单位, 右单位]"}},"required":["pairs"]}"#),
        make: mk_compare,
    },
    NativeServiceDef {
        name: "inference_dimension_verify_formula",
        sensitive: false,
        description: "物理公式量纲一致性验证(已知变量量纲推导四则与幂表达式,确定性)",
        parameters: Some(r#"{"type":"object","properties":{"formulas":{"type":"array","items":{"type":"string"},"description":"物理公式字符串数组,如 F == m*a"},"si_units":{"type":"object","additionalProperties":{"type":"string"},"description":"可选,变量到 SI 单位的映射,键为变量名值为单位字符串;缺省空映射,未知变量如实报无法推导"}},"required":["formulas"]}"#),
        make: mk_verify_formula,
    },
    NativeServiceDef {
        name: "inference_rw_extract",
        sensitive: false,
        description: "规则读写集静态提取(规范化路径+I/O出边+动态标注,纯函数确定性)",
        parameters: Some(r#"{"type":"object","properties":{"rules":{"type":"array","items":{"type":"object"},"description":"规则对象合并列表,按合并序排列(core_eval 在前 + rules_dir 字典序)"},"sources":{"type":"array","items":{"type":"string"},"description":"来源标签数组,与 rules 等长且按序对齐,如 core_eval、server_eval、rules/x.json"},"include_constitution":{"type":"boolean","description":"可选,缺省 false,宪法段豁免"}},"required":["rules","sources"]}"#),
        make: mk_rw_extract,
    },
    NativeServiceDef {
        name: "inference_cycle_detect",
        sensitive: false,
        description: "规则集内环路检测(Kahn拓扑+环路径提取,静态/保守分层,确定性)",
        parameters: Some(r#"{"type":"object","properties":{"rules":{"type":"array","items":{"type":"object"},"description":"规则对象合并列表,按合并序排列(core_eval 在前 + rules_dir 字典序)"},"sources":{"type":"array","items":{"type":"string"},"description":"来源标签数组,与 rules 等长且按序对齐,如 core_eval、server_eval、rules/x.json"},"include_constitution":{"type":"boolean","description":"可选,缺省 false,宪法段豁免"}},"required":["rules","sources"]}"#),
        make: mk_cycle_detect,
    },
    NativeServiceDef {
        name: "inference_conflict_detect",
        sensitive: false,
        description:
            "规则集内冲突检测(六类型学:常量矛盾/顺序遮蔽/写竞争/互补域/循环写/IO串台,只报告不拦截)",
        parameters: Some(r#"{"type":"object","properties":{"rules":{"type":"array","items":{"type":"object"},"description":"规则对象合并列表,按合并序排列(core_eval 在前 + rules_dir 字典序)"},"sources":{"type":"array","items":{"type":"string"},"description":"来源标签数组,与 rules 等长且按序对齐,如 core_eval、server_eval、rules/x.json"},"include_constitution":{"type":"boolean","description":"可选,缺省 false,宪法段豁免"}},"required":["rules","sources"]}"#),
        make: mk_conflict_detect,
    },
    NativeServiceDef {
        name: "inference_forward_simulate",
        sensitive: false,
        description: "前向链仿真(合成__exec__文档迭代到不动点,enforce前置门对齐,io诚实断流,确定性)",
        parameters: Some(r#"{"type":"object","properties":{"rules":{"type":"array","items":{"type":"object"},"description":"规则对象合并列表,按合并序排列(core_eval 在前 + rules_dir 字典序)"},"sources":{"type":"array","items":{"type":"string"},"description":"来源标签数组,与 rules 等长且按序对齐"},"include_constitution":{"type":"boolean","description":"可选,缺省 false,宪法段豁免"},"initial_document":{"type":"object","description":"合成 __exec__ 初始文档,形如 {instruction:{...},payload:{...},queue:[...]}"},"max_steps":{"type":"integer","description":"可选,仿真最大步数,非负整数,超硬上限报错"}},"required":["rules","sources","initial_document"]}"#),
        make: mk_forward_simulate,
    },
    NativeServiceDef {
        name: "inference_backward_plan",
        sensitive: false,
        description: "后向链规划(目标路径反推生产者规则链,复用读写集提取,静态可达性,确定性)",
        parameters: Some(r#"{"type":"object","properties":{"rules":{"type":"array","items":{"type":"object"},"description":"规则对象合并列表,按合并序排列(core_eval 在前 + rules_dir 字典序)"},"sources":{"type":"array","items":{"type":"string"},"description":"来源标签数组,与 rules 等长且按序对齐"},"include_constitution":{"type":"boolean","description":"可选,缺省 false,宪法段豁免"},"target_path":{"type":"string","description":"目标写路径,规范化点分形态,如 payload.x"},"max_depth":{"type":"integer","description":"可选,最大反推深度,非负整数"},"available_paths":{"type":"array","items":{"type":"string"},"description":"可选,初始可用路径集字符串数组"}},"required":["rules","sources","target_path"]}"#),
        make: mk_backward_plan,
    },
    NativeServiceDef {
        name: "inference_effect_predict",
        sensitive: false,
        description: "规则效果预测(影响分级+动态/IO诚实警告,基于读写集提取薄层,确定性)",
        parameters: Some(r#"{"type":"object","properties":{"rules":{"type":"array","items":{"type":"object"},"description":"规则对象合并列表,按合并序排列(core_eval 在前 + rules_dir 字典序)"},"sources":{"type":"array","items":{"type":"string"},"description":"来源标签数组,与 rules 等长且按序对齐,如 core_eval、server_eval、rules/x.json"},"include_constitution":{"type":"boolean","description":"可选,缺省 false,宪法段豁免"}},"required":["rules","sources"]}"#),
        make: mk_effect_predict,
    },
    NativeServiceDef {
        name: "inference_convergence_check",
        sensitive: false,
        description: "收敛检测(自修改/环复用/步数上界,静态可判定子集,undetermined显式,确定性)",
        parameters: Some(r#"{"type":"object","properties":{"rules":{"type":"array","items":{"type":"object"},"description":"规则对象合并列表,按合并序排列(core_eval 在前 + rules_dir 字典序)"},"sources":{"type":"array","items":{"type":"string"},"description":"来源标签数组,与 rules 等长且按序对齐,如 core_eval、server_eval、rules/x.json"},"include_constitution":{"type":"boolean","description":"可选,缺省 false,宪法段豁免"}},"required":["rules","sources"]}"#),
        make: mk_convergence_check,
    },
    NativeServiceDef {
        name: "inference_counterfactual_analyze",
        sensitive: false,
        description: "反事实差异分析(双前向仿真+终态可观测面diff,控制面单列,确定性解释,确定性)",
        parameters: Some(r#"{"type":"object","properties":{"rules":{"type":"array","items":{"type":"object"},"description":"规则对象合并列表,按合并序排列(core_eval 在前 + rules_dir 字典序)"},"sources":{"type":"array","items":{"type":"string"},"description":"来源标签数组,与 rules 等长且按序对齐"},"include_constitution":{"type":"boolean","description":"可选,缺省 false,宪法段豁免"},"original_document":{"type":"object","description":"原场景合成 __exec__ 文档"},"hypothetical_document":{"type":"object","description":"假设场景合成 __exec__ 文档"},"max_steps":{"type":"integer","description":"可选,仿真最大步数,非负整数"}},"required":["rules","sources","original_document","hypothetical_document"]}"#),
        make: mk_counterfactual_analyze,
    },
    NativeServiceDef {
        name: "inference_equivalence_probe",
        sensitive: false,
        description:
            "等价探针(结构比较+有限探针行为证据,probe_equivalent为证据语义非数学证明,确定性)",
        parameters: Some(r#"{"type":"object","properties":{"rules1":{"type":"array","items":{"type":"object"},"description":"第一组规则对象列表"},"sources1":{"type":"array","items":{"type":"string"},"description":"第一组来源标签数组,与 rules1 等长且按序对齐"},"rules2":{"type":"array","items":{"type":"object"},"description":"第二组规则对象列表"},"sources2":{"type":"array","items":{"type":"string"},"description":"第二组来源标签数组,与 rules2 等长且按序对齐"},"probe_inputs":{"type":"array","description":"可选,合成 __exec__ 探针输入文档数组,缺省空(仅做结构比较);本服务不接受 max_steps"}},"required":["rules1","sources1","rules2","sources2"]}"#),
        make: mk_equivalence_probe,
    },
    NativeServiceDef {
        name: "inference_differential_diff",
        sensitive: false,
        description: "规则集差分(读写集结构化diff,写异必不同/读异可能不同的保守方向,确定性)",
        parameters: Some(r#"{"type":"object","properties":{"rules1":{"type":"array","items":{"type":"object"},"description":"第一组规则对象列表"},"sources1":{"type":"array","items":{"type":"string"},"description":"第一组来源标签数组,与 rules1 等长且按序对齐"},"rules2":{"type":"array","items":{"type":"object"},"description":"第二组规则对象列表"},"sources2":{"type":"array","items":{"type":"string"},"description":"第二组来源标签数组,与 rules2 等长且按序对齐"},"include_constitution":{"type":"boolean","description":"可选,缺省 false,宪法段豁免"}},"required":["rules1","sources1","rules2","sources2"]}"#),
        make: mk_differential_diff,
    },
    NativeServiceDef {
        name: "inference_counterfactual_sweep",
        sensitive: false,
        description: "反事实参数扫描(参数路径逐值双仿真,每点差异面+汇总,复用反事实语义,确定性)",
        parameters: Some(r#"{"type":"object","properties":{"rules":{"type":"array","items":{"type":"object"},"description":"规则对象合并列表,按合并序排列(core_eval 在前 + rules_dir 字典序)"},"sources":{"type":"array","items":{"type":"string"},"description":"来源标签数组,与 rules 等长且按序对齐"},"include_constitution":{"type":"boolean","description":"可选,缺省 false,宪法段豁免"},"document":{"type":"object","description":"合成 __exec__ 基线文档"},"param_path":{"type":"string","description":"扫描的参数点路径,点分形态,如 payload.threshold"},"values":{"type":"array","description":"扫描值序列,由调用方显式给出(数字/字符串皆可,原样拼入文档零类型强转),如 [1, 5, 10]"},"max_steps":{"type":"integer","description":"可选,每点仿真最大步数,非负整数"}},"required":["rules","sources","document","param_path","values"]}"#),
        make: mk_counterfactual_sweep,
    },
];

// ============================================================================
// 复合路由：原生优先，HTTP 回落（机制件在 plugin-kit，本 crate 薄壳委托）
// ============================================================================

/// 复合 IoHandler：`service_name` 命中原生服务名 → 原生执行；否则回落 HTTP。
///
/// 薄壳具名路由器：机制(new / with_enabled 三拒绝 / 声明序查找 / 回落 /
/// split_params)已上提 plugin-kit(行为逐字节等价)，本插件仅自持声明表并保持具名 API。
pub struct InferenceServiceRouter(evorule_plugin_kit::NativeServiceRouter);

impl InferenceServiceRouter {
    /// 构造复合路由。`fallback` 为未命中原生服务名时的 HTTP 处理器
    /// （通常传 `Arc<ServiceRegistryHandler>`，由调用方按 --allow-loopback 构造）。
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
impl IoHandler for InferenceServiceRouter {
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
        for d in NATIVE_SERVICES {
            let _ = (d.make)();
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
        let router = InferenceServiceRouter::new(Arc::new(SentinelHandler));
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

    #[test]
    fn test_native_service_table_matches_declaration_file() -> Result<(), String> {
        // 同步守卫:本插件声明文件 official_native_services.json 为 SSOT。
        // 本表(name/sensitive/description)与文件三字段+顺序全量比对——
        // 新增服务 = 改文件 + 本表追加 make 项,漂移即失败(不静默)。
        let raw = include_str!("../official_native_services.json");
        let file: serde_json::Value =
            serde_json::from_str(raw).map_err(|e| format!("声明文件非法 JSON: {e}"))?;
        let services = file
            .get("services")
            .and_then(|v| v.as_array())
            .ok_or("声明文件缺 services 数组")?;
        if services.len() != NATIVE_SERVICES.len() {
            return Err(format!(
                "声明文件 {} 项 vs 声明表 {} 项 — 数量不一致",
                services.len(),
                NATIVE_SERVICES.len()
            ));
        }
        for (i, (f, d)) in services.iter().zip(NATIVE_SERVICES).enumerate() {
            let name = f.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let sensitive = f
                .get("sensitive")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let description = f.get("description").and_then(|v| v.as_str()).unwrap_or("");
            if name != d.name {
                return Err(format!("第 {i} 项 name 漂移: 文件={name} 表={}", d.name));
            }
            if sensitive != d.sensitive {
                return Err(format!("服务 {name} sensitive 漂移"));
            }
            if description != d.description {
                return Err(format!("服务 {name} description 漂移"));
            }
        }
        Ok(())
    }
}
