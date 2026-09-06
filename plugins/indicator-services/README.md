# evorule-indicator-services — 确定性金融技术指标原生服务

第三个进程内原生插件 crate：把 Python/pandas 参考实现
（源：`规则引擎+数据处理器/indicator_calculator.py`）的指标语义**重写**为
evorule-server 原生 `IoHandler` 服务，使
`io_request(call_service/call_external, service_name=indicator_*)` 在进程内确定性执行。

> 与 `demo-services` / `physics-services` 结构同构：声明式注册（`NATIVE_SERVICES`）+
> 声明文件 SSOT（`official_native_services.json`，泛化）+ 插件清单启停。
> 本 crate 验证**第二种集成模式**——非 vendored 快照复制，而是「Python 参考实现
> 语义逐位对齐的 Rust 重写」，且新插件全链仅需追加式登记、机制代码零改动。
> 路由器机制件（trait / 声明项 / 过滤路由器）已上提 `core/plugin-kit` 公共 crate
> 三插件归一，本 crate 为薄壳具名委托，行为逐字节等价。

## 服务面（4 个，全部无状态、sensitive=false）

| 服务名 | 入参（args） | 出参 |
|---|---|---|
| `indicator_sma` | `series`（必填）、`window`（可选，默认 5，≥1） | `status`/`window`/`count`/`values`（前 window-1 位 warmup null） |
| `indicator_ema` | `series`（必填）、`span`（可选，默认 5，≥1） | `status`/`span`/`count`/`values` |
| `indicator_macd` | `series`（必填）、`fast`（可选，默认 12）、`slow`（可选，默认 26）、`signal_period`（可选，默认 9）；要求 fast < slow | `status`/`fast`/`slow`/`signal_period`/`count`/`macd`/`signal`/`hist` |
| `indicator_rsi` | `series`（必填）、`period`（可选，默认 14，≥2） | `status`/`period`/`min_periods`/`count`/`values`（屏蔽期 null） |

- `series` 为数值数组（元素：Integer 或数字字符串，TCB 无 Float 变体）；出参浮点一律字符串化（Rust `{:?}`，与 Python `repr` 同为最短往返表示，整值浮点保留 `.0` 后缀）。
- warmup/屏蔽期输出 JSON `null`（对应 pandas NaN → null 的语义契约）；NaN/Infinity 输入显式拒绝；空序列/窗口非法 fail-fast 如实报错。
- SMA 示例：

```json
{
  "service_name": "indicator_sma",
  "args": { "series": [1, 2, 3, 4, 5, 6], "window": 3 }
}
```

## 与 pandas 的语义对齐（正确性基准）

- 黄金值由 `gen_golden.py` 以 **pandas 3.0.5 实算**生成，硬编码进单测逐位断言。
- SMA：逐行移植 pandas `_libs/window/aggregations.pyx` 的
  `roll_mean`/`add_mean`/`remove_mean`/`calc_mean`——滚动窗口 Kahan 补偿求和
  （add/remove 各持独立持久补偿，先删后加）+ 同值/全符号产物修正
  （3000 组随机序列 Python 实证脚本 `verify_roll_mean.py` 随行）。
- EMA/MACD：`ewm(span=N, adjust=False)` 递推 `y_t = (1-α)·y_{t-1} + α·x_t`，
  α = 2/(N+1)；MACD = EMA(fast)−EMA(slow)，Signal = EMA(signal_period)(MACD 序列)，
  Hist = MACD−Signal。形态经实算验证逐位一致
  （`y += α(x−y)` 形态不匹配，`verify_ewm_form.py` 随行佐证），勿改。
- RSI：Wilder 平滑 `ewm(alpha=1/N, adjust=False)`；种子含 diff 首位 NaN 占位
  （0 / −0.0 IEEE 负零）与 min_periods 屏蔽期语义
  （`verify_rsi_warmup.py` 随行佐证）；分类边界（全涨=100/全跌=0/横盘=50）逐分支对齐。

## 确定性边界（诚实声明）

- 服务无状态、无墙钟、无随机源；f64 顺序固定的递推/累加 → **同平台同输入逐位一致**（单测锁定）。
- 跨平台浮点差异（x87 扩展精度、编译器 FFLAG 收缩等）不在承诺范围。
- 浮点字符串化超过日常量级（>1e16）的科学计数法格式差异不在承诺范围。

## 测试套

服务层单测（18 项）：4 服务黄金值逐位对齐（pandas 3.0.5 实算）/ 递推性质 /
同输入两次执行逐位一致 / warmup null 语义 / 非法输入 fail-fast（缺 series、
窗口非法、fast ≥ slow、NaN/Inf 拒绝）/ 路由与启用子集确定性 / 声明文件双侧守卫。

```bash
cargo test -p evorule-indicator-services
```
