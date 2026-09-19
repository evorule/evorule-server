// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 声明 × 实载 **对账**（阶段 2 · 探活可信度补强）。
//!
//! # 为什么需要这个模块
//! 跨仓契约的探活只问一个问题：`/health` 是否 **2xx 且 JSON 可解析**。
//! 于是「进程活着但一个 UDF 都没加载」也会被判为 `online`——
//! **探活只能证明进程存活，不能证明服务可用**。
//!
//! # 为什么结论必须走状态码
//! server 侧 `plugin_probe::classify_response` 只看两件事：状态码区间 + body 是否
//! JSON。body 里写多少诊断信息都不会改变判定结果，故对账结论**必须外化为
//! HTTP 状态码**（degraded → 503）才能传导到 server 的告警面上。
//!
//! # 被拦截的三个静默故障（此前均表现为 `status: ok`）
//! ① **零能力**：`WASM_HOST_DIR` 给错 / 目录为空 → 进程在，能力为零；
//! ② **声明悬空**：plugin.json 声明 N 个服务、实际只加载 M<N 个
//!    → server 照声明派生路由，调用**必 404**（装载日志一切正常）；
//! ③ **实载悬空**：目录里有未声明的 `.wasm` → 服务对不上号，**永远不可达**
//!    （放进去的模块静默失效，正是 plugin.json 自己警告过的漂移）。
//!
//! # 三态语义
//! - [`ReconciliationState::Ok`] → `/health` 200：声明与实载一致且至少 1 个模块；
//! - [`ReconciliationState::Degraded`] → `/health` **503**：**能判定**的不一致，
//!   必须报警（可判定 ≠ 允许沉默）；
//! - [`ReconciliationState::Unavailable`] → `/health` 200 但 body 显式标注：
//!   **无法判定**（找不到 / 读不懂 plugin.json）。此处刻意**不**报 degraded——
//!   把"无法判定"当成"故障"会制造假警，而假警会训练运维忽略真警
//!   （单人多插件运维下，报警疲劳比漏警更危险）。启动期有 WARN，body 有显式字段，
//!   不是静默放过；`WASM_HOST_PLUGIN_JSON` 可显式指定声明位置消除该态。
//!
//! 注：plugin.json 解析失败同样归 `Unavailable` 而非 `Degraded`——
//! 该文件是 server 侧 `load_external_plugins` 的 fail-fast 输入，
//! 它坏掉时 server 根本起不来，重复报警无增量信息。
//!
//! # auto_discover 模式（历史批次）：目录 = 身份事实源，策略表 = 超集校验
//! plugin.json 声明 `"auto_discover": true` 时，`services[]` 降级为**策略表**
//! （server 侧拉取 `GET /services` 实载清单合入未声明增量，plugin.json 不再
//! 承载身份），对账语义随之演进——**只换声明源，不换机制**：
//! - **missing（策略表有、实载无）仍是 `Degraded`**：server 已照策略表注册路由，
//!   调用必 404，与静态制同罪；
//! - **undeclared（实载有、策略表无）不再是故障**：这正是自动发现的合法形态
//!   （「放 .wasm + 重启即可用」的判定依据），仅在 body `undeclared` 数组中
//!   留痕供核对（谁被默认策略接管，一眼可查）；
//! - **零模块仍是 `Degraded`**：与声明模式无关，进程存活但能力为零。
//!
//! 不开启（缺省）时一切如旧——存量行为逐字节不变。

use std::path::{Path, PathBuf};

/// 对账结论三态
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReconciliationState {
    /// 声明与实载一致，且至少加载了 1 个模块
    Ok,
    /// 可判定为不一致（进程活着但能力与声明不符）→ `/health` 返回 503
    Degraded,
    /// 无法判定（找不到 / 读不懂 plugin.json）→ `/health` 维持 200 但显式标注
    Unavailable,
}

impl ReconciliationState {
    pub fn as_str(&self) -> &'static str {
        match self {
            ReconciliationState::Ok => "ok",
            ReconciliationState::Degraded => "degraded",
            ReconciliationState::Unavailable => "unavailable",
        }
    }
}

/// plugin.json 中的服务声明（只取对账需要的字段）
#[derive(Debug, Clone)]
pub struct Declaration {
    pub source: PathBuf,
    pub services: Vec<String>,
    /// 历史批次自动发现开关：true 时 `services[]` 是策略表（超集校验），
    /// 身份以实载清单为准；缺省 false = 静态声明制（行为逐字节不变）。
    pub auto_discover: bool,
}

impl Declaration {
    /// 读取并解析声明文件。
    ///
    /// 服务名取 `services[].name`——与 server 侧 `ExternalPluginManifest`
    /// 派生路由用的是同一个字段，**同一事实源**，不另立口径。
    ///
    /// `auto_discover` 取 `auto_discover` 布尔字段，**缺省/类型错一律 false**：
    /// 该开关决定对账严格度，宁可保守回退到静态制（多报一次 degraded），
    /// 也不让一个写错的值静默放宽安全检查。
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("读取 {} 失败: {e}", path.display()))?;
        let value: serde_json::Value = serde_json::from_str(&text)
            .map_err(|e| format!("{} 不是合法 JSON: {e}", path.display()))?;

        let services = value
            .get("services")
            .and_then(|s| s.as_array())
            .ok_or_else(|| format!("{} 缺 services 数组", path.display()))?
            .iter()
            .filter_map(|s| s.get("name").and_then(|n| n.as_str()))
            .map(str::to_string)
            .collect::<Vec<_>>();

        let auto_discover = value
            .get("auto_discover")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        Ok(Self {
            source: path.to_path_buf(),
            services,
            auto_discover,
        })
    }
}

/// 定位 plugin.json（按可信度降序取第一个存在的）：
/// ① `WASM_HOST_PLUGIN_JSON` 显式指定（消除一切猜测）；
/// ② `<wasm_dir>/../wasm-host/plugin.json`——规范布局
///    （`plugins/wasm` → `plugins/wasm-host`），也是本仓库的部署形态。
///
/// **刻意不提供「cwd 的 `./plugin.json`」兜底**（曾有过，已删）：
/// ① 它在全部规范布局下都是冗余的——`WASM_HOST_DIR=../wasm` 与绝对路径两种
///    部署形态都由 ② 命中；② 它只在非规范场景触发，而那时它更可能是错的：
///    实测教训是 T4 负面测试把 fixture 暂存在临时目录、cwd 却是插件包目录，
///    于是拿**真清单**去和 fixture 模块对账，误判 degraded。少一个 cwd 依赖，
///    就少一类"同一条命令换个目录就换了语义"的惊喜。
pub fn resolve_source(wasm_dir: &Path, explicit: Option<&str>) -> Option<PathBuf> {
    if let Some(p) = explicit {
        let p = PathBuf::from(p);
        return if p.is_file() { Some(p) } else { None };
    }
    let sibling = wasm_dir.join("..").join("wasm-host").join("plugin.json");
    if sibling.is_file() {
        return Some(sibling);
    }
    None
}

/// 零模块的诊断文案（可判定故障，与能否对账无关，故为常量单点）
const ZERO_MODULE_REASON: &str =
    "未加载任何 UDF —— 进程存活但零能力（检查 WASM_HOST_DIR 是否指向真实的 .wasm 目录）";

/// 双向差集（纯函数）：`(声明有实载无, 实载有声明无)`
pub fn diff(declared: &[String], loaded: &[String]) -> (Vec<String>, Vec<String>) {
    let missing = declared
        .iter()
        .filter(|d| !loaded.contains(d))
        .cloned()
        .collect();
    let undeclared = loaded
        .iter()
        .filter(|l| !declared.contains(l))
        .cloned()
        .collect();
    (missing, undeclared)
}

/// 判定（纯函数）：`(状态, 原因)`。`declared = None` = 无法对账。
///
/// `declared.auto_discover` 为 true 时 `services[]` 按**策略表**校验
/// （missing 报警、undeclared 合法）；false 时按静态声明制双向校验（历史批次）。
pub fn evaluate(
    loaded: &[String],
    declared: Option<&Declaration>,
) -> (ReconciliationState, Option<String>) {
    if loaded.is_empty() {
        return (
            ReconciliationState::Degraded,
            Some(ZERO_MODULE_REASON.to_string()),
        );
    }
    let Some(decl) = declared else {
        return (ReconciliationState::Unavailable, None);
    };
    let (missing, undeclared) = diff(&decl.services, loaded);
    if !missing.is_empty() {
        if decl.auto_discover {
            return (
                ReconciliationState::Degraded,
                Some(format!(
                    "策略表声明了但未加载: {missing:?} —— server 已照策略表注册路由，\
                     调用这些服务必 404（auto_discover 模式下策略表为超集校验，\
                     声明的服务必须在实载清单内）"
                )),
            );
        }
        return (
            ReconciliationState::Degraded,
            Some(format!(
                "plugin.json 声明了但未加载: {missing:?} —— server 已照声明派生路由，\
                 调用这些服务必 404（检查 .wasm 是否漏构建/漏拷入 WASM_HOST_DIR）"
            )),
        );
    }
    if !undeclared.is_empty() && !decl.auto_discover {
        return (
            ReconciliationState::Degraded,
            Some(format!(
                "已加载但 plugin.json 未声明: {undeclared:?} —— 服务名对不上号，\
                 这些模块永远不可达（补声明并重启 server，或从目录移除；\
                 或在 plugin.json 开启 auto_discover 交给自动发现接管）"
            )),
        );
    }
    (ReconciliationState::Ok, None)
}

/// 一轮完整对账（含读盘）
#[derive(Debug, Clone)]
pub struct Reconciliation {
    pub state: ReconciliationState,
    /// 声明来源（相对路径按原样呈现，便于运维对上启动命令的 cwd）
    pub source: Option<String>,
    pub declared: Vec<String>,
    pub loaded: Vec<String>,
    /// 声明有、实载无 → 调用必 404
    pub missing: Vec<String>,
    /// 实载有、声明无 → 静态制=永远不可达（degraded）；auto_discover=默认策略接管（合法，留痕核对）
    pub undeclared: Vec<String>,
    /// 对账模式（Declaration.auto_discover 透传；无法对账时为 false）
    pub auto_discover: bool,
    /// degraded 的故障原因 / unavailable 的跳过原因
    pub reason: Option<String>,
}

impl Reconciliation {
    /// 扫描后调用：解析声明 → 求差集 → 判定。
    ///
    /// 与 cwd **无关**：声明源只认显式指定或 `<wasm_dir>/../wasm-host/plugin.json`
    /// 这一种规范布局（见 `resolve_source` 的取舍说明）。
    pub fn run(wasm_dir: &Path, explicit: Option<&str>, loaded: &[String]) -> Self {
        let Some(path) = resolve_source(wasm_dir, explicit) else {
            // 拿不到声明源：零模块仍是**可判定**故障；有模块才是"无法判定"
            let (state, reason) = if loaded.is_empty() {
                (
                    ReconciliationState::Degraded,
                    Some(ZERO_MODULE_REASON.to_string()),
                )
            } else {
                let why = match explicit {
                    Some(p) => format!(
                        "WASM_HOST_PLUGIN_JSON 指定的声明文件不存在: {p} —— 无法对账，\
                         仅能证明进程存活"
                    ),
                    None => format!(
                        "未找到 plugin.json（已尝试: {}/../wasm-host/plugin.json）—— \
                         无法对账，仅能证明进程存活",
                        wasm_dir.display()
                    ),
                };
                (ReconciliationState::Unavailable, Some(why))
            };
            return Self {
                state,
                source: None,
                declared: Vec::new(),
                loaded: loaded.to_vec(),
                missing: Vec::new(),
                undeclared: Vec::new(),
                auto_discover: false,
                reason,
            };
        };

        let declared = match Declaration::load(&path) {
            Ok(d) => d,
            Err(e) => {
                return Self {
                    state: ReconciliationState::Unavailable,
                    source: Some(path.display().to_string()),
                    declared: Vec::new(),
                    loaded: loaded.to_vec(),
                    missing: Vec::new(),
                    undeclared: Vec::new(),
                    auto_discover: false,
                    reason: Some(format!("声明不可用: {e}")),
                }
            }
        };

        let (state, reason) = evaluate(loaded, Some(&declared));
        let (missing, undeclared) = diff(&declared.services, loaded);
        Self {
            state,
            source: Some(declared.source.display().to_string()),
            declared: declared.services,
            loaded: loaded.to_vec(),
            missing,
            undeclared,
            auto_discover: declared.auto_discover,
            reason,
        }
    }

    /// 是否应在 `/health` 上判为不健康（→ 503 → server 判 offline 并告警）
    pub fn is_degraded(&self) -> bool {
        self.state == ReconciliationState::Degraded
    }

    /// 供 `/health` body 使用的结构化呈现（确定性：键序由 BTreeMap 固定）
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "state": self.state.as_str(),
            "declaration_source": self.source,
            "auto_discover": self.auto_discover,
            "declared": self.declared,
            "declared_count": self.declared.len(),
            "loaded_count": self.loaded.len(),
            "missing": self.missing,
            "undeclared": self.undeclared,
            "reason": self.reason,
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    /// 静态声明制 Declaration（缺省 auto_discover=false）
    fn decl(v: &[&str]) -> Declaration {
        Declaration {
            source: PathBuf::from("plugin.json"),
            services: s(v),
            auto_discover: false,
        }
    }

    /// auto_discover 策略表 Declaration
    fn decl_auto(v: &[&str]) -> Declaration {
        Declaration {
            source: PathBuf::from("plugin.json"),
            services: s(v),
            auto_discover: true,
        }
    }

    // ===== diff：双向差集 =====

    #[test]
    fn test_diff_both_directions() {
        let (missing, undeclared) = diff(&s(&["a", "b", "c"]), &s(&["b", "d"]));
        assert_eq!(missing, s(&["a", "c"]));
        assert_eq!(undeclared, s(&["d"]));
    }

    #[test]
    fn test_diff_identical_is_empty() {
        let (m, u) = diff(&s(&["a", "b"]), &s(&["a", "b"]));
        assert!(m.is_empty() && u.is_empty());
    }

    // ===== evaluate：三态判定 =====

    #[test]
    fn test_evaluate_zero_modules_is_degraded_even_without_declaration() {
        // 零模块是**可判定**故障：即便没有声明也必须报（这是本次修复的主目标）
        let (state, reason) = evaluate(&[], Some(&decl(&["a"])));
        assert_eq!(state, ReconciliationState::Degraded);
        assert!(reason.unwrap().contains("未加载任何 UDF"));
        let (state, _) = evaluate(&[], None);
        assert_eq!(state, ReconciliationState::Degraded);
    }

    #[test]
    fn test_evaluate_matched_is_ok() {
        let (state, reason) = evaluate(&s(&["a", "b"]), Some(&decl(&["a", "b"])));
        assert_eq!(state, ReconciliationState::Ok);
        assert!(reason.is_none());
    }

    #[test]
    fn test_evaluate_declared_but_not_loaded_is_degraded() {
        // 声明悬空：server 照声明路由 → 调用必 404
        let (state, reason) = evaluate(&s(&["a"]), Some(&decl(&["a", "b"])));
        assert_eq!(state, ReconciliationState::Degraded);
        let r = reason.unwrap();
        assert!(r.contains("未加载") && r.contains("404"));
    }

    #[test]
    fn test_evaluate_loaded_but_not_declared_is_degraded() {
        // 实载悬空：模块静默失效，永远不可达
        let (state, reason) = evaluate(&s(&["a", "b"]), Some(&decl(&["a"])));
        assert_eq!(state, ReconciliationState::Degraded);
        assert!(reason.unwrap().contains("未声明"));
    }

    #[test]
    fn test_evaluate_no_declaration_with_modules_is_unavailable_not_degraded() {
        // 无法判定 ≠ 故障：有模块但无声明源 → Unavailable（不制造假警）
        let (state, reason) = evaluate(&s(&["a"]), None);
        assert_eq!(state, ReconciliationState::Unavailable);
        assert!(reason.is_none());
    }

    // ===== evaluate：auto_discover 策略表模式（历史批次）=====

    #[test]
    fn test_evaluate_auto_discover_undeclared_is_ok() {
        // 实载有、策略表无 → 自动发现的合法形态（F1 判定依据），不再 degraded
        let (state, reason) = evaluate(&s(&["a", "new_udf"]), Some(&decl_auto(&["a"])));
        assert_eq!(state, ReconciliationState::Ok);
        assert!(reason.is_none());
    }

    #[test]
    fn test_evaluate_auto_discover_missing_is_still_degraded() {
        // 策略表声明了但未加载 → 仍 degraded（server 照策略表注册了路由，调用必 404）
        let (state, reason) = evaluate(&s(&["a"]), Some(&decl_auto(&["a", "ghost"])));
        assert_eq!(state, ReconciliationState::Degraded);
        let r = reason.unwrap();
        assert!(r.contains("策略表") && r.contains("404"));
    }

    #[test]
    fn test_evaluate_auto_discover_zero_modules_still_degraded() {
        // 零模块与声明模式无关
        let (state, reason) = evaluate(&[], Some(&decl_auto(&["a"])));
        assert_eq!(state, ReconciliationState::Degraded);
        assert!(reason.unwrap().contains("未加载任何 UDF"));
    }

    // ===== Reconciliation::run 集成（真实读盘）=====

    #[test]
    fn test_run_explicit_missing_declaration_is_unavailable_with_reason() {
        // 传**显式且不存在**的路径（而非 None）——否则 resolve_source 会兜底到
        // cwd 的 ./plugin.json（cargo test 的 cwd 就是包目录，那里有真清单），
        // 结果随 cwd 变化，测试即失真。
        let dir = std::env::temp_dir().join("evorule-decl-none-xyz");
        let rec = Reconciliation::run(&dir, Some("/no/such/plugin.json"), &s(&["udf_x"]));
        assert_eq!(rec.state, ReconciliationState::Unavailable);
        assert!(rec.source.is_none());
        let r = rec.reason.unwrap();
        assert!(r.contains("WASM_HOST_PLUGIN_JSON") && r.contains("/no/such/plugin.json"));
    }

    #[test]
    fn test_run_zero_modules_stays_degraded_even_without_declaration_source() {
        // 零模块与"能否对账"无关：没有声明源也必须报 degraded
        let dir = std::env::temp_dir().join("evorule-decl-none-xyz2");
        let rec = Reconciliation::run(&dir, Some("/no/such/plugin.json"), &[]);
        assert_eq!(rec.state, ReconciliationState::Degraded);
        assert!(rec.reason.unwrap().contains("未加载任何 UDF"));
    }

    #[test]
    fn test_resolve_source_explicit_then_sibling_and_never_cwd() {
        let dir = std::env::temp_dir().join("evorule-decl-resolve-xyz");
        let wasm_dir = dir.join("wasm");
        let pkg_dir = dir.join("wasm-host");
        std::fs::create_dir_all(&wasm_dir).unwrap();
        std::fs::create_dir_all(&pkg_dir).unwrap();
        std::fs::write(pkg_dir.join("plugin.json"), r#"{"services":[]}"#).unwrap();

        // ① 显式路径不存在 → None（**不回落**到 sibling/其他）
        assert!(resolve_source(&wasm_dir, Some("/tmp/explicit.json")).is_none());

        // ② 无显式 → 命中包同级规范布局
        let got = resolve_source(&wasm_dir, None).unwrap();
        assert_eq!(
            got,
            wasm_dir.join("..").join("wasm-host").join("plugin.json")
        );

        // ③ **不认 cwd**：cargo test 的 cwd 是插件包目录（那里有真 plugin.json），
        //    但 wasm 目录的 sibling 不存在时必须返回 None——这条锁死"与 cwd 无关"
        let orphan = std::env::temp_dir().join("evorule-decl-orphan-xyz");
        std::fs::create_dir_all(&orphan).unwrap();
        assert!(
            resolve_source(&orphan, None).is_none(),
            "cwd 兜底已删除：不该因为 cwd 里恰好有 plugin.json 就把它当声明源"
        );

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&orphan);
    }

    #[test]
    fn test_run_reads_declaration_and_matches() {
        let dir = std::env::temp_dir().join("evorule-decl-ok-xyz");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("plugin.json");
        std::fs::write(
            &p,
            r#"{"id":"wasm-host","services":[{"name":"udf_a"},{"name":"udf_b"}]}"#,
        )
        .unwrap();
        let rec = Reconciliation::run(&dir, Some(p.to_str().unwrap()), &s(&["udf_a", "udf_b"]));
        assert_eq!(rec.state, ReconciliationState::Ok);
        assert_eq!(rec.declared, s(&["udf_a", "udf_b"]));
        assert!(!rec.is_degraded());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_run_mismatch_marks_degraded() {
        let dir = std::env::temp_dir().join("evorule-decl-bad-xyz");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("plugin.json");
        std::fs::write(
            &p,
            r#"{"services":[{"name":"udf_a"},{"name":"udf_ghost"}]}"#,
        )
        .unwrap();
        let rec = Reconciliation::run(&dir, Some(p.to_str().unwrap()), &s(&["udf_a"]));
        assert_eq!(rec.state, ReconciliationState::Degraded);
        assert!(rec.is_degraded());
        assert_eq!(rec.missing, s(&["udf_ghost"]));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_run_malformed_declaration_is_unavailable_not_degraded() {
        // 解析失败归 Unavailable：该文件坏掉时 server 侧 fail-fast 已拦，
        // 重复报警无增量信息
        let dir = std::env::temp_dir().join("evorule-decl-broken-xyz");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("plugin.json");
        std::fs::write(&p, "{ not-json").unwrap();
        let rec = Reconciliation::run(&dir, Some(p.to_str().unwrap()), &s(&["udf_a"]));
        assert_eq!(rec.state, ReconciliationState::Unavailable);
        assert!(rec.reason.unwrap().contains("声明不可用"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ===== auto_discover：load 解析 + run 集成（历史批次）=====

    #[test]
    fn test_load_auto_discover_flag_variants() {
        let dir = std::env::temp_dir().join("evorule-decl-autodiscover-xyz");
        std::fs::create_dir_all(&dir).unwrap();

        // ① 显式 true
        let p = dir.join("on.json");
        std::fs::write(&p, r#"{"services":[],"auto_discover":true}"#).unwrap();
        assert!(Declaration::load(&p).unwrap().auto_discover);

        // ② 缺省 false（存量零迁移）
        let p = dir.join("default.json");
        std::fs::write(&p, r#"{"services":[]}"#).unwrap();
        assert!(!Declaration::load(&p).unwrap().auto_discover);

        // ③ 类型错（字符串 "true"）→ 保守回退 false：宁可多报一次 degraded，
        //    不让写错的值静默放宽校验
        let p = dir.join("wrongtype.json");
        std::fs::write(&p, r#"{"services":[],"auto_discover":"true"}"#).unwrap();
        assert!(!Declaration::load(&p).unwrap().auto_discover);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_run_auto_discover_undeclared_is_ok_not_degraded() {
        // F1 判定依据：目录=身份事实源——实载超出策略表是自动发现的合法形态
        let dir = std::env::temp_dir().join("evorule-decl-ad-run-xyz");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("plugin.json");
        std::fs::write(
            &p,
            r#"{"services":[{"name":"udf_a"}],"auto_discover":true}"#,
        )
        .unwrap();
        let rec = Reconciliation::run(&dir, Some(p.to_str().unwrap()), &s(&["udf_a", "udf_new"]));
        assert_eq!(rec.state, ReconciliationState::Ok);
        assert!(!rec.is_degraded());
        assert!(rec.auto_discover);
        // 留痕仍在：undeclared 数组如实呈现，供核对谁被默认策略接管
        assert_eq!(rec.undeclared, s(&["udf_new"]));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_run_auto_discover_missing_still_degraded() {
        // 策略表=超集校验：声明的服务必须在实载清单内，缺失仍是 degraded
        let dir = std::env::temp_dir().join("evorule-decl-ad-miss-xyz");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("plugin.json");
        std::fs::write(
            &p,
            r#"{"services":[{"name":"udf_a"},{"name":"udf_ghost"}],"auto_discover":true}"#,
        )
        .unwrap();
        let rec = Reconciliation::run(&dir, Some(p.to_str().unwrap()), &s(&["udf_a"]));
        assert_eq!(rec.state, ReconciliationState::Degraded);
        assert_eq!(rec.missing, s(&["udf_ghost"]));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_explicit_env_path_missing_falls_back_to_none_not_panic() {
        let dir = std::env::temp_dir().join("evorule-decl-envmiss-xyz");
        assert!(resolve_source(&dir, Some("/no/such/plugin.json")).is_none());
    }

    // ===== to_json：字段完备性（/health body 契约）=====

    #[test]
    fn test_to_json_has_all_contract_fields() {
        let rec = Reconciliation {
            state: ReconciliationState::Degraded,
            source: Some("plugins/wasm-host/plugin.json".to_string()),
            declared: s(&["a", "b"]),
            loaded: s(&["a"]),
            missing: s(&["b"]),
            undeclared: Vec::new(),
            auto_discover: false,
            reason: Some("测试".to_string()),
        };
        let v = rec.to_json();
        assert_eq!(v["state"], "degraded");
        assert_eq!(v["declared_count"], 2);
        assert_eq!(v["loaded_count"], 1);
        assert_eq!(v["missing"][0], "b");
        assert_eq!(v["declaration_source"], "plugins/wasm-host/plugin.json");
        assert_eq!(v["auto_discover"], false);
        assert_eq!(v["reason"], "测试");
    }
}
