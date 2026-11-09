//! 原生服务实现：量纲一致性校验（JSON 入 / JSON 出，统一信封 v1）。
//!
//! # 确定性
//! 纯字符串解析 + 整数幂次代数（law_schema::dimensional 单源），
//! 无浮点运算（幂次为 i8 整数）、无时钟、无随机。同输入两次执行
//! 结果逐位一致。
//!
//! # 判定语义（重要）
//! 「量纲不一致」是 `consistent=false` 的**正常判定结果**，不是 error；
//! 「单位表达式不可解析（未知记号/文法外记号/加减操作分量纲不等）」
//! 才是 fail-visible error——判定与故障分离，调用方据 consistent 与
//! error 两层分别处置。量纲向量渲染串（如 `kg*m*s^-2`、`currency`）
//! 随结果回显，供规则层/调用方留痕报因。
//!
//! # 诚实边界
//! - 只对比量纲：km 与 m 同维判一致，量值/单位换算不归本层；
//! - 异币种（CNY/USD/…）同归货币维，币种间量值换算不归本层。

#![forbid(unsafe_code)]

use std::sync::Arc;

use evorule_law_schema::dimensional::parse_unit_expr;
use evorule_plugin_kit::NativeService;
use evorule_reactor::IoResult;
use evorule_tcb::JsonValue;

// ============================================================================
// 参数提取助手（fail-visible 带自诊断指引）
// ============================================================================

fn req_str<'a>(args: &'a JsonValue, key: &str) -> Result<&'a str, String> {
    match args.get(key) {
        Some(v) => v
            .as_str()
            .ok_or_else(|| format!("参数 {key} 须为字符串 — 自诊断指引: 见服务 parameters 契约")),
        None => Err(format!(
            "缺少参数 {key} — 自诊断指引: 见服务 parameters 契约"
        )),
    }
}

fn envelope(pairs: &[(&str, JsonValue)]) -> JsonValue {
    let mut all: Vec<(&str, JsonValue)> = vec![
        ("status", JsonValue::string("determined")),
        ("determinism", JsonValue::string("bitwise_replayable")),
    ];
    all.extend_from_slice(pairs);
    JsonValue::object_from_pairs(&all)
}

// ============================================================================
// dimensional_check — 单位表达式量纲一致性对比
// ============================================================================

struct DimensionalCheckService;

impl NativeService for DimensionalCheckService {
    fn execute(&self, args: &JsonValue) -> IoResult {
        let lhs_raw = req_str(args, "lhs")?;
        let rhs_raw = req_str(args, "rhs")?;
        // 额外键（如立法指令透传的 target）静默忽略——入参契约只承诺 lhs/rhs
        let lhs = parse_unit_expr(lhs_raw.trim())
            .map_err(|e| format!("lhs 单位表达式不可解析: {e} — 自诊断指引: 记号须为已登记单位(SI 七基本量纲+货币维),文法为 `expr := term (('+','-') term)*` / `term := factor (('*','/') factor)*` / `factor := base ('^' ['-'] digits)?`"));
        let rhs = parse_unit_expr(rhs_raw.trim())
            .map_err(|e| format!("rhs 单位表达式不可解析: {e} — 自诊断指引: 记号须为已登记单位(SI 七基本量纲+货币维),文法为 `expr := term (('+','-') term)*` / `term := factor (('*','/') factor)*` / `factor := base ('^' ['-'] digits)?`"));
        let lhs = lhs?;
        let rhs = rhs?;
        let consistent = lhs == rhs;
        Ok(envelope(&[
            ("consistent", JsonValue::Bool(consistent)),
            ("lhs_expr", JsonValue::string(lhs_raw.trim())),
            ("lhs_dims", JsonValue::string(lhs.render())),
            ("rhs_expr", JsonValue::string(rhs_raw.trim())),
            ("rhs_dims", JsonValue::string(rhs.render())),
            (
                "algorithm",
                JsonValue::string("law-schema-dimension-vector-v2"),
            ),
        ]))
    }
}

pub fn mk_dimensional_check() -> Arc<dyn NativeService> {
    Arc::new(DimensionalCheckService)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    fn args(pairs: &[(&str, JsonValue)]) -> JsonValue {
        JsonValue::object_from_pairs(pairs)
    }

    fn field<'a>(out: &'a JsonValue, key: &str) -> &'a str {
        out.get(key)
            .and_then(|v| v.as_str())
            .unwrap_or_else(|| panic!("{key} 应为字符串"))
    }

    fn bool_of(out: &JsonValue, key: &str) -> bool {
        out.get(key)
            .and_then(|v| match v {
                JsonValue::Bool(b) => Some(*b),
                _ => None,
            })
            .unwrap_or_else(|| panic!("{key} 应为布尔"))
    }

    /// 黄金向量：同维一致（kg*m*s^-2 vs newton）→ consistent=true；
    /// 异维不一致（m vs CNY）→ consistent=false 且两侧向量渲染回显。
    #[test]
    fn dimensional_check_golden() {
        let ok = DimensionalCheckService
            .execute(&args(&[
                ("lhs", JsonValue::string("kg*m*s^-2")),
                ("rhs", JsonValue::string("newton")),
            ]))
            .expect("同维对比应成功");
        assert!(bool_of(&ok, "consistent"));
        assert_eq!(field(&ok, "lhs_dims"), "kg*m*s^-2");
        assert_eq!(field(&ok, "rhs_dims"), "kg*m*s^-2");

        let mixed = DimensionalCheckService
            .execute(&args(&[
                ("lhs", JsonValue::string("m")),
                ("rhs", JsonValue::string("CNY")),
            ]))
            .expect("异维对比应成功（判定结果非 error）");
        assert!(!bool_of(&mixed, "consistent"), "米/元异维应判不一致");
        assert_eq!(field(&mixed, "lhs_dims"), "m");
        assert_eq!(field(&mixed, "rhs_dims"), "currency");

        // 货币维内对比：异币种同维（USD vs yuan）→ consistent=true
        let cur = DimensionalCheckService
            .execute(&args(&[
                ("lhs", JsonValue::string("USD")),
                ("rhs", JsonValue::string("yuan")),
            ]))
            .expect("货币维对比应成功");
        assert!(bool_of(&cur, "consistent"), "异币种同货币维应判一致");
        assert_eq!(field(&cur, "lhs_dims"), "currency");
        assert_eq!(field(&cur, "rhs_dims"), "currency");

        // 空白容忍 + 额外键静默忽略（立法指令透传 target 不影响）
        let ws = DimensionalCheckService
            .execute(&args(&[
                ("lhs", JsonValue::string(" CNY*s^-1 ")),
                ("rhs", JsonValue::string("yuan/s")),
                ("target", JsonValue::string("payload.verdict")),
            ]))
            .expect("带额外键的对比应成功");
        assert!(bool_of(&ws, "consistent"));
        assert_eq!(field(&ws, "lhs_dims"), "s^-1*currency");
    }

    /// fail-visible：缺参 / 单位表达式不可解析均显式报错带自诊断指引。
    #[test]
    fn dimensional_check_fail_visible_errors() {
        let missing = DimensionalCheckService.execute(&args(&[("lhs", JsonValue::string("m"))]));
        let err = missing.expect_err("缺 rhs 应报错");
        assert!(err.contains("缺少参数 rhs"), "err = {err}");

        let unknown = DimensionalCheckService
            .execute(&args(&[
                ("lhs", JsonValue::string("newtn")),
                ("rhs", JsonValue::string("N")),
            ]))
            .expect_err("未知单位应报错");
        assert!(
            unknown.contains("未知单位") && unknown.contains("lhs"),
            "err = {unknown}"
        );

        let additive = DimensionalCheckService
            .execute(&args(&[
                ("lhs", JsonValue::string("m + s")),
                ("rhs", JsonValue::string("m")),
            ]))
            .expect_err("加减操作分量纲不等应报错");
        assert!(
            additive.contains("加减两侧操作分量纲不一致"),
            "err = {additive}"
        );
    }
}
