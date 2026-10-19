# Quickstart：从零到第一次确定性计算

> 本指南带你在十分钟内完成：启动 evorule-server → 挂载技能/接入 MCP →
> 发起第一次可验证的确定性计算调用。

## 前置条件

- **evorule-server 已运行**：安装包安装后随系统启动；开发形态可用
  `pwsh scripts/start-resident-serve.ps1`（仓库内）或 Docker 镜像；
- **curl** 或任意 HTTP 客户端（示例用 curl）；
- 走 Agent/MCP 路线时额外需要：Node ≥ 18（MCP 适配器，零 npm 依赖）。

## 1. 确认服务在跑

```bash
curl http://127.0.0.1:18080/api/health
```

返回 `"success": true` 即就绪。返回体里 `plugins` 字段按家族列出全部
在役服务——这就是你的**能力目录**。

## 2. 发起第一次调用（REST）

原生计算服务全部走同一个入口：

```bash
curl -X POST http://127.0.0.1:18080/api/services/gov_ratio_split/invoke \
  -H "Content-Type: application/json" \
  -d '{"total":"100","weights":["1","1","1"],"mode":"integer","scale":0,"rounding":"half_even"}'
```

返回：

```json
{
  "status": "determined",
  "determinism": "bitwise_replayable",
  "shares": [...],      # [34, 33, 33] —— 和恰为 100
  "sum_check": true
}
```

`status: determined` + `determinism: bitwise_replayable` 是每个结果的
承诺：**同一输入再跑一遍，输出逐位一致**。不信？现在就再跑一次对照。

### 调用契约三条（记住这三条就不会踩坑）

1. **数字一律字符串**：`"19.99"` 或精确分数 `"7992739871/1000000000"`——
   永远不用 JSON number，杜绝浮点；
2. **契约要求 `scale`/`rounding` 就必须给**：服务端从不隐式舍入；
3. **失败是显式的**：错误信息 fail-visible 且带自诊断指引（缺什么、
   违反什么约束、怎么改），原样读即可；`undetermined` 是诚实的能力
   边界声明，不是故障。

## 3. 三种用法，按需选择

### 3.1 REST 直调（程序集成）

见上。全部端点见 `GET /api/services`（含每个服务的参数契约 JSON Schema）。

### 3.2 挂载到你的 AI Agent（Agent Skills 开放标准）

把 `skills/evorule-deterministic-compute/` 目录复制进你的 agent 技能
目录即可。技能教会模型：**哪些计算应该路由给基座、如何填参、如何
解读结果**——模型负责理解问题，基座负责算得对。

### 3.3 MCP 接入（Claude Desktop 等 MCP 宿主）

零依赖、零安装（Node ≥ 18）：

```json
{
  "mcpServers": {
    "evorule": {
      "command": "node",
      "args": ["/path/to/evorule-server/mcp/evorule-mcp.mjs"],
      "env": { "EVORULE_BASE": "http://127.0.0.1:18080" }
    }
  }
}
```

两个工具：`list_services`（目录+契约）、`invoke_service`（确定性调用）。
详见 `mcp/README.md`。

## 4. 能力家族速览

| 家族 | 能力 |
|---|---|
| numeric | 精确有理/十进制算术、数论、利息/摊销、单位与量纲换算、变量绑定 |
| mechanics | 牛顿第二定律、运动学、动量冲量、机械能、欧姆定律、电阻网络、PID 单步 |
| gnss | GPS 卫星钟：工厂预偏置、广播钟多项式、偏心率修正、相对论漂移、总修正合成、测距误差 |
| grav | 史瓦西半径、近日点进动、光线偏折 |
| fluid | 伯努利、连续性、静水压 |
| quantum | 无限深势阱、量子谐振子能级（J 与 eV） |
| metrology | GUM 不确定度传播 |
| checksum | Luhn、ISBN-10/13、IBAN（校验+补位） |
| cny | 人民币大写金额（票据规范） |
| gov | 比例分配（最大余数法）、数组聚合、时长差、区间对 |
| legal | 期间届日计算（起算日不计入、对应日、节假日/周末顺延） |
| indicator | SMA/EMA/MACD/RSI |
| physics | 刚体仿真、能量、重力带 |
| inference | SI 量纲提取/比较/验证 |
| geometry | 碰撞/间隙/遮挡检查 |
| template | 确定性模板渲染 |

完整目录以 `GET /api/services` 实时返回为准——新服务上线无需更新本文。

## 5. 下一步

- 逐家族示例与诚实边界：`skills/evorule-deterministic-compute/SKILL.md`
- MCP 配置详情：`mcp/README.md`
- 把确定性计算接进你自己的 agent：见 SKILL.md 的"何时路由"一节
