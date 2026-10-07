# template-services — 确定性模板渲染原生服务

面向「规则输出 → 文档/消息」的数据转换域：**上下文 + 模板 → JSON / Markdown / 纯文本**。
渲染为确定性纯函数（无时钟、无随机、无网络、无 IO），同输入逐字节同输出。

## 双入口（同一引擎）

| 入口 | 通路 | 落账 |
|---|---|---|
| 规则内 | `io_request(call_service, service_name=template_render, args={template, context, format})` | 既有 IO 审计通路 |
| REST | `POST /api/templates/render`（请求体 `{template, context, format}`） | 纯函数无落库（与 pack 模板 generate 同口径） |

两入口共用同一渲染引擎（本 crate `render` 模块），行为逐字节一致。

## 服务契约（fail-fast，不静默回退）

- `template`：必填字符串，非空，长度 ≤ 1 MiB；
- `context`：可选对象（缺省空对象），模板变量的取值来源；数值域为 TCB 值系统（仅整数，无浮点）；
- `format`：可选 `text | markdown | json`（缺省 `text`）；声明 `json` 时渲染产物额外做合法性校验（不是合法 JSON 即报错）；
- 成功返回：`{"status":"ok","format":<声明值>,"content":<渲染文本>}`。

## 语法 v1（与 pack 模板 `{{}}` 家族对齐）

### 变量替换

```
你好, {{user.name}}
```

- 路径为点分段（`a.b.c`），每段 `[A-Za-z_][A-Za-z0-9_]*`（数组下标访问不在 v1 语法内）；
- 标签内首尾空白忽略（`{{ name }}` ≡ `{{name}}`）；
- **路径缺失或解析为 null → 显式报错**（不静默空串回退）。

### 条件分支

```
{% if user.vip %}VIP 客户{% else %}普通客户{% endif %}
```

- 判定 = 路径解析成功且非 null 且非布尔假（`0` / `""` / `[]` 均为真）；
- **路径缺失按假处理**（存在性分支正是其用途——这是唯一允许缺失的位置）；
- `{% else %}` 可省略。

### 循环

```
{% for o in user.orders %}| {{o.id}} | {{o.total}} |
{% endfor %}
```

- 仅对数组迭代；**迭代源缺失或非数组 → 显式报错**（不静默空迭代）；
- 循环变量在体内可见；元素为对象时 `{{o.field}}` 直接取字段；
- 循环内仍可访问 context 根；同名时内层循环变量遮蔽外层与根。

### 值字符串化

| 值类型 | 输出 |
|---|---|
| String | 原样（不加引号） |
| Integer | 十进制 |
| Bool | `true` / `false` |
| Array / Object | 紧凑 JSON 形态（键序 = 排序序） |
| Null | 拒绝（报错） |

### 执行预算（超出如实报错，非静默截断）

- 模板 ≤ 1 MiB；输出 ≤ 8 MiB；循环总迭代数 ≤ 100,000（跨全部 `for` 累计，放大器防护）。

## v1 显式边界（不做即为不支持）

- 无过滤器（`{{a | upper}}` 之类不存在）；
- 无字面转义：含 `{{` 或 `{%` 的字面文本无法表达；
- 无行首空白控制（`{%-` 之类不存在）；
- 无内置时钟/随机函数——时间等非确定量由调用方作为显式参数放入 context（确定性红线）；
- `format=json` 不做自动转义：字符串值含引号/特殊字符时直接拼入 JSON 文本会产生非法
  JSON，校验将显式报错（构造 JSON 文档建议用 Object 值整体替换或规避特殊字符）。

## 与其他模板面的域边界

- **pack 规则模板**（`{{pack}}/{{template}}/{{form.*}}` 静态骨架参数替换）：规则实例化域；
  本服务语法与其同属 `{{}}` 家族，后续 pack 侧是否接入控制流另行评审，本插件不改 pack 语义；
- **evo-agent task_template**（`{node_id}` 单花括号，上游结果替换）：agent 编排域，不同域不强行统一。

## 服务声明

- 服务名：`template_render`（sensitive=false）；
- 声明 SSOT：本目录 `official_native_services.json`（治理侧种子经
  `scripts/sync-native-services.ps1` 同步，双侧守卫锁定漂移）。
