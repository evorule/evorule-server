# evorule-physics-services — 确定性物理仿真原生服务

第二个进程内原生插件 crate（UV-035 泛化验证载体）：把 rpsm-demo 的确定性物理内核
（vendored 快照）封装为 evorule-server 原生 `IoHandler` 服务，使
`io_request(call_service/call_external, service_name=physics_*)` 在进程内确定性执行。

> 与 `demo-services` 结构同构：声明式注册（`NATIVE_SERVICES`）+ 声明文件 SSOT
> （`official_native_services.json`，UV-029 泛化）+ 插件清单启停（UV-030）。
> 新增插件不再需要修改机制代码中的专属特判——这是本 crate 的验证目标。
> 路由器机制件（trait / 声明项 / 过滤路由器）已上提 `core/plugin-kit` 公共 crate
> 三插件归一，本 crate 为薄壳具名委托，行为逐字节等价。

## 服务面（3 个，全部无状态、sensitive=false）

| 服务名 | 入参（args） | 出参 |
|---|---|---|
| `physics_simulate` | `bodies`（必填）、`dt`（必填，(0,1]）、`steps`（必填，[1,100000]）、`gravity`（可选，默认 [0,-9.81,0]）、`integrator_order`（可选，1\|2）、`restitution`（可选，0..=1）；刚体项：`mass`/`pos`/`vel` 必填，`radius`/`drag`/`friction` 可选 | `status`/`steps`/`integrator_order`/`bodies`（终态快照）/`total_mechanical_energy` |
| `physics_energy` | `bodies`（必填）、`gravity`（可选）、`grav_band`（可选，{lo,hi}，影响势能口径） | `status`/`total_mechanical_energy` |
| `physics_grav_band` | `physics_simulate` 全部参数 + `grav_band`（必填，{lo,hi}） | `status`/`steps`/`band`/`bodies`（含 `escaped`=终态高于带顶）/`total_mechanical_energy` |

- 浮点入参接受 Integer 或数字字符串（TCB 无 Float 变体）；出参浮点一律字符串化。
- 一切数值必须有限（NaN/Infinity 显式拒绝）；数量/步数上限为确定性执行预算保护。
- 单刚体示例：

```json
{
  "service_name": "physics_simulate",
  "args": {
    "bodies": [{ "mass": "1.0", "pos": ["0", "10", "0"], "vel": ["0", "0", "0"] }],
    "dt": "0.01", "steps": 100
  }
}
```

## 确定性边界（诚实声明）

- 内核：辛积分器（辛欧拉/速度 Verlet）+ 编译期锁定常量（G/C，无 setter）+ 固定 f64
  精度 + 无墙钟/无随机源/无外部 IO → **同平台同输入逐位一致**（单测锁定）。
- 跨平台浮点差异（x87 扩展精度、编译器 FFLAG 收缩等）不在承诺范围。
- 重力方向属可配置场，非锁定常量；锁定常量仅 G（多体引力）与 C。

## 内核来源与同步边界

- vendored 自独立实验仓 `rpsm-demo/rpsm-core`（workspace v0.1.0，2026-09-01 快照），
  一次性引入，除导入路径调整与 lint 定点豁免外逐行保真（`src/kernel/` 各文件头注）。
- rpsm 侧后续演进**不自动回灌**，升级须另立专项。
- 许可一致：两侧均为 AGPL-3.0-or-later。

## 测试套

服务层单测（13 项：3 服务行为/输入校验/同输入逐位一致/声明文件守卫）+ 内核集成测试
（`tests/`，自 rpsm `rpsm/tests` 于 2026-09-01 移植的内核正确性套件，65 项）：

| 移植文件 | 覆盖 |
|---|---|
| test_conservation / test_rotation | 能量守恒（两种积分器/双体轨道）、取向积分与自由旋转守恒 |
| test_contact_forces / test_spring | 弹簧-阻尼、接触摩擦、外力槽注入链路 |
| test_air_drag / test_quad_drag | 线性/平方空气阻力（终速解析判据、耗散单调性） |
| test_double_well / test_stratified_gravity | 双势阱保守势、有界重力带（箱阱势） |
| test_joint / test_hinge | 软铰（动量守恒）与刚性铰链（位置/角度约束、能量有界） |
| test_collision / test_rolling_collision | 恢复系数行为、旋转-碰撞耦合（滚动摩擦） |
| test_environment / test_constants_lock / test_thermal_determinism | 模板重力注入行为、G/C 锁定、BLAKE3 双跑终态哈希确定性 |

移植边界（诚实声明）：仅 rpsm-core 内核被 vendored，故依赖 rpsm_pla /
rpsm_hci / rpsm_dkel 的测试段以等效形式移植（模板查值→常量字面量、规则求值→
同语义本地函数、面板缺省→内核缺省），并在文件头注记逐条说明；PLA 观测/
回溯/账本 3 个测试文件（test_pla / test_pla_hinge / e2e_modeled_force_demo）
无内核侧等效面，未移植。各文件测试逻辑相对原文件逐行保真（除 import 改路）。

## 构建/测试

```bash
cargo test -p evorule-physics-services
```
