//! 原生服务实现：治理数学四服务（JSON 入 / JSON 出，统一信封 v1）。
//!
//! # 确定性
//! 全程有理数/定点表示（epoch 用 i64），无浮点、无时钟、无随机；
//! 舍入只发生在输出端且 `scale`/`rounding` 必填（杜绝隐式舍入）。
//! 同输入两次执行结果逐位一致。
//!
//! # 语义要点（逐服务）
//! - `gov_ratio_split`：份额ᵢ = total·wᵢ/Σw（exact 模式纯精确）；
//!   integer 模式=最大余数法（先取下整，余数按降序逐席进位，并列取
//!   下标序——确定性打破并列），保证 Σ份额 = total；
//! - `gov_aggregate`：median 排序后偶数个取中间两数平均（精确）；
//! - `gov_window_duration`：end−start（i64 checked，溢出 fail-visible）；
//! - `gov_window_overlap`：overlap = max(0, min(e₁,e₂) − max(s₁,s₂))；
//!   关系标签按完全含/恒等/相交/邻接/分离判定（邻接=overlap 为零且
//!   端点相触）。

#![forbid(unsafe_code)]

use std::sync::Arc;

use evorule_numeric_services::numcore::{decimal_string, parse_rounding, round_to_scale, Rounding};
use evorule_numeric_services::services::parse_value_as_rational;
use evorule_plugin_kit::NativeService;
use evorule_reactor::IoResult;
use evorule_tcb::JsonValue;
use num_bigint::BigInt;
use num_rational::BigRational;
use num_traits::{ToPrimitive, Zero};

pub type Q = BigRational;

fn int_const(n: i64) -> Q {
    Q::from(BigInt::from(n))
}

// ============================================================================
// 参数提取助手（全部 fail-visible 带自诊断指引）
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

fn req_i64(args: &JsonValue, key: &str) -> Result<i64, String> {
    match args.get(key) {
        Some(JsonValue::Integer(n)) => Ok(*n),
        Some(_) => Err(format!("参数 {key} 须为整数")),
        None => Err(format!(
            "缺少参数 {key} — 自诊断指引: 见服务 parameters 契约"
        )),
    }
}

fn req_scale(args: &JsonValue) -> Result<i32, String> {
    let s = req_i64(args, "scale")?;
    i32::try_from(s).map_err(|_| format!("参数 scale={s} 超出允许范围"))
}

fn req_rounding(args: &JsonValue) -> Result<Rounding, String> {
    parse_rounding(req_str(args, "rounding")?)
}

fn envelope(pairs: &[(&str, JsonValue)]) -> JsonValue {
    let mut all: Vec<(&str, JsonValue)> = vec![
        ("status", JsonValue::string("determined")),
        ("determinism", JsonValue::string("bitwise_replayable")),
    ];
    all.extend_from_slice(pairs);
    JsonValue::object_from_pairs(&all)
}

fn dec_obj(q: &Q, scale: i32, mode: Rounding) -> Result<JsonValue, String> {
    let (unscaled, scale) = round_to_scale(q, scale, mode)?;
    Ok(JsonValue::object_from_pairs(&[
        ("unscaled", JsonValue::string(unscaled.to_string())),
        ("scale", JsonValue::Integer(scale as i64)),
        (
            "decimal_string",
            JsonValue::string(decimal_string(&unscaled, scale)),
        ),
    ]))
}

fn q_str(q: &Q) -> JsonValue {
    JsonValue::string(q.to_string())
}

fn req_rational(args: &JsonValue, key: &str) -> Result<Q, String> {
    match args.get(key) {
        Some(v) => v
            .as_str()
            .map(parse_value_as_rational)
            .unwrap_or_else(|| Err(format!("参数 {key} 须为字符串编码数值"))),
        None => Err(format!(
            "缺少参数 {key} — 自诊断指引: 见服务 parameters 契约"
        )),
    }
}

/// 字符串数组参数（十进制字面量或 a/b 分数）。
fn req_rational_array(args: &JsonValue, key: &str) -> Result<Vec<Q>, String> {
    match args.get(key) {
        Some(JsonValue::Array(items)) => {
            if items.is_empty() {
                return Err(format!("参数 {key} 为空数组 — 自诊断指引: 至少 1 项"));
            }
            items
                .iter()
                .enumerate()
                .map(|(i, v)| {
                    v.as_str()
                        .ok_or_else(|| format!("{key}[{i}] 须为字符串编码数值"))
                        .and_then(|s| {
                            parse_value_as_rational(s)
                                .map_err(|e| format!("{key}[{i}] 解析失败: {e}"))
                        })
                })
                .collect()
        }
        Some(_) => Err(format!("参数 {key} 须为字符串数组")),
        None => Err(format!(
            "缺少参数 {key} — 自诊断指引: 见服务 parameters 契约"
        )),
    }
}

// ============================================================================
// gov_ratio_split — 比例分配（最大余数法）
// ============================================================================

struct RatioSplitService;

impl NativeService for RatioSplitService {
    fn execute(&self, args: &JsonValue) -> IoResult {
        let total = req_rational(args, "total")?;
        let weights = req_rational_array(args, "weights")?;
        let mode = req_str(args, "mode")?;
        if !matches!(mode, "exact" | "integer") {
            return Err(format!(
                "未知 mode '{mode}' — 自诊断指引: 合法值 exact/integer"
            ));
        }
        if total < int_const(0) {
            return Err("参数 total 须为非负 — 自诊断指引: 负总量无分配语义".to_string());
        }
        if weights.iter().any(|w| w < &int_const(0)) {
            return Err("权重须为非负 — 自诊断指引: 负权重无治理语义".to_string());
        }
        let weight_sum = weights.iter().cloned().sum::<Q>();
        if weight_sum.is_zero() {
            return Err("全零权重 — 自诊断指引: Σw = 0 时份额无定义,至少给一个正权重".to_string());
        }
        let scale = req_scale(args)?;
        let rounding = req_rounding(args)?;

        // 精确有理份额ᵢ = total·wᵢ/Σw。
        let exact: Vec<Q> = weights
            .iter()
            .map(|w| total.clone() * w.clone() / weight_sum.clone())
            .collect();

        let out = match mode {
            "exact" => {
                let shares: Vec<JsonValue> = exact
                    .iter()
                    .map(|q| {
                        dec_obj(q, scale, rounding).unwrap_or_else(|_| JsonValue::string("ERR"))
                    })
                    .collect();
                vec![
                    ("mode", JsonValue::string(mode)),
                    ("shares", JsonValue::Array(shares)),
                    (
                        "shares_exact",
                        JsonValue::Array(exact.iter().map(q_str).collect()),
                    ),
                    ("weight_sum", q_str(&weight_sum)),
                ]
            }
            "integer" => {
                // 最大余数法要求可分总量为非负整数、份额取整。
                let total_int = total.to_integer().to_i64().ok_or_else(|| {
                    "total 超出 i64 范围 — 自诊断指引: integer 模式总量为 i64".to_string()
                })?;
                if total != int_const(total_int) {
                    return Err(format!(
                        "integer 模式要求 total 为非负整数,得到 {total} — 自诊断指引: \
                         分数总量请用 exact 模式"
                    ));
                }
                // 下整 + 余数（exact_i − floor_i,全部 ∈ [0,1)）。
                let floors: Vec<i64> = exact
                    .iter()
                    .map(|q| q.floor().to_integer().to_i64().unwrap_or(0))
                    .collect();
                let mut allocated: i64 = floors.iter().sum();
                let remainders: Vec<(usize, Q)> = exact
                    .iter()
                    .enumerate()
                    .map(|(i, q)| (i, q.clone() - int_const(floors[i])))
                    .collect();
                // 余数降序,并列取下标序（确定性打破并列）。
                let mut order: Vec<usize> = (0..exact.len()).collect();
                order.sort_by(|&a, &b| remainders[b].1.cmp(&remainders[a].1));
                let mut shares = floors;
                let mut i = 0;
                while allocated < total_int {
                    shares[order[i % order.len()]] += 1;
                    allocated += 1;
                    i += 1;
                }
                let share_vals: Vec<JsonValue> = shares
                    .iter()
                    .map(|s| {
                        dec_obj(&int_const(*s), scale, rounding)
                            .unwrap_or_else(|_| JsonValue::string("ERR"))
                    })
                    .collect();
                vec![
                    ("mode", JsonValue::string(mode)),
                    ("shares", JsonValue::Array(share_vals)),
                    (
                        "shares_exact",
                        JsonValue::Array(exact.iter().map(q_str).collect()),
                    ),
                    (
                        "sum_check",
                        JsonValue::Bool(shares.iter().sum::<i64>() == total_int),
                    ),
                    ("weight_sum", q_str(&weight_sum)),
                    (
                        "note",
                        JsonValue::string(
                            "最大余数法: 下整后余数降序逐席进位(并列取下标序),Σ份额恰为 total",
                        ),
                    ),
                ]
            }
            _ => unreachable!("mode 已校验"),
        };
        Ok(envelope(&out))
    }
}

pub fn mk_ratio_split() -> Arc<dyn NativeService> {
    Arc::new(RatioSplitService)
}

// ============================================================================
// gov_aggregate — 数组聚合
// ============================================================================

struct AggregateService;

impl NativeService for AggregateService {
    fn execute(&self, args: &JsonValue) -> IoResult {
        let values = req_rational_array(args, "values")?;
        let op = req_str(args, "op")?;
        let scale = req_scale(args)?;
        let rounding = req_rounding(args)?;
        let n = int_const(values.len() as i64);
        let result = match op {
            "sum" => values.iter().cloned().sum::<Q>(),
            "avg" => values.iter().cloned().sum::<Q>() / n,
            "median" => {
                let mut sorted = values.clone();
                sorted.sort();
                let len = sorted.len();
                if len % 2 == 1 {
                    sorted[len / 2].clone()
                } else {
                    (sorted[len / 2 - 1].clone() + sorted[len / 2].clone()) / int_const(2)
                }
            }
            "min" => values
                .iter()
                .cloned()
                .min()
                .ok_or_else(|| "空数组".to_string())?,
            "max" => values
                .iter()
                .cloned()
                .max()
                .ok_or_else(|| "空数组".to_string())?,
            other => {
                return Err(format!(
                    "未知 op '{other}' — 自诊断指引: 合法值 sum/avg/median/min/max"
                ))
            }
        };
        let out = vec![
            ("op", JsonValue::string(op)),
            ("value", dec_obj(&result, scale, rounding)?),
            ("value_exact", q_str(&result)),
            ("count", JsonValue::Integer(values.len() as i64)),
        ];
        Ok(envelope(&out))
    }
}

pub fn mk_aggregate() -> Arc<dyn NativeService> {
    Arc::new(AggregateService)
}

// ============================================================================
// gov_window_duration — 时长差
// ============================================================================

struct WindowDurationService;

impl NativeService for WindowDurationService {
    fn execute(&self, args: &JsonValue) -> IoResult {
        let end = req_i64(args, "end")?;
        let start = req_i64(args, "start")?;
        let scale = req_scale(args)?;
        let rounding = req_rounding(args)?;
        let duration = end
            .checked_sub(start)
            .ok_or_else(|| "时长差溢出 i64 — 自诊断指引: epoch 值差距过大".to_string())?;
        let out = vec![
            ("duration", dec_obj(&int_const(duration), scale, rounding)?),
            ("duration_exact", q_str(&int_const(duration))),
            (
                "note",
                JsonValue::string("end−start,epoch 秒;负值=end<start 的输入事实,如实输出"),
            ),
        ];
        Ok(envelope(&out))
    }
}

pub fn mk_window_duration() -> Arc<dyn NativeService> {
    Arc::new(WindowDurationService)
}

// ============================================================================
// gov_window_overlap — 区间对判定
// ============================================================================

struct WindowOverlapService;

impl NativeService for WindowOverlapService {
    fn execute(&self, args: &JsonValue) -> IoResult {
        let s1 = req_rational(args, "s1")?;
        let e1 = req_rational(args, "e1")?;
        let s2 = req_rational(args, "s2")?;
        let e2 = req_rational(args, "e2")?;
        if e1 < s1 {
            return Err("区间 1 非法: e1 < s1 — 自诊断指引: 止点须不小于起点".to_string());
        }
        if e2 < s2 {
            return Err("区间 2 非法: e2 < s2 — 自诊断指引: 止点须不小于起点".to_string());
        }
        let scale = req_scale(args)?;
        let rounding = req_rounding(args)?;
        let lo = s1.clone().max(s2.clone());
        let hi = e1.clone().min(e2.clone());
        let overlap_raw = hi.clone() - lo.clone();
        let overlap = if overlap_raw < int_const(0) {
            int_const(0)
        } else {
            overlap_raw.clone()
        };
        // 关系标签（确定性判定序:恒等→包含→相交→邻接→分离）。
        let relation = if s1 == s2 && e1 == e2 {
            "identical"
        } else if s1 >= s2 && e1 <= e2 {
            "contained_1_in_2"
        } else if s2 >= s1 && e2 <= e1 {
            "contained_2_in_1"
        } else if overlap > int_const(0) {
            "overlap"
        } else if lo == hi {
            "adjacent"
        } else {
            "no_overlap"
        };
        let out = vec![
            ("overlap", dec_obj(&overlap, scale, rounding)?),
            ("overlap_exact", q_str(&overlap)),
            ("relation", JsonValue::string(relation)),
            (
                "note",
                JsonValue::string(
                    "overlap = max(0, min(e₁,e₂) − max(s₁,s₂));端点相触且无重叠 = adjacent",
                ),
            ),
        ];
        Ok(envelope(&out))
    }
}

pub fn mk_window_overlap() -> Arc<dyn NativeService> {
    Arc::new(WindowOverlapService)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    fn args(pairs: &[(&str, JsonValue)]) -> JsonValue {
        JsonValue::object_from_pairs(pairs)
    }

    fn arr(values: &[&str]) -> JsonValue {
        JsonValue::Array(values.iter().map(|v| JsonValue::string(*v)).collect())
    }

    fn dec_of<'a>(out: &'a JsonValue, key: &str) -> &'a str {
        out.get(key)
            .and_then(|v| v.get("decimal_string"))
            .and_then(|v| v.as_str())
            .unwrap_or_else(|| panic!("{key}.decimal_string 应存在"))
    }

    fn rounding() -> JsonValue {
        JsonValue::string("half_even")
    }

    /// 黄金向量：total=100 三等分 — exact 各 100/3；integer 最大余数法
    /// → [34,33,33]（余数降序进位,Σ=100）；全零权重 fail-visible。
    #[test]
    fn ratio_split_golden() {
        let ws = JsonValue::Array(vec![
            JsonValue::string("1"),
            JsonValue::string("1"),
            JsonValue::string("1"),
        ]);
        let out = RatioSplitService
            .execute(&args(&[
                ("total", JsonValue::string("100")),
                ("weights", ws.clone()),
                ("mode", JsonValue::string("exact")),
                ("scale", JsonValue::Integer(4)),
                ("rounding", rounding()),
            ]))
            .expect("exact 应成功");
        let shares = out.get("shares").and_then(|v| v.as_array()).unwrap();
        assert_eq!(shares.len(), 3);
        assert_eq!(
            shares[0].get("decimal_string").and_then(|v| v.as_str()),
            Some("33.3333")
        );
        let out = RatioSplitService
            .execute(&args(&[
                ("total", JsonValue::string("100")),
                ("weights", ws),
                ("mode", JsonValue::string("integer")),
                ("scale", JsonValue::Integer(0)),
                ("rounding", rounding()),
            ]))
            .expect("integer 应成功");
        let shares = out.get("shares").and_then(|v| v.as_array()).unwrap();
        let ints: Vec<i64> = shares
            .iter()
            .map(|s| {
                s.get("unscaled")
                    .and_then(|v| v.as_str())
                    .unwrap()
                    .parse()
                    .unwrap()
            })
            .collect();
        assert_eq!(ints, vec![34, 33, 33], "余数降序进位(下标 0 先)");
        assert_eq!(out.get("sum_check").and_then(|v| v.as_bool()), Some(true));
        // 全零权重 fail-visible。
        let err = RatioSplitService
            .execute(&args(&[
                ("total", JsonValue::string("100")),
                (
                    "weights",
                    JsonValue::Array(vec![JsonValue::string("0"), JsonValue::string("0")]),
                ),
                ("mode", JsonValue::string("exact")),
                ("scale", JsonValue::Integer(2)),
                ("rounding", rounding()),
            ]))
            .expect_err("全零权重应报错");
        assert!(err.contains("全零权重"), "报错应点名全零权重: {err}");
    }

    /// 黄金向量：[1,2,3,4] — sum 10 / avg 5/2 / median 5/2（偶数取平均）。
    #[test]
    fn aggregate_golden() {
        let vals = arr(&["1", "2", "3", "4"]);
        for (op, expected) in [
            ("sum", "10.00"),
            ("avg", "2.50"),
            ("median", "2.50"),
            ("min", "1.00"),
            ("max", "4.00"),
        ] {
            let out = AggregateService
                .execute(&args(&[
                    ("values", vals.clone()),
                    ("op", JsonValue::string(op)),
                    ("scale", JsonValue::Integer(2)),
                    ("rounding", rounding()),
                ]))
                .unwrap_or_else(|e| panic!("{op} 应成功: {e}"));
            assert_eq!(dec_of(&out, "value"), expected, "{op} 黄金值");
        }
    }

    /// 黄金向量：epoch 时长差 end−start = 100 s；负值如实输出。
    #[test]
    fn duration_golden() {
        let out = WindowDurationService
            .execute(&args(&[
                ("end", JsonValue::Integer(1_700_000_100)),
                ("start", JsonValue::Integer(1_700_000_000)),
                ("scale", JsonValue::Integer(0)),
                ("rounding", rounding()),
            ]))
            .expect("时长差应成功");
        assert_eq!(dec_of(&out, "duration"), "100");
        let out = WindowDurationService
            .execute(&args(&[
                ("end", JsonValue::Integer(1_700_000_000)),
                ("start", JsonValue::Integer(1_700_000_050)),
                ("scale", JsonValue::Integer(0)),
                ("rounding", rounding()),
            ]))
            .expect("负时长应成功");
        assert_eq!(dec_of(&out, "duration"), "-50", "负值如实输出");
    }

    /// 黄金向量：[0,10]×[5,15] → overlap 5 / 相交；
    /// [0,10]×[10,20] → adjacent；[0,5]×[2,3] → contained_2_in_1。
    #[test]
    fn overlap_golden() {
        let case = |s1: &str, e1: &str, s2: &str, e2: &str| {
            WindowOverlapService.execute(&args(&[
                ("s1", JsonValue::string(s1)),
                ("e1", JsonValue::string(e1)),
                ("s2", JsonValue::string(s2)),
                ("e2", JsonValue::string(e2)),
                ("scale", JsonValue::Integer(2)),
                ("rounding", rounding()),
            ]))
        };
        let out = case("0", "10", "5", "15").expect("相交用例应成功");
        assert_eq!(dec_of(&out, "overlap"), "5.00");
        assert_eq!(
            out.get("relation").and_then(|v| v.as_str()),
            Some("overlap")
        );
        let out = case("0", "10", "10", "20").expect("邻接用例应成功");
        assert_eq!(
            out.get("relation").and_then(|v| v.as_str()),
            Some("adjacent")
        );
        let out = case("0", "5", "2", "3").expect("包含用例应成功");
        assert_eq!(
            out.get("relation").and_then(|v| v.as_str()),
            Some("contained_2_in_1")
        );
        assert_eq!(dec_of(&out, "overlap"), "1.00");
        // e<s fail-visible。
        let err = case("10", "0", "0", "5").expect_err("e1<s1 应报错");
        assert!(err.contains("非法"), "报错应点名区间非法: {err}");
        let _ = int_const(0);
    }
}
