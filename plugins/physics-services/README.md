# evorule-physics-services — 确定性物理仿真原生服务

第二个进程内原生插件 crate（UV-035 泛化验证载体）：把 rpsm-demo 的确定性物理内核
（vendored 快照）封装为 evorule-server 原生 `IoHandler` 服务，使
`io_request(call_service/call_external, service_name=physics_*)` 在进程内确定性执行。

> 与 `demo-services` 结构同构：声明式注册（`NATIVE_SERVICES`）+ 声明文件 SSOT
> （`official_native_services.json`，UV-029 泛化）+ 插件清单启停（UV-030）。
> 新增插件不再需要修改机制代码中的专属特判——这是本 crate 的验证目标。

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

## 构建/测试

```bash
cargo test -p evorule-physics-services
```
