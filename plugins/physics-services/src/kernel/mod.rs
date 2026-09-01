// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 确定性物理内核(vendored 自 rpsm-demo `rpsm-core` v0.1.0,2026-09-01 快照)。
//!
//! # 来源与同步边界(诚实声明)
//! - 本模块自独立实验仓 `rpsm-demo/rpsm-core`(workspace v0.1.0)一次性 vendored 引入
//!   (UV-035),除导入路径调整(`crate::math` → `crate::kernel::math`)与 lint 豁免标注外,
//!   逐行保持原实现;rpsm 侧后续演进不自动回灌,升级须另立专项。
//! - 许可一致:两侧均为 AGPL-3.0-or-later。
//!
//! # 确定性边界
//! - 辛积分器 + 编译期锁定常量(G/C,无 setter)+ 固定 f64 精度:同平台同输入逐位一致;
//! - 纯函数式步进 `tick(dt)`:无墙钟、无随机源、无外部 IO;
//! - 跨平台浮点(如 x87 扩展精度、FFLAG 收缩)不在本内核承诺范围,服务文档如实声明。

pub mod constants;
pub mod integrators;
pub mod math;
pub mod rigid_body;

pub use constants::{C, G};
pub use integrators::{build_integrator, Integrator, SymplecticEuler, VelocityVerlet};
pub use math::{integrate_orientation, Quaternion, Vec3};
pub use rigid_body::{HingeJoint, Joint, RigidBody};

/// 物理内核：持有积分器、重力方向、恢复系数与刚体列表。
pub struct PhysicalKernel {
    integrator: Box<dyn Integrator>,
    /// 当前生效的积分器阶数（1=辛欧拉，2=速度 Verlet），与 `integrator` 同步。
    integrator_order: u8,
    gravity: Vec3,
    pub bodies: Vec<RigidBody>,
    /// 恢复系数（0..=1），由 HCI 面板注入；触地时竖直速度衰减。
    restitution: f64,
    /// 每帧持久的外力输入（与 `bodies` 对齐），由应用层（如 DKEL 弹簧）通过
    /// `set_external_force` 注入。保持内核纯净——不感知外力来源语义。
    external_forces: Vec<Vec3>,
    /// 每帧持久的外力矩输入（与 `bodies` 对齐），由应用层通过 `set_external_torque`
    /// 注入（如恒定力矩、控制律力矩）。仅在 `inertia>0` 刚体上生效：`α = τ/I`，
    /// `ω += α·dt` 后再积分取向。零力矩时退化为自由旋转守恒（向后兼容）。
    external_torques: Vec<Vec3>,
    /// 有界重力带（分层势场）：`Some((lo, hi))` 时重力仅在刚体竖直位置
    /// `lo <= pos.y <= hi` 内生效，带外重力归零。形成「箱阱/平台势」——
    /// 出带上界的刚体以恒速逃逸、不再回落（受控于归一化重力方向的 -Y 轴）。
    /// `None`（默认）为全空间均匀场，向后兼容。
    grav_band: Option<(f64, f64)>,
}

impl Clone for PhysicalKernel {
    // vendored 原实现使用 expect(order 已由构造器校验恒合法);工作区 lint deny expect_used,
    // 此处定点豁免以保持 vendored 逐行保真。
    #[allow(clippy::expect_used)]
    fn clone(&self) -> Self {
        Self {
            integrator: build_integrator(self.integrator_order).expect("integrator_order 合法"),
            integrator_order: self.integrator_order,
            gravity: self.gravity,
            bodies: self.bodies.clone(),
            restitution: self.restitution,
            external_forces: self.external_forces.clone(),
            external_torques: self.external_torques.clone(),
            grav_band: self.grav_band,
        }
    }
}

impl std::fmt::Debug for PhysicalKernel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PhysicalKernel")
            .field("integrator_order", &self.integrator_order)
            .field("gravity", &self.gravity)
            .field("bodies", &self.bodies)
            .field("restitution", &self.restitution)
            .field("external_forces", &self.external_forces)
            .field("external_torques", &self.external_torques)
            .field("grav_band", &self.grav_band)
            .finish()
    }
}

// vendored 原实现使用 expect(order=1 恒合法);工作区 lint deny expect_used,定点豁免。
#[allow(clippy::expect_used)]
impl Default for PhysicalKernel {
    fn default() -> Self {
        Self::new(Vec3::new(0.0, -G, 0.0))
    }
}

impl PhysicalKernel {
    // vendored 原实现使用 expect(order=1 恒合法);工作区 lint deny expect_used,定点豁免。
    #[allow(clippy::expect_used)]
    pub fn new(gravity: Vec3) -> Self {
        Self::with_integrator(gravity, 1).expect("默认 integrator_order=1 必然合法")
    }

    /// 构造内核并指定积分器阶数（1=辛欧拉，2=速度 Verlet）。
    pub fn with_integrator(gravity: Vec3, order: u8) -> Result<Self, String> {
        let integrator = build_integrator(order)?;
        Ok(Self {
            integrator,
            integrator_order: order,
            gravity,
            bodies: Vec::new(),
            restitution: 0.8,
            external_forces: Vec::new(),
            external_torques: Vec::new(),
            grav_band: None,
        })
    }

    /// 运行时切换积分器（HCI 热重载 `integrator_order` 时调用）。
    pub fn set_integrator_order(&mut self, order: u8) -> Result<(), String> {
        self.integrator = build_integrator(order)?;
        self.integrator_order = order;
        Ok(())
    }

    /// 当前生效的积分器阶数（1 或 2）。
    pub fn integrator_order(&self) -> u8 {
        self.integrator_order
    }

    /// 当前重力方向（可由 HCI 缩放，但不改动锁定常数）。
    pub fn gravity(&self) -> &Vec3 {
        &self.gravity
    }

    /// 更新重力方向（供 HCI/DKEL 在运行时调整）。
    pub fn set_gravity(&mut self, gravity: Vec3) {
        self.gravity = gravity;
    }

    /// 启用有界重力带 `[lo, hi]`（分层势场）：重力仅在 `lo <= pos.y <= hi` 内生效，
    /// 带外归零。`lo`/`hi` 各自 clamp，确保 `lo <= hi`；带内刚体视为被捕获、带外可逃逸。
    pub fn set_gravity_band(&mut self, lo: f64, hi: f64) {
        let lo = lo.min(hi);
        let hi = hi.max(lo);
        self.grav_band = Some((lo, hi));
    }

    /// 当前重力带（`None` 表示全空间均匀场）。
    pub fn gravity_band(&self) -> Option<(f64, f64)> {
        self.grav_band
    }

    /// 关闭有界重力，恢复全空间均匀场（向后兼容默认行为）。
    pub fn clear_gravity_band(&mut self) {
        self.grav_band = None;
    }

    /// 设置恢复系数（clamp 到 [0,1]）。
    pub fn set_restitution(&mut self, e: f64) {
        self.restitution = e.clamp(0.0, 1.0);
    }

    /// 恢复系数（测试/PLA 读取）。
    pub fn restitution(&self) -> f64 {
        self.restitution
    }

    /// 注入第 `index` 个刚体的持久外力（应用层在每帧积分前调用）。
    /// 若向量尚未扩展到该长度，则自动补齐为零。
    pub fn set_external_force(&mut self, index: usize, force: Vec3) {
        if index >= self.external_forces.len() {
            self.external_forces.resize(index + 1, Vec3::zero());
        }
        self.external_forces[index] = force;
    }

    /// 清零全部外力槽（如需彻底移除 DKEL 外力时调用）。
    pub fn clear_external_forces(&mut self) {
        for f in &mut self.external_forces {
            *f = Vec3::zero();
        }
    }

    /// 注入第 `index` 个刚体的持久外力矩（应用层在每帧积分前调用）。
    /// 仅在 `inertia>0` 的刚体上生效：`α = τ/I`。若向量尚未扩展到该长度则补齐为零。
    pub fn set_external_torque(&mut self, index: usize, torque: Vec3) {
        if index >= self.external_torques.len() {
            self.external_torques.resize(index + 1, Vec3::zero());
        }
        self.external_torques[index] = torque;
    }

    /// 清零全部外力矩槽（如需彻底移除注入力矩时调用）。
    pub fn clear_external_torques(&mut self) {
        for t in &mut self.external_torques {
            *t = Vec3::zero();
        }
    }

    /// 推进一帧（含地面碰撞响应）。
    pub fn tick(&mut self, dt: f64) {
        self.tick_collision_optional(dt, true);
    }

    /// 推进一帧，可选择是否执行地面碰撞响应。
    ///
    /// PLA 真回溯时传入 `apply_collision=false` 以区分「已知碰撞损耗是否计入
    /// 预期能量基线」（决策 C）。保持确定性：给定相同输入与开关，产出唯一结果。
    pub fn tick_collision_optional(&mut self, dt: f64, apply_collision: bool) {
        // 1) 累加合力（互引力 + 均匀场 + 外力）与合力矩（刚性铰链/约束），基于当前位置与取向。
        self.accumulate_forces();

        let order = self.integrator_order;
        if order == 2 {
            // 速度 Verlet（二阶辛）：平动与旋转同步「kick-drift-kick」，位置/取向/速度/角速度
            // 按 leapfrog 结构交错半步，保持时间可逆。旋转若用单遍显式 ω 踢（旧取向处的力矩
            // 一次踢完）会与平动的半步交错失配，在平移-旋转耦合（如铰链力臂力矩）下泵能
            // （探针实测能量爆炸 1e5 量级）；两遍半踢 + 重算后取平均即恢复有界。
            let a_old: Vec<Vec3> = self.bodies.iter().map(|b| b.force_accum / b.mass).collect();
            if self.external_torques.len() != self.bodies.len() {
                self.external_torques
                    .resize(self.bodies.len(), Vec3::zero());
            }
            let alpha_old: Vec<Vec3> = self
                .bodies
                .iter()
                .zip(&self.external_torques)
                .map(|(b, te)| effective_alpha(b, *te))
                .collect();
            // drift：平动位置按旧加速度漂移。
            for (b, a) in self.bodies.iter_mut().zip(&a_old) {
                b.pos += b.vel * dt + *a * 0.5 * dt * dt;
            }
            // drift：取向以「半踢后的角速度」推进（ω_{n+1/2} → q_{n+1}）。
            for (b, alpha) in self.bodies.iter_mut().zip(&alpha_old) {
                if b.inertia > 0.0 {
                    b.angular_velocity += *alpha * 0.5 * dt;
                    b.orientation = integrate_orientation(b.orientation, b.angular_velocity, dt);
                }
            }
            // 重算新位置、新取向处的力/力矩。
            self.accumulate_forces();
            // kick-2：平动速度用 (a_old + a_new)/2。
            for (b, a) in self.bodies.iter_mut().zip(&a_old) {
                let a_new = b.force_accum / b.mass;
                b.vel += (*a + a_new) * 0.5 * dt;
            }
            // kick-2：角速度用 α_new 补足另半踢。
            for (b, te) in self.bodies.iter_mut().zip(&self.external_torques) {
                if b.inertia > 0.0 {
                    let alpha_new = effective_alpha(b, *te);
                    b.angular_velocity += alpha_new * 0.5 * dt;
                }
            }
        } else {
            // 辛欧拉（半隐式，order == 1）：先更新速度，再更新位置。
            for b in &mut self.bodies {
                let a = b.force_accum / b.mass;
                b.vel += a * dt;
                b.pos += b.vel * dt;
            }
            // 旋转同半隐式：先 ω 全踢，再推进取向（与平动一致的半步交错，能量有界）。
            if self.external_torques.len() != self.bodies.len() {
                self.external_torques
                    .resize(self.bodies.len(), Vec3::zero());
            }
            for (b, te) in self.bodies.iter_mut().zip(&self.external_torques) {
                if b.inertia > 0.0 {
                    let alpha = effective_alpha(b, *te);
                    b.angular_velocity += alpha * dt;
                    b.orientation = integrate_orientation(b.orientation, b.angular_velocity, dt);
                }
            }
        }

        // 3) 地面碰撞响应（y 方向平面碰撞 + 切向摩擦；可按需关闭，供 PLA 回溯控制损耗口径）。
        if apply_collision {
            self.apply_collision_response(dt);
        }

        // 力累加器每帧清零。
        for body in &mut self.bodies {
            body.force_accum = Vec3::default();
            body.torque_accum = Vec3::default();
        }
    }

    /// 累加每刚体合力到 `force_accum`：多体牛顿引力 + 均匀重力场 + 应用层外力。
    /// 每帧开始调用一次；速度 Verlet 在位置更新后调用第二次，以重算新位置处的力。
    // 多体引力 O(n²) 对遍历 + 逐步按来源累加,拆函数需共享 bodies 可变借,
    // 详见 GATE_REFERENCE.md §六(豁免索引)
    #[allow(clippy::cognitive_complexity, clippy::too_many_lines)]
    fn accumulate_forces(&mut self) {
        for body in &mut self.bodies {
            body.force_accum = Vec3::default();
            // 力矩累加器与力同生命周期：每帧开始清零、由刚性铰链在当前位置重算，
            // 结束后归零（velocity Verlet 新旧位置各求一次，保持辛映射与快照整洁）。
            body.torque_accum = Vec3::default();
        }
        // 多体引力：任意两体间 F = G·m_i·m_j / r²，方向沿连线（万有引力）。
        let n = self.bodies.len();
        for i in 0..n {
            for j in (i + 1)..n {
                let delta = self.bodies[j].pos - self.bodies[i].pos;
                let r_sq = delta.length_squared();
                // 零距离（重合）时跳过，避免除零/爆破。
                if r_sq <= f64::EPSILON {
                    continue;
                }
                let force_mag = G * self.bodies[i].mass * self.bodies[j].mass / r_sq;
                let direction = delta / r_sq.sqrt();
                let force = direction * force_mag;
                self.bodies[i].force_accum += force;
                self.bodies[j].force_accum -= force;
            }
        }
        // 叠加重力场（与多体并存，符合牛顿叠加原理）。默认全空间均匀场；
        // 若启用重力带（分层势场），则仅在带内刚体受力，带外逃逸刚体不再收敛。
        for body in &mut self.bodies {
            let in_band = match self.grav_band {
                Some((lo, hi)) => body.pos.y >= lo && body.pos.y <= hi,
                None => true,
            };
            if in_band {
                body.force_accum += self.gravity * body.mass;
            }
        }
        // 叠加线性空气阻力（F = -b·v，确定性非保守力）：系数 0 时等效于无阻力。
        // 在引力之后、应用层外力之前加入，保持力合成顺序固定（见 DESIGN §2.1）。
        for body in &mut self.bodies {
            if body.drag > 0.0 && body.vel != Vec3::zero() {
                body.force_accum -= body.vel * body.drag;
            }
        }
        // 叠加平方空气阻力（F = -c2·|v|·v，确定性非保守力）：系数 0 时等效于无阻力。
        // 自由落体终速 v_t = √(m·g/c2)。与线性阻力可并存叠加；顺序紧跟线性阻力，固定不变。
        for body in &mut self.bodies {
            if body.drag_quadratic > 0.0 && body.vel != Vec3::zero() {
                body.force_accum -= body.vel * body.vel.length() * body.drag_quadratic;
            }
        }
        // 叠加双势阱保守势（沿 X 轴）：F_x = −dV/dx = −4a·x·(x²−m)。
        // 随位置实时重算（velocity Verlet 新旧各求一次），保持映射辛性与能量有界。
        for body in &mut self.bodies {
            if let Some(w) = body.double_well {
                let x = body.pos.x;
                let f = -4.0 * w.a * x * (x * x - w.m);
                body.force_accum.x += f;
            }
        }
        // 叠加弹簧-阻尼器（F = -k·(p−a) − c·v，确定性）：anchor 为 None 或无位移时无作用。
        // 纯弹簧（c=0）为保守力（势能入 total_mechanical_energy），阻尼 c>0 为耗散项。
        for body in &mut self.bodies {
            if let Some(anchor) = body.anchor {
                if body.stiffness > 0.0 {
                    body.force_accum -= (body.pos - anchor) * body.stiffness;
                }
                if body.damping > 0.0 && body.vel != Vec3::zero() {
                    body.force_accum -= body.vel * body.damping;
                }
            }
        }
        // 叠加双体软铰（球铰中央力约束，确定性多体相互约束）：对每个已声明关节，
        // 按「自体质心 → 对端质心」连线计算等值反作用弹簧-阻尼力。
        //   u = (p_j − p_i)/|p_j − p_i|；along = (v_i − v_j)·u（连线方向相对分离速度）；
        //   F_i = u·(k·(d−L) − c·along)，F_j = −F_i。
        // 符号依据：拉伸（d>L）时 +u·k·(d−L) 把 i 拉向 j（回复力）；阻尼项 −c·along
        // 抵消相对分离（along>0 时给 i 反向力）。功 = F_i·(v_i−v_j) = k(d−L)·along − c·along²，
        // 其中 k 项是保守弹簧（与势能 ½k(d−L)² 的 −d/dt 一致），c 项恒 ≤ 0 保证确定性耗散。
        // 按当前位置重算（velocity Verlet 新旧各求一次），中央保守力保持辛映射、能量可证。
        // 对端索引越界时安全忽略。
        let n = self.bodies.len();
        for i in 0..n {
            let jr = self.bodies[i].joint; // Copy 值拷贝，避免借冲突
            if let Some(j) = jr {
                if j.body >= n || (j.stiffness <= 0.0 && j.damping <= 0.0) {
                    continue;
                }
                let pi = self.bodies[i].pos;
                let vi = self.bodies[i].vel;
                let pj = self.bodies[j.body].pos;
                let vj = self.bodies[j.body].vel;
                let delta = pj - pi;
                let d = delta.length();
                if d <= f64::EPSILON {
                    continue; // 重合时无定义方向，安全跳过（防除零）
                }
                let u = delta / d;
                let along = u.x * (vi.x - vj.x) + u.y * (vi.y - vj.y) + u.z * (vi.z - vj.z);
                let f_scalar = j.stiffness * (d - j.rest_length) - j.damping * along;
                let f = u * f_scalar;
                self.bodies[i].force_accum += f;
                self.bodies[j.body].force_accum -= f;
            }
        }
        // 叠加刚性旋转铰链（revolute joint，机械臂肘关节/运动链的刚性转角约束 + 力矩耦合）。
        // 与软铰（质心中央力）相对：把**铰接锚点**世界坐标锁在一起（位置约束）、把**铰轴方向**
        // 对齐（角度约束），只允许两刚体绕铰轴相对转动。按当前位置在 velocity Verlet 新旧各求
        // 一次（保守核心保持辛映射、能量可证）：
        //   - 位置投影（保守）：锚点误差 `e = a_j − a_i` 产生力 `F = k_p·e`（作用于锚点，
        //     折算为质心平动 `F` + 力矩 `τ = r×F`；对端 `−F` + `−r_j×F`），
        //     势能 `½·k_p·|e|²` 入总能量；
        //   - 角度投影（保守）：铰轴错位 `n_i×n_j` 产生复位力矩 `τ_i = k_θ·(n_i×n_j)`、
        //     对端 `−τ_i`（只对齐轴向、允许绕轴相对转动），势能 `½·k_θ·|n_i−n_j|²` 入总能量；
        //   - 速度投影（耗散，可选）：`damping>0` 时沿相对角速度施加速度投影阻尼
        //     `τ = −c·(ω_i − ω_j)`（revolute 关节摩擦，确定性耗散，PLA 证伪注入用）。
        // 力矩写入 `torque_accum`，随后并入旋转积分。对端索引越界或固定世界锚点安全处理。
        // 自铰链（body==i）时对端贡献恒为零（锚点/轴同体无误差），安全忽略。
        let n = self.bodies.len();
        for i in 0..n {
            let h = self.bodies[i].hinge; // Copy 值拷贝，避免借冲突
            if let Some(hinge) = h {
                if hinge.position_stiffness <= 0.0
                    && hinge.angular_stiffness <= 0.0
                    && hinge.damping <= 0.0
                {
                    continue;
                }
                let bi = self.bodies[i]; // 本刚体全量快照（pos/orient/vel/omega）
                let n_i = bi.orientation.rotate_vec(hinge.local_axis);
                let a_i = bi.pos + bi.orientation.rotate_vec(hinge.local_pivot);
                let r_i = a_i - bi.pos;
                // 对端世界系锚点/轴：Some(j) 取刚体快照；None 锚定固定世界锚点/方向。
                let (j_opt, bj, a_j, n_j): (Option<usize>, Option<RigidBody>, Vec3, Vec3) =
                    match hinge.body {
                        Some(j) if j < n => (
                            Some(j),
                            Some(self.bodies[j]),
                            self.bodies[j].pos
                                + self.bodies[j]
                                    .orientation
                                    .rotate_vec(hinge.other_local_pivot),
                            self.bodies[j]
                                .orientation
                                .rotate_vec(hinge.other_local_axis),
                        ),
                        // 越界对端索引：安全忽略本约束（等同无铰链自由体），与软铰同约定。
                        Some(_) => continue,
                        None => (None, None, hinge.anchor, hinge.other_local_axis),
                    };
                let r_j = match bj {
                    Some(b) => a_j - b.pos,
                    None => Vec3::zero(),
                };
                let mut f_i = Vec3::zero();
                let mut tau_i = Vec3::zero();
                let mut f_j = Vec3::zero();
                let mut tau_j = Vec3::zero();
                // 位置投影：锚点误差力 + 力臂力矩。
                let e = a_j - a_i;
                if hinge.position_stiffness > 0.0 && e != Vec3::zero() {
                    let f = e * hinge.position_stiffness;
                    f_i += f;
                    tau_i += r_i.cross(f);
                    f_j -= f;
                    tau_j -= r_j.cross(f);
                }
                // 角度投影：轴错位复位力矩（只对齐轴向、允许绕轴相对转动）。
                let mis = n_i.cross(n_j);
                if hinge.angular_stiffness > 0.0 && mis != Vec3::zero() {
                    let tau = mis * hinge.angular_stiffness;
                    tau_i += tau;
                    tau_j -= tau;
                }
                // 速度投影（关节摩擦阻尼，非保守）：**相对角速度**阻尼 `τ = −c·(ω_i − ω_j)`。
                // revolute 关节只留「绕铰轴相对转动」一个自由 DOF，摩擦正解应抵消相对角速度
                // （固定世界锚点 ω_j=0）；功率 = τ_i·ω_i + τ_j·ω_j = −c·|ω_i−ω_j|² ≤ 0，
                // 确定性耗散（PLA 证伪注入用）。
                // 不做锚点相对**平动**阻尼——铰接点被位置约束锁紧，其相对速度恒 ≈ 0，
                // 平动阻尼不耗散任何可观能量（诚实披露：初期实现即为此缺陷，已修正）。
                if hinge.damping > 0.0 {
                    let w_rel = bi.angular_velocity
                        - match bj {
                            Some(b) => b.angular_velocity,
                            None => Vec3::zero(),
                        };
                    if w_rel != Vec3::zero() {
                        tau_i += w_rel * (-hinge.damping);
                        tau_j += w_rel * hinge.damping;
                    }
                }
                // 施加：本刚体 + 对端（自铰链 j==i 时对端贡献恒为零，安全忽略）。
                self.bodies[i].force_accum += f_i;
                self.bodies[i].torque_accum += tau_i;
                if let Some(j) = j_opt {
                    if j != i {
                        self.bodies[j].force_accum += f_j;
                        self.bodies[j].torque_accum += tau_j;
                    }
                }
            }
        }
        // 叠加应用层注入的持久外力（如 DKEL 保守力），对齐到 bodies 长度。
        if self.external_forces.len() != self.bodies.len() {
            self.external_forces.resize(self.bodies.len(), Vec3::zero());
        }
        for (i, body) in self.bodies.iter_mut().enumerate() {
            body.force_accum += self.external_forces[i];
        }
    }

    /// 地面碰撞检测与响应：刚体底部触及 y=0 时，修正位置并反向衰减竖直速度；
    /// 若刚体设有摩擦系数，则一并施加切向动力学摩擦（`μ·g·dt`，封顶不反向）。
    fn apply_collision_response(&mut self, dt: f64) {
        for body in &mut self.bodies {
            // 半径 <= 0 视作质点，不参与碰撞。
            if body.radius <= 0.0 || body.pos.y - body.radius > 0.0 {
                continue;
            }
            // 位置修正到地面。
            body.pos.y = body.radius;
            // 仅当向下运动时反弹，避免重复触发。
            if body.vel.y < 0.0 {
                body.vel.y = -body.vel.y * self.restitution;
            }
            // 极小速度直接停止，防残留微振动。
            if body.vel.y.abs() < 0.001 {
                body.vel.y = 0.0;
            }
            // 切向动力学摩擦（μ·g·dt，封顶不反向）：法向压力 N=m·g ⇒ 摩擦加速度
            // = μ·m·g/m = μ·g；削减水平速度，且永不反向。
            if body.friction > 0.0 {
                let g = self.gravity.length();
                if g > 0.0 {
                    let dv = body.friction * g * dt;
                    let hx = body.vel.x;
                    let hz = body.vel.z;
                    let hs = (hx * hx + hz * hz).sqrt();
                    if hs > 0.0 {
                        let new_hs = (hs - dv).max(0.0);
                        let scale = new_hs / hs;
                        body.vel.x = hx * scale;
                        body.vel.z = hz * scale;
                    }
                    // 旋转-碰撞耦合（滚动摩擦）：对可旋转（inertia>0）且具几何（radius>0）
                    // 的球体，把同一滑摩擦同时灌入水平方向角速度（滚动分量 ωx/ωz，不含
                    // 绕竖轴的自旋 ωy）——「旋转动能进切向摩擦」，防止接触中自旋持续堆积
                    // 导致能量发散。自旋与平移一并对滑摩擦耗散，能量单调不增。
                    if body.inertia > 0.0 {
                        let wx = body.angular_velocity.x;
                        let wz = body.angular_velocity.z;
                        let ws = (wx * wx + wz * wz).sqrt();
                        if ws > 0.0 {
                            // 同一接触切向阻力折算到滚动阻尼：dω = dv/r（接触点切向位移一致）。
                            let d_omega = dv / body.radius.max(f64::EPSILON);
                            let new_ws = (ws - d_omega).max(0.0);
                            let oscale = new_ws / ws;
                            body.angular_velocity.x = wx * oscale;
                            body.angular_velocity.z = wz * oscale;
                        }
                    }
                }
            }
        }
    }

    /// 重力势能（受重力带影响）：
    /// - `grav_band=None`：全空间均匀场势能 `-m·g_y·y`（以 y=0 为零点，向后兼容）。
    /// - `Some((lo, hi))`：分层势场的**连续箱阱势**。
    ///   `g = |self.gravity|` 取重力大小、方向为 -Y：
    ///   - `y >= hi`：`0`（上平台）；
    ///   - `lo <= y <= hi`：`m·g·(y − hi)`（带内随高度线性增加，`dU/dy = m·g`，
    ///     给出向下重力 `F_y = −dU/dy = −m·g`，与带内合力一致）；
    ///   - `y <= lo`：`−m·g·(hi − lo)`（下平台，连续衔接带内）。
    ///
    /// 势函连续、力场一致，构成真正保守的分层势场（能量守恒可断言）。
    fn gravity_potential(&self, b: &RigidBody) -> f64 {
        match self.grav_band {
            None => -b.mass * self.gravity.y * b.pos.y,
            Some((lo, hi)) => {
                let g = self.gravity.length();
                if g <= 0.0 {
                    return 0.0;
                }
                if b.pos.y >= hi {
                    0.0
                } else if b.pos.y <= lo {
                    -b.mass * g * (hi - lo).max(0.0)
                } else {
                    b.mass * g * (b.pos.y - hi)
                }
            }
        }
    }

    /// 计算全系统总机械能（动能 + 均匀场势能 + 弹簧势能 + 多体引力势能）。
    /// 用于能量守恒验证；单刚体场景退化为纯均匀场衰减，与前版一致。
    /// 弹簧势能 `½·k·|p−a|²` 计入其中，使纯弹簧（c=0）系统下 KE+PE 守恒可被断言；
    /// 转动动能 `½·I·|ω|²` 一并计入，使自由旋转（inertia>0、无外力矩）下系统能量守恒；
    /// 阻尼（c>0）与摩擦属耗散，不构成势能，能量自然下降。
    pub fn total_mechanical_energy(&self) -> f64 {
        let mut energy = self
            .bodies
            .iter()
            .map(|b| {
                let ke = 0.5 * b.mass * b.vel.length_squared();
                // 转动动能：½·I·|ω|²（仅 inertia>0 的刚体贡献；0 视为无旋转自由度）。
                let ke_rot = if b.inertia > 0.0 {
                    0.5 * b.inertia * b.angular_velocity.length_squared()
                } else {
                    0.0
                };
                // 重力势能：默认以 y=0 为零点的均匀场 m*g*h；启用重力带时用箱阱势
                //（带内线性、带外平台，见 gravity_potential）。两者连续且力一致。
                let pe = self.gravity_potential(b);
                // 弹簧势能：½·k·|p−a|²
                let spring = match b.anchor {
                    Some(anchor) if b.stiffness > 0.0 => {
                        0.5 * b.stiffness * (b.pos - anchor).length_squared()
                    }
                    _ => 0.0,
                };
                // 双势阱势能：a·(x²−m)²（保守，计入能量守恒断言）。
                let well = match b.double_well {
                    Some(w) => {
                        let x = b.pos.x;
                        w.a * (x * x - w.m) * (x * x - w.m)
                    }
                    None => 0.0,
                };
                ke + ke_rot + pe + spring + well
            })
            .sum::<f64>();

        // 多体引力势能：U = -G * m_i * m_j / r，成对求和一次。
        let n = self.bodies.len();
        for i in 0..n {
            for j in (i + 1)..n {
                let r = (self.bodies[j].pos - self.bodies[i].pos).length();
                if r > f64::EPSILON {
                    energy -= G * self.bodies[i].mass * self.bodies[j].mass / r;
                }
            }
        }
        // 软铰弹簧势能：每个已声明关节对 ½·k·(d−L)²（与 force 同一语义，每次关节声明
        // 计一次）。c=0 纯弹簧下为保守核心，能量守恒可断言；阻尼为耗散不构成势能。
        for i in 0..n {
            if let Some(j) = self.bodies[i].joint {
                if j.body < n && j.stiffness > 0.0 {
                    let d = (self.bodies[j.body].pos - self.bodies[i].pos).length();
                    if d > f64::EPSILON {
                        let ex = d - j.rest_length;
                        energy += 0.5 * j.stiffness * ex * ex;
                    }
                }
            }
        }
        // 刚性铰链势能：位置约束 `½·k_p·|e|²`（锚点误差）+ 角度约束 `½·k_θ·|n_i−n_j|²`
        // （轴错位）。与铰链保守力同语义（`k_p`/`k_θ`>0 时计入），故纯约束（damping=0）下
        // 能量守恒可断言；`damping` 为耗散不构成势能。
        for i in 0..n {
            if let Some(h) = self.bodies[i].hinge {
                let o_i = self.bodies[i].orientation;
                let n_i = o_i.rotate_vec(h.local_axis);
                let a_i = self.bodies[i].pos + o_i.rotate_vec(h.local_pivot);
                let (a_j, n_j) = match h.body {
                    Some(j) if j < n => {
                        let o = self.bodies[j].orientation;
                        (
                            self.bodies[j].pos + o.rotate_vec(h.other_local_pivot),
                            o.rotate_vec(h.other_local_axis),
                        )
                    }
                    _ => (h.anchor, h.other_local_axis),
                };
                if h.position_stiffness > 0.0 {
                    let e = a_j - a_i;
                    energy += 0.5 * h.position_stiffness * e.length_squared();
                }
                if h.angular_stiffness > 0.0 {
                    let dn = n_i - n_j;
                    energy += 0.5 * h.angular_stiffness * dn.length_squared();
                }
            }
        }
        energy
    }
}

/// 有效角加速度：`α = (注入外力矩 + 有界姿态约束力矩 + 刚性铰链约束力矩)/I`。
/// 对 `inertia<=0`（无旋转自由度）的刚体恒为零。在 velocity Verlet 新旧状态各求一次，
/// 供旋转自由度做两遍半踢（与平动 (a_old+a_new)/2 同步，保持辛映射）。
fn effective_alpha(body: &RigidBody, tau_ext: Vec3) -> Vec3 {
    if body.inertia <= 0.0 {
        return Vec3::zero();
    }
    let tau = tau_ext + constraint_torque(body) + body.torque_accum;
    if tau == Vec3::zero() {
        Vec3::zero()
    } else {
        tau / body.inertia
    }
}

/// 有界姿态约束力矩（旋转域弹簧-阻尼 PD 控制）：
/// `τ = +k_ang·err − c_ang·ω`，其中 `err` 为「目标取向 ⊗ 当前取向⁻¹」的旋转向量——
/// 即把当前取向拉向目标所需的正向矫正旋转，故刚度项取正号（类比把 `p` 拉向 `a` 的方向）。
/// `|τ|` 钳制到 `max_torque`（`<=0` 视为无界）。k_ang=c_ang=0 或无约束时返回零力矩。
/// 纯耗散收敛、不发散——与无界恒定外力矩相对，是「有界控制力矩」的确定性实现。
fn constraint_torque(body: &RigidBody) -> Vec3 {
    if body.angular_stiffness <= 0.0 && body.angular_damping <= 0.0 {
        return Vec3::zero();
    }
    let err = match body.orient_target {
        Some(target) => (target * body.orientation.conjugate()).rotation_vector(),
        None => Vec3::zero(),
    };
    let mut tau = Vec3::zero();
    if body.angular_stiffness > 0.0 {
        tau += err * body.angular_stiffness;
    }
    if body.angular_damping > 0.0 {
        tau -= body.angular_velocity * body.angular_damping;
    }
    // 有界：|τ| 钳制到 max_torque（>0 生效；<=0 视为无约束上限）。
    let len = tau.length();
    let limit = if body.max_torque > 0.0 {
        body.max_torque.max(f64::EPSILON)
    } else {
        f64::INFINITY
    };
    if len > limit {
        tau = tau * (limit / len);
    }
    tau
}
