// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! `inverse_kinematics_solver` 原生实现 —— UR5 简化 D-H 正解 + 数值雅可比 + 阻尼最小二乘（DLS）。
//!
//! 从 Python `ik_core.py` 逐行移植（无 numpy，纯 f64 数组）：
//! - 结果刻意带 `converged_ok: bool`，让 TCB 只做 eq 判断（浮点比较留在服务内部）
//! - 浮点字段（joint_positions / residual）以字符串返回（TCB 无 Float 变体）
//! - 确定性：相同输入 → 相同输出，无随机/墙钟

use evorule_tcb::JsonValue;

use crate::{arg_f64, arg_i64, float_str, obj, NativeService};

/// UR5 简化 D-H 参数（权威源 ik_core.py `_A` / `_D`）
const A: [f64; 6] = [0.0, -0.425, -0.3922, 0.0, 0.0, 0.0];
const D: [f64; 6] = [0.0892, 0.0, 0.0, 0.1093, 0.09475, 0.0825];

/// 4x4 矩阵乘法
fn mat4_mul(x: [[f64; 4]; 4], y: [[f64; 4]; 4]) -> [[f64; 4]; 4] {
    let mut r = [[0.0; 4]; 4];
    for i in 0..4 {
        for j in 0..4 {
            r[i][j] = x[i][0] * y[0][j] + x[i][1] * y[1][j] + x[i][2] * y[2][j] + x[i][3] * y[3][j];
        }
    }
    r
}

/// 单位 4x4
fn mat4_identity() -> [[f64; 4]; 4] {
    let mut m = [[0.0; 4]; 4];
    for (i, row) in m.iter_mut().enumerate() {
        row[i] = 1.0;
    }
    m
}

/// 由关节角 q 计算末端 xyz（对应 ik_core.py `_forward_pos`）
fn forward_pos(q: &[f64; 6]) -> [f64; 3] {
    let mut t = mat4_identity();
    for i in 0..6 {
        let theta = q[i];
        let ai = A[i];
        let di = D[i];
        let (st, ct) = theta.sin_cos();
        let (sa, ca) = ai.sin_cos();
        // Ti = M1 @ M2，其中 M2 是沿 x 平移 ai（见 ik_core.py 矩阵展开）
        let ti = [
            [ct, -st, 0.0, ct * ai],
            [st * ca, ct * ca, -sa, st * ca * ai - sa * di],
            [st * sa, ct * sa, ca, st * sa * ai + ca * di],
            [0.0, 0.0, 0.0, 1.0],
        ];
        t = mat4_mul(t, ti);
    }
    [t[0][3], t[1][3], t[2][3]]
}

/// 求解 3x3 线性方程组 A x = b（Gauss-Jordan 部分主元；A 奇异时返回 None）
fn solve3(a: [[f64; 3]; 3], b: [f64; 3]) -> Option<[f64; 3]> {
    let mut m = [
        [a[0][0], a[0][1], a[0][2], b[0]],
        [a[1][0], a[1][1], a[1][2], b[1]],
        [a[2][0], a[2][1], a[2][2], b[2]],
    ];
    for col in 0..3 {
        let mut piv = col;
        for r in (col + 1)..3 {
            if m[r][col].abs() > m[piv][col].abs() {
                piv = r;
            }
        }
        if m[piv][col].abs() < 1e-300 {
            return None;
        }
        m.swap(col, piv);
        for r in 0..3 {
            if r == col {
                continue;
            }
            let factor = m[r][col] / m[col][col];
            let pivot_row = m[col];
            for (x, y) in m[r][col..4].iter_mut().zip(pivot_row[col..4].iter()) {
                *x -= factor * y;
            }
        }
    }
    let mut x = [0.0; 3];
    for i in 0..3 {
        x[i] = m[i][3] / m[i][i];
    }
    Some(x)
}

/// 阻尼最小二乘迭代求解（对应 ik_core.py `solve_ik` 的数值雅可比 + DLS 分支）
fn solve_ik(
    target_xyz: [f64; 3],
    mut q: [f64; 6],
    max_iterations: usize,
    tolerance: f64,
) -> (Vec<f64>, bool, f64) {
    let step = 1e-4; // 数值差分步长（与 ik_core.py 一致）
    let lam = 0.1; // 阻尼系数
    let mut converged = false;
    let mut residual = f64::INFINITY;

    for _ in 0..std::cmp::max(1, max_iterations) {
        let xyz = forward_pos(&q);
        let err = [
            target_xyz[0] - xyz[0],
            target_xyz[1] - xyz[1],
            target_xyz[2] - xyz[2],
        ];
        residual = (err[0] * err[0] + err[1] * err[1] + err[2] * err[2]).sqrt();
        if residual <= tolerance {
            converged = true;
            break;
        }
        // 数值雅可比 J（3x6）
        let mut jac = [[0.0; 6]; 3];
        for k in 0..6 {
            q[k] += step;
            let xyz2 = forward_pos(&q);
            for r in 0..3 {
                jac[r][k] = (xyz2[r] - xyz[r]) / step;
            }
            q[k] -= step;
        }
        // DLS：dq = J^T (J J^T + λ²I)^{-1} err
        // 先算 JJt（3x3 对称）
        let mut jjt = [[0.0; 3]; 3];
        for r in 0..3 {
            for c in 0..3 {
                let s = jac[r].iter().zip(jac[c].iter()).map(|(a, b)| a * b).sum();
                jjt[r][c] = s;
            }
        }
        jjt[0][0] += lam * lam;
        jjt[1][1] += lam * lam;
        jjt[2][2] += lam * lam;
        if let Some(x) = solve3(jjt, err) {
            let mut dq = [0.0; 6];
            for k in 0..6 {
                dq[k] = jac[0][k] * x[0] + jac[1][k] * x[1] + jac[2][k] * x[2];
                dq[k] = dq[k].clamp(-0.5, 0.5); // 对应 np.clip(dq, -0.5, 0.5)
            }
            for k in 0..6 {
                q[k] += dq[k];
            }
        } else {
            break; // 奇异，无法求解（对应 np.linalg.LinAlgError → break）
        }
    }
    (q.to_vec(), converged, residual)
}

/// `inverse_kinematics_solver` 原生服务
pub struct IkSolver;

impl NativeService for IkSolver {
    fn execute(&self, args: &JsonValue) -> evorule_reactor::IoResult {
        let target_pose = args
            .get("target_pose")
            .cloned()
            .unwrap_or_else(JsonValue::empty_object);
        let target_xyz = [
            arg_f64(&target_pose, "x", 0.0),
            arg_f64(&target_pose, "y", 0.0),
            arg_f64(&target_pose, "z", 0.0),
        ];
        let tolerance = arg_f64(args, "tolerance", 1e-3);
        let max_iterations = arg_i64(args, "max_iterations", 100).max(1) as usize;

        // 初始关节角：current_joints 或全 0，补齐到 6（对应 ik_core.py L54-57）
        let mut q = [0.0; 6];
        if let Some(JsonValue::Array(arr)) = args.get("current_joints") {
            for (i, v) in arr.iter().take(6).enumerate() {
                q[i] = match v {
                    JsonValue::Integer(n) => *n as f64,
                    JsonValue::String(s) => s.parse().unwrap_or(0.0),
                    _ => 0.0,
                };
            }
        }

        let (joint_positions, converged, residual) =
            solve_ik(target_xyz, q, max_iterations, tolerance);

        Ok(obj(vec![
            (
                "joint_positions",
                JsonValue::Array(joint_positions.iter().map(|x| float_str(*x)).collect()),
            ),
            ("converged", JsonValue::Bool(converged)),
            ("residual", float_str(residual)),
            (
                "converged_ok",
                JsonValue::Bool(converged && residual <= tolerance.max(0.0)),
            ),
        ]))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use crate::NativeService;

    #[test]
    fn test_forward_pos_identity() {
        // 零关节角下正解应返回有限值（对应 Python 基线行为）
        let q = [0.0; 6];
        let p = forward_pos(&q);
        assert!(p.iter().all(|v| v.is_finite()));
    }

    #[test]
    fn test_ik_converges_near_target() {
        // 与 Python 基线一致：{0.5, 0.3, 0.2}、tolerance 1e-3 应收敛
        let (joints, converged, residual) = solve_ik([0.5, 0.3, 0.2], [0.0; 6], 100, 1e-3);
        assert!(converged, "近目标应收敛, residual={residual}");
        assert!(residual <= 1e-3);
        assert_eq!(joints.len(), 6);
    }

    #[test]
    fn test_ik_not_converge_far_target() {
        // {10,10,10} 超出工作空间 → 不应收敛（对应 D9 测试的断言路径）
        let (_joints, converged, residual) = solve_ik([10.0, 10.0, 10.0], [0.0; 6], 100, 1e-3);
        assert!(!converged, "远目标不应收敛, residual={residual}");
        assert!(residual > 1e-3);
    }

    #[test]
    fn test_ik_service_output_shape() {
        let svc = IkSolver;
        let args = JsonValue::object_from_pairs(&[
            (
                "target_pose",
                JsonValue::object_from_pairs(&[
                    ("x", JsonValue::string("0.5")),
                    ("y", JsonValue::string("0.3")),
                    ("z", JsonValue::string("0.2")),
                ]),
            ),
            ("tolerance", JsonValue::string("0.001")),
        ]);
        let r = svc.execute(&args).unwrap();
        assert_eq!(r.get("converged_ok").and_then(|v| v.as_bool()), Some(true));
        assert_eq!(
            r.get("joint_positions")
                .and_then(|v| v.as_array())
                .map(|a| a.len()),
            Some(6)
        );
        assert!(r.get("residual").and_then(|v| v.as_str()).is_some());
    }
}
