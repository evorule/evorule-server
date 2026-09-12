// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 插件契约 v1 落地：声明式 pack 装载 + 资产面只读 API + 模板草稿生成纯函数
//!
//! 契约 SSOT：`D:\knowledge\2-Projects\evorule-plugin\02-Plugin-Contract-v1.md`
//!
//! 红线对照（契约 §0，实施前复核表 §8）：
//! - R1 确定性：`generate_draft` 是纯函数——同（模板字节, 表单值）→ 字节级同输出；
//!   零随机/零时钟/零 IO。生成语言 v1 只有 `{{form.X}}`/`{{pack}}`/`{{template}}`
//!   无逻辑替换，未知占位符 fail-fast（装载期与生成期双重校验）。
//! - R2 协议不可达：用户表单值只落在模板骨架的**值位**；`{{form.X.path}}` 的取值域
//!   被 `scene_field` 类型锁定为"场景已注册字段"，非自由字符串。
//! - R3 draft-only：本模块无任何写入治理状态/规则库的路径——generate 只在内存
//!   中构造草稿并返回，不落库；生效必须经用户确认走既有 Draft→Publish 链。
//! - R5 locale 纯展示：display_name 等双语字段仅为展示数据，不进事实/命令。

use std::collections::BTreeMap;
use std::path::Path as FsPath;
use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use serde_json::Value;

use super::server::SessionApi;

// ===== 任务级操作者上下文（契约 §3.3 身份透传） =====

/// 当前请求的操作者上下文（由 invoke 处理器经 task_local 作用域注入，
/// 由 [`actor_headers`] 在 ServiceRegistryHandler 动态头解析器中读取）。
#[derive(Debug, Clone)]
pub struct ActorContext {
    /// `user` | `anonymous`（v1 薄切片；service 身份随平台服务 token 演进）
    pub actor_type: String,
    /// 不透明操作者 id（登录名 / "anonymous"）
    pub actor_id: String,
}

tokio::task_local! {
    pub(crate) static CURRENT_ACTOR: ActorContext;
}

/// 动态头解析器入口（main.rs 注入 `ServiceRegistryHandler::with_dynamic_headers`）。
///
/// 在 [`ActorContext`] 作用域内（REST invoke 路径）返回身份头；作用域外
/// （会话 io_request 链等）返回空——io_request 的审计归因走 Fact 链，不靠头。
pub fn actor_headers() -> Vec<(String, String)> {
    CURRENT_ACTOR
        .try_with(|a| {
            vec![
                ("X-Evorule-Actor-Type".to_string(), a.actor_type.clone()),
                ("X-Evorule-Actor-Id".to_string(), a.actor_id.clone()),
            ]
        })
        .unwrap_or_default()
}

// ===== Pack 数据模型 =====

/// v1 表单控件词表（契约 §4.5 固定枚举；新增 = 契约 v2 事件，console 冻结线 R4）
pub const CONTROL_VOCAB: &[&str] = &[
    "text",
    "textarea",
    "number",
    "currency",
    "date",
    "boolean",
    "enum",
    "scene_field",
];

/// v1 已知能力集（契约 §2.2）
const KNOWN_CAPABILITIES: &[&str] = &["assets", "services", "flow-compile", "ai-assist"];

/// 已装载的声明式插件包（启动期从 pack 目录读盘注册，运行期只读——契约 §4.1.3）
#[derive(Debug)]
pub struct PluginPack {
    pub id: String,
    pub contract_version: String,
    pub version: String,
    pub description: String,
    pub capabilities: Vec<String>,
    /// 已校验的场景资产原值（API 原样返回）
    pub scenes: Vec<Value>,
    /// 场景字段索引：scene_id → (field_id → state path)。scene_field 替换与
    /// `.path` 锁定（R2）的唯一事实来源；path 缺失的字段不在索引内（不可用于 .path）
    pub scene_fields: BTreeMap<String, BTreeMap<String, String>>,
    pub templates: Vec<TemplateAsset>,
    /// 已校验的流程资产原值（契约 v1.1；API 原样返回；引用字段在编译代理时才解析）
    pub flows: Vec<Value>,
    /// 已校验的设计器节点类型资产（契约 §4.4；画布节点面板/属性表单唯一来源，R4）
    pub node_types: Vec<Value>,
    /// flow-compile 能力包的编译服务根地址（契约 v1.1：本地插件必须 http://127.0.0.1:*）
    pub base_url: Option<String>,
}

#[derive(Debug)]
pub struct TemplateAsset {
    pub template_id: String,
    pub raw: Value,
    pub params_form: Vec<ParamField>,
    /// 模板骨架（已做装载期占位符全量校验：只含合法 `{{...}}`）
    pub skeleton: Value,
}

#[derive(Debug, Clone)]
pub struct ParamField {
    pub field_id: String,
    pub ftype: String,
    pub required: bool,
    pub default: Option<Value>,
    pub options: Option<Vec<String>>,
    pub scene_ref: Option<String>,
}

/// 已注册 pack 注册表（SessionApi 共享状态成员；启动期构造后不可变）
#[derive(Debug, Clone, Default)]
pub struct PluginPackRegistry {
    packs: BTreeMap<String, Arc<PluginPack>>,
}

impl PluginPackRegistry {
    pub fn empty() -> Self {
        Self::default()
    }

    pub fn get(&self, id: &str) -> Option<Arc<PluginPack>> {
        self.packs.get(id).cloned()
    }

    pub fn list(&self) -> Vec<Arc<PluginPack>> {
        self.packs.values().cloned().collect()
    }

    pub fn len(&self) -> usize {
        self.packs.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.packs.is_empty()
    }

    fn insert(&mut self, pack: PluginPack) {
        self.packs.insert(pack.id.clone(), Arc::new(pack));
    }
}

// ===== 装载（fail-fast，契约 §2.4） =====

/// 插件清单中 pack 条目的并行解析模型（与 main.rs `PluginManifestFile` 同构兼容：
/// 旧清单无 pack 字段 → 本模型解析仍成功，只是没有 pack 条目）
#[derive(serde::Deserialize)]
struct PackManifestFile {
    #[serde(default)]
    plugins: BTreeMap<String, PackManifestEntry>,
}

#[derive(serde::Deserialize)]
struct PackManifestEntry {
    #[serde(default = "default_true")]
    enabled: bool,
    #[serde(default)]
    manifest: Option<String>,
    /// 声明式 pack 路径（相对插件清单所在目录；契约 §2.1）
    #[serde(default)]
    pack: Option<String>,
}

fn default_true() -> bool {
    true
}

/// 从插件清单装载全部声明式 pack（fail-fast：任何校验失败 = 拒绝启动 + 自诊断指引）。
///
/// 与 main.rs `load_external_plugins` 同读一份插件清单；pack 条目用 `pack` 键，
/// 外部服务包用 `manifest` 键，二者同条目混用 → fail-fast。
pub fn load_plugin_packs(manifest_path: Option<&FsPath>) -> Result<PluginPackRegistry, String> {
    let Some(p) = manifest_path else {
        return Ok(PluginPackRegistry::empty());
    };
    let content = std::fs::read_to_string(p).map_err(|e| {
        format!(
            "读取插件清单失败 {}: {}（自诊断指引: ① 确认 --plugins 路径正确; \
             ② 确认进程对该文件有读权限）",
            p.display(),
            e
        )
    })?;
    let manifest: PackManifestFile = serde_json::from_str(&content)
        .map_err(|e| format!("插件清单 JSON 非法 {}: {}", p.display(), e))?;
    let manifest_base = p
        .parent()
        .map(FsPath::to_path_buf)
        .unwrap_or_else(PathBuf::new);
    let mut registry = PluginPackRegistry::empty();
    for (id, entry) in &manifest.plugins {
        let Some(rel) = entry.pack.as_ref() else {
            continue;
        };
        if entry.manifest.is_some() {
            return Err(format!(
                "插件清单条目 '{id}' 同时声明 pack 与 manifest — 二者互斥 \
                 （pack=声明式资产包, manifest=外部服务包）"
            ));
        }
        if !entry.enabled {
            tracing::info!("插件契约 pack: {id} enabled=false — 不装载");
            continue;
        }
        let rel_path = PathBuf::from(rel);
        let pk_path = if rel_path.is_absolute() {
            rel_path
        } else {
            manifest_base.join(rel_path)
        };
        let pack = load_pack(id, &pk_path)?;
        tracing::info!(
            "插件契约 pack: {id} v{} 装载（{} 场景 + {} 模板, contract {}）",
            pack.version,
            pack.scenes.len(),
            pack.templates.len(),
            pack.contract_version,
        );
        registry.insert(pack);
    }
    Ok(registry)
}

/// 装载单个 pack.json（fail-fast 校验链：字段存在性 → 契约版本 → 能力集 → 资产）
// 各资产 kind（scenes/templates/flows/node_types）的装载链共享 fail-fast 错误上下文，
// 提取子函数会切断错误信息中的 pack id 指引；Phase C 累积后超行数阈值，按仓库先例豁免。
#[allow(clippy::too_many_lines)]
fn load_pack(entry_id: &str, pack_json_path: &FsPath) -> Result<PluginPack, String> {
    let raw = std::fs::read_to_string(pack_json_path).map_err(|e| {
        format!(
            "pack.json 读取失败 {}: {e}（自诊断指引: ① 路径相对插件清单所在目录; \
             ② pack.json 是声明式包 SSOT,不可读即拒绝装载）",
            pack_json_path.display()
        )
    })?;
    let v: Value = serde_json::from_str(&raw)
        .map_err(|e| format!("pack.json JSON 非法 {}: {e}", pack_json_path.display()))?;
    let obj = v.as_object().ok_or_else(|| {
        format!(
            "pack.json {} 顶层必须是 JSON object",
            pack_json_path.display()
        )
    })?;
    // 必填字段（契约 §2.2）
    let id = str_field(obj, "id", pack_json_path)?;
    let contract_version = str_field(obj, "contract_version", pack_json_path)?;
    let version = str_field(obj, "version", pack_json_path)?;
    let description = str_field(obj, "description", pack_json_path)?;
    if id != entry_id {
        return Err(format!(
            "pack id 漂移: 清单条目键 '{entry_id}' 与 pack.json 声明 id '{id}' 不一致（{}）",
            pack_json_path.display()
        ));
    }
    // 契约版本 MAJOR 不符拒载（契约 §7.1；与生态 fail-fast 纪律一致）
    if !contract_version.starts_with("1.") {
        return Err(format!(
            "pack {id} contract_version='{contract_version}' 与本 server 支持的 v1.x 不兼容 \
             — MAJOR 不符拒载（契约 §7.1）；请升级 pack 或 server"
        ));
    }
    let capabilities = obj
        .get("capabilities")
        .ok_or_else(|| missing("capabilities", pack_json_path))?
        .as_array()
        .ok_or_else(|| format!("pack {id} capabilities 必须是字符串数组"))?
        .iter()
        .map(|c| {
            c.as_str()
                .map(String::from)
                .ok_or_else(|| format!("pack {id} capabilities 必须是字符串数组"))
        })
        .collect::<Result<Vec<String>, String>>()?;
    for c in &capabilities {
        if !KNOWN_CAPABILITIES.contains(&c.as_str()) {
            return Err(format!(
                "pack {id} 声明了未知能力 '{c}'（合法集: {KNOWN_CAPABILITIES:?}）— \
                 未知能力禁止静默忽略（fail-fast）"
            ));
        }
    }
    // assets 节（capabilities 含 assets 时必填）
    let has_flow_compile = capabilities.iter().any(|c| c == "flow-compile");
    let mut scenes = Vec::new();
    let mut scene_fields = BTreeMap::new();
    let mut templates = Vec::new();
    let mut flows = Vec::new();
    let mut node_types = Vec::new();
    let assets = obj.get("assets");
    if capabilities.contains(&"assets".to_string()) {
        let assets = assets
            .ok_or_else(|| format!("pack {id} 声明 assets 能力但缺 assets 节（契约 §2.2）"))?
            .as_object()
            .ok_or_else(|| format!("pack {id} assets 必须是 object"))?;
        let pack_root = pack_json_path
            .parent()
            .map(FsPath::to_path_buf)
            .unwrap_or_default();
        // 场景（契约 §4.2）
        for path in resolve_globs(&id, &pack_root, assets.get("scenes"), "scenes")? {
            let sv = read_json(&path)?;
            let (scene_id, fields) = validate_scene(&id, &sv)?;
            if scene_fields.insert(scene_id.clone(), fields).is_some() {
                return Err(format!("pack {id} 场景 id 重复: '{scene_id}'"));
            }
            scenes.push(sv);
        }
        // 模板（契约 §4.3；scene_ref 解析 + 占位符全量装载期校验）
        for path in resolve_globs(&id, &pack_root, assets.get("templates"), "templates")? {
            let tv = read_json(&path)?;
            let tpl = validate_template(&id, &tv, &scene_fields)?;
            if templates
                .iter()
                .any(|t: &TemplateAsset| t.template_id == tpl.template_id)
            {
                return Err(format!("pack {id} 模板 id 重复: '{}'", tpl.template_id));
            }
            templates.push(tpl);
        }
        // 流程（契约 v1.1 §4.6；结构 fail-fast + form_ref 对场景索引锁定）
        // flows 与 flow-compile 编译器同包绑定：声明 assets.flows 就必须声明 flow-compile
        if assets.get("flows").is_some() && !has_flow_compile {
            return Err(format!(
                "pack {id} 声明 assets.flows 但 capabilities 未含 flow-compile — \
                 flows 与编译服务同包绑定（契约 v1.1 §4.6,fail-fast）"
            ));
        }
        for path in resolve_globs(&id, &pack_root, assets.get("flows"), "flows")? {
            let fv = read_json(&path)?;
            let flow_id = validate_flow(&id, &fv, &scene_fields)?;
            if flows
                .iter()
                .any(|f: &Value| f.get("flow_id").and_then(Value::as_str) == Some(flow_id.as_str()))
            {
                return Err(format!("pack {id} 流程 id 重复: '{flow_id}'"));
            }
            flows.push(fv);
        }
        // 节点类型（契约 §4.4；第二期 Phase C 生效：画布节点面板/属性表单唯一来源）
        for path in resolve_globs(&id, &pack_root, assets.get("node_types"), "node_types")? {
            let nv = read_json(&path)?;
            let nt = validate_node_type(&id, &nv, &scene_fields)?;
            if node_types
                .iter()
                .any(|v: &Value| v.get("node_type").and_then(Value::as_str) == Some(nt.as_str()))
            {
                return Err(format!("pack {id} 节点类型重复: '{nt}'"));
            }
            node_types.push(nv);
        }
    } else if assets.is_some() {
        return Err(format!(
            "pack {id} 有 assets 节但 capabilities 未声明 assets — 请对齐（fail-fast）"
        ));
    }
    // service 节（契约 v1.1 §4.6）：flow-compile 能力包的编译服务地址（仅本地 loopback）
    let base_url = match obj.get("service") {
        Some(svc) => {
            if !has_flow_compile {
                return Err(format!(
                    "pack {id} 声明了 service 节但 capabilities 未含 flow-compile — 请对齐（fail-fast）"
                ));
            }
            let so = svc
                .as_object()
                .ok_or_else(|| format!("pack {id} service 必须是 object"))?;
            for k in so.keys() {
                if k != "base_url" {
                    return Err(format!(
                        "pack {id} service 存在未知字段 '{k}'（契约 v1.1 钉死: 仅 base_url）"
                    ));
                }
            }
            let u = so.get("base_url").and_then(Value::as_str).ok_or_else(|| {
                format!("pack {id} service.base_url 必须是非空字符串")
            })?;
            Some(validate_loopback_base_url(&id, u)?)
        }
        None => {
            if has_flow_compile {
                return Err(format!(
                    "pack {id} 声明 flow-compile 能力但缺 service.base_url — \
                     编译代理需要目标地址（契约 v1.1 §2.2,fail-fast）"
                ));
            }
            None
        }
    };
    // 未知顶层键拒收（契约字段钉死；新字段 = 契约 MAJOR/MINOR 演进）
    for k in obj.keys() {
        if !matches!(
            k.as_str(),
            "id" | "contract_version" | "version" | "description" | "capabilities" | "assets"
                | "service"
        ) {
            return Err(format!(
                "pack {id} 存在未知顶层字段 '{k}' — pack.json 字段由契约 v1 钉死, \
                 新字段须走契约演进（fail-fast,不静默忽略）"
            ));
        }
    }
    Ok(PluginPack {
        id,
        contract_version,
        version,
        description,
        capabilities,
        scenes,
        scene_fields,
        templates,
        flows,
        node_types,
        base_url,
    })
}

/// v1.1 基地址校验：仅允许本地插件 `http://127.0.0.1:<port>`（SSRF 防线，契约钉死）。
/// 返回去掉尾随 `/` 的规范化基地址。
fn validate_loopback_base_url(pack_id: &str, u: &str) -> Result<String, String> {
    let rest = u.strip_prefix("http://127.0.0.1:").ok_or_else(|| {
        format!(
            "pack {pack_id} service.base_url='{u}' 非法 — 契约 v1.1 仅允许本地插件 \
             http://127.0.0.1:<port>（SSRF 防线,不放开其他主机）"
        )
    })?;
    let port_part = rest.strip_suffix('/').unwrap_or(rest);
    if port_part.is_empty() || port_part.len() > 5 || !port_part.bytes().all(|b| b.is_ascii_digit())
    {
        return Err(format!(
            "pack {pack_id} service.base_url='{u}' 端口段非法 — 期望 http://127.0.0.1:<port>"
        ));
    }
    let port: u16 = port_part
        .parse()
        .map_err(|_| format!("pack {pack_id} service.base_url='{u}' 端口越界（u16）"))?;
    if port == 0 {
        return Err(format!("pack {pack_id} service.base_url 端口不能为 0"));
    }
    Ok(u.trim_end_matches('/').to_string())
}

fn missing(field: &str, path: &FsPath) -> String {
    format!("pack.json {} 缺必填字段 '{field}'", path.display())
}

fn str_field(
    obj: &serde_json::Map<String, Value>,
    key: &str,
    path: &FsPath,
) -> Result<String, String> {
    obj.get(key)
        .and_then(Value::as_str)
        .map(String::from)
        .ok_or_else(|| format!("{} 字段 '{key}' 必须是非空字符串", path.display()))
}

fn read_json(path: &FsPath) -> Result<Value, String> {
    let s = std::fs::read_to_string(path)
        .map_err(|e| format!("资产文件读取失败 {}: {e}", path.display()))?;
    serde_json::from_str(&s).map_err(|e| format!("资产文件 JSON 非法 {}: {e}", path.display()))
}

/// 极简 glob：仅支持 `*.json` 形态与显式相对路径（契约 §2.2；不引第三方 glob 依赖）。
/// 模式 = 目录列表 + 前后缀匹配；结果按文件名排序（装载序确定性，R1）。
fn resolve_globs(
    pack_id: &str,
    pack_root: &FsPath,
    v: Option<&Value>,
    kind: &str,
) -> Result<Vec<PathBuf>, String> {
    let Some(entries) = v else {
        return Ok(Vec::new()); // 该 kind 无资产 = 合法空集
    };
    let arr = entries
        .as_array()
        .ok_or_else(|| format!("pack {pack_id} assets.{kind} 必须是字符串数组"))?;
    let mut out = Vec::new();
    for e in arr {
        let pat = e
            .as_str()
            .ok_or_else(|| format!("pack {pack_id} assets.{kind} 必须是字符串数组"))?;
        let p = FsPath::new(pat);
        let full = if p.is_absolute() {
            p.to_path_buf()
        } else {
            pack_root.join(p)
        };
        let file_name = full
            .file_name()
            .and_then(std::ffi::OsStr::to_str)
            .ok_or_else(|| format!("pack {pack_id} assets.{kind} 条目非法: '{pat}'"))?;
        if let Some((prefix, suffix)) = file_name.split_once('*') {
            let dir = full.parent().unwrap_or(FsPath::new("."));
            let mut matched: Vec<PathBuf> = std::fs::read_dir(dir)
                .map_err(|e| {
                    format!(
                        "pack {pack_id} assets.{kind} 目录不可读 {}: {e}（自诊断指引: \
                         确认 glob 相对 pack.json 所在目录且目录存在）",
                        dir.display()
                    )
                })?
                .filter_map(std::result::Result::ok)
                .map(|de| de.path())
                .filter(|c| {
                    c.is_file()
                        && c.file_name()
                            .and_then(std::ffi::OsStr::to_str)
                            .map(|n| n.starts_with(prefix) && n.ends_with(suffix))
                            .unwrap_or(false)
                })
                .collect();
            matched.sort();
            if matched.is_empty() {
                return Err(format!(
                    "pack {pack_id} assets.{kind} glob '{pat}' 无匹配文件 — \
                     声明了资产就必须存在（fail-fast,不静默）"
                ));
            }
            out.extend(matched);
        } else {
            if !full.is_file() {
                return Err(format!(
                    "pack {pack_id} assets.{kind} 文件不存在: {pat}（相对 pack.json 所在目录）"
                ));
            }
            out.push(full);
        }
    }
    Ok(out)
}

// ===== 资产校验（装载期 fail-fast） =====

fn display_name_ok(v: &Value) -> bool {
    v.as_object()
        .map(|o| {
            !o.is_empty()
                && o.iter()
                    .all(|(k, dv)| matches!(k.as_str(), "zh" | "en") && dv.is_string())
        })
        .unwrap_or(false)
}

/// 校验场景资产（契约 §4.2）：返回 (scene_id, field_id → path)
fn validate_scene(pack_id: &str, v: &Value) -> Result<(String, BTreeMap<String, String>), String> {
    let obj = v
        .as_object()
        .ok_or_else(|| format!("pack {pack_id} 场景资产顶层必须是 object"))?;
    for k in obj.keys() {
        if !matches!(
            k.as_str(),
            "scene_id" | "display_name" | "description" | "business_objects"
        ) {
            return Err(format!(
                "pack {pack_id} 场景资产存在未知顶层字段 '{k}'（契约 §4.2 钉死）"
            ));
        }
    }
    let scene_id = str_field(obj, "scene_id", FsPath::new("scene"))
        .map_err(|_| format!("pack {pack_id} 场景资产缺 scene_id（必须是非空字符串）"))?;
    let dn = obj
        .get("display_name")
        .ok_or_else(|| format!("pack {pack_id} 场景 {scene_id} 缺 display_name"))?;
    if !display_name_ok(dn) {
        return Err(format!(
            "pack {pack_id} 场景 {scene_id} display_name 必须是 {{zh,en}} 双语字符串对象（R5 纯展示数据）"
        ));
    }
    let bos = obj
        .get("business_objects")
        .and_then(Value::as_array)
        .ok_or_else(|| format!("pack {pack_id} 场景 {scene_id} 缺 business_objects 数组"))?;
    let mut fields = BTreeMap::new();
    for bo in bos {
        let bo = bo.as_object().ok_or_else(|| {
            format!("pack {pack_id} 场景 {scene_id} business_objects 元素必须是 object")
        })?;
        for k in bo.keys() {
            if !matches!(k.as_str(), "object_id" | "display_name" | "fields") {
                return Err(format!(
                    "pack {pack_id} 场景 {scene_id} 业务对象存在未知字段 '{k}'（契约 §4.2 钉死）"
                ));
            }
        }
        let fs = bo
            .get("fields")
            .and_then(Value::as_array)
            .ok_or_else(|| format!("pack {pack_id} 场景 {scene_id} 业务对象缺 fields 数组"))?;
        for f in fs {
            let f = f.as_object().ok_or_else(|| {
                format!("pack {pack_id} 场景 {scene_id} fields 元素必须是 object")
            })?;
            for k in f.keys() {
                if !matches!(
                    k.as_str(),
                    "field_id" | "display_name" | "type" | "options" | "path" | "unit"
                ) {
                    return Err(format!(
                        "pack {pack_id} 场景 {scene_id} 字段存在未知键 '{k}'（契约 §4.2 钉死）"
                    ));
                }
            }
            let field_id = f
                .get("field_id")
                .and_then(Value::as_str)
                .ok_or_else(|| format!("pack {pack_id} 场景 {scene_id} 字段缺 field_id"))?;
            let ftype = f
                .get("type")
                .and_then(Value::as_str)
                .ok_or_else(|| format!("pack {pack_id} 场景 {scene_id} 字段 {field_id} 缺 type"))?;
            if !CONTROL_VOCAB.contains(&ftype) || ftype == "scene_field" {
                return Err(format!(
                    "pack {pack_id} 场景 {scene_id} 字段 {field_id} type='{ftype}' 不在场景字段词表 \
                     （scene_field 仅用于模板表单,场景字段不允许）"
                ));
            }
            if ftype == "enum"
                && !f
                    .get("options")
                    .and_then(Value::as_array)
                    .map(|o| !o.is_empty())
                    .unwrap_or(false)
            {
                return Err(format!(
                    "pack {pack_id} 场景 {scene_id} 字段 {field_id} type=enum 必须带非空 options"
                ));
            }
            if let Some(pv) = f.get("path") {
                let p = pv.as_str().ok_or_else(|| {
                    format!("pack {pack_id} 场景 {scene_id} 字段 {field_id} path 必须是字符串")
                })?;
                if fields.insert(field_id.to_string(), p.to_string()).is_some() {
                    return Err(format!(
                        "pack {pack_id} 场景 {scene_id} 字段 id 重复: '{field_id}'"
                    ));
                }
            }
        }
    }
    Ok((scene_id, fields))
}

/// params_form 通用校验（契约 §4.5 控件词表 + scene_ref 场景锁定，R2）：
/// 模板（§4.3）与节点类型资产（§4.4）共用。`ctx` 为错误信息上下文
/// （如 "模板 {template_id}" / "节点类型 {node_type}"）；
/// `default_scene_ref` 为层级回退（模板级 scene_ref；节点类型资产无此层）。
fn validate_params_form(
    pack_id: &str,
    ctx: &str,
    params_form_v: &[Value],
    default_scene_ref: Option<&str>,
    scene_fields: &BTreeMap<String, BTreeMap<String, String>>,
) -> Result<Vec<ParamField>, String> {
    let mut params = Vec::new();
    for pf in params_form_v {
        let pf = pf
            .as_object()
            .ok_or_else(|| format!("pack {pack_id} {ctx} params_form 元素必须是 object"))?;
        for k in pf.keys() {
            if !matches!(
                k.as_str(),
                "field_id"
                    | "display_name"
                    | "type"
                    | "required"
                    | "default"
                    | "options"
                    | "scene_ref"
            ) {
                return Err(format!(
                    "pack {pack_id} {ctx} params_form 存在未知键 '{k}'（契约 §4.3 钉死）"
                ));
            }
        }
        let field_id = pf
            .get("field_id")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("pack {pack_id} {ctx} 参数缺 field_id"))?;
        let ftype = pf
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("pack {pack_id} {ctx} 参数 {field_id} 缺 type"))?;
        if !CONTROL_VOCAB.contains(&ftype) {
            return Err(format!(
                "pack {pack_id} {ctx} 参数 {field_id} type='{ftype}' 不在控件词表 \
                 （§4.5 固定枚举;新控件 = 契约 v2 事件）"
            ));
        }
        let pf_scene_ref = pf
            .get("scene_ref")
            .and_then(Value::as_str)
            .map(String::from);
        // scene_field 的有效 scene_ref：参数级 > 层级回退（解析后存值，生成期直接用）
        let effective_scene_ref = if ftype == "scene_field" {
            let sr = pf_scene_ref.as_deref().or(default_scene_ref).ok_or_else(|| {
                format!(
                    "pack {pack_id} {ctx} 参数 {field_id} type=scene_field \
                     必须带 scene_ref（自身或层级回退）"
                )
            })?;
            if !scene_fields.contains_key(sr) {
                return Err(format!(
                    "pack {pack_id} {ctx} 参数 {field_id} scene_ref='{sr}' \
                     未在 pack 场景中注册（R2:取值域锁定来源）"
                ));
            }
            Some(sr.to_string())
        } else {
            pf_scene_ref
        };
        if ftype == "enum"
            && !pf
                .get("options")
                .and_then(Value::as_array)
                .map(|o| !o.is_empty())
                .unwrap_or(false)
        {
            return Err(format!(
                "pack {pack_id} {ctx} 参数 {field_id} type=enum 必须带非空 options"
            ));
        }
        params.push(ParamField {
            field_id: field_id.to_string(),
            ftype: ftype.to_string(),
            required: pf.get("required").and_then(Value::as_bool).unwrap_or(false),
            default: pf.get("default").cloned(),
            options: pf.get("options").and_then(Value::as_array).map(|o| {
                o.iter()
                    .filter_map(Value::as_str)
                    .map(String::from)
                    .collect()
            }),
            scene_ref: effective_scene_ref,
        });
    }
    // params field_id 去重
    {
        let mut seen = std::collections::BTreeSet::new();
        for p in &params {
            if !seen.insert(p.field_id.clone()) {
                return Err(format!(
                    "pack {pack_id} {ctx} params_form field_id 重复: '{}'",
                    p.field_id
                ));
            }
        }
    }
    Ok(params)
}

/// 校验模板资产（契约 §4.3）：params_form + 骨架占位符全量装载期校验
fn validate_template(
    pack_id: &str,
    v: &Value,
    scene_fields: &BTreeMap<String, BTreeMap<String, String>>,
) -> Result<TemplateAsset, String> {
    let obj = v
        .as_object()
        .ok_or_else(|| format!("pack {pack_id} 模板资产顶层必须是 object"))?;
    for k in obj.keys() {
        if !matches!(
            k.as_str(),
            "template_id"
                | "display_name"
                | "description"
                | "scene_ref"
                | "params_form"
                | "rule_draft_skeleton"
        ) {
            return Err(format!(
                "pack {pack_id} 模板资产存在未知顶层字段 '{k}'（契约 §4.3 钉死）"
            ));
        }
    }
    let template_id = obj
        .get("template_id")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("pack {pack_id} 模板资产缺 template_id"))?;
    let dn = obj
        .get("display_name")
        .ok_or_else(|| format!("pack {pack_id} 模板 {template_id} 缺 display_name"))?;
    if !display_name_ok(dn) {
        return Err(format!(
            "pack {pack_id} 模板 {template_id} display_name 必须是 {{zh,en}} 双语字符串对象"
        ));
    }
    let scene_ref = obj
        .get("scene_ref")
        .and_then(Value::as_str)
        .map(String::from);
    let params_form_v = obj
        .get("params_form")
        .and_then(Value::as_array)
        .ok_or_else(|| format!("pack {pack_id} 模板 {template_id} 缺 params_form 数组"))?;
    let params = validate_params_form(
        pack_id,
        &format!("模板 {template_id}"),
        params_form_v,
        scene_ref.as_deref(),
        scene_fields,
    )?;
    let skeleton = obj
        .get("rule_draft_skeleton")
        .cloned()
        .ok_or_else(|| format!("pack {pack_id} 模板 {template_id} 缺 rule_draft_skeleton"))?;
    // 装载期占位符全量校验（R1/R2：未知占位符/scene_field 裸用/键内占位符一律 fail-fast）
    scan_placeholders(pack_id, template_id, &skeleton, &params)?;
    Ok(TemplateAsset {
        template_id: template_id.to_string(),
        raw: v.clone(),
        params_form: params,
        skeleton,
    })
}

/// v0 流程节点类型词表（契约 v1.1 §4.6 钉死；扩充 = 契约演进事件）
const FLOW_NODE_TYPES: &[&str] = &["start", "end", "approval"];

/// 校验设计器节点类型资产（契约 §4.4；第二期 Phase C 生效，v1.2 增 out_guards）：
/// - `node_type` 收敛于 v0 流程节点词表 [`FLOW_NODE_TYPES`]（新节点类型 = 契约
///   演进事件，装载期 fail-fast，不允许资产静默扩词表）；
/// - `display_name` 双语（R5）；`params_form` 复用 §4.3 控件词表校验——画布
///   属性面板唯一来源（R4：前端零领域知识）；
/// - `out_guards`（v1.2 可选）= 该类型节点出边允许的 guard 取值域声明（画布
///   提示面），成员收敛 v0 guard 词表 `['approved']`（词表扩充 = 契约演进
///   事件）；缺省/空数组 = 该类型出边禁 guard（与 v1.0/v1.1 行为一致）；
/// - `compile_hint.emits` 收敛于 R2 transform 词表（画布纯展示提示，
///   不参与编译语义；编译产物由 server 侧 R2 门禁强制兜底，契约 §6）。
///
/// 返回 node_type。
fn validate_node_type(
    pack_id: &str,
    v: &Value,
    scene_fields: &BTreeMap<String, BTreeMap<String, String>>,
) -> Result<String, String> {
    let fail = |msg: String| -> String { format!("pack {pack_id} 节点类型资产: {msg}") };
    let obj = v
        .as_object()
        .ok_or_else(|| fail("顶层必须是 object".to_string()))?;
    for k in obj.keys() {
        if !matches!(
            k.as_str(),
            "node_type" | "display_name" | "description" | "params_form" | "compile_hint"
                | "out_guards"
        ) {
            return Err(fail(format!("未知顶层字段 '{k}'（契约 §4.4 钉死）")));
        }
    }
    let node_type = obj
        .get("node_type")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| fail("缺 node_type（必须是非空字符串）".to_string()))?;
    if !FLOW_NODE_TYPES.contains(&node_type) {
        return Err(fail(format!(
            "node_type='{node_type}' 不在 v0 流程节点词表 {FLOW_NODE_TYPES:?} \
             （新类型 = 契约演进事件,fail-fast）"
        )));
    }
    let dn = obj
        .get("display_name")
        .ok_or_else(|| fail(format!("节点类型 {node_type} 缺 display_name")))?;
    if !display_name_ok(dn) {
        return Err(fail(
            "display_name 必须是 {zh,en} 双语字符串对象（R5 纯展示数据）".to_string(),
        ));
    }
    if let Some(d) = obj.get("description") {
        if d.as_str().is_none() {
            return Err(fail("description 必须是字符串".to_string()));
        }
    }
    if let Some(ch) = obj.get("compile_hint") {
        let ch = ch
            .as_object()
            .ok_or_else(|| fail(format!("节点类型 {node_type} compile_hint 必须是 object")))?;
        for k in ch.keys() {
            if !matches!(k.as_str(), "emits" | "note") {
                return Err(fail(format!(
                    "节点类型 {node_type} compile_hint 存在未知键 '{k}'（契约 §4.4 钉死: 仅 emits|note）"
                )));
            }
        }
        let emits = ch
            .get("emits")
            .and_then(Value::as_str)
            .ok_or_else(|| fail(format!("节点类型 {node_type} compile_hint 缺 emits")))?;
        if !R2_TRANSFORM_TYPES.contains(&emits) {
            return Err(fail(format!(
                "节点类型 {node_type} compile_hint.emits='{emits}' 不在 R2 transform 词表 \
                 {R2_TRANSFORM_TYPES:?}（编译产物收敛内核既有指令,契约 §6）"
            )));
        }
    }
    if let Some(pf) = obj.get("params_form") {
        let pf = pf
            .as_array()
            .ok_or_else(|| fail(format!("节点类型 {node_type} params_form 必须是数组")))?;
        validate_params_form(
            pack_id,
            &format!("节点类型 {node_type}"),
            pf,
            None,
            scene_fields,
        )?;
    }
    if let Some(og) = obj.get("out_guards") {
        let og = og
            .as_array()
            .ok_or_else(|| fail(format!("节点类型 {node_type} out_guards 必须是数组")))?;
        for g in og {
            let gs = g
                .as_str()
                .filter(|s| !s.is_empty())
                .ok_or_else(|| {
                    fail(format!("节点类型 {node_type} out_guards 成员必须是非空字符串"))
                })?;
            if gs != FLOW_GUARD_APPROVED {
                return Err(fail(format!(
                    "节点类型 {node_type} out_guards 成员 '{gs}' 不在 v0 guard 词表 \
                     ['{FLOW_GUARD_APPROVED}']（词表扩充 = 契约演进事件,fail-fast）"
                )));
            }
        }
    }
    Ok(node_type.to_string())
}

/// 审批出边唯一合法 guard 值（v0 线性链的声明式契约标记）
const FLOW_GUARD_APPROVED: &str = "approved";

/// 校验流程资产（契约 v1.1 §4.6）：结构 fail-fast + form_ref 对场景索引锁定（R2）。
///
/// v0 图形约束：线性链（1 start / 1 end / 非起点恰 1 入边 / 非终点恰 1 出边 /
/// start 可达全部节点）；审批出边必须带 guard="approved"，其余出边禁 guard；
/// 审批节点 params{role,prompt} 必填、form_ref{scene,field} 必填且 field 须在
/// 场景中注册且显式声明 path（编译产物需要 state path，R2 取值域锁定）；
/// threshold 可选正整数（domain lt 仅 i64）。
///
/// 返回 flow_id。
// v0 图形/审批约束逐条 fail-fast（每条带自诊断指引），按仓库先例豁免行数阈值。
#[allow(clippy::too_many_lines)]
fn validate_flow(
    pack_id: &str,
    v: &Value,
    scene_fields: &BTreeMap<String, BTreeMap<String, String>>,
) -> Result<String, String> {
    let fail = |msg: String| -> String { format!("pack {pack_id} 流程资产: {msg}") };
    let obj = v
        .as_object()
        .ok_or_else(|| fail("顶层必须是 object".to_string()))?;
    for k in obj.keys() {
        if !matches!(
            k.as_str(),
            "flow_id" | "display_name" | "description" | "version" | "nodes" | "edges"
        ) {
            return Err(fail(format!("未知顶层字段 '{k}'（契约 v1.1 §4.6 钉死）")));
        }
    }
    let flow_id = obj
        .get("flow_id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| fail("缺 flow_id（必须是非空字符串）".to_string()))?;
    if let Some(dn) = obj.get("display_name") {
        if !display_name_ok(dn) {
            return Err(fail(
                "display_name 必须是 {zh,en} 双语字符串对象（R5 纯展示数据）".to_string(),
            ));
        }
    }
    if let Some(d) = obj.get("description") {
        if d.as_str().is_none() {
            return Err(fail("description 必须是字符串".to_string()));
        }
    }
    let version = obj
        .get("version")
        .and_then(Value::as_i64)
        .ok_or_else(|| fail("缺 version（必须是整数）".to_string()))?;
    if version != 1 {
        return Err(fail(format!("version={version} 不被支持（flow v0 仅 version=1）")));
    }
    let nodes = obj
        .get("nodes")
        .and_then(Value::as_array)
        .ok_or_else(|| fail("缺 nodes 数组".to_string()))?;
    if nodes.is_empty() {
        return Err(fail("nodes 不能为空".to_string()));
    }
    let edges = obj
        .get("edges")
        .and_then(Value::as_array)
        .ok_or_else(|| fail("缺 edges 数组".to_string()))?;

    // ---- 节点逐个校验 ----
    let mut node_types: BTreeMap<&str, &str> = BTreeMap::new();
    let mut starts = 0usize;
    let mut ends = 0usize;
    for n in nodes {
        let n = n.as_object().ok_or_else(|| fail("nodes 元素必须是 object".to_string()))?;
        for k in n.keys() {
            if !matches!(
                k.as_str(),
                "node_id" | "node_type" | "params" | "form_ref" | "threshold"
            ) {
                return Err(fail(format!("节点存在未知字段 '{k}'（契约 v1.1 §4.6 钉死）")));
            }
        }
        let node_id = n
            .get("node_id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| fail("节点缺 node_id（必须是非空字符串）".to_string()))?;
        let node_type = n
            .get("node_type")
            .and_then(Value::as_str)
            .ok_or_else(|| fail(format!("节点 {node_id} 缺 node_type")))?;
        if !FLOW_NODE_TYPES.contains(&node_type) {
            return Err(fail(format!(
                "节点 {node_id} node_type='{node_type}' 不在 v0 词表 {FLOW_NODE_TYPES:?}"
            )));
        }
        if node_types.insert(node_id, node_type).is_some() {
            return Err(fail(format!("节点 id 重复: '{node_id}'")));
        }
        if node_type == "approval" {
            let params = n
                .get("params")
                .and_then(Value::as_object)
                .ok_or_else(|| fail(format!("审批节点 {node_id} 缺 params object")))?;
            for k in params.keys() {
                if !matches!(k.as_str(), "role" | "prompt") {
                    return Err(fail(format!(
                        "审批节点 {node_id} params 存在未知键 '{k}'（v0 仅 role|prompt）"
                    )));
                }
            }
            for k in ["role", "prompt"] {
                let s = params
                    .get(k)
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| fail(format!("审批节点 {node_id} params.{k} 必须是非空字符串")))?;
                let _ = s;
            }
            let fr = n
                .get("form_ref")
                .and_then(Value::as_object)
                .ok_or_else(|| fail(format!("审批节点 {node_id} 缺 form_ref object")))?;
            for k in fr.keys() {
                if !matches!(k.as_str(), "scene" | "field") {
                    return Err(fail(format!(
                        "审批节点 {node_id} form_ref 存在未知键 '{k}'（v0 仅 scene|field）"
                    )));
                }
            }
            let scene = fr
                .get("scene")
                .and_then(Value::as_str)
                .ok_or_else(|| fail(format!("审批节点 {node_id} form_ref 缺 scene")))?;
            let field = fr
                .get("field")
                .and_then(Value::as_str)
                .ok_or_else(|| fail(format!("审批节点 {node_id} form_ref 缺 field")))?;
            let registered = scene_fields.get(scene).ok_or_else(|| {
                fail(format!(
                    "审批节点 {node_id} form_ref.scene='{scene}' 未在 pack 场景注册（R2 取值域锁定）"
                ))
            })?;
            if !registered.contains_key(field) {
                return Err(fail(format!(
                    "审批节点 {node_id} form_ref {scene}.{field} 未声明 path \
                     — 编译产物需要 state path（R2:取值域锁定,fail-fast）"
                )));
            }
            if let Some(t) = n.get("threshold") {
                let tv = t.as_i64().ok_or_else(|| {
                    fail(format!(
                        "审批节点 {node_id} threshold 必须是整数（domain lt 仅 i64,确定性）"
                    ))
                })?;
                if tv < 0 {
                    return Err(fail(format!("审批节点 {node_id} threshold 不能为负")));
                }
            }
        } else {
            for k in ["params", "form_ref", "threshold"] {
                if n.contains_key(k) {
                    return Err(fail(format!(
                        "{node_type} 节点 {node_id} 不允许携带 '{k}'（仅审批节点可带）"
                    )));
                }
            }
        }
        if node_type == "start" {
            starts += 1;
        }
        if node_type == "end" {
            ends += 1;
        }
    }
    if starts != 1 {
        return Err(fail(format!("必须恰好 1 个 start 节点（当前 {starts}）")));
    }
    if ends != 1 {
        return Err(fail(format!("必须恰好 1 个 end 节点（当前 {ends}）")));
    }

    // ---- 边校验 + 线性链 ----
    // out: from → (to, guard)
    let mut out: BTreeMap<&str, (&str, Option<&str>)> = BTreeMap::new();
    for e in edges {
        let e = e.as_object().ok_or_else(|| fail("edges 元素必须是 object".to_string()))?;
        for k in e.keys() {
            if !matches!(k.as_str(), "from" | "to" | "guard") {
                return Err(fail(format!("边存在未知字段 '{k}'（v0 仅 from|to|guard）")));
            }
        }
        let from = e
            .get("from")
            .and_then(Value::as_str)
            .ok_or_else(|| fail("边缺 from".to_string()))?;
        let to = e
            .get("to")
            .and_then(Value::as_str)
            .ok_or_else(|| fail("边缺 to".to_string()))?;
        if !node_types.contains_key(from) {
            return Err(fail(format!("边 from='{from}' 未在 nodes 中声明")));
        }
        if !node_types.contains_key(to) {
            return Err(fail(format!("边 to='{to}' 未在 nodes 中声明")));
        }
        if from == to {
            return Err(fail(format!("自环边 {from}→{to}（v0 线性链禁止）")));
        }
        let guard = e.get("guard").and_then(Value::as_str);
        match node_types[from] {
            "approval" => {
                if guard != Some(FLOW_GUARD_APPROVED) {
                    return Err(fail(format!(
                        "审批节点 {from} 的出边 guard 必须是 '{FLOW_GUARD_APPROVED}'（v0 声明式契约标记）"
                    )));
                }
            }
            "start" => {
                if guard.is_some() {
                    return Err(fail(format!("start 节点 {from} 的出边不允许 guard")));
                }
            }
            _ => {
                return Err(fail(format!("end 节点 {from} 不允许有出边（v0 线性链）")));
            }
        }
        if out.insert(from, (to, guard)).is_some() {
            return Err(fail(format!("节点 {from} 有多条出边（v0 仅支持线性链）")));
        }
    }
    // 连通性走链：start 起步逐步跟随唯一出边，visited 覆盖全部节点且终点为 end
    let start_id = node_types
        .iter()
        .find(|(_, t)| **t == "start")
        .map(|(id, _)| *id)
        .ok_or_else(|| fail("缺 start 节点（前面计数应已拦截,防御性兜底）".to_string()))?;
    let mut visited: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
    let mut cur = start_id;
    loop {
        if !visited.insert(cur) {
            return Err(fail(format!("在节点 {cur} 检测到环（v0 线性链禁止）")));
        }
        if node_types[cur] == "end" {
            break;
        }
        let Some(&(to, _)) = out.get(cur) else {
            return Err(fail(format!("节点 {cur} 缺出边（v0 线性链）")));
        };
        cur = to;
    }
    if visited.len() != nodes.len() {
        return Err(fail(
            "存在不可从 start 到达的节点（v0 线性链要求全连通,fail-fast）".to_string(),
        ));
    }
    Ok(flow_id.to_string())
}

/// 单个占位符的解析形态
#[derive(Debug, Clone)]
enum Placeholder {
    Pack,
    Template,
    /// form.<field>（非 scene_field：整槽类型化替换）
    FormValue(String),
    /// form.<field>.<id|path>（scene_field 限定属性）
    SceneAttr {
        field: String,
        attr: String,
    },
}

fn parse_placeholder(s: &str) -> Option<Placeholder> {
    let parts: Vec<&str> = s.split('.').collect();
    match parts.as_slice() {
        ["pack"] => Some(Placeholder::Pack),
        ["template"] => Some(Placeholder::Template),
        ["form", f] => Some(Placeholder::FormValue((*f).to_string())),
        ["form", f, attr] if *attr == "id" || *attr == "path" => Some(Placeholder::SceneAttr {
            field: (*f).to_string(),
            attr: (*attr).to_string(),
        }),
        _ => None,
    }
}

/// 骨架扫描：键禁占位符；值占位符必须合法且 scene_field 必须走属性访问；
/// `.path` 要求场景字段显式声明 path（fail-fast，契约 §4.3）
fn scan_placeholders(
    pack_id: &str,
    template_id: &str,
    v: &Value,
    params: &[ParamField],
) -> Result<(), String> {
    let fail = |msg: String| -> String { format!("pack {pack_id} 模板 {template_id}: {msg}") };
    match v {
        Value::Object(o) => {
            for (k, val) in o {
                if k.contains("{{") || k.contains("}}") {
                    return Err(fail(format!(
                        "骨架对象键 '{k}' 禁止占位符（键是结构位,R2）"
                    )));
                }
                scan_placeholders(pack_id, template_id, val, params)?;
            }
            Ok(())
        }
        Value::Array(a) => {
            for val in a {
                scan_placeholders(pack_id, template_id, val, params)?;
            }
            Ok(())
        }
        Value::String(s) => {
            for inner in extract_placeholders(s) {
                let ph = parse_placeholder(&inner)
                    .ok_or_else(|| fail(format!("未知占位符 '{{{{{inner}}}}}'（v1 仅 form.X / form.X.id|path / pack / template）")))?;
                match ph {
                    Placeholder::FormValue(f) => {
                        let p = params.iter().find(|p| p.field_id == f).ok_or_else(|| {
                            fail(format!("占位符 '{{{{form.{f}}}}}' 引用未声明的表单字段"))
                        })?;
                        if p.ftype == "scene_field" {
                            return Err(fail(format!(
                                "占位符 '{{{{form.{f}}}}}' 为 scene_field 裸用 — \
                                 必须走 .id/.path 属性访问（R2:对象不可落入值位）"
                            )));
                        }
                    }
                    Placeholder::SceneAttr { field, attr } => {
                        let p = params.iter().find(|p| p.field_id == field).ok_or_else(|| {
                            fail(format!(
                                "占位符 '{{{{form.{field}.{attr}}}}}' 引用未声明的表单字段"
                            ))
                        })?;
                        if p.ftype != "scene_field" {
                            return Err(fail(format!(
                                "占位符 '{{{{form.{field}.{attr}}}}}' 仅允许 scene_field 类型参数使用"
                            )));
                        }
                        // `.path` 的"字段须显式声明 path"校验在生成期按值判定
                        // （取值域 = 场景已注册且声明 path 的字段,见 generate_draft 的 R2 锁定）——
                        // 装载期无法枚举用户选择,故此处只锁类型与引用合法性。
                    }
                    Placeholder::Pack | Placeholder::Template => {}
                }
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

/// 提取字符串中全部 `{{...}}` 占位符内文（不合法的 `{{`/`}}` 会在装载期报错）
fn extract_placeholders(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = s;
    while let Some(start) = rest.find("{{") {
        let after = &rest[start + 2..];
        match after.find("}}") {
            Some(end) => {
                out.push(after[..end].trim().to_string());
                rest = &after[end + 2..];
            }
            None => {
                out.push(format!("\u{0}unclosed:{start}")); // 未闭合 → parse 必失败
                break;
            }
        }
    }
    out
}

// ===== 生成（纯函数，R1：同输入字节级同输出） =====

/// 表单值解析结果
enum Resolved {
    Val(Value),
    /// scene_field 已选字段 → (field_id, state_path)
    Scene {
        field_id: String,
        path: String,
    },
}

/// 模板草稿生成（契约 §4.3）：`(模板, 表单值) → 规则 JSON 草稿`。
///
/// - 纯函数：零随机/零时钟/零 IO/零落库（R3 draft-only，R1 确定性）。
/// - 错误全部显式（400 语义），绝不静默替换。
/// - 返回值带 provenance 包裹（契约 §10 开放问题 #1 的 v1 裁决：
///   来源标记不注入规则 JSON _meta——避免触碰 _meta schema 权威面（约束族 D），
///   v1.1 再评估是否入 _meta）。
pub fn generate_draft(
    pack: &PluginPack,
    tpl: &TemplateAsset,
    form: &Value,
) -> Result<Value, String> {
    let form_obj = form
        .as_object()
        .ok_or_else(|| "表单值必须是 JSON object（field_id → 值）".to_string())?;
    // 1. 逐参数解析 + 类型检查（契约 §4.3 类型规则）
    let mut resolved: BTreeMap<String, Resolved> = BTreeMap::new();
    for p in &tpl.params_form {
        let raw = match form_obj.get(&p.field_id) {
            Some(v) => v.clone(),
            None => match &p.default {
                Some(d) => d.clone(),
                None => {
                    if p.required {
                        return Err(format!("缺必填表单字段 '{}'", p.field_id));
                    }
                    continue; // 可选且无默认：不进 VarMap（占位符引用即报错）
                }
            },
        };
        match p.ftype.as_str() {
            "number" | "currency" => {
                if !raw.is_number() {
                    return Err(format!("字段 '{}' 类型应为 number，收到 {raw}", p.field_id));
                }
                resolved.insert(p.field_id.clone(), Resolved::Val(raw));
            }
            "text" | "textarea" | "date" => {
                if raw.as_str().map(str::is_empty).unwrap_or(true) {
                    return Err(format!("字段 '{}' 类型应为非空字符串", p.field_id));
                }
                resolved.insert(p.field_id.clone(), Resolved::Val(raw));
            }
            "boolean" => {
                if !raw.is_boolean() {
                    return Err(format!(
                        "字段 '{}' 类型应为 boolean，收到 {raw}",
                        p.field_id
                    ));
                }
                resolved.insert(p.field_id.clone(), Resolved::Val(raw));
            }
            "enum" => {
                let s = raw.as_str().ok_or_else(|| {
                    format!("字段 '{}' 类型应为 enum 字符串，收到 {raw}", p.field_id)
                })?;
                let opts = p.options.as_deref().unwrap_or(&[]);
                if !opts.iter().any(|o| o == s) {
                    return Err(format!(
                        "字段 '{}' 取值 '{s}' 不在 options {opts:?} 内（fail-fast,不静默）",
                        p.field_id
                    ));
                }
                resolved.insert(p.field_id.clone(), Resolved::Val(raw));
            }
            "scene_field" => {
                let s = raw.as_str().ok_or_else(|| {
                    format!(
                        "字段 '{}' 应为场景字段 id（下拉选择），收到 {raw}",
                        p.field_id
                    )
                })?;
                let sr = p
                    .scene_ref
                    .as_deref()
                    .ok_or_else(|| format!("参数 '{}' 缺 scene_ref（装载校验缺陷）", p.field_id))?;
                let fields = pack
                    .scene_fields
                    .get(sr)
                    .ok_or_else(|| format!("scene_ref '{sr}' 未注册（装载校验缺陷）"))?;
                let path = fields.get(s).ok_or_else(|| {
                    // R2 锁定：取值域 = 场景已注册且显式声明 path 的字段
                    format!(
                        "字段 '{}' 取值 '{s}' 不是场景 {sr} 中声明了 path 的字段 \
                         （R2:取值域锁定,fail-fast）",
                        p.field_id
                    )
                })?;
                resolved.insert(
                    p.field_id.clone(),
                    Resolved::Scene {
                        field_id: s.to_string(),
                        path: path.clone(),
                    },
                );
            }
            other => {
                return Err(format!(
                    "参数 '{}' 未知类型 '{other}'（校验缺陷）",
                    p.field_id
                ))
            }
        }
    }
    // 2. 骨架递归替换
    let draft = substitute(pack, tpl, &tpl.skeleton, &resolved)?;
    // 3. provenance 包裹（不触碰规则 JSON 结构，见函数注释）
    Ok(serde_json::json!({
        "rule_draft": draft,
        "provenance": {
            "pack": pack.id,
            "pack_version": pack.version,
            "template": tpl.template_id,
            "contract_version": pack.contract_version,
        }
    }))
}

fn substitute(
    pack: &PluginPack,
    tpl: &TemplateAsset,
    v: &Value,
    vars: &BTreeMap<String, Resolved>,
) -> Result<Value, String> {
    match v {
        Value::Object(o) => {
            let mut out = serde_json::Map::new();
            for (k, val) in o {
                out.insert(k.clone(), substitute(pack, tpl, val, vars)?);
            }
            Ok(Value::Object(out))
        }
        Value::Array(a) => Ok(Value::Array(
            a.iter()
                .map(|val| substitute(pack, tpl, val, vars))
                .collect::<Result<Vec<_>, String>>()?,
        )),
        Value::String(s) => substitute_string(pack, tpl, s, vars),
        other => Ok(other.clone()),
    }
}

fn substitute_string(
    pack: &PluginPack,
    tpl: &TemplateAsset,
    s: &str,
    vars: &BTreeMap<String, Resolved>,
) -> Result<Value, String> {
    let trimmed = s.trim();
    // 整槽替换：字符串本身就是单个占位符 → 类型化 JSON 值（非 scene_field）
    if trimmed.starts_with("{{")
        && trimmed.ends_with("}}")
        && !trimmed[2..trimmed.len() - 2].contains('{')
    {
        let inner = trimmed[2..trimmed.len() - 2].trim();
        if let Some(Placeholder::FormValue(f)) = parse_placeholder(inner) {
            let r = vars.get(&f).ok_or_else(|| {
                format!("占位符 '{{{{{inner}}}}}' 无可用表单值（缺字段或可选未填）")
            })?;
            return match r {
                Resolved::Val(v) => Ok(v.clone()),
                Resolved::Scene { .. } => Err(format!(
                    "占位符 '{{{{{inner}}}}}' 为 scene_field 裸用（装载校验应已拦截）"
                )),
            };
        }
    }
    // 嵌入替换：逐占位符串接
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(pos) = rest.find("{{") {
        out.push_str(&rest[..pos]);
        let after = &rest[pos + 2..];
        let end = after
            .find("}}")
            .ok_or_else(|| "占位符未闭合（装载校验应已拦截）".to_string())?;
        let inner = after[..end].trim();
        match parse_placeholder(inner) {
            Some(Placeholder::Pack) => out.push_str(&pack.id),
            Some(Placeholder::Template) => out.push_str(&tpl.template_id),
            Some(Placeholder::FormValue(f)) => {
                let r = vars
                    .get(&f)
                    .ok_or_else(|| format!("占位符 '{{{{{inner}}}}}' 无可用表单值"))?;
                match r {
                    Resolved::Val(Value::String(x)) => out.push_str(x),
                    Resolved::Val(Value::Number(n)) => out.push_str(&n.to_string()),
                    Resolved::Val(Value::Bool(b)) => {
                        out.push_str(if *b { "true" } else { "false" })
                    }
                    Resolved::Val(other) => {
                        return Err(format!(
                        "占位符 '{{{{{inner}}}}}' 值 {other} 不可嵌入字符串（对象请走 .id/.path）"
                    ))
                    }
                    Resolved::Scene { .. } => {
                        return Err(format!(
                            "占位符 '{{{{{inner}}}}}' 为 scene_field 裸用（装载校验应已拦截）"
                        ))
                    }
                }
            }
            Some(Placeholder::SceneAttr { field, attr }) => {
                let r = vars
                    .get(&field)
                    .ok_or_else(|| format!("占位符 '{{{{{inner}}}}}' 无可用表单值"))?;
                match r {
                    Resolved::Scene { field_id, path } => {
                        let val = if attr == "path" { path } else { field_id };
                        out.push_str(val);
                    }
                    Resolved::Val(_) => {
                        return Err(format!(
                            "占位符 '{{{{{inner}}}}}' 仅允许 scene_field（装载校验应已拦截）"
                        ))
                    }
                }
            }
            None => {
                return Err(format!(
                    "未知占位符 '{{{{{inner}}}}}'（v1 生成语言无此形式）"
                ))
            }
        }
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    if out.contains("{{") {
        return Err("替换后仍残留占位符 — 存在未识别形态（fail-fast,不静默）".to_string());
    }
    Ok(Value::String(out))
}

// ===== HTTP API（契约 §5 三端点；只读 + 纯函数，R3/R4） =====

fn api_err(status: StatusCode, msg: String) -> (StatusCode, Json<Value>) {
    (status, Json(serde_json::json!({ "error": msg })))
}

/// GET /api/plugins —— 已注册声明式 pack 清单（契约 §5）
pub async fn list_plugins_handler(State(api): State<SessionApi>) -> Json<Value> {
    let reg = api.plugin_packs();
    let plugins: Vec<Value> = reg
        .list()
        .iter()
        .map(|p| {
            serde_json::json!({
                "id": p.id,
                "contract_version": p.contract_version,
                "version": p.version,
                "description": p.description,
                "capabilities": p.capabilities,
                "assets": {
                    "scenes": p.scenes.len(),
                    "templates": p.templates.len(),
                    "flows": p.flows.len(),
                    "node_types": p.node_types.len(),
                },
            })
        })
        .collect();
    // server 支持的契约级别（v1.2 增 node_types.out_guards 声明面；MINOR 向后兼容）
    Json(serde_json::json!({ "contract_version": "1.2", "plugins": plugins }))
}

/// GET /api/plugins/{pack_id}/assets/{kind} —— 包资产只读面
/// （契约 §5 / v1.1；kind ∈ scenes|templates|flows|node_types）
pub async fn plugin_assets_handler(
    State(api): State<SessionApi>,
    Path((pack_id, kind)): Path<(String, String)>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if !matches!(kind.as_str(), "scenes" | "templates" | "flows" | "node_types") {
        return Err(api_err(
            StatusCode::NOT_FOUND,
            format!(
                "未知资产 kind '{kind}'（合法集: scenes | templates | flows | node_types）"
            ),
        ));
    }
    let Some(pack) = api.plugin_packs().get(&pack_id) else {
        return Err(api_err(
            StatusCode::NOT_FOUND,
            format!("未知 pack '{pack_id}'（合法 id 见 GET /api/plugins）"),
        ));
    };
    let assets = if kind == "scenes" {
        pack.scenes.clone()
    } else if kind == "templates" {
        pack.templates.iter().map(|t| t.raw.clone()).collect()
    } else if kind == "flows" {
        pack.flows.clone()
    } else {
        pack.node_types.clone()
    };
    Ok(Json(serde_json::json!({
        "pack": pack_id,
        "kind": kind,
        "assets": assets,
    })))
}

/// POST /api/plugins/templates/{pack_id}/{template_id}/generate —— 草稿生成纯函数面（契约 §5）
///
/// body = 表单值对象；响应 = { rule_draft, provenance }。**不落库**——
/// 草稿进工作区仍由用户确认后走既有治理链（R3）。
pub async fn generate_template_handler(
    State(api): State<SessionApi>,
    Path((pack_id, template_id)): Path<(String, String)>,
    Json(form): Json<Value>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let Some(pack) = api.plugin_packs().get(&pack_id) else {
        return Err(api_err(
            StatusCode::NOT_FOUND,
            format!("未知 pack '{pack_id}'（合法 id 见 GET /api/plugins）"),
        ));
    };
    let Some(tpl) = pack.templates.iter().find(|t| t.template_id == template_id) else {
        return Err(api_err(
            StatusCode::NOT_FOUND,
            format!(
                "pack {pack_id} 无模板 '{template_id}'（合法模板见 \
                 GET /api/plugins/{pack_id}/assets/templates）"
            ),
        ));
    };
    let draft = generate_draft(&pack, tpl, &form).map_err(|e| {
        api_err(
            StatusCode::BAD_REQUEST,
            format!("模板 {pack_id}/{template_id} 生成失败: {e}"),
        )
    })?;
    Ok(Json(draft))
}

// ===== 流程编译代理（契约 v1.1 §6；draft-only，R3） =====

/// 内核 transform 元指令词表（对齐 evorule-tcb executor / evorule-governance
/// rule_validation 的 6 元指令白名单；R2 等价性门禁的基准面）
const R2_TRANSFORM_TYPES: &[&str] = &["branch", "set", "push", "io_request", "collect", "merge"];

/// 内核域函数词表（对齐 evorule-tcb domain 7 域类型）
const R2_DOMAIN_TYPES: &[&str] = &["eq", "lt", "exists", "instruction", "all", "not", "has_fields"];

/// 编译服务调用超时（设计期操作，非运行时链路；固定值不配置化，契约钉死）
const COMPILE_TIMEOUT_SECS: u64 = 10;

/// R2 等价性门禁（契约 v1.1 §6）：编译产物全树 `type` 白名单校验。
///
/// 产物中任何对象若带 `type` 字段，值必须 ∈ transform 词表 ∪ domain 词表；
/// 越界（含把指令层词 conditional/while_loop/sequence 混进 transform 位、
/// 或未知自定义类型）一律 Err——**不静默放行**（协议不可达面，方案 Phase B 风险 #1）。
fn r2_gate(v: &Value) -> Result<(), String> {
    match v {
        Value::Object(o) => {
            if let Some(Value::String(t)) = o.get("type") {
                if !R2_TRANSFORM_TYPES.contains(&t.as_str())
                    && !R2_DOMAIN_TYPES.contains(&t.as_str())
                {
                    return Err(format!(
                        "type '{t}' 不在内核词表（transform {R2_TRANSFORM_TYPES:?} ∪ \
                         domain {R2_DOMAIN_TYPES:?}）— R2 等价性门禁拒绝"
                    ));
                }
            }
            for val in o.values() {
                r2_gate(val)?;
            }
            Ok(())
        }
        Value::Array(a) => {
            for val in a {
                r2_gate(val)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

/// 编译代理前置（R2 唯一事实来源）：把 flow 中审批节点的 form_ref 解析为
/// 场景 state path，产出带 `form_ref_resolved` 的 flow 副本发往编译器。
///
/// server 持有场景索引（装载期 R2 锁定的唯一权威）；编译器是独立进程，
/// 只做纯 JSON→JSON 变换，不持场景数据——取值域锁定留在 server 侧。
fn resolve_flow_refs(pack: &PluginPack, flow: &Value) -> Result<Value, String> {
    let flow_id = flow.get("flow_id").and_then(Value::as_str).unwrap_or("?");
    let mut flow = flow.clone();
    let nodes = flow
        .get_mut("nodes")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| format!("flow {flow_id} 缺 nodes 数组（装载校验缺陷）"))?;
    for n in nodes {
        if n.get("node_type").and_then(Value::as_str) != Some("approval") {
            continue;
        }
        let (scene, field) = {
            let fr = n.get("form_ref").and_then(Value::as_object).ok_or_else(|| {
                format!("flow {flow_id} 审批节点缺 form_ref（装载校验缺陷）")
            })?;
            let s = fr
                .get("scene")
                .and_then(Value::as_str)
                .ok_or_else(|| format!("flow {flow_id} form_ref 缺 scene（装载校验缺陷）"))?;
            let f = fr
                .get("field")
                .and_then(Value::as_str)
                .ok_or_else(|| format!("flow {flow_id} form_ref 缺 field（装载校验缺陷）"))?;
            (s.to_string(), f.to_string())
        };
        let path = pack
            .scene_fields
            .get(&scene)
            .and_then(|m| m.get(&field))
            .ok_or_else(|| {
                format!(
                    "flow {flow_id} form_ref {scene}.{field} 未声明 path \
                     — R2 取值域锁定拒绝（取值域 = 场景中显式声明 path 的字段）"
                )
            })?
            .clone();
        let obj = n
            .as_object_mut()
            .ok_or_else(|| format!("flow {flow_id} 节点必须是 object（装载校验缺陷）"))?;
        obj.insert(
            "form_ref_resolved".to_string(),
            serde_json::json!({ "scene": scene, "field": field, "path": path }),
        );
    }
    Ok(flow)
}

/// 调用编译服务 `{base_url}/v1/compile`（契约 v1.1 §6 信封）。
///
/// 请求 = `{ "flow": <已解析 form_ref 的 flow> }`；
/// 响应 = `{ "rule_draft": <规则草稿>, "compiler_version": "<semver>" }`。
/// 传输/超时/解析失败全部显式 Err（调用方映射 502 + 自诊断），不静默。
async fn compile_via_service(
    base_url: &str,
    flow: &Value,
) -> Result<(Value, String), String> {
    let url = format!("{}/v1/compile", base_url.trim_end_matches('/'));
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(COMPILE_TIMEOUT_SECS))
        .build()
        .map_err(|e| format!("编译 HTTP 客户端构建失败: {e}"))?;
    let resp = client
        .post(&url)
        .json(&serde_json::json!({ "flow": flow }))
        .send()
        .await
        .map_err(|e| {
            format!(
                "编译服务不可达 {url}: {e}（自诊断指引: ① 确认 flow-studio 插件服务已启动; \
                 ② 核对 pack.json service.base_url 与服务端口一致）"
            )
        })?;
    let status = resp.status();
    let body: Value = resp.json().await.map_err(|e| {
        format!("编译服务响应非 JSON（HTTP {status}）: {e}")
    })?;
    if !status.is_success() {
        let msg = body
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("（无 error 字段）");
        return Err(format!(
            "编译服务返回 HTTP {status}: {msg}（编译失败是显式错误,不静默）"
        ));
    }
    let draft = body
        .get("rule_draft")
        .cloned()
        .ok_or_else(|| "编译服务响应缺 rule_draft 字段（契约 v1.1 §6 信封）".to_string())?;
    let compiler_version = body
        .get("compiler_version")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string();
    Ok((draft, compiler_version))
}

/// 编译源分派（纯函数）：`body_value` 为 None（请求体为空）→ 编译已装载 flow
/// 资产（"asset"）；`Some({"flow": {...}})` → 编译画布草稿（"draft"，过同一套
/// [`validate_flow`] 校验链 + flow_id 防漂移）。其余形态显式 Err（fail-fast）。
fn resolve_compile_source<'a>(
    pack: &'a PluginPack,
    flow_id: &str,
    body_value: Option<&'a Value>,
) -> Result<(&'a Value, &'static str), String> {
    let Some(v) = body_value else {
        let f = pack
            .flows
            .iter()
            .find(|f| f.get("flow_id").and_then(Value::as_str) == Some(flow_id))
            .ok_or_else(|| {
                format!(
                    "pack {} 无流程 '{flow_id}'（合法流程见 \
                     GET /api/plugins/{flow_id}/assets/flows）",
                    pack.id
                )
            })?;
        return Ok((f, "asset"));
    };
    let draft_flow = v
        .get("flow")
        .ok_or_else(|| "请求体必须为空（编译已装载流程）或含 flow 键（编译画布草稿,契约 v1.1 §6）")?;
    let body_id = draft_flow
        .get("flow_id")
        .and_then(Value::as_str)
        .ok_or_else(|| "画布草稿 flow 缺 flow_id（契约 v1.1 §4.6）")?;
    if body_id != flow_id {
        return Err(format!(
            "草稿 flow_id='{body_id}' 与路径 flow_id='{flow_id}' 漂移 — 请对齐（fail-fast,不静默）"
        ));
    }
    validate_flow(&pack.id, draft_flow, &pack.scene_fields)
        .map_err(|e| format!("画布草稿 flow 校验失败: {e}"))?;
    Ok((draft_flow, "draft"))
}

/// POST /api/plugins/flows/{pack_id}/{flow_id}/compile —— 流程编译代理（契约 v1.1 §6）
///
/// 双编译源（Phase C 画布端到端；两分支产物都 **draft-only 不落库**）：
/// - 请求体为空 → 编译**已装载**的同名 flow 资产（pack 作者审核过的登记流程）；
/// - 请求体 `{"flow": {...}}` → 编译**画布草稿 flow**（工作台现场编辑的流程）：
///   先过与装载期同一套 [`validate_flow`] 校验链（fail-fast + form_ref 场景
///   锁定 R2），且 body flow_id 必须与路径 flow_id 一致（防 id 漂移）。
///
/// 两分支同链路：form_ref 场景解析（R2，server 持索引）→ POST 编译服务
/// → **R2 等价性门禁**（全树 type 白名单，越界 502 + 自诊断，不静默）
/// → provenance 包裹返回。草稿进工作区仍由用户确认后走既有治理链（R3）。
#[allow(clippy::too_many_lines)]
pub async fn compile_flow_handler(
    State(api): State<SessionApi>,
    Path((pack_id, flow_id)): Path<(String, String)>,
    body: String,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let Some(pack) = api.plugin_packs().get(&pack_id) else {
        return Err(api_err(
            StatusCode::NOT_FOUND,
            format!("未知 pack '{pack_id}'（合法 id 见 GET /api/plugins）"),
        ));
    };
    if !pack.capabilities.iter().any(|c| c == "flow-compile") {
        return Err(api_err(
            StatusCode::BAD_REQUEST,
            format!(
                "pack {pack_id} 未声明 flow-compile 能力 — 编译代理仅服务 \
                 flow-compile 能力包（契约 v1.1 §6）"
            ),
        ));
    }
    let Some(base_url) = pack.base_url.as_deref() else {
        return Err(api_err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!(
                "pack {pack_id} 声明 flow-compile 但缺 service.base_url \
                 （装载校验缺陷,fail-fast 应已拦截）"
            ),
        ));
    };
    // 编译源分派：body 空 → 已装载资产；body.flow → 画布草稿（同一校验链）
    let body_trimmed = body.trim();
    let body_value: Option<Value> = if body_trimmed.is_empty() {
        None
    } else {
        Some(serde_json::from_str(body_trimmed).map_err(|e| {
            api_err(
                StatusCode::BAD_REQUEST,
                format!("编译请求体 JSON 非法: {e}"),
            )
        })?)
    };
    let (flow, source) = resolve_compile_source(&pack, &flow_id, body_value.as_ref())
        .map_err(|e| api_err(StatusCode::BAD_REQUEST, e))?;
    let resolved = resolve_flow_refs(&pack, flow).map_err(|e| {
        api_err(
            StatusCode::BAD_REQUEST,
            format!("流程 {pack_id}/{flow_id} form_ref 解析失败: {e}"),
        )
    })?;
    let (draft, compiler_version) = compile_via_service(base_url, &resolved)
        .await
        .map_err(|e| api_err(StatusCode::BAD_GATEWAY, e))?;
    if let Err(e) = r2_gate(&draft) {
        return Err(api_err(
            StatusCode::BAD_GATEWAY,
            format!(
                "R2 等价性门禁拒绝编译产物: {e}（自诊断指引: ① 核对编译器版本与契约 v1.1 对齐; \
                 ② 编译器升级引入新 type 须先走契约演进,门禁不静默放行）"
            ),
        ));
    }
    Ok(Json(serde_json::json!({
        "rule_draft": draft,
        "provenance": {
            "pack": pack.id,
            "pack_version": pack.version,
            "flow": flow_id,
            "compiler": compiler_version,
            "contract_version": pack.contract_version,
            "source": source,
        }
    })))
}

// ===== 单元测试 =====

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::expect_used, clippy::panic)]
    use super::*;
    use serde_json::json;

    fn scene_value() -> Value {
        json!({
            "scene_id": "expense",
            "display_name": { "zh": "费用报销", "en": "Expense Claim" },
            "business_objects": [
                { "object_id": "expense_form", "display_name": { "zh": "报销单" },
                  "fields": [
                    { "field_id": "amount", "display_name": { "zh": "报销金额" },
                      "type": "number", "unit": "元", "path": "__exec__.payload.amount" },
                    { "field_id": "dept", "display_name": { "zh": "申请部门" },
                      "type": "enum", "options": ["sales", "hr"] }
                  ] }
            ]
        })
    }

    fn template_value() -> Value {
        json!({
            "template_id": "amount_threshold_approval",
            "display_name": { "zh": "金额阈值审批" },
            "scene_ref": "expense",
            "params_form": [
                { "field_id": "threshold", "display_name": { "zh": "阈值" }, "type": "number",
                  "required": true, "default": 5000 },
                { "field_id": "approver", "display_name": { "zh": "审批角色" }, "type": "enum",
                  "required": true, "options": ["CFO", "finance_manager"] },
                { "field_id": "amount_field", "display_name": { "zh": "比较字段" },
                  "type": "scene_field", "required": true }
            ],
            "rule_draft_skeleton": {
                "id": "{{pack}}.{{template}}",
                "version": 1,
                "description": "{{form.threshold}} 以上需 {{form.approver}} 审批",
                "transform": [
                    { "type": "branch", "params": {
                        "domain": { "type": "lt", "path": "{{form.amount_field.path}}", "value": "{{form.threshold}}" },
                        "on_false": [
                            { "type": "io_request", "params": {
                                "io_type": "call_external", "role": "{{form.approver}}",
                                "prompt": "{{form.threshold}} 以上需 {{form.approver}} 审批" } }
                        ]
                    } }
                ]
            }
        })
    }

    fn pack_with(scene: Value, tpl: Value) -> PluginPack {
        let mut scene_fields = BTreeMap::new();
        let (_, fields) = validate_scene("p", &scene).unwrap();
        scene_fields.insert("expense".to_string(), fields);
        let tpl = validate_template("p", &tpl, &scene_fields).unwrap();
        PluginPack {
            id: "p".to_string(),
            contract_version: "1.0".to_string(),
            version: "0.1.0".to_string(),
            description: "test".to_string(),
            capabilities: vec!["assets".to_string()],
            scenes: vec![scene],
            scene_fields,
            templates: vec![tpl],
            flows: Vec::new(),
            node_types: Vec::new(),
            base_url: None,
        }
    }

    #[test]
    fn scene_validation_ok() {
        let (_, fields) = validate_scene("p", &scene_value()).unwrap();
        assert_eq!(
            fields.get("amount").map(String::as_str),
            Some("__exec__.payload.amount")
        );
        assert!(
            !fields.contains_key("dept"),
            "未声明 path 的字段不进 .path 索引"
        );
    }

    #[test]
    fn scene_rejects_unknown_field_key() {
        let mut s = scene_value();
        s["business_objects"][0]["fields"][0]["evil"] = json!(1);
        assert!(validate_scene("p", &s).is_err());
    }

    #[test]
    fn scene_rejects_scene_field_type() {
        let mut s = scene_value();
        s["business_objects"][0]["fields"][0]["type"] = json!("scene_field");
        assert!(validate_scene("p", &s).is_err());
    }

    #[test]
    fn template_rejects_scene_field_bare_use() {
        let mut t = template_value();
        t["rule_draft_skeleton"]["description"] = json!("字段 {{form.amount_field}} 需审批");
        let mut scene_fields = BTreeMap::new();
        let (_, f) = validate_scene("p", &scene_value()).unwrap();
        scene_fields.insert("expense".to_string(), f);
        let err = validate_template("p", &t, &scene_fields).unwrap_err();
        assert!(err.contains("scene_field 裸用"), "got: {err}");
    }

    #[test]
    fn template_rejects_unknown_placeholder() {
        let mut t = template_value();
        t["rule_draft_skeleton"]["description"] = json!("{{form.nosuch}} 常量");
        let mut scene_fields = BTreeMap::new();
        let (_, f) = validate_scene("p", &scene_value()).unwrap();
        scene_fields.insert("expense".to_string(), f);
        assert!(validate_template("p", &t, &scene_fields).is_err());
    }

    #[test]
    fn template_rejects_placeholder_in_key() {
        let mut t = template_value();
        t["rule_draft_skeleton"]["{{form.threshold}}"] = json!(1);
        let mut scene_fields = BTreeMap::new();
        let (_, f) = validate_scene("p", &scene_value()).unwrap();
        scene_fields.insert("expense".to_string(), f);
        let err = validate_template("p", &t, &scene_fields).unwrap_err();
        assert!(err.contains("键"), "got: {err}");
    }

    #[test]
    fn template_rejects_path_attr_on_non_scene_field() {
        let mut t = template_value();
        t["rule_draft_skeleton"]["description"] = json!("{{form.threshold.path}}");
        let mut scene_fields = BTreeMap::new();
        let (_, f) = validate_scene("p", &scene_value()).unwrap();
        scene_fields.insert("expense".to_string(), f);
        assert!(validate_template("p", &t, &scene_fields).is_err());
    }

    #[test]
    fn generate_is_deterministic_and_typed() {
        let pack = pack_with(scene_value(), template_value());
        let tpl = &pack.templates[0];
        let form = json!({ "threshold": 5000, "approver": "CFO", "amount_field": "amount" });
        let d1 = generate_draft(&pack, tpl, &form).unwrap();
        let d2 = generate_draft(&pack, tpl, &form).unwrap();
        assert_eq!(d1, d2, "R1: 同输入必须字节级同输出");
        assert_eq!(
            serde_json::to_string(&d1).unwrap(),
            serde_json::to_string(&d2).unwrap()
        );
        let draft = &d1["rule_draft"];
        // 值位类型化：number 占位符整槽替换后必须仍是 number（非字符串）
        assert_eq!(
            draft["transform"][0]["params"]["domain"]["value"],
            json!(5000)
        );
        // scene_field .path：只可能是场景注册的 path（R2 锁定）
        assert_eq!(
            draft["transform"][0]["params"]["domain"]["path"],
            json!("__exec__.payload.amount")
        );
        // pack/template 占位符
        assert_eq!(draft["id"], json!("p.amount_threshold_approval"));
        // 嵌入替换：number 进文案
        assert_eq!(draft["description"], json!("5000 以上需 CFO 审批"));
        // provenance 包裹（不触碰规则 JSON 结构）
        assert_eq!(
            d1["provenance"]["template"],
            json!("amount_threshold_approval")
        );
    }

    #[test]
    fn generate_rejects_enum_out_of_options() {
        let pack = pack_with(scene_value(), template_value());
        let form = json!({ "threshold": 1, "approver": "HACKER", "amount_field": "amount" });
        let err = generate_draft(&pack, &pack.templates[0], &form).unwrap_err();
        assert!(err.contains("options"), "got: {err}");
    }

    #[test]
    fn generate_rejects_scene_field_not_registered() {
        let pack = pack_with(scene_value(), template_value());
        let form = json!({ "threshold": 1, "approver": "CFO", "amount_field": "dept" });
        // dept 未声明 path → 不在 .path 索引 → R2 锁定拒绝
        let err = generate_draft(&pack, &pack.templates[0], &form).unwrap_err();
        assert!(err.contains("R2"), "got: {err}");
    }

    #[test]
    fn generate_rejects_missing_required() {
        let mut t = template_value();
        // required 的语义：表单与 default 都缺失才报错（default 职责是填充缺失值）；
        // 此处移除 default 以单独验证必填报错路径
        t["params_form"][0]
            .as_object_mut()
            .unwrap()
            .remove("default");
        let pack = pack_with(scene_value(), t);
        let form = json!({ "approver": "CFO", "amount_field": "amount" });
        let err = generate_draft(&pack, &pack.templates[0], &form).unwrap_err();
        assert!(err.contains("必填"), "got: {err}");
    }

    #[test]
    fn generate_uses_default_when_field_absent() {
        let mut t = template_value();
        t["params_form"][1]["required"] = json!(false);
        t["params_form"][1]["default"] = json!("CFO");
        let pack = pack_with(scene_value(), t);
        let form = json!({ "threshold": 1, "amount_field": "amount" });
        let d = generate_draft(&pack, &pack.templates[0], &form).unwrap();
        assert_eq!(d["rule_draft"]["description"], json!("1 以上需 CFO 审批"));
    }

    #[test]
    fn load_pack_rejects_major_mismatch() {
        let dir = std::env::temp_dir().join(format!("evorule-pk-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let pj = dir.join("pack.json");
        std::fs::write(
            &pj,
            json!({
                "id": "p", "contract_version": "2.0", "version": "0.1.0",
                "description": "x", "capabilities": []
            })
            .to_string(),
        )
        .unwrap();
        let err = load_pack("p", &pj).unwrap_err();
        assert!(err.contains("MAJOR"), "got: {err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_pack_rejects_unknown_top_level_field() {
        let dir = std::env::temp_dir().join(format!("evorule-pk-test2-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let pj = dir.join("pack.json");
        std::fs::write(
            &pj,
            json!({
                "id": "p", "contract_version": "1.0", "version": "0.1.0",
                "description": "x", "capabilities": [], "evil_field": 1
            })
            .to_string(),
        )
        .unwrap();
        let err = load_pack("p", &pj).unwrap_err();
        assert!(err.contains("未知顶层字段"), "got: {err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn extract_placeholders_basic() {
        assert_eq!(
            extract_placeholders("a {{form.x}} b {{pack}} c"),
            vec!["form.x".to_string(), "pack".to_string()]
        );
        assert_eq!(extract_placeholders("no placeholder"), Vec::<String>::new());
    }

    /// 参考实现包随仓门禁：plugins/finance-pack 必须始终通过装载校验，
    /// 且两个模板都能用合法表单确定性地生成草稿（契约 §4 形态回归闸门）。
    #[test]
    fn finance_pack_reference_impl_loads_and_generates() {
        let pack_json = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("plugins")
            .join("finance-pack")
            .join("pack.json");
        let pack = load_pack("finance-pack", &pack_json)
            .unwrap_or_else(|e| panic!("参考实现包必须可装载: {e}"));
        assert_eq!(pack.scenes.len(), 1);
        assert_eq!(pack.templates.len(), 2);
        let scene_fields = &pack.scene_fields["expense"];
        // 每个 scene_field 参数取场景中第一个已注册 path 的字段名（合法 .path 值域）
        let pathed = scene_fields.keys().next().unwrap().clone();
        for tpl in &pack.templates {
            let mut form = serde_json::Map::new();
            for p in &tpl.params_form {
                if let Some(d) = &p.default {
                    form.insert(p.field_id.clone(), d.clone());
                    continue;
                }
                let v = match p.ftype.as_str() {
                    "number" | "currency" => json!(1000),
                    "boolean" => json!(true),
                    "date" => json!("2026-01-01"),
                    "enum" => json!(p.options.as_ref().unwrap()[0]),
                    "scene_field" => json!(pathed),
                    _ => json!("v"), // text / textarea
                };
                form.insert(p.field_id.clone(), v);
            }
            let d1 = generate_draft(&pack, tpl, &Value::Object(form.clone()))
                .unwrap_or_else(|e| panic!("模板 {} 生成失败: {e}", tpl.template_id));
            let d2 = generate_draft(&pack, tpl, &Value::Object(form)).unwrap();
            assert_eq!(
                serde_json::to_string(&d1).unwrap(),
                serde_json::to_string(&d2).unwrap(),
                "R1: 同输入字节级同输出"
            );
            assert_eq!(d1["provenance"]["pack"], json!("finance-pack"));
            assert_eq!(d1["provenance"]["template"], json!(tpl.template_id));
            assert_eq!(d1["provenance"]["contract_version"], json!("1.0"));
        }
    }

    /// 领域复制包随仓门禁：plugins/hr-pack 必须始终通过装载校验，
    /// 且两个模板都能用合法表单确定性地生成草稿（第二期 Phase A 复制验证闸门：
    /// 零 loader 改动、纯数据包即可新增领域，契约 §1"资产是静态数据"的实证）。
    /// Phase B 起该包升级契约 v1.1（+flow-compile 能力 + 1 流程资产）——
    /// v1.0/v1.1 混装同仓装载 = MINOR 向后兼容的回归证据。
    /// UV-178 批次E 起升 v1.2（node_types.out_guards 声明面）——
    /// v1.0 finance-pack（无 node_types）与 v1.2 hr-pack 混装 = 同款回归证据。
    #[test]
    fn hr_pack_replication_loads_and_generates() {
        let pack_json = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("plugins")
            .join("hr-pack")
            .join("pack.json");
        let pack = load_pack("hr-pack", &pack_json)
            .unwrap_or_else(|e| panic!("领域复制包必须可装载: {e}"));
        assert_eq!(pack.scenes.len(), 1);
        assert_eq!(pack.templates.len(), 2);
        // 契约 v1.1：flow-compile 能力 + 流程资产 + 编译服务地址（本地 loopback）
        assert!(pack.capabilities.contains(&"flow-compile".to_string()));
        assert_eq!(pack.flows.len(), 1);
        // 契约 §4.4（第二期 Phase C）：节点类型资产装载（画布面板唯一来源）
        assert_eq!(pack.node_types.len(), 3);
        let nt_ids: Vec<&str> = pack
            .node_types
            .iter()
            .filter_map(|v| v.get("node_type").and_then(Value::as_str))
            .collect();
        assert_eq!(nt_ids, vec!["approval", "end", "start"], "glob 装载序需确定");
        assert_eq!(
            pack.base_url.as_deref(),
            Some("http://127.0.0.1:9120"),
            "v1.1 flow-compile 包必须带本地编译服务地址"
        );
        let scene_fields = &pack.scene_fields["leave_request"];
        // 每个 scene_field 参数取场景中第一个已注册 path 的字段名（合法 .path 值域）
        let pathed = scene_fields.keys().next().unwrap().clone();
        for tpl in &pack.templates {
            let mut form = serde_json::Map::new();
            for p in &tpl.params_form {
                if let Some(d) = &p.default {
                    form.insert(p.field_id.clone(), d.clone());
                    continue;
                }
                let v = match p.ftype.as_str() {
                    "number" | "currency" => json!(1000),
                    "boolean" => json!(true),
                    "date" => json!("2026-01-01"),
                    "enum" => json!(p.options.as_ref().unwrap()[0]),
                    "scene_field" => json!(pathed),
                    _ => json!("v"), // text / textarea
                };
                form.insert(p.field_id.clone(), v);
            }
            let d1 = generate_draft(&pack, tpl, &Value::Object(form.clone()))
                .unwrap_or_else(|e| panic!("模板 {} 生成失败: {e}", tpl.template_id));
            let d2 = generate_draft(&pack, tpl, &Value::Object(form)).unwrap();
            assert_eq!(
                serde_json::to_string(&d1).unwrap(),
                serde_json::to_string(&d2).unwrap(),
                "R1: 同输入字节级同输出"
            );
            assert_eq!(d1["provenance"]["pack"], json!("hr-pack"));
            assert_eq!(d1["provenance"]["template"], json!(tpl.template_id));
            // v1.2 起 hr-pack 升位（node_types.out_guards 声明面）；MINOR 向后兼容
            assert_eq!(d1["provenance"]["contract_version"], json!("1.2"));
        }
    }

    // ===== Phase B：流程资产装载/校验 + 编译代理（契约 v1.1） =====

    fn flow_scene() -> BTreeMap<String, BTreeMap<String, String>> {
        let mut m = BTreeMap::new();
        let (_, f) = validate_scene("p", &scene_value()).unwrap();
        m.insert("expense".to_string(), f);
        m
    }

    fn flow_value() -> Value {
        json!({
            "flow_id": "expense_approval_flow",
            "display_name": { "zh": "费用审批流程", "en": "Expense Approval Flow" },
            "description": "金额阈值审批流程",
            "version": 1,
            "nodes": [
                { "node_id": "n1", "node_type": "start" },
                { "node_id": "n2", "node_type": "approval",
                  "params": { "role": "CFO", "prompt": "超阈值审批" },
                  "form_ref": { "scene": "expense", "field": "amount" },
                  "threshold": 5000 },
                { "node_id": "n3", "node_type": "end" }
            ],
            "edges": [
                { "from": "n1", "to": "n2" },
                { "from": "n2", "to": "n3", "guard": "approved" }
            ]
        })
    }

    #[test]
    fn flow_validation_ok_returns_flow_id() {
        let id = validate_flow("p", &flow_value(), &flow_scene()).unwrap();
        assert_eq!(id, "expense_approval_flow");
    }

    #[test]
    fn flow_rejects_unknown_node_key() {
        let mut f = flow_value();
        f["nodes"][1]["evil"] = json!(1);
        let err = validate_flow("p", &f, &flow_scene()).unwrap_err();
        assert!(err.contains("未知字段"), "got: {err}");
    }

    #[test]
    fn flow_rejects_unknown_node_type() {
        let mut f = flow_value();
        f["nodes"][1]["node_type"] = json!("condition");
        let err = validate_flow("p", &f, &flow_scene()).unwrap_err();
        assert!(err.contains("v0 词表"), "got: {err}");
    }

    #[test]
    fn flow_rejects_multiple_outgoing_edges() {
        let mut f = flow_value();
        f["edges"].as_array_mut().unwrap().push(
            json!({ "from": "n2", "to": "n3", "guard": "approved" }),
        );
        let err = validate_flow("p", &f, &flow_scene()).unwrap_err();
        assert!(err.contains("多条出边"), "got: {err}");
    }

    #[test]
    fn flow_rejects_unreachable_node() {
        let mut f = flow_value();
        f["nodes"]
            .as_array_mut()
            .unwrap()
            .push(json!({ "node_id": "n4", "node_type": "end" }));
        let err = validate_flow("p", &f, &flow_scene()).unwrap_err();
        assert!(err.contains("2"), "end 计数应为 2: {err}");
    }

    #[test]
    fn flow_rejects_cycle() {
        let mut f = flow_value();
        // start→n2→n1 环（end 无入边仍计数合法,走链必现环）
        f["edges"][1] = json!({ "from": "n2", "to": "n1", "guard": "approved" });
        let err = validate_flow("p", &f, &flow_scene()).unwrap_err();
        assert!(err.contains("环") || err.contains("到达"), "got: {err}");
    }

    #[test]
    fn flow_rejects_form_ref_scene_not_registered() {
        let mut f = flow_value();
        f["nodes"][1]["form_ref"]["scene"] = json!("unknown_scene");
        let err = validate_flow("p", &f, &flow_scene()).unwrap_err();
        assert!(err.contains("R2"), "got: {err}");
    }

    #[test]
    fn flow_rejects_form_ref_field_without_path() {
        let mut f = flow_value();
        f["nodes"][1]["form_ref"]["field"] = json!("dept");
        let err = validate_flow("p", &f, &flow_scene()).unwrap_err();
        assert!(err.contains("path"), "got: {err}");
    }

    #[test]
    fn flow_rejects_missing_guard_on_approval_edge() {
        let mut f = flow_value();
        f["edges"][1].as_object_mut().unwrap().remove("guard");
        let err = validate_flow("p", &f, &flow_scene()).unwrap_err();
        assert!(err.contains("approved"), "got: {err}");
    }

    #[test]
    fn flow_rejects_guard_on_start_edge() {
        let mut f = flow_value();
        f["edges"][0]["guard"] = json!("approved");
        let err = validate_flow("p", &f, &flow_scene()).unwrap_err();
        assert!(err.contains("guard"), "got: {err}");
    }

    #[test]
    fn flow_rejects_params_on_start_node() {
        let mut f = flow_value();
        f["nodes"][0]["params"] = json!({ "role": "x" });
        let err = validate_flow("p", &f, &flow_scene()).unwrap_err();
        assert!(err.contains("不允许携带"), "got: {err}");
    }

    #[test]
    fn flow_rejects_float_threshold() {
        let mut f = flow_value();
        f["nodes"][1]["threshold"] = json!(3.5);
        let err = validate_flow("p", &f, &flow_scene()).unwrap_err();
        assert!(err.contains("整数"), "got: {err}");
    }

    fn write_pack_json(dir: &std::path::Path, tag: &str, pack: Value) -> PathBuf {
        let pj = dir.join(format!("pack-{tag}.json"));
        std::fs::write(&pj, pack.to_string()).unwrap();
        pj
    }

    #[test]
    fn load_pack_requires_base_url_for_flow_compile() {
        let dir = std::env::temp_dir().join(format!("evorule-flow-t1-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let pj = write_pack_json(
            &dir,
            "a",
            json!({
                "id": "p", "contract_version": "1.1", "version": "0.1.0",
                "description": "x", "capabilities": ["flow-compile"]
            }),
        );
        let err = load_pack("p", &pj).unwrap_err();
        assert!(err.contains("service.base_url"), "got: {err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_pack_rejects_non_loopback_base_url() {
        let dir = std::env::temp_dir().join(format!("evorule-flow-t2-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let pj = write_pack_json(
            &dir,
            "b",
            json!({
                "id": "p", "contract_version": "1.1", "version": "0.1.0",
                "description": "x", "capabilities": ["flow-compile"],
                "service": { "base_url": "http://0.0.0.0:9120" }
            }),
        );
        let err = load_pack("p", &pj).unwrap_err();
        assert!(err.contains("127.0.0.1"), "got: {err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_pack_rejects_service_without_flow_compile() {
        let dir = std::env::temp_dir().join(format!("evorule-flow-t3-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let pj = write_pack_json(
            &dir,
            "c",
            json!({
                "id": "p", "contract_version": "1.1", "version": "0.1.0",
                "description": "x", "capabilities": [],
                "service": { "base_url": "http://127.0.0.1:9120" }
            }),
        );
        let err = load_pack("p", &pj).unwrap_err();
        assert!(err.contains("flow-compile"), "got: {err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn r2_gate_accepts_compiled_draft() {
        let draft = json!({
            "id": "f", "version": 1, "description": "d",
            "transform": [
                { "type": "branch", "params": {
                    "domain": { "type": "lt", "path": "__exec__.payload.amount", "value": 5000 },
                    "on_false": [ { "type": "io_request", "params": {
                        "io_type": "call_external", "role": "CFO", "prompt": "p" } } ]
                } }
            ]
        });
        assert!(r2_gate(&draft).is_ok());
    }

    #[test]
    fn r2_gate_rejects_instruction_layer_and_unknown_types() {
        // 指令层词混入 transform 位（G8 禁止词形态）
        let bad1 = json!({ "transform": [ { "type": "while_loop" } ] });
        let err = r2_gate(&bad1).unwrap_err();
        assert!(err.contains("R2"), "got: {err}");
        // 未知自定义类型
        let bad2 = json!({ "transform": [ { "type": "custom_step" } ] });
        assert!(r2_gate(&bad2).is_err());
        // 嵌套深层也要被扫到
        let bad3 = json!({ "a": [ { "b": { "type": "conditional" } } ] });
        assert!(r2_gate(&bad3).is_err());
    }

    #[test]
    fn resolve_flow_refs_adds_resolved_path_and_locks_unpathed() {
        let pack = pack_with(scene_value(), template_value());
        let mut flow = flow_value();
        let resolved = resolve_flow_refs(&pack, &flow).unwrap();
        let node = &resolved["nodes"][1];
        assert_eq!(
            node["form_ref_resolved"]["path"],
            json!("__exec__.payload.amount"),
            "R2: form_ref 必须解析为场景注册的 state path"
        );
        // 原始 form_ref 保留（编译器可读声明形态）
        assert_eq!(node["form_ref"]["field"], json!("amount"));
        // 未声明 path 的字段被锁定拒绝
        flow["nodes"][1]["form_ref"]["field"] = json!("dept");
        let err = resolve_flow_refs(&pack, &flow).unwrap_err();
        assert!(err.contains("R2"), "got: {err}");
    }

    /// 编译代理管道 e2e（不含真编译器）：mock 编译服务返回合法信封 →
    /// compile_via_service 解析出 (draft, version)；R2 门禁对合法草稿放行。
    #[tokio::test]
    async fn compile_via_service_roundtrip_with_mock_server() {
        let app = axum::Router::new().route(
            "/v1/compile",
            axum::routing::post(|Json(_body): Json<Value>| async {
                Json(json!({
                    "rule_draft": {
                        "id": "expense_approval_flow", "version": 1,
                        "description": "d",
                        "transform": [
                            { "type": "branch", "params": {
                                "domain": { "type": "lt", "path": "__exec__.payload.amount", "value": 5000 },
                                "on_false": [ { "type": "io_request", "params": {
                                    "io_type": "call_external", "role": "CFO", "prompt": "p" } } ]
                            } }
                        ]
                    },
                    "compiler_version": "0.1.0"
                }))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap(); });
        let flow = flow_value();
        let (draft, ver) =
            compile_via_service(&format!("http://127.0.0.1:{}", addr.port()), &flow)
                .await
                .unwrap();
        assert_eq!(ver, "0.1.0");
        assert_eq!(draft["id"], json!("expense_approval_flow"));
        assert!(r2_gate(&draft).is_ok(), "合法产物必须通过 R2 门禁");
    }

    /// 编译服务显式失败必须透传（不静默吞错）。
    #[tokio::test]
    async fn compile_via_service_propagates_compiler_error() {
        let app = axum::Router::new().route(
            "/v1/compile",
            axum::routing::post(|| async {
                (axum::http::StatusCode::BAD_REQUEST, Json(json!({ "error": "flow 缺 nodes" })))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap(); });
        let err = compile_via_service(&format!("http://127.0.0.1:{}", addr.port()), &flow_value())
            .await
            .unwrap_err();
        assert!(err.contains("400"), "got: {err}");
        assert!(err.contains("flow 缺 nodes"), "got: {err}");
    }

    // ===== Phase C：节点类型资产装载/校验（契约 §4.4） =====

    fn node_type_approval() -> Value {
        json!({
            "node_type": "approval",
            "display_name": { "zh": "审批", "en": "Approval" },
            "description": "人工审批节点",
            "params_form": [
                { "field_id": "role", "type": "text", "required": true },
                { "field_id": "form_ref", "type": "scene_field", "scene_ref": "expense", "required": true },
                { "field_id": "threshold", "type": "number" }
            ],
            "compile_hint": { "emits": "io_request", "note": "阈值编译为 branch(lt)" }
        })
    }

    #[test]
    fn node_type_validation_ok_returns_node_type() {
        let nt = validate_node_type("p", &node_type_approval(), &flow_scene()).unwrap();
        assert_eq!(nt, "approval");
    }

    #[test]
    fn node_type_minimal_start_ok() {
        let v = json!({
            "node_type": "start",
            "display_name": { "zh": "开始", "en": "Start" }
        });
        assert_eq!(validate_node_type("p", &v, &flow_scene()).unwrap(), "start");
    }

    #[test]
    fn node_type_rejects_unknown_top_field() {
        let mut v = node_type_approval();
        v.as_object_mut().unwrap().insert("icon".to_string(), json!("shield"));
        let err = validate_node_type("p", &v, &flow_scene()).unwrap_err();
        assert!(err.contains("未知顶层字段") && err.contains("icon"), "got: {err}");
    }

    #[test]
    fn node_type_rejects_type_outside_v0_vocab() {
        let v = json!({
            "node_type": "counter_sign",
            "display_name": { "zh": "会签", "en": "Counter-sign" }
        });
        let err = validate_node_type("p", &v, &flow_scene()).unwrap_err();
        assert!(err.contains("counter_sign") && err.contains("契约演进"), "got: {err}");
    }

    #[test]
    fn node_type_rejects_emits_outside_r2_vocab() {
        let mut v = node_type_approval();
        v["compile_hint"]["emits"] = json!("while_loop");
        let err = validate_node_type("p", &v, &flow_scene()).unwrap_err();
        assert!(err.contains("emits") && err.contains("R2"), "got: {err}");
    }

    #[test]
    fn node_type_rejects_params_form_outside_control_vocab() {
        let mut v = node_type_approval();
        v["params_form"][0]["type"] = json!("color_picker");
        let err = validate_node_type("p", &v, &flow_scene()).unwrap_err();
        assert!(err.contains("控件词表"), "got: {err}");
    }

    #[test]
    fn node_type_rejects_scene_ref_not_registered_r2() {
        let mut v = node_type_approval();
        v["params_form"][1]["scene_ref"] = json!("unregistered_scene");
        let err = validate_node_type("p", &v, &flow_scene()).unwrap_err();
        assert!(err.contains("未在 pack 场景中注册") && err.contains("R2"), "got: {err}");
    }

    #[test]
    fn node_type_rejects_display_name_non_bilingual() {
        let v = json!({ "node_type": "end", "display_name": "结束" });
        let err = validate_node_type("p", &v, &flow_scene()).unwrap_err();
        assert!(err.contains("display_name"), "got: {err}");
    }

    // ===== v1.2：node_types.out_guards 声明面（画布 guard 取值域提示） =====

    #[test]
    fn node_type_accepts_out_guards_within_v0_vocab() {
        let mut v = node_type_approval();
        v.as_object_mut()
            .unwrap()
            .insert("out_guards".to_string(), json!(["approved"]));
        assert_eq!(
            validate_node_type("p", &v, &flow_scene()).unwrap(),
            "approval"
        );
    }

    #[test]
    fn node_type_accepts_out_guards_empty_and_absent() {
        let mut v = node_type_approval();
        v.as_object_mut()
            .unwrap()
            .insert("out_guards".to_string(), json!([]));
        assert_eq!(
            validate_node_type("p", &v, &flow_scene()).unwrap(),
            "approval"
        );
        // 缺省 = 该类型出边禁 guard（v1.0/v1.1 行为向后兼容）
        let plain = node_type_approval();
        assert_eq!(
            validate_node_type("p", &plain, &flow_scene()).unwrap(),
            "approval"
        );
    }

    #[test]
    fn node_type_rejects_out_guards_outside_v0_vocab() {
        let mut v = node_type_approval();
        v.as_object_mut()
            .unwrap()
            .insert("out_guards".to_string(), json!(["rejected"]));
        let err = validate_node_type("p", &v, &flow_scene()).unwrap_err();
        assert!(
            err.contains("out_guards") && err.contains("guard 词表"),
            "got: {err}"
        );
    }

    #[test]
    fn node_type_rejects_out_guards_malformed() {
        let mut v = node_type_approval();
        v.as_object_mut()
            .unwrap()
            .insert("out_guards".to_string(), json!("approved"));
        let err = validate_node_type("p", &v, &flow_scene()).unwrap_err();
        assert!(err.contains("out_guards 必须是数组"), "got: {err}");
        let mut v2 = node_type_approval();
        v2.as_object_mut()
            .unwrap()
            .insert("out_guards".to_string(), json!([""]));
        let err2 = validate_node_type("p", &v2, &flow_scene()).unwrap_err();
        assert!(err2.contains("非空字符串"), "got: {err2}");
    }

    // ===== Phase C：编译源分派（画布草稿编译,契约 v1.1 §6） =====

    /// 真实 hr-pack（含已装载 flow 资产 + leave_request 场景索引）
    fn hr_pack() -> PluginPack {
        let pack_json = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("plugins")
            .join("hr-pack")
            .join("pack.json");
        load_pack("hr-pack", &pack_json).unwrap_or_else(|e| panic!("hr-pack 必须可装载: {e}"))
    }

    fn canvas_draft_flow(flow_id: &str) -> Value {
        json!({
            "flow": {
                "flow_id": flow_id,
                "display_name": { "zh": "画布草稿", "en": "Canvas Draft" },
                "version": 1,
                "nodes": [
                    { "node_id": "n1", "node_type": "start" },
                    { "node_id": "n2", "node_type": "approval",
                      "params": { "role": "hr_manager", "prompt": "请审批" },
                      "form_ref": { "scene": "leave_request", "field": "days" },
                      "threshold": 3 },
                    { "node_id": "n3", "node_type": "end" }
                ],
                "edges": [
                    { "from": "n1", "to": "n2" },
                    { "from": "n2", "to": "n3", "guard": "approved" }
                ]
            }
        })
    }

    #[test]
    fn compile_source_none_uses_loaded_asset() {
        let pack = hr_pack();
        let (flow, source) = resolve_compile_source(&pack, "leave_approval_flow", None).unwrap();
        assert_eq!(source, "asset");
        assert_eq!(
            flow.get("flow_id").and_then(Value::as_str),
            Some("leave_approval_flow")
        );
    }

    #[test]
    fn compile_source_none_unknown_flow_is_explicit_error() {
        let pack = hr_pack();
        let err = resolve_compile_source(&pack, "no_such_flow", None).unwrap_err();
        assert!(err.contains("无流程"), "got: {err}");
    }

    #[test]
    fn compile_source_draft_flow_ok() {
        let pack = hr_pack();
        let body = canvas_draft_flow("canvas_draft_flow");
        let (flow, source) =
            resolve_compile_source(&pack, "canvas_draft_flow", Some(&body)).unwrap();
        assert_eq!(source, "draft");
        assert_eq!(
            flow.get("flow_id").and_then(Value::as_str),
            Some("canvas_draft_flow")
        );
    }

    #[test]
    fn compile_source_rejects_flow_id_drift() {
        let pack = hr_pack();
        let body = canvas_draft_flow("other_id");
        let err = resolve_compile_source(&pack, "canvas_draft_flow", Some(&body)).unwrap_err();
        assert!(err.contains("漂移"), "got: {err}");
    }

    #[test]
    fn compile_source_rejects_body_without_flow_key() {
        let pack = hr_pack();
        let body = json!({ "something": 1 });
        let err = resolve_compile_source(&pack, "x", Some(&body)).unwrap_err();
        assert!(err.contains("必须为空") && err.contains("flow 键"), "got: {err}");
    }

    #[test]
    fn compile_source_draft_passes_same_validation_chain_r2() {
        let pack = hr_pack();
        let mut body = canvas_draft_flow("canvas_draft_flow");
        // form_ref 指向未注册字段 → 与装载期同一校验链拒绝（R2 取值域锁定）
        body["flow"]["nodes"][1]["form_ref"]["field"] = json!("not_registered");
        let err = resolve_compile_source(&pack, "canvas_draft_flow", Some(&body)).unwrap_err();
        assert!(err.contains("校验失败") && err.contains("R2"), "got: {err}");
    }

    #[test]
    fn actor_headers_outside_scope_is_empty() {
        assert!(actor_headers().is_empty());
    }

    #[tokio::test]
    async fn actor_headers_inside_scope() {
        let ctx = ActorContext {
            actor_type: "user".to_string(),
            actor_id: "alice".to_string(),
        };
        let got = CURRENT_ACTOR.scope(ctx, async { actor_headers() }).await;
        assert_eq!(got[0].0, "X-Evorule-Actor-Type");
        assert_eq!(got[0].1, "user");
        assert_eq!(got[1].1, "alice");
    }
}
