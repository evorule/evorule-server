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
    let mut scenes = Vec::new();
    let mut scene_fields = BTreeMap::new();
    let mut templates = Vec::new();
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
    } else if assets.is_some() {
        return Err(format!(
            "pack {id} 有 assets 节但 capabilities 未声明 assets — 请对齐（fail-fast）"
        ));
    }
    // 未知顶层键拒收（契约字段钉死；新字段 = 契约 MAJOR/MINOR 演进）
    for k in obj.keys() {
        if !matches!(
            k.as_str(),
            "id" | "contract_version" | "version" | "description" | "capabilities" | "assets"
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
    })
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
    let mut params = Vec::new();
    for pf in params_form_v {
        let pf = pf.as_object().ok_or_else(|| {
            format!("pack {pack_id} 模板 {template_id} params_form 元素必须是 object")
        })?;
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
                    "pack {pack_id} 模板 {template_id} params_form 存在未知键 '{k}'（契约 §4.3 钉死）"
                ));
            }
        }
        let field_id = pf
            .get("field_id")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("pack {pack_id} 模板 {template_id} 参数缺 field_id"))?;
        let ftype = pf
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("pack {pack_id} 模板 {template_id} 参数 {field_id} 缺 type"))?;
        if !CONTROL_VOCAB.contains(&ftype) {
            return Err(format!(
                "pack {pack_id} 模板 {template_id} 参数 {field_id} type='{ftype}' 不在控件词表 \
                 （§4.5 固定枚举;新控件 = 契约 v2 事件）"
            ));
        }
        let pf_scene_ref = pf
            .get("scene_ref")
            .and_then(Value::as_str)
            .map(String::from);
        // scene_field 的有效 scene_ref：参数级 > 模板级回退（解析后存值，生成期直接用）
        let effective_scene_ref = if ftype == "scene_field" {
            let sr = pf_scene_ref
                .as_ref()
                .or(scene_ref.as_ref())
                .ok_or_else(|| {
                    format!(
                        "pack {pack_id} 模板 {template_id} 参数 {field_id} type=scene_field \
                     必须带 scene_ref（自身或模板级）"
                    )
                })?;
            if !scene_fields.contains_key(sr) {
                return Err(format!(
                    "pack {pack_id} 模板 {template_id} 参数 {field_id} scene_ref='{sr}' \
                     未在 pack 场景中注册（R2:取值域锁定来源）"
                ));
            }
            Some(sr.clone())
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
                "pack {pack_id} 模板 {template_id} 参数 {field_id} type=enum 必须带非空 options"
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
                    "pack {pack_id} 模板 {template_id} params_form field_id 重复: '{}'",
                    p.field_id
                ));
            }
        }
    }
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
                "assets": { "scenes": p.scenes.len(), "templates": p.templates.len() },
            })
        })
        .collect();
    Json(serde_json::json!({ "contract_version": "1.0", "plugins": plugins }))
}

/// GET /api/plugins/{pack_id}/assets/{kind} —— 包资产只读面（契约 §5；kind ∈ scenes|templates）
pub async fn plugin_assets_handler(
    State(api): State<SessionApi>,
    Path((pack_id, kind)): Path<(String, String)>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if !matches!(kind.as_str(), "scenes" | "templates") {
        return Err(api_err(
            StatusCode::NOT_FOUND,
            format!("未知资产 kind '{kind}'（v1 合法集: scenes | templates）"),
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
    } else {
        pack.templates.iter().map(|t| t.raw.clone()).collect()
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
