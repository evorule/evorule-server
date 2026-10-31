//! evorule 几何检查原生服务插件
//!
//! 确定性 2D 几何谓词服务族，供规则治理面消费（AI 生成工件的几何验证：
//! 碰撞 / 间隙 / 遮挡序）。纯计算，无状态、无墙钟、无随机源。
//!
//! # 服务清单（3 项）
//!
//! - `geometry_collision_check`：零件两两相交判定（触即算，多报方向）；
//! - `geometry_clearance_check`：两两最小几何距离 vs 下限（宽容方向容差）；
//! - `geometry_occlusion_check`：重叠零件绘制序 vs 声明 z 层一致性（画家算法）。
//!
//! # 形态与坐标
//!
//! 零件 = `{id, shape, z?}`，绘制序 = 数组下标；shape 三形态
//! `bbox / circle / polygon`，坐标为整数或十进制字符串（f64 承载）。
//!
//! # 适用域（regime 声明，域外显式拒绝）
//!
//! - **2D 平面像素级**：collision / clearance / occlusion 三件套坐标为
//!   `Pt{x,y}`，无几何 z 维度——`z` 字段仅为遮挡序声明层（画家算法逻辑层高），
//!   不是空间坐标；本族不做 3D 实体测量（3D 测量属 CAD spike 另批范围）；
//! - **绝对容差域**：EPS = 1e-9 为绝对值容差，声明语义成立于坐标量级 ≫ EPS
//!   的像素级场景（SVG 图样级）；亚纳米级坐标量级属适用域外（相对容差语义
//!   需另行立法，本族不静默升精度）；
//! - **f64 逐位一致域**：同平台同输入逐位一致（corpus_locked 验收项）；
//!   跨平台位元一致性不在本声明域内；
//! - **执行预算**：`MAX_PARTS = 4096`、图样件 `MAX_POINTS` / `MAX_WAYPOINTS`
//!   （两两核算 O(n²)），超限 fail-fast 并附自诊断指引；
//! - **图样三件**（iso_project / tube_route / figure_audit）：接受 3D 点输入
//!   `[x,y,z]`，产出为屏幕投影 / 走线校核 / 图样级保守审计（间隙为下界
//!   `lower_bound`），非制造级测量——保守方向声明即契约；
//! - **域外动作**：不可解析（NaN/Inf、杂质字符串）、超预算、退化输入（零长
//!   段、高度退化、重复 id、越界角度）一律经 `Err(String)` 通道显式拒绝并附
//!   自诊断指引——本族无 undetermined 路径，每个可解析输入恒得唯一确定判定
//!   （status 恒 determined），域外不给绿。
//!
//! # 确定性与数值边界（诚实声明，对齐 indicator-services 先例）
//!
//! - f64 运算序固定 → 同平台同输入逐位一致；双跑位元一致是验收项；
//! - 判定容差 EPS = 1e-9（绝对值，像素级坐标语义），容差方向声明即契约：
//!   碰撞偏严（触即算）、间隙偏宽（达标线内缩 EPS）；
//! - 浮点输出采用 Rust `{:?}`（最短往返表示，对齐 Python `repr`）；
//!   NaN/Inf 在输入解析期即拒绝，不进入运算。
//!
//! # 统一信封 v1
//!
//! 成功出参含保留键 `status`（恒 `"determined"`：容差方向声明即契约，
//! 每个可解析输入均得到唯一确定判定，本族无 undetermined 路径）与
//! `determinism`（`"corpus_locked"`：双跑位元一致 + 语料回归锁定）。
//! 既有字段原位保留；error 维持框架 `Err(String)` 通道，不入成功信封。
//!
//! # 装配
//!
//! - `GeometryServiceRouter` 实现 [`IoHandler`]，按 `params.service_name` 分发；
//! - 未命中 → 回落 ServiceRegistryHandler（HTTP，兼容其他外部服务）；
//! - 原生实现接收的入参 = `params.args`。

pub mod figure;
pub mod geometry;
pub mod services;

use std::sync::Arc;

use async_trait::async_trait;
use evorule_reactor::{IoHandler, IoResult};
use evorule_tcb::JsonValue;

// 机制件(trait/声明项/过滤路由器)在 plugin-kit,本 crate 自持声明表+薄壳具名路由器
pub use evorule_plugin_kit::{NativeService, NativeServiceDef};

use crate::services::{
    mk_clearance_check, mk_collision_check, mk_figure_audit, mk_iso_project, mk_occlusion_check,
    mk_tube_route,
};

// ============================================================================
// 声明表（SSOT 配对文件: official_native_services.json; 顺序即声明序,
// name/sensitive/description 三字段与顺序均被双侧守卫锁定）
// ============================================================================

/// 原生服务声明表（新增原生能力 = 本表追加一项 + SSOT 文件追加一项 +
/// 运行 sync 脚本同步治理侧嵌入副本）
pub const NATIVE_SERVICES: &[NativeServiceDef] = &[
    NativeServiceDef {
        name: "geometry_collision_check",
        sensitive: false,
        description: "几何碰撞检测(零件两两相交判定,bbox/圆/多边形,触即算,确定性)",
        parameters: None,
        make: mk_collision_check,
    },
    NativeServiceDef {
        name: "geometry_clearance_check",
        sensitive: false,
        description: "几何间隙检查(零件两两最小距离vs阈值,违规清单+全局最小,宽容方向容差,确定性)",
        parameters: None,
        make: mk_clearance_check,
    },
    NativeServiceDef {
        name: "geometry_occlusion_check",
        sensitive: false,
        description: "几何遮挡序检查(重叠零件绘制序vs声明z层,画家算法语义,确定性)",
        parameters: None,
        make: mk_occlusion_check,
    },
    NativeServiceDef {
        name: "geometry_iso_project",
        sensitive: false,
        description: "等距投影(三维点集→屏幕坐标,方位角/仰角/视口适配,契约化变换,确定性)",
        parameters: None,
        make: mk_iso_project,
    },
    NativeServiceDef {
        name: "geometry_tube_route",
        sensitive: false,
        description: "管路走线校核(转折点序列→段长/转角/弯径切线需求,弯径超限违规,确定性)",
        parameters: None,
        make: mk_tube_route,
    },
    NativeServiceDef {
        name: "geometry_figure_audit",
        sensitive: false,
        description: "图样场景审计(棱柱/管路成对间隙下界+端点承托,保守方向声明,确定性)",
        parameters: None,
        make: mk_figure_audit,
    },
];

// ============================================================================
// 具名路由器（薄壳，机制在 plugin-kit）
// ============================================================================

/// 几何服务路由器：原生优先，未命中回落 HTTP 注册表
pub struct GeometryServiceRouter(evorule_plugin_kit::NativeServiceRouter);

impl GeometryServiceRouter {
    pub fn new(fallback: Arc<dyn IoHandler>) -> Self {
        Self(evorule_plugin_kit::NativeServiceRouter::new(
            NATIVE_SERVICES,
            fallback,
        ))
    }

    pub fn with_enabled(fallback: Arc<dyn IoHandler>, enabled: &[&str]) -> Result<Self, String> {
        Ok(Self(evorule_plugin_kit::NativeServiceRouter::with_enabled(
            NATIVE_SERVICES,
            fallback,
            enabled,
        )?))
    }

    pub fn native_service_names() -> Vec<&'static str> {
        evorule_plugin_kit::NativeServiceRouter::native_service_names(NATIVE_SERVICES)
    }
}

#[async_trait]
impl IoHandler for GeometryServiceRouter {
    async fn execute(&self, params: &JsonValue) -> IoResult {
        self.0.execute(params).await
    }
}

// ============================================================================
// 守卫测试
// ============================================================================

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
        let router = GeometryServiceRouter::new(Arc::new(SentinelHandler));
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
        let file = include_str!("../official_native_services.json");
        let file: serde_json::Value =
            serde_json::from_str(file).map_err(|e| format!("声明文件非法 JSON: {e}"))?;
        let services = file
            .get("services")
            .and_then(|v| v.as_array())
            .ok_or_else(|| "声明文件缺 services 数组".to_string())?;
        if services.len() != NATIVE_SERVICES.len() {
            return Err(format!(
                "声明文件与声明表条目数漂移: 文件={} 表={}",
                services.len(),
                NATIVE_SERVICES.len()
            ));
        }
        for (i, (s, d)) in services.iter().zip(NATIVE_SERVICES.iter()).enumerate() {
            let name = s
                .get("name")
                .and_then(|v| v.as_str())
                .ok_or_else(|| format!("声明文件第 {i} 条缺 name"))?;
            let sensitive = s
                .get("sensitive")
                .and_then(|v| v.as_bool())
                .ok_or_else(|| format!("声明文件第 {i} 条缺 sensitive"))?;
            let description = s
                .get("description")
                .and_then(|v| v.as_str())
                .ok_or_else(|| format!("声明文件第 {i} 条缺 description"))?;
            if name != d.name {
                return Err(format!("第 {i} 条服务名漂移: 文件={name} 表={}", d.name));
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
