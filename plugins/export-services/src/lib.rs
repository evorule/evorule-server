// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 交付物导出原生服务。
//!
//! # 定位与双入口（同一引擎 SSOT）
//! - 原生服务 [`export_report_pdf`]：服务目录（GET /api/services）、REST 直调
//!   （POST /api/services/{name}/invoke）、规则内调用（io_request call_service）
//!   三面可用——MCP 适配器（list_services/invoke_service）随之免费获得本能力；
//! - REST 端点 `POST /api/export/pdf`（evorule-server api/pdf_export.rs 薄壳）
//!   消费本 crate [`pdf::render_pdf`]，二进制直返，不经落盘。
//!
//! # 落盘纪律（D-2 裁定形态：落盘+返回 {path,bytes,blake3}，base64 不过模型上下文）
//! - 目录：环境变量 `EVORULE_EXPORT_DIR` 覆盖，缺省 `data/exports`（相对 server 工作目录）；
//! - 文件名：调用方可选指定，一律消毒（剥路径分隔符/`.`·`_`-` 白名单字符集/长度帽/
//!   强制 `.pdf` 后缀）；缺省按 content_type+内容哈希前缀命名——同输入同文件名幂等覆写；
//! - 原子性：tmp+rename 覆写，不留半写文件；
//! - 内容寻址：信封带 `blake3:<64hex>`（族 B 哈希纪律，evorule-hash 单源）。
//!
//! # 统一信封 v1（科学计算服务面规范）
//! - `status` 恒 determined、`determinism` = bitwise_replayable：渲染为确定性纯函数
//!   （无墙钟/无随机/无非确定元数据），同输入+同字体环境逐字节同输出；字体文件属
//!   环境常量，跨机差异以信封 blake3 对账归因（不静默）。

#![forbid(unsafe_code)]

pub mod docx;
pub mod pdf;
pub mod xlsx;

use std::sync::Arc;

use evorule_reactor::IoResult;
use evorule_tcb::JsonValue;

pub use evorule_plugin_kit::{NativeService, NativeServiceDef};

// ============================================================================
// 原生服务声明表（SSOT：official_native_services.json 为声明基准，
// 本表持有 make 构造子；同步守卫锁定三字段+顺序，漂移即测试红）
// ============================================================================

/// 原生服务声明表(本插件原生叶子能力全量清单,顺序即路由查找序)。
pub const NATIVE_SERVICES: &[NativeServiceDef] = &[
    NativeServiceDef {
    name: "export_report_pdf",
    sensitive: false,
    description: "报告PDF导出(审计/业务数据→纯Rust文本型PDF渲染→落盘导出目录,返回绝对路径与字节数及blake3内容哈希,幂等覆写)",
    parameters: Some(
        r#"{"type":"object","properties":{"content_type":{"type":"string","description":"内容类型(必填),如 audit_chain/comprehensive"},"session_id":{"description":"会话标识(入元信息块)"},"ruleset_version":{"description":"规则集版本(入元信息块)"},"range":{"description":"导出范围(入元信息块)"},"raw_data":{"type":"array","description":"事实/数据数组(渲染为数据摘要表,超限截断加省略标记)"},"business_data":{"description":"业务数据块(渲染为数据摘要表)"},"integrity":{"type":"object","description":"完整性块(算法/内容哈希/条数/结论)"},"meta":{"type":"object","description":"元信息(operator/exported_at/range_description/template_id/console_version)"},"options":{"type":"object","properties":{"pdf_title":{"type":"string"},"pdf_organization":{"type":"string"},"include_integrity":{"type":"boolean"},"include_meta":{"type":"boolean"}},"description":"渲染选项"},"filename":{"type":"string","description":"导出文件名(可选;消毒后强制 .pdf 后缀;缺省按内容类型+内容哈希命名)"}},"required":["content_type"]}"#,
    ),
    make: mk_export_report_pdf,
},
    NativeServiceDef {
        name: "export_workbook_xlsx",
        sensitive: false,
        description: "工作簿xlsx导出(多表+表头/整数字符串值域→rust_xlsxwriter确定性渲染,固定文档时间戳,落盘导出目录,返回绝对路径与字节数及blake3内容哈希,幂等覆写)",
        parameters: Some(
            r#"{"type":"object","properties":{"title":{"type":"string","description":"工作簿标题(入文档属性)"},"sheets":{"type":"array","description":"数据表列表","items":{"type":"object","properties":{"name":{"type":"string","description":"表名(工作表标签)"},"header":{"type":"array","items":{"type":"string"},"description":"表头"},"rows":{"type":"array","items":{"type":"array","items":{"type":["string","integer"]},"description":"数据行(字符串或整数,无浮点——浮点需求走 numeric 服务字符串出参)"}},"required":["name","header","rows"]}}},"provenance":{"type":"array","description":"溯源清单(渲染为首表:断言出自哪个服务/入参/信封摘要,可复算交付物入口)","items":{"type":"object","properties":{"claim":{"type":"string"},"service":{"type":"string"},"args":{"type":"string","description":"入参压缩 JSON"},"digest":{"type":"string"}},"required":["claim","service","args","digest"]}},"filename":{"type":"string","description":"导出文件名(可选;消毒后强制 .xlsx 后缀;缺省按标题+内容哈希命名)"}},"required":["sheets"]}"#,
        ),
        make: mk_export_workbook_xlsx,
    },
    NativeServiceDef {
        name: "export_document_docx",
        sensitive: false,
        description: "文档docx导出(标题+段落+表格+溯源表→docx-rs确定性渲染(零时间戳内嵌),落盘导出目录,返回绝对路径与字节数及blake3内容哈希,幂等覆写)",
        parameters: Some(
            r#"{"type":"object","properties":{"title":{"type":"string","description":"文档标题(加粗首段)"},"paragraphs":{"type":"array","items":{"type":"string"},"description":"正文段落(依序)"},"tables":{"type":"array","description":"表格列表","items":{"type":"object","properties":{"header":{"type":"array","items":{"type":"string"}},"rows":{"type":"array","items":{"type":"array","items":{"type":"string"}}}},"required":["header","rows"]}}},"provenance":{"type":"array","description":"溯源清单(渲染为文末溯源表:断言出自哪个服务/入参/信封摘要)","items":{"type":"object","properties":{"claim":{"type":"string"},"service":{"type":"string"},"args":{"type":"string"},"digest":{"type":"string"}},"required":["claim","service","args","digest"]}},"filename":{"type":"string","description":"导出文件名(可选;消毒后强制 .docx 后缀;缺省按标题+内容哈希命名)"}},"required":["title"]}"#,
        ),
        make: mk_export_document_docx,
    },
];

fn mk_export_report_pdf() -> Arc<dyn NativeService> {
    Arc::new(ExportReportPdf)
}

fn mk_export_workbook_xlsx() -> Arc<dyn NativeService> {
    Arc::new(ExportWorkbookXlsx)
}

fn mk_export_document_docx() -> Arc<dyn NativeService> {
    Arc::new(ExportDocumentDocx)
}

// ============================================================================
// export_report_pdf 服务实现：args = PdfExportRequest 形状（serde 宽松）+ filename?
// ============================================================================

/// 导出文件名长度帽（消毒后基名，不含 .pdf 后缀）——防异常超长名打爆文件系统。
const MAX_FILENAME_STEM: usize = 80;

struct ExportReportPdf;

impl NativeService for ExportReportPdf {
    fn execute(&self, args: &JsonValue) -> IoResult {
        // ① filename 单独取走（显式取走便于消毒；PdfExportRequest 反序列化对其宽松忽略）
        let filename = args
            .get("filename")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        // ② JsonValue(TCB) → serde_json::Value → PdfExportRequest（显式转换，不赌 Display 格式）
        let sv = tcb_to_serde(args)?;
        let req: pdf::PdfExportRequest = serde_json::from_value(sv).map_err(|e| {
            format!(
                "export_report_pdf 入参非法: {e} — 自诊断指引: args 形如 \
                 {{\"content_type\":\"audit_chain\",\"raw_data\":[...],\"filename\":\"report.pdf\"}}"
            )
        })?;
        // ③ 渲染（确定性纯函数；缺 content_type/缺字形等在此 fail-fast 透传）
        let bytes = pdf::render_pdf(&req)?;
        // ④⑤⑥ 落盘+信封（三服务共享尾部：哈希/消毒/原子覆写/绝对路径）
        persist_and_envelope(&bytes, filename.as_deref(), &req.content_type, "pdf")
    }
}

// ============================================================================
// export_workbook_xlsx / export_document_docx 服务实现
// ============================================================================

struct ExportWorkbookXlsx;

impl NativeService for ExportWorkbookXlsx {
    fn execute(&self, args: &JsonValue) -> IoResult {
        let filename = args.get("filename").and_then(|v| v.as_str()).map(str::to_string);
        let title = args.get("title").and_then(|v| v.as_str()).unwrap_or("工作簿").to_string();
        let sheets_val = args
            .get("sheets")
            .ok_or_else(|| "缺 sheets 参数 — 需为表数组。自诊断指引: args 形如 {\"title\":\"..\",\"sheets\":[{\"name\":\"..\",\"header\":[..],\"rows\":[[..]]}],\"provenance\":[..],\"filename\":\"..\"}".to_string())?;
        let mut sheets = Vec::new();
        for (i, sv) in sheets_val
            .as_array()
            .ok_or_else(|| "sheets 需为数组（每项含 name/header/rows）".to_string())?
            .iter()
            .enumerate()
        {
            let name = sv.get("name").and_then(|v| v.as_str()).ok_or_else(|| {
                format!("sheets[{i}] 缺 name（字符串表名）")
            })?.to_string();
            let header: Vec<String> = sv
                .get("header")
                .and_then(|v| v.as_array())
                .ok_or_else(|| format!("sheets[{i}] 缺 header（字符串数组）"))?
                .iter()
                .map(|c| c.as_str().map(str::to_string).unwrap_or_default())
                .collect();
            let mut rows = Vec::new();
            for rv in sv
                .get("rows")
                .and_then(|v| v.as_array())
                .ok_or_else(|| format!("sheets[{i}] 缺 rows（行数组）"))?
            {
                let row = rv.as_array().ok_or_else(|| format!("sheets[{i}].rows 需为行数组"))?;
                let mut cells = Vec::new();
                for cv in row {
                    if let Some(t) = cv.as_str() {
                        cells.push(xlsx::Cell::Text(t.to_string()));
                    } else if let Some(n) = cv.as_i64() {
                        cells.push(xlsx::Cell::Int(n));
                    } else {
                        return Err(format!(
                            "sheets[{i}] 单元格值域=字符串|整数（浮点需求走 numeric 服务字符串出参），得到 {cv}"
                        ));
                    }
                }
                rows.push(cells);
            }
            sheets.push(xlsx::SheetSpec { name, header, rows });
        }
        let provenance = parse_provenance(args)?;
        let bytes = xlsx::render_xlsx(&title, &sheets, &provenance)?;
        persist_and_envelope(&bytes, filename.as_deref(), &title, "xlsx")
    }
}

struct ExportDocumentDocx;

impl NativeService for ExportDocumentDocx {
    fn execute(&self, args: &JsonValue) -> IoResult {
        let filename = args.get("filename").and_then(|v| v.as_str()).map(str::to_string);
        let title = args
            .get("title")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "缺 title 参数 — 需为字符串标题。自诊断指引: args 形如 {\"title\":\"..\",\"paragraphs\":[..],\"tables\":[{\"header\":[..],\"rows\":[[..]]}],\"provenance\":[..],\"filename\":\"..\"}".to_string())?
            .to_string();
        let paragraphs: Vec<String> = args
            .get("paragraphs")
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter().map(|p| p.as_str().map(str::to_string).unwrap_or_default()).collect())
            .unwrap_or_default();
        let mut tables = Vec::new();
        if let Some(tv) = args.get("tables").and_then(|v| v.as_array()) {
            for (i, t) in tv.iter().enumerate() {
                let header: Vec<String> = t
                    .get("header")
                    .and_then(|v| v.as_array())
                    .ok_or_else(|| format!("tables[{i}] 缺 header（字符串数组）"))?
                    .iter()
                    .map(|c| c.as_str().map(str::to_string).unwrap_or_default())
                    .collect();
                let mut rows = Vec::new();
                for rv in t.get("rows").and_then(|v| v.as_array()).ok_or_else(|| {
                    format!("tables[{i}] 缺 rows（行数组，单元格为字符串）")
                })? {
                    rows.push(
                        rv.as_array()
                            .ok_or_else(|| format!("tables[{i}].rows 需为行数组"))?
                            .iter()
                            .map(|c| c.as_str().map(str::to_string).unwrap_or_default())
                            .collect::<Vec<String>>(),
                    );
                }
                tables.push(docx::TableSpec { header, rows });
            }
        }
        // 溯源清单 → 文末溯源表（可复算交付物入口）
        let provenance = parse_provenance(args)?;
        if !provenance.is_empty() {
            tables.push(docx::TableSpec {
                header: vec!["断言".into(), "服务".into(), "入参".into(), "信封摘要".into()],
                rows: provenance
                    .iter()
                    .map(|(c, s, a, d)| vec![c.clone(), s.clone(), a.clone(), d.clone()])
                    .collect(),
            });
        }
        let bytes = docx::render_docx(&docx::DocxSpec { title: title.clone(), paragraphs, tables })?;
        persist_and_envelope(&bytes, filename.as_deref(), &title, "docx")
    }
}

/// 溯源清单解析：(claim, service, args, digest) 四元组列表；缺省空。
fn parse_provenance(args: &JsonValue) -> Result<Vec<(String, String, String, String)>, String> {
    let Some(arr) = args.get("provenance").and_then(|v| v.as_array()) else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for (i, p) in arr.iter().enumerate() {
        let get = |k: &str| -> Result<String, String> {
            p.get(k)
                .and_then(|v| v.as_str())
                .map(str::to_string)
                .ok_or_else(|| format!("provenance[{i}] 缺 {k}（字符串）"))
        };
        out.push((get("claim")?, get("service")?, get("args")?, get("digest")?));
    }
    Ok(out)
}

// ============================================================================
// 三服务共享尾部：内容哈希 + 文件名消毒 + 原子落盘 + 统一信封
// ============================================================================

/// 渲染产物落盘并构造统一信封 v1（D-2 裁定形态：落盘+返回 {path,bytes,blake3}，
/// base64 不过模型上下文）。
fn persist_and_envelope(
    bytes: &[u8],
    filename: Option<&str>,
    stem_hint: &str,
    ext: &str,
) -> IoResult {
    let hex = evorule_hash::digest(bytes);
    let fname = sanitize_filename(filename, stem_hint, &hex, ext);
    let dir = export_dir()?;
    std::fs::create_dir_all(&dir).map_err(|e| {
        format!(
            "创建导出目录失败 {}: {e}（可用环境变量 EVORULE_EXPORT_DIR 覆盖落盘位置）",
            dir.display()
        )
    })?;
    let final_path = dir.join(&fname);
    let tmp_path = dir.join(format!("{fname}.tmp"));
    std::fs::write(&tmp_path, bytes)
        .map_err(|e| format!("写入导出临时文件失败 {}: {e}", tmp_path.display()))?;
    std::fs::rename(&tmp_path, &final_path)
        .map_err(|e| format!("落盘导出文件失败 {}: {e}", final_path.display()))?;
    let abs = final_path
        .canonicalize()
        .map_err(|e| format!("解析导出绝对路径失败 {}: {e}", final_path.display()))?;
    Ok(JsonValue::object_from_pairs(&[
        ("status", JsonValue::string("determined")),
        ("determinism", JsonValue::string("bitwise_replayable")),
        ("path", JsonValue::string(abs.to_string_lossy().into_owned())),
        ("filename", JsonValue::string(fname)),
        ("bytes", JsonValue::Integer(bytes.len() as i64)),
        ("blake3", JsonValue::string(format!("blake3:{hex}"))),
    ]))
}

// ============================================================================
// 内部工具：JsonValue→serde Value 显式转换 / 落盘目录 / 文件名消毒
// ============================================================================

/// TCB [`JsonValue`] → `serde_json::Value` 显式递归转换。
///
/// JsonValue 六变体（Null/Bool/Integer/String/Array/Object）逐一映射；
/// 无浮点变体——TCB 语义本就不携带 f64（数值精度域归各计算服务自己管），
/// 浮点入参在服务链更早处即已如实报错，本转换不造数。
fn tcb_to_serde(v: &JsonValue) -> Result<serde_json::Value, String> {
    Ok(match v {
        JsonValue::Null => serde_json::Value::Null,
        JsonValue::Bool(b) => serde_json::Value::Bool(*b),
        JsonValue::Integer(i) => serde_json::Value::from(*i),
        JsonValue::String(s) => serde_json::Value::String(s.to_string()),
        JsonValue::Array(items) => serde_json::Value::Array(
            items
                .iter()
                .map(tcb_to_serde)
                .collect::<Result<Vec<_>, _>>()?,
        ),
        JsonValue::Object(map) => {
            let mut obj = serde_json::Map::new();
            for (k, val) in map.iter() {
                obj.insert(k.clone(), tcb_to_serde(val)?);
            }
            serde_json::Value::Object(obj)
        }
    })
}

/// 导出落盘目录：`EVORULE_EXPORT_DIR` 覆盖，缺省 `data/exports`。
fn export_dir() -> Result<std::path::PathBuf, String> {
    match std::env::var("EVORULE_EXPORT_DIR") {
        Ok(p) if !p.trim().is_empty() => Ok(std::path::PathBuf::from(p)),
        _ => Ok(std::path::PathBuf::from("data").join("exports")),
    }
}

/// 导出文件名消毒：防路径穿越 + 字符白名单 + 长度帽 + 强制后缀。
///
/// - 指定名：剥任意路径分量（`/` `\` 分段取末段），`.` 之外的非常规字符替换 `_`，
///   保留字母/数字/`-`/`_`/`.`；基名超 [`MAX_FILENAME_STEM`] 截断；补 `.{ext}` 后缀；
/// - 缺省名：`{stem_hint 消毒}_{内容哈希前 12 hex}.{ext}`——同输入同文件名，
///   幂等覆写（内容寻址语义）；
/// - 消毒后为空（如指定名全是非法字符）→ 回落缺省名，不静默失败也不裸奔。
fn sanitize_filename(specified: Option<&str>, stem_hint: &str, hex: &str, ext: &str) -> String {
    let sanitize_stem = |s: &str| -> String {
        s.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                    c
                } else {
                    '_'
                }
            })
            .collect::<String>()
            .trim_matches(['_', '.'])
            .chars()
            .take(MAX_FILENAME_STEM)
            .collect()
    };
    let default_stem = || {
        let ct = sanitize_stem(stem_hint);
        let ct = if ct.is_empty() { "report".to_string() } else { ct };
        format!("{ct}_{}", &hex.chars().take(12).collect::<String>())
    };
    let stem = match specified {
        Some(raw) => {
            // 剥路径分量（正反斜杠皆防），取末段再消毒
            let last = raw.rsplit(['/', '\\']).next().unwrap_or("");
            let s = sanitize_stem(last);
            // 去掉消毒后残留的多余扩展段（仅保留基名，后缀统一由本函数追加）
            let suffix = format!(".{ext}");
            let s = s.strip_suffix(suffix.as_str()).unwrap_or(&s).to_string();
            if s.is_empty() {
                default_stem()
            } else {
                s
            }
        }
        None => default_stem(),
    };
    format!("{stem}.{ext}")
}

// ============================================================================
// 测试
// ============================================================================

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    /// SSOT 守卫：声明表 vs official_native_services.json（三字段+顺序，漂移即红）
    #[test]
    fn ssot_declaration_matches_json() {
        let doc: serde_json::Value =
            serde_json::from_str(include_str!("../official_native_services.json")).unwrap();
        let svcs = doc["services"].as_array().unwrap();
        assert_eq!(
            svcs.len(),
            NATIVE_SERVICES.len(),
            "SSOT 与声明表件数应一致"
        );
        for (j, d) in svcs.iter().zip(NATIVE_SERVICES.iter()) {
            assert_eq!(j["name"].as_str().unwrap(), d.name, "name 应一致");
            assert_eq!(
                j["sensitive"].as_bool().unwrap(),
                d.sensitive,
                "sensitive 应一致（{}）",
                d.name
            );
            assert_eq!(
                j["description"].as_str().unwrap(),
                d.description,
                "description 应与 SSOT 逐字节一致（{}）",
                d.name
            );
        }
    }

    #[test]
    fn tcb_to_serde_roundshapes() {
        let v = JsonValue::object_from_pairs(&[
            ("b", JsonValue::Bool(true)),
            ("i", JsonValue::Integer(-7)),
            ("s", JsonValue::string("中")),
            (
                "a",
                JsonValue::array(vec![JsonValue::Null, JsonValue::Integer(1)]),
            ),
        ]);
        let sv = tcb_to_serde(&v).unwrap();
        assert_eq!(sv["b"], serde_json::json!(true));
        assert_eq!(sv["i"], serde_json::json!(-7));
        assert_eq!(sv["s"], serde_json::json!("中"));
        assert_eq!(sv["a"], serde_json::json!([null, 1]));
    }

    /// 文件名消毒：路径穿越/非法字符/后缀/缺省名幂等语义
    #[test]
    fn filename_sanitization_blocks_traversal() {
        let f = sanitize_filename(Some("../../evil/name"), "audit_chain", "abcd1234ef56", "pdf");
        assert!(!f.contains('/'), "不应含路径分隔符: {f}");
        assert!(!f.contains('\\'), "不应含反斜杠: {f}");
        assert!(!f.contains(".."), "不应含相对上跳: {f}");
        assert!(f.ends_with(".pdf"), "应强制 .pdf 后缀: {f}");
        assert!(f.starts_with("name"), "应保留合法末段基名: {f}");

        // 全非法字符 → 回落缺省名（不空、不裸奔）
        let g = sanitize_filename(Some("///"), "audit_chain", "abcd1234ef56", "pdf");
        assert!(g.starts_with("audit_chain_"), "全非法字符应回落缺省名: {g}");

        // 缺省名确定性：同输入同名（幂等覆写语义）
        let a = sanitize_filename(None, "audit_chain", "abcd1234ef56", "pdf");
        let b = sanitize_filename(None, "audit_chain", "abcd1234ef56", "pdf");
        assert_eq!(a, b, "缺省名应确定性");
        assert!(a.starts_with("audit_chain_abcd1234ef56"), "缺省名含内容哈希前缀: {a}");

        // 显式 .pdf 后缀不双写
        let c = sanitize_filename(Some("x.pdf"), "t", "00", "pdf");
        assert_eq!(c, "x.pdf", "后缀不应双写: {c}");
    }

    /// xlsx 服务黄金：双跑同 blake3 + zip 魔数 + 跨进程确定性由渲染器旋钮保证
    #[test]
    fn xlsx_service_golden_and_deterministic() {
        let args = JsonValue::object_from_pairs(&[
            ("title", JsonValue::string("比例分配对账")),
            (
                "sheets",
                JsonValue::array(vec![JsonValue::object_from_pairs(&[
                    ("name", JsonValue::string("对账表")),
                    (
                        "header",
                        JsonValue::array(vec![
                            JsonValue::string("份额方"),
                            JsonValue::string("金额(分)"),
                        ]),
                    ),
                    (
                        "rows",
                        JsonValue::array(vec![
                            JsonValue::array(vec![
                                JsonValue::string("甲"),
                                JsonValue::Integer(34),
                            ]),
                            JsonValue::array(vec![
                                JsonValue::string("乙"),
                                JsonValue::Integer(33),
                            ]),
                        ]),
                    ),
                ])]),
            ),
            (
                "provenance",
                JsonValue::array(vec![JsonValue::object_from_pairs(&[
                    ("claim", JsonValue::string("三份均分 100")),
                    ("service", JsonValue::string("gov_ratio_split")),
                    ("args", JsonValue::string("{\"total\":100,\"weights\":[1,1,1]}")),
                    ("digest", JsonValue::string("blake3:demo")),
                ])]),
            ),
            ("filename", JsonValue::string("golden_split.xlsx")),
        ]);
        let svc = mk_export_workbook_xlsx();
        let e1 = svc.execute(&args).expect("xlsx 应渲染成功");
        let e2 = svc.execute(&args).expect("第二次调用应同样成功");
        assert_eq!(
            e1.get("blake3").and_then(|v| v.as_str()),
            e2.get("blake3").and_then(|v| v.as_str()),
            "xlsx 同输入双跑 blake3 应一致（确定性门）"
        );
        let path = e1.get("path").and_then(|v| v.as_str()).unwrap();
        assert!(path.ends_with("golden_split.xlsx"), "落盘路径应含指定名: {path}");
        let bytes = std::fs::read(path).unwrap();
        assert!(bytes.starts_with(b"PK") || bytes.starts_with(b"PK"), "xlsx 应为 zip 魔数开头");
    }

    /// docx 服务黄金：双跑同 blake3 + zip 魔数（docx 亦为 zip 容器）
    #[test]
    fn docx_service_golden_and_deterministic() {
        let args = JsonValue::object_from_pairs(&[
            ("title", JsonValue::string("确定性交付物示例")),
            (
                "paragraphs",
                JsonValue::array(vec![JsonValue::string(
                    "本报告全部数字出自确定性服务，可按溯源清单逐项重算。",
                )]),
            ),
            (
                "tables",
                JsonValue::array(vec![JsonValue::object_from_pairs(&[
                    (
                        "header",
                        JsonValue::array(vec![
                            JsonValue::string("合计(分)"),
                            JsonValue::string("校验"),
                        ]),
                    ),
                    (
                        "rows",
                        JsonValue::array(vec![JsonValue::array(vec![
                            JsonValue::string("100"),
                            JsonValue::string("一致"),
                        ])]),
                    ),
                ])]),
            ),
            ("filename", JsonValue::string("golden_report.docx")),
        ]);
        let svc = mk_export_document_docx();
        let e1 = svc.execute(&args).expect("docx 应渲染成功");
        let e2 = svc.execute(&args).expect("第二次调用应同样成功");
        assert_eq!(
            e1.get("blake3").and_then(|v| v.as_str()),
            e2.get("blake3").and_then(|v| v.as_str()),
            "docx 同输入双跑 blake3 应一致（确定性门）"
        );
        let path = e1.get("path").and_then(|v| v.as_str()).unwrap();
        let bytes = std::fs::read(path).unwrap();
        assert!(bytes.starts_with(b"PK"), "docx 应为 zip 魔数开头");
    }

    /// 服务面黄金：真渲染→落盘→信封（无字体环境如实跳过，不假装通过）。
    /// 同时是确定性双跑门：同输入两次调用 blake3 一致。
    #[test]
    fn service_envelope_golden_and_deterministic() {
        if pdf_probe::no_font_env() {
            eprintln!("SKIP service_envelope_golden_and_deterministic: 本机无中文字体环境");
            return;
        }
        let args = JsonValue::object_from_pairs(&[
            ("content_type", JsonValue::string("audit_chain")),
            (
                "raw_data",
                JsonValue::array(vec![
                    JsonValue::object_from_pairs(&[
                        ("fact_id", JsonValue::string("#30001")),
                        (
                            "payload",
                            JsonValue::object_from_pairs(&[("规则", JsonValue::string("上限 5000 元"))]),
                        ),
                    ]),
                ]),
            ),
            ("filename", JsonValue::string("golden_audit.pdf")),
        ]);
        let svc = mk_export_report_pdf();
        let e1 = svc.execute(&args).expect("有字体环境应渲染成功");
        let e2 = svc.execute(&args).expect("第二次调用应同样成功");
        assert_eq!(
            e1.get("blake3").and_then(|v| v.as_str()),
            e2.get("blake3").and_then(|v| v.as_str()),
            "同输入双跑 blake3 应一致（确定性门）"
        );
        assert_eq!(
            e1.get("status").and_then(|v| v.as_str()),
            Some("determined")
        );
        let path = e1.get("path").and_then(|v| v.as_str()).unwrap();
        assert!(path.ends_with("golden_audit.pdf"), "落盘路径应含指定名: {path}");
        let bytes = std::fs::read(path).unwrap();
        assert!(bytes.starts_with(b"%PDF"), "落盘文件应为 PDF 魔数开头");
    }

    /// 测试辅助：探测本机是否有可用中文字体（复用 pdf 模块内部探测；
    /// 探测为私有实现，此处经行为侧感知——缺字体时 render_pdf 报字体错）
    mod pdf_probe {
        pub(super) fn no_font_env() -> bool {
            let probe = crate::pdf::PdfExportRequest {
                content_type: String::new(),
                ..Default::default()
            };
            // 缺 content_type 的报错先于字体探测——用合法请求走完整路径探测字体
            let probe_full = crate::pdf::PdfExportRequest {
                content_type: "probe".to_string(),
                ..Default::default()
            };
            let _ = probe;
            match crate::pdf::render_pdf(&probe_full) {
                Err(e) => e.contains("字体"),
                Ok(_) => false,
            }
        }
    }
}
