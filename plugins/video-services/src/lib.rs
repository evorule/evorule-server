// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 视频域判定原生服务（插件组骨架）。
//!
//! # 定位
//! - 视频生产链路的可判定事项判定层：颜色对比度、帧序列事件、主体轨迹
//!   守恒、字幕时间轴一致性（判定核逐批落位）；与 geometry 族互补
//!   （平面几何谓词 vs 视频域时序/颜色谓词）；独立 crate，仅经
//!   `IoDispatcher` 挂载；
//! - 骨架批先落插件组骨架：SSOT 声明 + 薄壳路由器 + 双侧守卫，业务
//!   服务体按批次追加（追加式，顺序即路由查找序）；
//! - 入参一律结构化证据（坐标/序列/计数/字符串有理数），禁文件路径/
//!   字节流/采样策略/感知语义；同输入双跑位元一致。
//!
//! # 路由设计
//! - `VideoServiceRouter` 实现 [`IoHandler`]，按 `params.service_name` 分发：
//!   - 命中原生服务名 → 调用对应原生实现（进程内，确定性）
//!   - 未命中 → 回落 `ServiceRegistryHandler`（HTTP，兼容其他外部服务）
//!
//! # 统一信封 v1
//! - 服务成功出参含保留键 `status`（恒 `determined`）+
//!   `determinism`（恒 `bitwise_replayable`）；error 走框架
//!   `Err(String)` 通道并带自诊断指引。
//!
//! # 服务清单（SSOT = official_native_services.json，双侧守卫锁定）
//! 1. `video_service_skeleton_probe` — 插件组骨架探针（装载/路由链路验证）
//! 2. `video_timeline_consistency_check` — 字幕时间轴一致性校验（整数毫秒判定）
//! 3. `color_contrast_ratio_eval` — 颜色对比度判定（WCAG 2.x，sRGB 字符串入参）
//! 4. `video_frame_sequence_eval` — 帧序列异常事件判定（黑帧/闪帧/静止段）
//! 5. `video_track_consistency_eval` — 主体轨迹守恒判定（位移/尺度漂移越限）

#![forbid(unsafe_code)]

use std::sync::Arc;

use async_trait::async_trait;
use evorule_plugin_kit::NativeServiceDef;
use evorule_reactor::{IoHandler, IoResult};
use evorule_tcb::JsonValue;

pub mod services;

use services::{
    mk_color_contrast_ratio_eval, mk_skeleton_probe, mk_timeline_consistency_check,
    mk_video_frame_sequence_eval, mk_video_track_consistency_eval,
};

// ============================================================================
// 声明表（SSOT 配对文件: official_native_services.json; 顺序即声明序,
// name/sensitive/description 三字段与顺序均被双侧守卫锁定）
// ============================================================================

/// 原生服务声明表（新增原生能力 = 本表追加一项 + SSOT 文件追加一项 +
/// 运行 sync 脚本同步治理侧嵌入副本）
pub const NATIVE_SERVICES: &[NativeServiceDef] = &[NativeServiceDef {
    name: "video_service_skeleton_probe",
    sensitive: false,
    description: "插件组骨架探针(回显输入并恒返固定结构,验证装载与路由链路;无业务语义,确定性)",
    parameters: Some(
        r#"{"type":"object","properties":{"echo":{"type":"string","description":"可选回显字符串(缺省空)"}}}"#,
    ),
    make: mk_skeleton_probe,
}, NativeServiceDef {
    name: "video_timeline_consistency_check",
    sensitive: false,
    description: "字幕时间轴一致性校验(镜头时间轴+字幕cue,整数毫秒判定:出镜/重叠/缝隙/超总时长;overlap_policy显式入参;确定性)",
    parameters: Some(
        r#"{"type":"object","properties":{"shots":{"type":"array","items":{"type":"object"},"description":"镜头时间轴,每项{id,start,end},整数毫秒"},"cues":{"type":"array","items":{"type":"object"},"description":"字幕cue,每项{id,shot_id,start,end,text}"},"total_duration":{"type":"integer","description":"总时长整数毫秒(可选)"},"overlap_policy":{"type":"string","enum":["allow","forbid"],"description":"同镜头cue重叠判定策略(缺省forbid)"}},"required":["shots","cues"]}"#,
    ),
    make: mk_timeline_consistency_check,
}, NativeServiceDef {
    name: "color_contrast_ratio_eval",
    sensitive: false,
    description: "颜色对比度判定核(WCAG 2.x 相对亮度+对比度比值,sRGB 三元组字符串入参,定点整数运算;字幕可读性 QC 判定用;确定性)",
    parameters: Some(
        r#"{"type":"object","properties":{"fg":{"type":"array","items":{"type":"string"},"description":"前景 sRGB [r,g,b],各通道 0..255 十进制字符串"},"bg":{"type":"array","items":{"type":"string"},"description":"背景 sRGB [r,g,b],各通道 0..255 十进制字符串"},"threshold":{"type":"string","description":"对比度阈值十进制有理数字符串(缺省 4.5)"}},"required":["fg","bg"]}"#,
    ),
    make: mk_color_contrast_ratio_eval,
}, NativeServiceDef {
    name: "video_frame_sequence_eval",
    sensitive: false,
    description: "帧序列异常事件判定核(逐帧亮度摘要序列→黑帧/闪帧/静止段,阈值显式入参缺省16/60/24帧,帧号=数组下标;解码抽帧统计留引擎侧;确定性)",
    parameters: Some(
        r#"{"type":"object","properties":{"lums":{"type":"array","items":{"type":"string"},"description":"逐帧平均亮度摘要,0..255 十进制字符串,帧号=数组下标"},"black_threshold":{"type":"string","description":"黑帧亮度阈值(缺省 16)"},"flash_delta":{"type":"string","description":"闪帧相邻帧亮度差阈值(缺省 60)"},"still_frames":{"type":"integer","description":"静止段长度阈值,以帧计(缺省 24)"}},"required":["lums"]}"#,
    ),
    make: mk_video_frame_sequence_eval,
}, NativeServiceDef {
    name: "video_track_consistency_eval",
    sensitive: false,
    description: "主体轨迹守恒判定核(参考 bbox+逐帧 bbox 序列→位移/尺度漂移越限判定,l2/linf 度量缺省 l2,l2 判定面平方整比较零开方,1e6 定点精确;感知检出留引擎侧显式喂入;确定性)",
    parameters: Some(
        r#"{"type":"object","properties":{"reference":{"type":"object","description":"参考主体框 {x,y,w,h},字符串有理数(引擎侧感知检出显式给出)"},"tracks":{"type":"array","description":"逐帧主体框 [{frame,bbox},...],frame 非负整数必填唯一,bbox 为 {x,y,w,h}"},"max_translation":{"type":"string","description":"位移上限 px,字符串如 \"40\""},"max_scale_delta":{"type":"string","description":"尺度漂移上限(相对面积变化),字符串如 \"0.2\""},"metric":{"type":"string","description":"位移度量 l2(缺省)或 linf"}},"required":["reference","tracks","max_translation","max_scale_delta"]}"#,
    ),
    make: mk_video_track_consistency_eval,
}];

/// 复合 IoHandler：`service_name` 命中原生服务名 → 原生执行；否则回落 HTTP。
///
/// 薄壳具名路由器：机制已上提 plugin-kit，本插件仅自持声明表并保持具名 API。
pub struct VideoServiceRouter(evorule_plugin_kit::NativeServiceRouter);

impl VideoServiceRouter {
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
impl IoHandler for VideoServiceRouter {
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
        assert_eq!(
            total, 5,
            "声明表应为 5 项(探针+时间轴校验+对比度+帧序列+轨迹守恒)"
        );
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
        let router = VideoServiceRouter::new(Arc::new(SentinelHandler));
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

    #[tokio::test]
    async fn test_skeleton_probe_envelope_and_echo() {
        // 探针信封保留键 + 回显语义 + 双跑位元一致
        struct SentinelHandler;
        #[async_trait]
        impl IoHandler for SentinelHandler {
            async fn execute(&self, _params: &JsonValue) -> IoResult {
                Err("fallback-called".to_string())
            }
        }
        let router = VideoServiceRouter::new(Arc::new(SentinelHandler));
        let make_params = || {
            JsonValue::object_from_pairs(&[
                (
                    "service_name",
                    JsonValue::string("video_service_skeleton_probe"),
                ),
                (
                    "args",
                    JsonValue::object_from_pairs(&[("echo", JsonValue::string("ping"))]),
                ),
            ])
        };
        let a = router.execute(&make_params()).await.expect("探针须成功");
        let b = router.execute(&make_params()).await.expect("探针须成功");
        assert_eq!(a, b, "同参双跑须位元一致");
        let obj = a.as_object().expect("出参须为对象");
        assert_eq!(
            obj.get("status").and_then(|v| v.as_str()),
            Some("determined"),
            "信封保留键 status"
        );
        assert_eq!(
            obj.get("determinism").and_then(|v| v.as_str()),
            Some("bitwise_replayable"),
            "信封保留键 determinism"
        );
        assert_eq!(
            obj.get("echo").and_then(|v| v.as_str()),
            Some("ping"),
            "回显语义"
        );
    }
}
