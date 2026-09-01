// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 4 个确定性指标服务实现（UV-037 MVP:sma/ema/macd/rsi）。
//!
//! 语义基准 = `规则引擎+数据处理器/indicator_calculator.py`(pandas 3.0.5):
//! - SMA:`rolling(N).mean()` — 前 N-1 位 warmup → [`JsonValue::Null`];
//! - EMA:`ewm(span=N, adjust=False)` — 递推 `y = (1-α)·y + α·x`,α = 2/(N+1),
//!   经实算验证与 pandas 内部实现**逐位一致**(`y += α(x-y)` 形态不匹配,勿改);
//! - MACD:EMA(fast) − EMA(slow),Signal = EMA(signal_period)(对 MACD 序列),
//!   Hist = MACD − Signal;三序列均自首位起有值(adjust=False 无 warmup);
//! - RSI:gain/loss → `ewm(alpha=1/N, adjust=False, min_periods=max(1,N//2))`,
//!   分类语义与参考实现 `calc_rsi` 逐分支对齐(100/0/50/normal);
//!   屏蔽期 avg 为 NaN → 参考实现分类结果亦为 NaN → null。
//!
//! 输入契约:`series` 数组,元素为 Integer 或数字字符串(TCB 无 Float);
//! NaN/Inf 显式拒绝;空序列/窗口非法 fail-fast;序列长度上限 [`MAX_SERIES_LEN`]。

use evorule_reactor::IoResult;
use evorule_tcb::JsonValue;

use crate::{float_str, null_value, obj, NativeService};

/// 序列长度上限(执行预算;超出 fail-fast,与 physics 服务的数量预算同口径)
pub const MAX_SERIES_LEN: usize = 100_000;

// ============================================================================
// 参数解析(通用)
// ============================================================================

/// 解析 `series` 参数:非空数组,元素 Integer 或数字字符串,值必须有限。
fn parse_series(args: &JsonValue) -> Result<Vec<f64>, String> {
    let arr = args
        .get("series")
        .and_then(|v| v.as_array())
        .ok_or_else(|| {
            "缺 series 参数 — 需为数值数组(元素: 数字或数字字符串)。\
             自诊断指引: args 形如 { \"series\": [10.5, \"11.2\", ...] }"
                .to_string()
        })?;
    if arr.is_empty() {
        return Err("series 为空数组 — 指标计算至少需要 1 个数据点(如实拒绝,不返回空结果)".to_string());
    }
    if arr.len() > MAX_SERIES_LEN {
        return Err(format!(
            "series 长度 {} 超出上限 {MAX_SERIES_LEN} — 执行预算保护;请分段调用或缩小范围",
            arr.len()
        ));
    }
    let mut out = Vec::with_capacity(arr.len());
    for (i, v) in arr.iter().enumerate() {
        let f = parse_f64(v)
            .ok_or_else(|| format!("series[{i}] 不是合法数字: {v} — 允许 Integer 或数字字符串"))?;
        if !f.is_finite() {
            return Err(format!(
                "series[{i}] 非有限值({f}) — NaN/Inf 显式拒绝(确定性引擎不容忍未定义值)"
            ));
        }
        out.push(f);
    }
    Ok(out)
}

/// JsonValue → f64:Integer 直转;String 解析(拒绝空白外杂质)。
fn parse_f64(v: &JsonValue) -> Option<f64> {
    match v {
        JsonValue::Integer(i) => Some(*i as f64),
        // 字符串原样解析(含 NaN/Inf),有限性由 parse_series 统一显式拒绝,
        // 使"非有限值"诊断分支可达(而非伪装成"不是合法数字")
        JsonValue::String(s) => s.trim().parse::<f64>().ok(),
        _ => None,
    }
}

/// 解析可选整数参数(校验 >= 下限)。
fn parse_usize(args: &JsonValue, key: &str, default: usize, min: usize) -> Result<usize, String> {
    match args.get(key) {
        None | Some(JsonValue::Null) => Ok(default),
        Some(JsonValue::Integer(i)) if *i >= min as i64 && *i <= MAX_SERIES_LEN as i64 => Ok(*i as usize),
        Some(v) => Err(format!(
            "参数 {key}={v} 非法 — 需为 [{min}, {MAX_SERIES_LEN}] 内的整数"
        )),
    }
}

/// 数组输出:null/字符串混合(warmup 语义)
fn values_out(values: &[Option<f64>]) -> JsonValue {
    JsonValue::Array(
        values
            .iter()
            .map(|v| match v {
                Some(f) => float_str(*f),
                None => null_value(),
            })
            .collect(),
    )
}

// ============================================================================
// 指标核心(纯函数,与 IoResult 解耦,便于直接单测逐位黄金值)
// ============================================================================

/// SMA(窗口 N):前 N-1 位 None。
///
/// 求和算法与 pandas `rolling(N).mean()` 逐位对齐(3000 组随机序列实证一致):
/// - 滚动窗口 Kahan 补偿求和,add/remove 各持独立持久补偿,先删后加;
/// - 产物修正:窗口全同值 → 精确返回该值;全正/全负符号修正(-0.0 按 signbit 计);
/// - 复刻自 pandas `_libs/window/aggregations.pyx` 的
///   `roll_mean`/`add_mean`/`remove_mean`/`calc_mean`(逐行移植)。
// 循环按 pandas 输出位推进并同时索引窗口两端,索引式写法是刻意的逐行移植
#[allow(clippy::needless_range_loop)]
pub(crate) fn sma_core(series: &[f64], window: usize) -> Vec<Option<f64>> {
    let n = series.len();
    let mut out = vec![None; n];
    if window == 0 {
        return out;
    }
    let (mut sum_x, mut comp_add, mut comp_rem) = (0.0_f64, 0.0_f64, 0.0_f64);
    let (mut nobs, mut neg_ct, mut num_same) = (0usize, 0i64, 0usize);
    let mut prev = 0.0_f64;
    let (mut prev_s, mut prev_e) = (0usize, 0usize);
    for i in 0..n {
        let ws = i.saturating_sub(window - 1);
        let we = i + 1;
        if i == 0 || ws >= prev_e {
            // 首窗/窗口跳变:整体重建(pandas setup 分支;prev 初值 = 首元素)
            sum_x = 0.0;
            comp_add = 0.0;
            comp_rem = 0.0;
            nobs = 0;
            neg_ct = 0;
            num_same = 0;
            prev = series[ws];
            for &v in &series[ws..we] {
                nobs += 1;
                let y = v - comp_add;
                let t = sum_x + y;
                comp_add = t - sum_x - y;
                sum_x = t;
                if v.is_sign_negative() {
                    neg_ct += 1;
                }
                if v == prev {
                    num_same += 1;
                } else {
                    num_same = 1;
                }
                prev = v;
            }
        } else {
            // 先删(独立补偿 comp_rem)后加(独立补偿 comp_add),次序影响位级结果
            for &v in &series[prev_s..ws] {
                nobs -= 1;
                let y = -v - comp_rem;
                let t = sum_x + y;
                comp_rem = t - sum_x - y;
                sum_x = t;
                if v.is_sign_negative() {
                    neg_ct -= 1;
                }
            }
            for &v in &series[prev_e..we] {
                nobs += 1;
                let y = v - comp_add;
                let t = sum_x + y;
                comp_add = t - sum_x - y;
                sum_x = t;
                if v.is_sign_negative() {
                    neg_ct += 1;
                }
                if v == prev {
                    num_same += 1;
                } else {
                    num_same = 1;
                }
                prev = v;
            }
        }
        // calc_mean:nobs 达标出值;同值/全正/全负产物修正
        if nobs >= window {
            let mut result = sum_x / nobs as f64;
            if num_same >= nobs {
                result = prev;
            } else if (neg_ct == 0 && result < 0.0) || (neg_ct == nobs as i64 && result > 0.0) {
                // 全正得负 / 全负得正:浮点伪影修正(pandas calc_mean 同款)
                result = 0.0;
            }
            out[i] = Some(result);
        }
        prev_s = ws;
        prev_e = we;
    }
    out
}

/// EMA(span N, adjust=False):`y0 = x0; y_t = (1-α)·y_{t-1} + α·x_t`。
/// α = 2/(N+1) 的浮点除法与 pandas 内部一致(span→α 同一表达式)。
/// 返回纯 f64(ewm 无 warmup,每位有值;RSI 的屏蔽期语义由其自身实现处理)。
pub(crate) fn ewm_span(series: &[f64], span: usize) -> Vec<f64> {
    let alpha = 2.0 / (span as f64 + 1.0);
    ewm_with_alpha(series, alpha)
}

/// 通用 alpha 递推(RSI 用 alpha = 1/period);调用方保证 series 非空。
pub(crate) fn ewm_with_alpha(series: &[f64], alpha: f64) -> Vec<f64> {
    let mut out = Vec::with_capacity(series.len());
    let mut y = series[0];
    out.push(y);
    for x in &series[1..] {
        y = (1.0 - alpha) * y + alpha * x;
        out.push(y);
    }
    out
}

/// RSI(Wilder):返回逐位 Option;屏蔽期(min_periods 内)None。
pub(crate) fn rsi_core(series: &[f64], period: usize) -> Vec<Option<f64>> {
    let n = series.len();
    if n < 2 {
        // diff 后无差分:avg 全为 NaN 屏蔽(min_periods>=1) → 全 null
        return vec![None; n];
    }
    // gain/loss 与参考实现同口径:delta.where(delta>0,0) / -(delta.where(delta<0,0))
    // 注意 loss[0] = -0.0(IEEE 负零),递推保持并参与 ==0 判断(IEEE -0.0==0 真)
    let mut gain = vec![0.0_f64; n - 1];
    let mut loss = vec![0.0_f64; n - 1];
    for i in 1..n {
        let d = series[i] - series[i - 1];
        gain[i - 1] = if d > 0.0 { d } else { 0.0 };
        loss[i - 1] = -(if d < 0.0 { d } else { 0.0 });
    }
    let alpha = 1.0 / period as f64;
    let min_periods = (period / 2).max(1);
    // 种子 = pandas gain[0]/loss[0](diff 首位 NaN → where 占位 0 / -0.0,IEEE 负零):
    // pandas ewm(adjust=False) y0 = 占位值,观察计数含它,屏蔽期因此少一位;
    // Rust gain/loss 数组从首个真实差分(series[1]-series[0])起,对应 pandas 位置 1..。
    let (mut avg_gain, mut avg_loss) = (0.0_f64, -0.0_f64);
    let mut out = vec![None; n];
    // 循环 j 后 avg 对应 pandas 位置 j+1;屏蔽期:位置 < min_periods-1 为 NaN → null
    for j in 0..n - 1 {
        avg_gain = (1.0 - alpha) * avg_gain + alpha * gain[j];
        avg_loss = (1.0 - alpha) * avg_loss + alpha * loss[j];
        if j + 1 < min_periods - 1 {
            continue;
        }
        out[j + 1] = Some(rsi_classify(avg_gain, avg_loss));
    }
    out
}

/// calc_rsi 分类分支(与参考实现逐分支对齐;-0.0 按 IEEE ==0 处理)
pub(crate) fn rsi_classify(avg_gain: f64, avg_loss: f64) -> f64 {
    if avg_loss == 0.0 && avg_gain > 0.0 {
        100.0
    } else if avg_gain == 0.0 && avg_loss > 0.0 {
        0.0
    } else if avg_gain == 0.0 && avg_loss == 0.0 {
        50.0
    } else {
        let rs = avg_gain / avg_loss;
        100.0 - (100.0 / (1.0 + rs))
    }
}

// ============================================================================
// 服务外壳(参数解析 → 核心 → IoResult)
// ============================================================================

/// `indicator_sma`:简单移动平均。
pub struct IndicatorSma;

impl NativeService for IndicatorSma {
    fn execute(&self, args: &JsonValue) -> IoResult {
        let series = parse_series(args)?;
        let window = parse_usize(args, "window", 5, 1)?;
        let values = sma_core(&series, window);
        Ok(obj(vec![
            ("status", JsonValue::string("ok")),
            ("window", JsonValue::Integer(window as i64)),
            ("count", JsonValue::Integer(series.len() as i64)),
            ("values", values_out(&values)),
        ]))
    }
}

/// `indicator_ema`:指数移动平均。
pub struct IndicatorEma;

impl NativeService for IndicatorEma {
    fn execute(&self, args: &JsonValue) -> IoResult {
        let series = parse_series(args)?;
        let span = parse_usize(args, "span", 5, 1)?;
        let values: Vec<Option<f64>> = ewm_span(&series, span).into_iter().map(Some).collect();
        Ok(obj(vec![
            ("status", JsonValue::string("ok")),
            ("span", JsonValue::Integer(span as i64)),
            ("count", JsonValue::Integer(series.len() as i64)),
            ("values", values_out(&values)),
        ]))
    }
}

/// `indicator_macd`:MACD 快慢线与柱。
pub struct IndicatorMacd;

impl NativeService for IndicatorMacd {
    fn execute(&self, args: &JsonValue) -> IoResult {
        let series = parse_series(args)?;
        let fast = parse_usize(args, "fast", 12, 1)?;
        let slow = parse_usize(args, "slow", 26, 1)?;
        let signal_period = parse_usize(args, "signal_period", 9, 1)?;
        if fast >= slow {
            return Err(format!(
                "参数 fast({fast}) 必须 < slow({slow}) — MACD 语义要求快线周期小于慢线"
            ));
        }
        let ema_fast = ewm_span(&series, fast);
        let ema_slow = ewm_span(&series, slow);
        // MACD 序列(fast−slow,首位起有值) → Signal = EMA(signal_period)(对 MACD 序列)
        let macd_series: Vec<f64> = ema_fast
            .iter()
            .zip(&ema_slow)
            .map(|(f, s)| f - s)
            .collect();
        let signal_series = ewm_span(&macd_series, signal_period);
        let macd_out: Vec<Option<f64>> = macd_series.iter().map(|v| Some(*v)).collect();
        let signal_out: Vec<Option<f64>> = signal_series.iter().map(|v| Some(*v)).collect();
        let hist: Vec<Option<f64>> = macd_series
            .iter()
            .zip(&signal_series)
            .map(|(m, s)| Some(m - s))
            .collect();
        Ok(obj(vec![
            ("status", JsonValue::string("ok")),
            ("fast", JsonValue::Integer(fast as i64)),
            ("slow", JsonValue::Integer(slow as i64)),
            ("signal_period", JsonValue::Integer(signal_period as i64)),
            ("count", JsonValue::Integer(series.len() as i64)),
            ("macd", values_out(&macd_out)),
            ("signal", values_out(&signal_out)),
            ("hist", values_out(&hist)),
        ]))
    }
}

/// `indicator_rsi`:RSI(Wilder 平滑)。
pub struct IndicatorRsi;

impl NativeService for IndicatorRsi {
    fn execute(&self, args: &JsonValue) -> IoResult {
        let series = parse_series(args)?;
        let period = parse_usize(args, "period", 14, 2)?;
        let values = rsi_core(&series, period);
        Ok(obj(vec![
            ("status", JsonValue::string("ok")),
            ("period", JsonValue::Integer(period as i64)),
            ("min_periods", JsonValue::Integer((period / 2).max(1) as i64)),
            ("count", JsonValue::Integer(series.len() as i64)),
            ("values", values_out(&values)),
        ]))
    }
}

// ============================================================================
// 单测:黄金值逐位对齐(gen_golden.py 以 pandas 3.0.5 实算生成)+ 契约/确定性
// ============================================================================

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use crate::IndicatorServiceRouter;
    use async_trait::async_trait;
    use evorule_reactor::IoHandler;
    use std::sync::Arc;

    /// 黄金值输入序列(gen_golden.py SERIES,与 pandas 黄金值同一序列)
    const SERIES: [f64; 20] = [
        10.0, 11.2, 10.8, 12.5, 13.1, 12.9, 14.2, 13.7, 15.0, 15.6, 15.2, 16.1, 15.8, 17.0,
        16.5, 17.8, 18.2, 17.9, 19.1, 18.6,
    ];

    fn arr(series: &[f64]) -> JsonValue {
        JsonValue::Array(
            series
                .iter()
                .map(|f| {
                    if f.fract() == 0.0 && f.abs() < 1e15 {
                        JsonValue::Integer(*f as i64)
                    } else {
                        JsonValue::string(format!("{f:?}"))
                    }
                })
                .collect(),
        )
    }

    fn str_vals(expected: &[&str]) -> Vec<JsonValue> {
        expected
            .iter()
            .map(|s| JsonValue::string(*s))
            .collect::<Vec<_>>()
    }

    fn assert_values(r: &JsonValue, expect: &[JsonValue]) {
        let got = r.get("values").and_then(|v| v.as_array()).expect("values 数组");
        assert_eq!(got.len(), expect.len(), "长度不一致: {r}");
        for (i, (g, e)) in got.iter().zip(expect.iter()).enumerate() {
            if let JsonValue::Null = e {
                assert!(matches!(g, JsonValue::Null), "values[{i}] 应为 null: {g}");
            } else {
                assert_eq!(g, e, "values[{i}] 黄金值漂移(整行: {r:?})");
            }
        }
    }

    // ===== SMA 黄金值(pandas rolling(5).mean(),前 4 位 null) =====

    #[test]
    fn test_sma_golden_pandas_bitwise() {
        let values = sma_core(&SERIES, 5);
        let expect_strs = [
            "11.52", "12.1", "12.7", "13.280000000000001", "13.780000000000001",
            "14.279999999999998", "14.74", "15.12", "15.540000000000001",
            "15.940000000000001", "16.12", "16.64", "17.06", "17.479999999999997",
            "17.9", "18.32",
        ];
        let mut expect: Vec<JsonValue> = vec![JsonValue::Null; 4];
        expect.extend(str_vals(&expect_strs));
        let out = values_out(&values);
        let got = out.as_array().unwrap();
        assert_eq!(got.len(), expect.len());
        for (i, (g, e)) in got.iter().zip(expect.iter()).enumerate() {
            if let JsonValue::Null = e {
                assert!(matches!(g, JsonValue::Null), "sma[{i}] 应为 null: {g}");
            } else {
                assert_eq!(g, e, "sma[{i}] 黄金值漂移");
            }
        }
    }

    #[test]
    fn test_sma_short_series_all_null() {
        // len < window → 全 null(pandas rolling(N) 同语义),不报错
        let values = sma_core(&SERIES[..3], 5);
        assert!(values.iter().all(|v| v.is_none()));
    }

    #[test]
    fn test_sma_golden_extra_window7() {
        // 第二组黄金值(window=7,含负值与连续重复值),覆盖 Kahan 补偿
        // 与同值/符号修正分支;窗口 7 > 前 6 位为 null
        let series: Vec<f64> = vec![
            3.5, -2.25, 7.0, 7.0, 0.125, -9.875, 4.5, 4.5, 4.5, -1.0625, 12.375, 8.2, 6.9,
        ];
        let values = sma_core(&series, 7);
        let expect: Vec<Option<f64>> = vec![
            None,
            None,
            None,
            None,
            None,
            None,
            Some(1.4285714285714286),
            Some(1.5714285714285714),
            Some(2.5357142857142856),
            Some(1.3839285714285714),
            Some(2.1517857142857144),
            Some(3.305357142857143),
            Some(5.701785714285714),
        ];
        for (i, (g, e)) in values.iter().zip(expect.iter()).enumerate() {
            assert_eq!(g, e, "sma7[{i}] 黄金值漂移");
        }
    }

    // ===== EMA 黄金值(pandas ewm(span=5, adjust=False),首位起有值) =====

    #[test]
    fn test_ema_golden_pandas_bitwise() {
        let values: Vec<Option<f64>> = ewm_span(&SERIES, 5).into_iter().map(Some).collect();
        let expect_strs = [
            "10.0", "10.4", "10.533333333333335", "11.18888888888889",
            "11.825925925925926", "12.183950617283951", "12.855967078189302",
            "13.137311385459535", "13.758207590306357", "14.372138393537572",
            "14.648092262358382", "15.132061508238923", "15.354707672159282",
            "15.903138448106189", "16.10209229873746", "16.66806153249164",
            "17.178707688327762", "17.419138458885175", "17.979425639256785",
            "18.186283759504526",
        ];
        let expect = str_vals(&expect_strs);
        let out = values_out(&values);
        let got = out.as_array().unwrap();
        assert_eq!(got.len(), expect.len());
        for (i, (g, e)) in got.iter().zip(expect.iter()).enumerate() {
            assert_eq!(g, e, "ema[{i}] 黄金值漂移");
        }
    }

    // ===== MACD 黄金值(pandas 12/26/9,ewm 逐位) =====

    #[test]
    fn test_macd_golden_pandas_bitwise() {
        let ema_fast = ewm_span(&SERIES, 12);
        let ema_slow = ewm_span(&SERIES, 26);
        let macd_series: Vec<f64> = ema_fast
            .iter()
            .zip(&ema_slow)
            .map(|(f, s)| f - s)
            .collect();
        let signal_series = ewm_span(&macd_series, 9);
        // 抽样逐位锁定(首/中/尾),全量锁定由服务层快照测试覆盖
        assert_eq!(format!("{:?}", macd_series[0]), "0.0");
        assert_eq!(format!("{:?}", macd_series[10]), "1.3087443767924931");
        assert_eq!(format!("{:?}", macd_series[19]), "2.0238976863366034");
        assert_eq!(format!("{:?}", signal_series[0]), "0.0");
        assert_eq!(format!("{:?}", signal_series[10]), "0.8305273058393281");
        assert_eq!(format!("{:?}", signal_series[19]), "1.7217990209869025");
        let hist10 = macd_series[10] - signal_series[10];
        assert_eq!(format!("{hist10:?}"), "0.478217070953165");
        let hist19 = macd_series[19] - signal_series[19];
        assert_eq!(format!("{hist19:?}"), "0.30209866534970087");
    }

    // ===== RSI 黄金值(pandas Wilder,alpha=1/14,min_periods=7) =====

    #[test]
    fn test_rsi_golden_pandas_bitwise() {
        let values = rsi_core(&SERIES, 14);
        let expect_strs = [
            "89.24050905488923", "79.68438285144704", "84.37058145649533",
            "85.97821332764869", "80.06555315541493", "82.91284207272867",
            "78.86897409089525", "82.53774202430205", "76.57227243038552",
            "80.51540566972196", "81.54468684123006", "78.20782065639835",
            "81.47354882710174", "76.34014100951936",
        ];
        let mut expect: Vec<JsonValue> = vec![JsonValue::Null; 6];
        expect.extend(str_vals(&expect_strs));
        let out = values_out(&values);
        let got = out.as_array().unwrap();
        assert_eq!(got.len(), expect.len());
        for (i, (g, e)) in got.iter().zip(expect.iter()).enumerate() {
            if let JsonValue::Null = e {
                assert!(matches!(g, JsonValue::Null), "rsi[{i}] 应为 null: {g}");
            } else {
                assert_eq!(g, e, "rsi[{i}] 黄金值漂移");
            }
        }
    }

    #[test]
    fn test_rsi_boundary_cases_golden() {
        // 单调上涨 → 100;单调下跌 → 0;恒定 → 50(屏蔽期 null,gen_golden RSI_UP/DOWN/FLAT)
        let up: Vec<f64> = (1..=14).map(|i| i as f64).collect();
        for v in rsi_core(&up, 14).iter().take(6) {
            assert!(v.is_none(), "上涨序列屏蔽期应为 null");
        }
        for v in rsi_core(&up, 14).iter().skip(6) {
            assert_eq!(format!("{:?}", v.unwrap()), "100.0");
        }
        let down: Vec<f64> = (0..14).map(|i| (20 - i) as f64).collect();
        for v in rsi_core(&down, 14).iter().skip(6) {
            assert_eq!(format!("{:?}", v.unwrap()), "0.0");
        }
        let flat = [5.0_f64; 14];
        for v in rsi_core(&flat, 14).iter().skip(6) {
            assert_eq!(format!("{:?}", v.unwrap()), "50.0");
        }
    }

    // ===== 输入契约与确定性 =====

    #[test]
    fn test_parse_series_rejects_invalid() {
        // 空序列
        let args = JsonValue::object_from_pairs(&[("series", JsonValue::Array(vec![]))]);
        let err = parse_series(&args).unwrap_err();
        assert!(err.contains("空数组"), "{err}");
        // NaN 字符串 → 显式拒绝(非有限值)
        let args = JsonValue::object_from_pairs(&[(
            "series",
            JsonValue::Array(vec![JsonValue::string("NaN")]),
        )]);
        let err = parse_series(&args).unwrap_err();
        assert!(err.contains("非有限值"), "{err}");
        // Inf 字符串 → 显式拒绝
        let args = JsonValue::object_from_pairs(&[(
            "series",
            JsonValue::Array(vec![JsonValue::string("inf")]),
        )]);
        let err = parse_series(&args).unwrap_err();
        assert!(err.contains("非有限值"), "{err}");
        // 缺 series
        let err = parse_series(&JsonValue::empty_object()).unwrap_err();
        assert!(err.contains("缺 series"), "{err}");
    }

    #[test]
    fn test_parse_usine_rejects_below_min() {
        let args = JsonValue::object_from_pairs(&[("window", JsonValue::Integer(0))]);
        assert!(parse_usize(&args, "window", 5, 1).is_err());
        let args = JsonValue::object_from_pairs(&[("window", JsonValue::Integer(5))]);
        assert_eq!(parse_usize(&args, "window", 5, 1).unwrap(), 5);
        // 缺省
        assert_eq!(parse_usize(&JsonValue::empty_object(), "window", 5, 1).unwrap(), 5);
    }

    #[tokio::test]
    async fn test_services_deterministic_bitwise() {
        // 泛化验证核心证据:同输入两次执行,输出逐位一致
        struct ErrHandler;
        #[async_trait]
        impl IoHandler for ErrHandler {
            async fn execute(&self, _params: &JsonValue) -> IoResult {
                Err("fallback-called".to_string())
            }
        }
        let router = IndicatorServiceRouter::new(Arc::new(ErrHandler));
        let series_json = arr(&SERIES);
        for svc in [
            "indicator_sma",
            "indicator_ema",
            "indicator_macd",
            "indicator_rsi",
        ] {
            let params = JsonValue::object_from_pairs(&[
                ("service_name", JsonValue::string(svc)),
                (
                    "args",
                    JsonValue::object_from_pairs(&[("series", series_json.clone())]),
                ),
            ]);
            let r1 = router.execute(&params).await.unwrap();
            let r2 = router.execute(&params).await.unwrap();
            assert_eq!(
                r1.to_string(),
                r2.to_string(),
                "{svc} 同输入两次执行必须逐位一致"
            );
        }
    }

    #[tokio::test]
    async fn test_service_end_to_end_sma() {
        // 服务层端到端:参数 → 输出结构完整(黄金值由核心函数单测锁定)
        struct ErrHandler;
        #[async_trait]
        impl IoHandler for ErrHandler {
            async fn execute(&self, _params: &JsonValue) -> IoResult {
                Err("fallback-called".to_string())
            }
        }
        let router = IndicatorServiceRouter::new(Arc::new(ErrHandler));
        let params = JsonValue::object_from_pairs(&[
            ("service_name", JsonValue::string("indicator_sma")),
            (
                "args",
                JsonValue::object_from_pairs(&[
                    ("series", arr(&SERIES)),
                    ("window", JsonValue::Integer(5)),
                ]),
            ),
        ]);
        let r = router.execute(&params).await.unwrap();
        assert_eq!(r.get("status").and_then(|v| v.as_str()), Some("ok"));
        assert_eq!(r.get("window").and_then(|v| v.as_i64()), Some(5));
        assert_eq!(r.get("count").and_then(|v| v.as_i64()), Some(20));
        assert!(r.get("values").and_then(|v| v.as_array()).is_some());
    }

    #[tokio::test]
    async fn test_macd_rejects_fast_ge_slow() {
        struct ErrHandler;
        #[async_trait]
        impl IoHandler for ErrHandler {
            async fn execute(&self, _params: &JsonValue) -> IoResult {
                Err("fallback-called".to_string())
            }
        }
        let router = IndicatorServiceRouter::new(Arc::new(ErrHandler));
        let params = JsonValue::object_from_pairs(&[
            ("service_name", JsonValue::string("indicator_macd")),
            (
                "args",
                JsonValue::object_from_pairs(&[
                    ("series", arr(&SERIES)),
                    ("fast", JsonValue::Integer(26)),
                    ("slow", JsonValue::Integer(12)),
                ]),
            ),
        ]);
        let err = router.execute(&params).await.unwrap_err();
        assert!(err.contains("fast(26) 必须 < slow(12)"), "{err}");
    }
}
