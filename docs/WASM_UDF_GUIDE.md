# WASM UDF 编写指南

> 适用版本：evorule-server 0.6.0+（wasm-host 插件契约 v1.2）
> 范本：[`plugins/wasm-host/examples/udf-finance-tax-calc/`](../plugins/wasm-host/examples/udf-finance-tax-calc/)（Rust guest）、
> [`plugins/wasm-host/examples/udf-finance-tax-calc-as/`](../plugins/wasm-host/examples/udf-finance-tax-calc-as/)（AssemblyScript guest，证明 ABI 语言无关）
> 外部插件包通用规范（plugin.json / 挂载 / 探活 / 管理面）见 [PLUGIN_GUIDE.md](PLUGIN_GUIDE.md)——本指南只讲 UDF 特有部分。

---

## 〇、UDF 是什么：先看清形态

UDF（User-Defined Function，自定义函数）是**在 WASM 沙箱内运行的纯计算函数**，以 `.wasm` 模块文件交付，由 `evorule-wasm-host` 宿主进程加载执行。对 server 而言，wasm-host 就是一个普通外部插件包（`plugins/wasm-host/plugin.json` + `plugin_manifest.json` 登记），UDF 是它内部装载的模块：

```
规则 call_service / REST invoke
  → evorule-server（按 plugin.json 声明路由）
  → POST http://127.0.0.1:9140/services/{服务名}
  → evorule-wasm-host（wasmtime 沙箱，进程外独立部署）
  → 执行 plugins/wasm/{服务名}.wasm
```

**与外部插件包的分工**：任意语言实现、有状态、需审批、需要网络/文件/数据库能力的业务能力 → 外部插件包（自管进程）；**纯计算、必须确定性、不碰任何外部世界**的函数 → UDF。二者都经 `GET /api/services` 对账清单呈现，调用方无感知差异。

**沙箱的核心承诺（也是对 UDF 的硬要求）**：同入参 → 逐字节同出参。沙箱内**不存在**时钟、随机数、环境变量、文件、网络——不是"不建议用"，是物理上不存在（零 import，见 §一）。做不到确定性的逻辑不适用 UDF 形态。

---

## 一、三条硬性约束（红线）

| # | 约束 | 违反后果 |
|---|------|---------|
| 1 | **零 import**：模块不得声明任何 import（WASI、env、时钟、随机、内存导入均不允许） | 加载期枚举 import，非空即**拒绝装载**（启动失败，fail-fast）；实例化期 Linker 零注册作为第二道防线 |
| 2 | **整数定点**：入参出参一律整数（金额用最小货币单位"分"、比率用"基点"，1bp = 0.01%） | 浮点在 server 侧被降级为字符串，**根本到不了 UDF**（TCB 的 JSON 值无浮点变体，防浮点尾差破坏审计哈希确定性） |
| 3 | **错误约定**：出参 JSON 顶层含 `error` 字段 = 业务失败 | 宿主返回 422，规则侧显式感知失败；不带 `error` 字段则按成功透传（200） |

约束 1 的理由：import 是 UDF 唯一能获得"沙箱外能力"的通道，零 import = 纯计算可静态证明。约束 2 的理由：浮点乘法在不同硬件/优化级别下可能有尾差，会让相同输入产生不同审计哈希——计税口径本就该用整数定点。

---

## 二、Guest ABI（模块必须导出三个符号）

| 导出 | 签名 | 职责 |
|------|------|------|
| `memory` | 线性内存 | 入参/出参的交换区（Rust `cdylib` 自动导出；AssemblyScript 需默认内存导出） |
| `alloc` | `(len: i32) -> i32` | 为入参分配空间，返回指针；**返回 0 表示失败**（宿主立即报错） |
| `udf` | `(ptr: i32, len: i32) -> i64` | UDF 入口。宿主先把入参 JSON 写入 `alloc` 返回的缓冲再调用；返回值打包为 **`(结果指针 << 32) \| 结果长度`**，指向出参 JSON 字节 |

调用时序（宿主侧固定流程，guest 只需实现三个导出）：

```
① 宿主调 alloc(in_len) 拿到写入地址
② 宿主把入参 JSON 字节写进该地址
③ 宿主调 udf(in_ptr, in_len)
④ 宿主从返回值解包 (ptr, len)，读出出参 JSON 字节
```

要点：

- **入参就是原始 JSON**：调用方 `args` 对象序列化后的字节，宿主不解析不包装，guest 自行按约定读取字段；
- **每次调用一个全新实例**：宿主每次调用新建 Store 与实例，调用结束即整体丢弃。上次调用的内存不残留、guest 内的全局变量回到初始态——UDF 天然无状态，也无需实现 `dealloc`（内存随实例回收）；
- **返回 0 指针 = 失败**：`udf` 返回值高 32 位为 0 时宿主报「UDF 返回空指针」（422）；
- **出参必须是合法 JSON**：解析失败同样 422（错误文案含中文时注意 UTF-8 编码，AssemblyScript 的 `charCodeAt` 是 UTF-16 码元，见范本 `put()` 的三字节编码处理）。

---

## 三、沙箱资源约束（宿主强制，UDF 无法绕过）

| 维度 | 缺省值 | 环境变量 | 超限行为 |
|------|--------|---------|---------|
| fuel 执行预算 | 100,000,000 | `WASM_HOST_FUEL` | 耗尽即 trap → 422「UDF 执行超出 fuel 预算——疑似死循环或计算量过大」 |
| 单模块内存上限 | 16 MiB | `WASM_HOST_MAX_MEM` | `memory.grow` 被拒绝 → guest 分配失败 → trap → 422（宿主进程不受影响） |
| 表元素上限 | 10,000 | （固定） | `table.grow` 被拒绝 → trap |

三项均有随仓负面测试实证（见 §八）：死循环模块在 fuel 耗尽时被精确拦截、巨型分配模块被内存上限即时拒绝、含 WASI import 的模块在**加载期**即被拒——宿主全程存活，失败秒级返回。

设计含义：

- **大计算量任务不适配**：fuel 预算对常规计算（如税额、校验、编码转换）绰绰有余，但遍历大数组/深度递归会撞墙。需要重计算的走外部插件包；
- **大内存任务不适配**：16 MiB 上限下不能把大文件整个读进内存处理；
- **不要依赖跨调用状态**：每次调用新建实例，任何"缓存上一次结果"的写法都无效。

---

## 四、写一个 UDF（两个语言范本）

### 4.1 Rust 范本（推荐）

范本：[`examples/udf-finance-tax-calc/`](../plugins/wasm-host/examples/udf-finance-tax-calc/)。关键配置（`Cargo.toml`）：

```toml
[lib]
crate-type = ["cdylib"]

[profile.release]
opt-level = "z"   # 产物越小越好（UDF 会被加载）
lto = true
strip = true
panic = "abort"   # UDF 无 unwinding 价值，abort 产物更小
```

代码骨架（完整实现见范本 `src/lib.rs`）：

```rust
use std::alloc::Layout;

#[no_mangle]
pub extern "C" fn alloc(len: i32) -> i32 {
    // Layout::from_size_align(len, 1) + std::alloc::alloc；失败返回 0
}

#[no_mangle]
pub extern "C" fn udf(ptr: i32, len: i32) -> i64 {
    // ① 读入参：std::slice::from_raw_parts(ptr as *const u8, len as usize)
    // ② 解析 JSON、做纯计算（整数定点！）
    // ③ 失败 → 出参写 {"error":"..."}；成功 → 写结果 JSON
    // ④ 写入新分配缓冲，返回 ((p as u64) << 32) | (bytes.len() as u64)
}
```

构建（产物 = `target/wasm32-unknown-unknown/release/udf_finance_tax_calc.wasm`）：

```bash
rustup target add wasm32-unknown-unknown
cd plugins/wasm-host/examples/udf-finance-tax-calc
cargo build --release --target wasm32-unknown-unknown
```

### 4.2 AssemblyScript 范本（ABI 语言无关实证）

范本：[`examples/udf-finance-tax-calc-as/`](../plugins/wasm-host/examples/udf-finance-tax-calc-as/)。与 Rust 版**语义完全相同、同参调用输出逐字节一致**（键序、数字格式对齐）。要点：

- 编译命令 `npm run build`（`asc --runtime stub --use abort=`）：stub runtime + 禁用 abort **保证零 import**——默认 runtime 会引入 `env.abort` 与 GC import，直接被沙箱拒载；
- 示例用静态缓冲 + 手写 JSON 字段扫描，不引入任何依赖（依赖库可能带 import）；
- 错误文案含非 ASCII 字符时必须做 UTF-8 编码（范本 `put()` 函数）。

---

## 五、部署与启停

### 5.1 上线一个新 UDF（两种模式）

**静态声明制（缺省，四步）**：

1. **放置模块**：`.wasm` 复制到 `plugins/wasm/`。**服务名 = 文件名去扩展名**（如 `udf_fx_spot_rate.wasm` → 服务名 `udf_fx_spot_rate`）；
2. **登记声明**：`plugins/wasm-host/plugin.json` 的 `services[]` 追加同名条目（`name`/`description`/`parameters` 参数契约/`sensitive`），字段规范同 [PLUGIN_GUIDE §三](PLUGIN_GUIDE.md#三pluginjson-规范插件包-ssot)；
3. **重启 host**：模块清单在启动期扫描，运行期不热加载；含坏模块（非法 wasm / 带 import）宿主**拒绝启动**并点名（fail-fast，不会带病运行）；
4. **重启 server**：plugin.json 变更需重新装载清单生效。

**自动发现模式（`plugin.json` 声明 `"auto_discover": true`，两步）**：

1. **放置模块**：`.wasm` 复制到 `plugins/wasm/`（服务名 = 文件名去扩展名，同上）；
2. **重启 host 与 server**（先 host 后 server）——server 装载期自动拉取 host 实载清单（`GET /services`）合入，plugin.json 零改动。

> 自动发现模式下 `services[]` 降级为**可选策略表**：默认策略 `sensitive:false` +
> 缺省超时（启动日志逐条明示）；需要敏感守卫或参数契约的服务，在策略表里**显式声明**
> （显式声明永远优先，不被自动发现覆盖）。完整语义（发现/合入/失败降级/loopback 约束）
> 见 [PLUGIN_GUIDE §3.3](PLUGIN_GUIDE.md#33-auto_discover自动发现策略表模式)。

> 静态声明制的注意点：声明是**静态**的——plugin.json 里声明的服务必须与 `plugins/wasm/` 实际模块一一对应。声明了但无模块（或反之）不会静默：宿主 `/health` 对账判 503（§六），server 探活随即报警。auto_discover 模式下「实载有、策略表无」是合法形态（自动发现接管），仅「策略表声明了但实载没有」仍判 503。

### 5.2 启停顺序：先 host，后 server

```
① evorule-wasm-host（先起，/health 就绪）
② evorule-server --plugins plugin_manifest.json（后起）
```

顺序反了不算故障：server 探活会把 host 判 offline 并报警，host 就绪后下一探活周期自动恢复并关警留痕。但规范顺序可以避免启动窗口的告警噪音。

验证就绪：

```bash
# host 侧：200 = 声明与实载一致（对账语义见 §六）
curl http://127.0.0.1:9140/health
# server 侧：plugins 节出现 wasm-host 且 status=online
curl http://127.0.0.1:18080/api/health
```

### 5.3 本地启停参数

| 进程 | 关键参数 |
|------|---------|
| host | `WASM_HOST_ADDR`（默认 `127.0.0.1:9140`，须与 plugin.json `base_url` 一致）、`WASM_HOST_DIR`（.wasm 目录，默认 `../wasm`）、`WASM_HOST_FUEL`、`WASM_HOST_MAX_MEM`、`WASM_HOST_PLUGIN_JSON` |
| server | `--plugins plugin_manifest.json`（**不传则外部插件全不装载**）、`--plugin-probe-interval <秒>`（探活周期，默认 30）、本地 127.0.0.1 需 `--allow-loopback` |

---

## 六、探活与健康对账（server 怎么看 host 死活）

wasm-host 的 `/health` 不是简单的"进程活着"：

| 对账结论 | /health | server 探活判定 |
|---------|---------|----------------|
| plugin.json 声明与实载模块一致 | **200** | `online` |
| 可判定的不一致（0 个模块 / 声明了但未加载 / 加载了但未声明） | **503** | `offline` → 记 `plugin_offline` 报警事件（控制台自诊断 + 审计链留痕） |
| auto_discover 模式：策略表声明了但实载没有 | **503** | `offline`（策略表 = 超集校验，声明必在实载内） |
| auto_discover 模式：实载有、策略表没有 | 200（body `undeclared` 留痕） | `online`（合法形态：目录 = 身份事实源，自动发现接管） |
| 找不到 plugin.json 无法对账 | 200（body 标注未对账） | `online`（拒绝假警：无法判定不当故障） |

**host 掉线时的调用方语义**（均有端到端验证）：直调 `POST /api/services/{名}/invoke` 返回 **502 + 「上游连接失败（服务不可达）」**（秒级返回，不悬挂；响应不泄漏内网拓扑，细节只进服务端日志）；`GET /api/health` 的 `plugins` 节该插件 `status=offline` 并附 `last_error`。恢复后探活自动翻转 `online` 并记 `plugin_online` 关警。

---

## 七、敏感服务守卫

plugin.json 声明 `sensitive: true` 的 UDF 服务，REST 直调一律 **403**——敏感操作必须经会话 `call_service` 指令走审计与审批链（治理语义同 [PLUGIN_GUIDE §八](PLUGIN_GUIDE.md#八安全与信任模型)）。纯计算 UDF 一般无需敏感标记；涉及业务敏感口径（如内部费率表）时如实声明。

---

## 八、验证资产（随仓回归）

| 资产 | 覆盖 |
|------|------|
| `plugins/wasm-host/tests/negative/run_negative_tests.py` | 三类恶意/缺陷模块的宿主韧性：死循环（fuel 拦截）、巨型分配（内存上限拦截）、WASI import（加载期拒绝）——全程宿主存活、失败秒级返回 |
| `plugins/wasm-host/tests/verify_v6_offline.py` | host 掉线全链语义：502 脱敏、探活翻转、报警双通道 |
| `plugins/wasm-host/tests/verify_t5_concurrency.py` | 16 路并发无串扰 + 确定性（同入参逐字节同出参） |
| `plugins/wasm-host/tests/verify_t6_sensitive_guard.py` | sensitive 声明 → 直调 403、对账清单透传 |

改动 guest ABI、资源约束或探活语义后必须全量重跑（脚本自带临时环境，互不污染）：

```bash
python plugins/wasm-host/tests/negative/run_negative_tests.py
python plugins/wasm-host/tests/verify_v6_offline.py
python plugins/wasm-host/tests/verify_t5_concurrency.py
python plugins/wasm-host/tests/verify_t6_sensitive_guard.py
```

---

## 九、常见陷阱速查

| 症状 | 根因 | 处置 |
|------|------|------|
| 浮点参数到 guest 手里是字符串（如 `"0.06"`） | TCB 无浮点变体，非整数 JSON number 降级字符串 | 改整数定点：金额用分（`100000`）、比率用基点（`600`） |
| host 启动失败「模块声明了 N 个 import」 | guest 引入了 WASI/env 依赖（Rust 默认 target 下用 panic=unwinding 或某些 crate 会引入） | 零依赖纯计算；Rust 用 `panic = "abort"`；AS 用 `--runtime stub --use abort=` |
| 直调 422「UDF 执行超出 fuel 预算」 | 计算量超过 fuel 预算（死循环或大循环） | 优化算法或降低规模；重任务改走外部插件包 |
| 直调 422 且报分配/内存相关 trap | 分配超过 16 MiB 内存上限 | 缩小内存足迹；大输入拆批 |
| 直调 422「返回值不是合法 JSON」 | 出参字节非法（AS 中文字符未做 UTF-8 编码是典型） | 见范本 `put()` 的编码处理 |
| `GET /api/services` 有服务但 `/health` 报 503 | plugin.json 声明与 `plugins/wasm/` 实际模块不同步 | 静态制：补齐缺失侧（放模块或删声明），重启 host；auto_discover 模式：仅「策略表声明了但实载没有」会 503，按 reason 自诊断补齐 |
| server 起来后没有该插件 | 未传 `--plugins`（显式安装语义） | 启动命令补 `--plugins plugin_manifest.json` |

---

## 相关文档

- [PLUGIN_GUIDE.md](PLUGIN_GUIDE.md) — 外部插件包通用规范（plugin.json 字段/挂载/探活/管理面/装卸）
- [INTEGRATION_GUIDE.md](INTEGRATION_GUIDE.md) — server 集成与启动参数
- 范本源码：[`plugins/wasm-host/examples/`](../plugins/wasm-host/examples/)（Rust + AssemblyScript 双语言）
