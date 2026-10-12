# 插件发布指南（WASM UDF 分发物）

> 面向读者：插件发布者（官方/第三方）与部署运维。
> 上游：[PLUGIN_GUIDE](PLUGIN_GUIDE.md) §8.1（分发完整性与信任模型）、[WASM_UDF_GUIDE](WASM_UDF_GUIDE.md)（UDF 编写）。
> 分发物 = **三件套**：`{name}.wasm` + `{name}.plugin-module.json`（分发清单）+ `{name}.wasm.sig`（分离签名）。

---

## 一、发布流程总览

```
① keygen   生成签名钥匙对（种子私钥私密保存，公钥入信任锚）
② build    编译 .wasm（wasm32-unknown-unknown，cdylib）
③ sign     生成清单 + 分离签名（wasm-host sign 子命令）
④ verify   离线自检（wasm-host verify 子命令，enforce 口径）
⑤ 提交     官方审核制：分发物交治理通道审核 → 运维放置市场目录
```

## 二、步骤详解

### ① 生成钥匙对

```powershell
evorule-wasm-host keygen
# seed_hex <64hex>        ← 私钥种子：私密保存（密码管理器/离线介质），永不落仓/入镜像/入日志
# public_key_hex <64hex>  ← 公钥：交运维登记信任锚
```

- 签名为 ed25519（RFC 8032 确定性）：同一私钥同一文件恒同签名，可离线复验。
- **公钥指纹多渠道公布**（官网/README/签署公告），运维登记前比对一致方可入锚。
- 私钥泄露应急：信任锚禁用对应 key_id（enabled=false）即可断供，泄露钥匙签发的
  历史分发物在 enforce 模式下即刻拒绝。

### ② 编译

```powershell
cd plugins/wasm-host/examples/<your-udf>
cargo build --release --target wasm32-unknown-unknown
# 产物：target/wasm32-unknown-unknown/release/<crate_name>.wasm
```

要求：纯计算（零 import）、三导出 ABI（memory/alloc/udf）、整数定点（TCB 无 Float）。

### ③ 签名（生成清单 + 分离签名）

```powershell
evorule-wasm-host sign --wasm <path>/<name>.wasm `
    --version 1.0.0 --key <seed_hex> --key-id <your-key-id> `
    [--notes "发布说明"] [--engine-min 0.2.0]
```

产出两件（与 .wasm 同目录）：
- `<name>.plugin-module.json`：清单（module/version/abi/blake3 内容哈希/purity 纯度声明）
- `<name>.wasm.sig`：分离签名（ed25519 over 清单文件精确字节）

签名链：`sig → 清单字节 → blake3 → .wasm 字节`——任何一环被篡改在加载期拒载。

### ④ 离线自检

```powershell
evorule-wasm-host verify --dir <分发目录> --keys trusted-keys.json --policy enforce
```

全 PASS 后方可提交。`enforce` 口径 = 三件套齐备 + 哈希一致 + 信任锚验签通过 + 纯度声明合法 + ABI 兼容。

### ⑤ 提交与放置（官方审核制）

1. 分发物三件套 + 发布说明交治理通道审核（内容审查 + 负面套件回归）。
2. 审核通过后运维放置：

```text
{marketplace_dir}/plugins/{plugin_id}/     ← 市场（console 插件类目展示，server 扫描即生效）
plugins/wasm/{plugin_id}.{三件套}          ← 运行目录（wasm-host 加载面）
```

3. `plugin.json` 策略表补服务声明（或依赖 auto_discover），重启 server 与 wasm-host。
4. 部署后巡检：`wasm-host /health` integrity 全 verified；市场列表 `GET /api/marketplace/plugins` 材料状态正确。
5. 上架完成后，console「插件」类目即刻可见（server 扫描式发现）：用户经治理确认一键安装
   （验签/落盘/热加载全自动，各阶段耗时回显）；安装动作进审计链（`platform.event.plugin_installed`）。

## 三、运维：信任锚管理

`wasm-host/trusted-keys.json`（示例见 `trusted-keys.example.json`）：

```json
{ "keys": [ { "key_id": "official-2026Q4", "public_key": "<64hex>",
              "note": "…", "enabled": true } ] }
```

- 登记/轮换/禁用 = **治理动作**（留痕）；验签执行 = 工程动作。
- `enabled=false` 视同未登记（该钥签发的一切分发物在加载期拒绝）。
- 验签策略 `WASM_HOST_SIGN_POLICY`：`warn`（缺省迁移期：材料缺失放行/存在即全量校验）｜
  `enforce`（三件套强制，生产推荐）｜`off`（等价既有行为）。
- **演示钥匙警示**：仓内 `marketplace-seed/trusted-keys.demo.json` 的 `demo-2026`
  为公开演示钥（私钥不公开但公钥开架），仅限本地试跑 enforce 流程；
  生产环境必须用 `keygen` 生成正式钥匙并按本章流程登记。

## 四、确定性红线（发布前自检清单）

- [ ] 纯计算：零 import（无文件/网络/随机/时钟——host 不提供，import 非空即拒）
- [ ] 整数定点：金额用最小单位（分）、比率用基点；TCB 的 JsonValue 无 Float
- [ ] 相对时间由调用方显式传入（如"今天"不得在 UDF 内取时钟）
- [ ] 相同输入逐字节相同输出（fuel 消耗可不同，输出必须确定）
- [ ] 入参 schema 写进清单/策略表 description（fail-visible 错误语义）

## 五、验收实测记录（本指南随批实测，2026-10-11）

- 5 官方示例（tax_calc / tax_bracket / tax_calc_as / text_normalize / date_diff_days）
  全三件套 + 演示钥签名，`verify --policy enforce` 全 PASS。
- enforce 模式加载运行目录 5/5 integrity=verified + 对账一致；
  两个新 UDF 真实调用回归（规范化 `hello world` / 闰年 29 天）。
- 市场放置后 `GET /api/marketplace/plugins` 5 个在架、三部件下载哈希对齐。

## 六、相关文档

- [PLUGIN_GUIDE](PLUGIN_GUIDE.md)（插件体系总纲、§8.1 分发完整性）
- [WASM_UDF_GUIDE](WASM_UDF_GUIDE.md)（UDF 编写与 ABI）
- 市场端点：`GET /api/marketplace/plugins`、`GET /api/marketplace/plugins/{id}/download?part=wasm|manifest|signature`
