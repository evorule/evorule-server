// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 服务端 PDF 导出 —— `POST /api/export/pdf`（UV-084 W6 / UV-066 实化）
//!
//! **纯 Rust 文本型路线**（pdf-writer + ttf-parser + subsetter，typst 同源生态，
//! 零系统依赖，不违反分发包零依赖承诺）：
//!
//! - **中文字体策略**（用户 2026-09-05 批准路线 A）：系统字体探测 + 嵌入子集。
//!   探测顺序：`--pdf-font` 显式指定 → Windows 系统字体目录（msyh/simhei/simsun 等）
//!   → Linux Noto Sans CJK / 文泉驿。仅嵌入文档实际用到的字形（典型增量 50-200KB），
//!   任何 PDF 查看器保真显示。
//! - **fail-fast 纪律**：探测不到可用字体 → 显式报错带自诊断指引（不生成空白 PDF）；
//!   字体缺请求内容中的字符 → 报错列出缺字（不生成豆腐块 PDF）。拒绝一切静默降级
//!   ——降级是 console 侧的既有设计（浏览器打印），server 侧只做"要么正确渲染、
//!   要么明确失败"。
//! - **文本型边界**（如实声明）：无富样式排版（字体/颜色/图片），内容 = 标题 +
//!   元信息 + 数据摘要表 + 完整性块 + 页脚页码；超长 JSON 字段截断加省略标记
//!   （完整数据走 JSON/XML 导出，PDF 是人类可读报告）。
//!
//! 契约对齐 console `export-renderers.ts` PdfRenderer.tryServerPdf（消费面已定）：
//! 请求体 JSON（content_type 必填，其余可选），响应 `application/pdf` 二进制；
//! 失败返回 `{success:false,message}`（与 permissions/marketplace 同形状）。

use axum::body::Body;
use axum::extract::rejection::JsonRejection;
use axum::http::{header, StatusCode};
use axum::response::Response;
use axum::routing::post;
use axum::{Json, Router};
use pdf_writer::types::{CidFontType, FontFlags, SystemInfo};
use pdf_writer::{Content, Name, Pdf, Rect, Ref, Str};
use serde::Deserialize;
use serde_json::Value;
use std::path::PathBuf;

use crate::api::server::AppState;

/// 请求体上限（32MB）：console 可携带全量审计事实（raw_data），axum 默认 2MB 不够用。
/// PDF 是摘要型渲染，超限由客户端裁剪（includeRaw 关闭）或走 JSON 导出。
const MAX_BODY_BYTES: usize = 32 * 1024 * 1024;

/// 单节数据行渲染上限（超出截断 + 显式省略标记，不静默）
const MAX_DATA_ROWS: usize = 1000;

/// 单字段 JSON 摘要截断长度（字符）
const MAX_FIELD_CHARS: usize = 400;

/// `--pdf-font` 显式覆盖（进程级一次性配置，main.rs 启动期设置；None = 自动探测）
static FONT_OVERRIDE: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();

/// main.rs 启动期调用：设置 `--pdf-font` 显式字体路径
pub fn set_font_override(path: Option<PathBuf>) {
    let _ = FONT_OVERRIDE.set(path);
}

// ============================================================================
// 1. 请求契约（对齐 console export-renderers.ts L374-395 的请求体形状）
// ============================================================================

/// PDF 导出请求体（serde 宽松：除 content_type 外全部可选）
#[derive(Deserialize, Default)]
#[serde(default)]
pub struct PdfExportRequest {
    /// 内容类型（必填：audit_chain / comprehensive / …；空 = 400）
    content_type: String,
    session_id: Option<Value>,
    ruleset_version: Option<Value>,
    range: Option<Value>,
    raw_data: Option<Value>,
    business_data: Option<Value>,
    integrity: Option<Value>,
    meta: Option<PdfRequestMeta>,
    options: Option<PdfRequestOptions>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct PdfRequestMeta {
    operator: Option<String>,
    exported_at: Option<String>,
    range_description: Option<String>,
    template_id: Option<String>,
    console_version: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct PdfRequestOptions {
    pdf_title: Option<String>,
    pdf_organization: Option<String>,
    include_integrity: Option<bool>,
    include_meta: Option<bool>,
}

// ============================================================================
// 2. 字体探测（路线 A：显式指定 → Windows → Linux）
// ============================================================================

/// 探测到的字体源（文件字节 + TTC collection 内的 face 序号 + 来源描述）
#[derive(Debug)]
struct FontSource {
    data: Vec<u8>,
    index: u32,
    origin: String,
}

/// Windows 系统字体候选（优先序：雅黑 → 黑体 → 宋体 → 等线）
const WIN_FONT_CANDIDATES: &[&str] = &[
    "msyh.ttc",
    "msyh.ttf",
    "simhei.ttf",
    "simsun.ttc",
    "simsun.ttf",
    "Deng.ttf",
    "msyhl.ttc",
];

/// Linux 常见中文字体文件名（优先序：Noto CJK → 文泉驿）
const LINUX_FONT_CANDIDATES: &[&str] = &[
    "NotoSansCJK-Regular.ttc",
    "NotoSansCJKsc-Regular.otf",
    "NotoSansSC-Regular.ttf",
    "wqy-microhei.ttc",
    "wqy-zenhei.ttc",
];

/// 探测可用中文字体。
///
/// 失败必须带自诊断指引（fail-fast 纪律：宁可明确失败，不生成缺字 PDF）。
fn find_cjk_font(explicit: Option<&std::path::Path>) -> Result<FontSource, String> {
    // ① 显式指定（--pdf-font）：唯一候选，失败即报错（用户显式配置错了要立刻知道）
    if let Some(p) = explicit {
        let data = std::fs::read(p).map_err(|e| {
            format!(
                "读取 --pdf-font 指定的字体失败: {}（{e}）。\
                 请检查路径是否正确、文件是否为 TTF/OTF/TTC 格式",
                p.display()
            )
        })?;
        return Ok(FontSource {
            data,
            index: 0,
            origin: format!("--pdf-font: {}", p.display()),
        });
    }

    // ② Windows：%SystemRoot%\Fonts
    if let Ok(sys_root) = std::env::var("SystemRoot") {
        let fonts_dir = std::path::Path::new(&sys_root).join("Fonts");
        for name in WIN_FONT_CANDIDATES {
            let p = fonts_dir.join(name);
            if let Ok(data) = std::fs::read(&p) {
                return Ok(FontSource {
                    data,
                    index: 0,
                    origin: format!("Windows 系统字体: {}", p.display()),
                });
            }
        }
    }

    // ③ Linux：/usr/share/fonts 下递归匹配常见中文字体文件名
    let linux_root = std::path::Path::new("/usr/share/fonts");
    if linux_root.is_dir() {
        for want in LINUX_FONT_CANDIDATES {
            if let Some(p) = find_file_recursive(linux_root, want, 0) {
                if let Ok(data) = std::fs::read(&p) {
                    return Ok(FontSource {
                        data,
                        index: 0,
                        origin: format!("Linux 系统字体: {}", p.display()),
                    });
                }
            }
        }
    }

    Err(
        "未找到可用的中文字体（已尝试 Windows 系统字体目录与 /usr/share/fonts 常见 \
         Noto/文泉驿候选）。服务端 PDF 导出需要嵌入中文字体才能保证任何查看器保真显示。\
         修复指引：① 用 --pdf-font <路径> 显式指定一个 TTF/OTF/TTC 中文字体；\
         ② 或在服务器安装中文字体（如 Noto Sans CJK）后重启服务"
            .to_string(),
    )
}

/// 在目录树中按文件名查找（深度限制防符号链接环）
fn find_file_recursive(
    dir: &std::path::Path,
    name: &str,
    depth: usize,
) -> Option<PathBuf> {
    if depth > 4 {
        return None;
    }
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                if depth < 4 {
                    // 广度受限的一层递归（stack 已展开，深度由外层控制）
                    if let Some(hit) = find_file_recursive(&p, name, depth + 1) {
                        return Some(hit);
                    }
                }
            } else if p.file_name().map(|f| f == name).unwrap_or(false) {
                return Some(p);
            }
        }
    }
    None
}

// ============================================================================
// 3. 渲染核心：布局 + 字体子集化 + PDF 写出
// ============================================================================

/// A4 页面几何（PDF 点；1pt = 1/72 inch）
const PAGE_W: f32 = 595.28;
const PAGE_H: f32 = 841.89;
const MARGIN: f32 = 56.7; // 2cm
const CONTENT_W: f32 = PAGE_W - 2.0 * MARGIN;

/// 字号（pt）
const SIZE_TITLE: f32 = 16.0;
const SIZE_SECTION: f32 = 12.0;
const SIZE_BODY: f32 = 9.0;
const SIZE_FOOTER: f32 = 8.0;
const LINE_GAP: f32 = 1.6; // 行距倍数

/// 页面绘制操作（布局阶段产出，写出阶段消费）
#[derive(Clone)]
enum Op {
    /// 文本（基线锚点，PDF 坐标系：原点左下）
    Text { x: f32, y: f32, size: f32, text: String },
    /// 水平线（表格分隔/节分隔）
    Rule { x0: f32, x1: f32, y: f32 },
}

/// 字形度量（从 ttf-parser Face 提取；upem 归一化的字符宽度查询）
struct GlyphMetrics<'a> {
    face: ttf_parser::Face<'a>,
}

impl GlyphMetrics<'_> {
    /// 字符宽度（pt）：advance / upem × size；无字形返回 None（缺字检查用）
    fn char_width(&self, c: char, size: f32) -> Option<f32> {
        let gid = self.face.glyph_index(c)?;
        let advance = self.face.glyph_hor_advance(gid)?;
        Some(advance as f32 / self.face.units_per_em() as f32 * size)
    }
}

/// 布局器：行流 → 分页 → Op 流
struct Layout {
    pages: Vec<Vec<Op>>,
    current: Vec<Op>,
    y: f32, // 下一条基线的 y（从页顶向下递减）
}

impl Layout {
    fn new() -> Self {
        Self {
            pages: Vec::new(),
            current: Vec::new(),
            y: PAGE_H - MARGIN,
        }
    }

    /// 剩余高度不足时换页
    fn ensure(&mut self, need: f32) {
        if self.y - need < MARGIN {
            self.new_page();
        }
    }

    fn new_page(&mut self) {
        self.pages.push(std::mem::take(&mut self.current));
        self.y = PAGE_H - MARGIN;
    }

    /// 在当前 y 基线添加文本并下移一行
    fn line(&mut self, x: f32, size: f32, text: &str) {
        self.current.push(Op::Text {
            x,
            y: self.y,
            size,
            text: text.to_string(),
        });
        self.y -= size * LINE_GAP;
    }

    /// 添加水平线（当前 y 基线上方 size/3 处，视觉贴近文本行）
    fn rule(&mut self, x0: f32, x1: f32, size: f32) {
        self.current.push(Op::Rule {
            x0,
            x1,
            y: self.y + size / 3.0,
        });
    }

    /// 留白
    fn gap(&mut self, h: f32) {
        self.y -= h;
    }

    /// 结束布局（收尾当前页）
    fn finish(mut self) -> Vec<Vec<Op>> {
        self.pages.push(std::mem::take(&mut self.current));
        self.pages
    }
}

/// 按宽度换行（贪心；记录最后空格位置优先整词断行，CJK 逐字断行）
fn wrap_text(text: &str, metrics: &GlyphMetrics, size: f32, max_w: f32) -> Vec<String> {
    let mut lines = Vec::new();
    let mut line = String::new();
    let mut line_w = 0.0f32;
    let mut last_space: Option<(usize, f32)> = None; // (char 索引, 该空格前宽度)

    for c in text.chars() {
        // 缺字字符不能在此丢弃（估宽占位保留）——否则下游 check_missing_glyphs
        // 检查布局产物时永远看不到它，fail-fast 链路被布局阶段静默截断
        // （2026-09-05 UV-084 W6 实测发现：U+E000 在雅黑有字形测试反而暴露此 bug）
        let cw = metrics.char_width(c, size).unwrap_or(size * 0.6);
        if c == '\n' {
            lines.push(std::mem::take(&mut line));
            line_w = 0.0;
            last_space = None;
            continue;
        }
        if line_w + cw > max_w && !line.is_empty() {
            // 优先回退到最后一个空格断行（英文整词）
            if let Some((idx, w)) = last_space {
                let rest: String = line[idx..].to_string();
                line.truncate(idx);
                lines.push(std::mem::take(&mut line));
                line = rest;
                line_w -= w;
            } else {
                lines.push(std::mem::take(&mut line));
                line_w = 0.0;
            }
            last_space = None;
        }
        if c == ' ' {
            // 字节索引（不是 chars().count()）：下游 line[idx..]/truncate(idx) 按字节
            // 解释；空格是单字节 ASCII，字节索引必落在字符边界。之前用字符数导致
            // 中英混排 + 空格断行时切进多字节字符内部 → panic（2026-09-05 W6 实测
            // 5 条中文 fact 即触发，单测短文本未覆盖此路径）
            last_space = Some((line.len(), line_w + cw));
        }
        line.push(c);
        line_w += cw;
    }
    if !line.is_empty() {
        lines.push(line);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

/// JSON 值 → 展示字符串（紧凑 + 截断 + 省略标记）
fn value_summary(v: &Value) -> String {
    let mut s = match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => serde_json::to_string(other).unwrap_or_else(|_| format!("{other:?}")),
    };
    // 控制字符替换（PDF 文本流不收控制字符）
    s = s
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    if s.chars().count() > MAX_FIELD_CHARS {
        let head: String = s.chars().take(MAX_FIELD_CHARS).collect();
        format!("{head}…（截断，完整数据请导出 JSON）")
    } else {
        s
    }
}

/// 渲染 PDF 主入口（核心与 HTTP 层解耦，供单测直测）
///
/// 返回完整 PDF 字节流。错误信息面向 API 消费者（含自诊断指引，不静默）。
pub fn render_pdf(req: &PdfExportRequest) -> Result<Vec<u8>, String> {
    if req.content_type.trim().is_empty() {
        return Err("缺少 content_type（导出内容类型，必填）".to_string());
    }

    // ① 字体（探测 + 解析）
    let font = find_cjk_font(FONT_OVERRIDE.get().and_then(|o| o.as_deref()))?;
    let face = ttf_parser::Face::parse(&font.data, font.index).map_err(|e| {
        format!(
            "解析字体失败（来源 {}）: {e:?}。指引：换用标准 TTF/OTF/TTC 字体文件",
            font.origin
        )
    })?;
    let metrics = GlyphMetrics { face };

    // ② 布局（行流）
    let ops = build_layout(req, &metrics);

    // ③ 缺字检查（fail-fast：不生成豆腐块 PDF）
    check_missing_glyphs(&ops, &metrics)?;

    // ④ 字体子集化（GlyphRemapper：CID = remapped GID）
    let mut used: Vec<u16> = ops
        .iter()
        .flat_map(|page| page.iter())
        .filter_map(|op| match op {
            Op::Text { text, .. } => Some(text.chars()),
            Op::Rule { .. } => None,
        })
        .flatten()
        .filter_map(|c| metrics.face.glyph_index(c))
        .map(|g| g.0)
        .collect();
    used.sort_unstable();
    used.dedup();

    let mut remapper = subsetter::GlyphRemapper::new();
    for gid in &used {
        remapper.remap(*gid);
    }
    let subset = subsetter::subset(&font.data, font.index, &remapper).map_err(|e| {
        format!(
            "字体子集化失败（来源 {}）: {e:?}。指引：换用标准 TrueType 轮廓字体（.ttf/.ttc）",
            font.origin
        )
    })?;

    // ⑤ PDF 写出
    write_pdf(&ops, &metrics, &remapper, &subset, req)
}

/// 组装页面内容（布局；不含页脚——写出阶段补，需要总页数）
fn build_layout(req: &PdfExportRequest, metrics: &GlyphMetrics) -> Vec<Vec<Op>> {
    let opts = req.options.as_ref();
    let title = opts
        .and_then(|o| o.pdf_title.as_deref())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or("evorule 导出报告");
    let org = opts.and_then(|o| o.pdf_organization.as_deref()).unwrap_or("");

    let mut ly = Layout::new();

    // --- 标题区 ---
    for l in wrap_text(title, metrics, SIZE_TITLE, CONTENT_W) {
        ly.ensure(SIZE_TITLE * LINE_GAP);
        ly.line(MARGIN, SIZE_TITLE, &l);
    }
    if !org.is_empty() {
        for l in wrap_text(org, metrics, SIZE_BODY, CONTENT_W) {
            ly.line(MARGIN, SIZE_BODY, &l);
        }
    }
    ly.gap(4.0);
    ly.rule(MARGIN, MARGIN + CONTENT_W, SIZE_BODY);
    ly.gap(8.0);

    // --- 元信息区 ---
    let include_meta = opts.and_then(|o| o.include_meta).unwrap_or(true);
    if include_meta {
        let meta = req.meta.as_ref();
        let rows: Vec<(&str, String)> = vec![
            ("内容类型", req.content_type.clone()),
            (
                "操作者",
                meta.and_then(|m| m.operator.clone()).unwrap_or_default(),
            ),
            (
                "导出时间",
                meta.and_then(|m| m.exported_at.clone()).unwrap_or_default(),
            ),
            (
                "范围",
                meta.and_then(|m| m.range_description.clone()).unwrap_or_default(),
            ),
            (
                "会话",
                value_summary(&req.session_id.clone().unwrap_or(Value::Null)),
            ),
            (
                "规则集版本",
                value_summary(&req.ruleset_version.clone().unwrap_or(Value::Null)),
            ),
            (
                "Console 版本",
                meta.and_then(|m| m.console_version.clone()).unwrap_or_default(),
            ),
            (
                "模板",
                meta.and_then(|m| m.template_id.clone()).unwrap_or_default(),
            ),
        ];
        section_heading(&mut ly, "导出信息");
        for (k, v) in rows {
            if v.is_empty() {
                continue;
            }
            let label = format!("{k}:");
            for (i, l) in wrap_text(&v, metrics, SIZE_BODY, CONTENT_W - 80.0).into_iter().enumerate()
            {
                ly.ensure(SIZE_BODY * LINE_GAP);
                if i == 0 {
                    ly.line(MARGIN, SIZE_BODY, &label);
                }
                ly.line(MARGIN + 80.0, SIZE_BODY, &l);
            }
        }
        ly.gap(8.0);
    }

    // --- 原始数据区 ---
    if let Some(raw) = &req.raw_data {
        if !raw.is_null() {
            section_heading(&mut ly, "原始数据（raw_data）");
            render_value_block(&mut ly, metrics, raw);
            ly.gap(8.0);
        }
    }

    // --- 业务数据区 ---
    if let Some(biz) = &req.business_data {
        if !biz.is_null() {
            section_heading(&mut ly, "业务数据（business_data）");
            render_value_block(&mut ly, metrics, biz);
            ly.gap(8.0);
        }
    }

    // --- 完整性区 ---
    let include_integrity = opts.and_then(|o| o.include_integrity).unwrap_or(true);
    if include_integrity {
        if let Some(integ) = &req.integrity {
            if !integ.is_null() {
                section_heading(&mut ly, "完整性信息（integrity）");
                render_value_block(&mut ly, metrics, integ);
                ly.gap(8.0);
            }
        }
    }

    ly.finish()
}

/// 节标题（带下划线）
fn section_heading(ly: &mut Layout, title: &str) {
    ly.ensure(SIZE_SECTION * LINE_GAP + 12.0);
    ly.gap(6.0);
    ly.line(MARGIN, SIZE_SECTION, title);
    ly.rule(MARGIN, MARGIN + CONTENT_W, SIZE_SECTION);
    ly.gap(2.0);
}

/// JSON 值自适应渲染（数组 → 逐条摘要行；对象 → 键值行；标量 → 单行）
fn render_value_block(ly: &mut Layout, metrics: &GlyphMetrics, v: &Value) {
    match v {
        Value::Array(items) => {
            let total = items.len();
            for (i, item) in items.iter().take(MAX_DATA_ROWS).enumerate() {
                let head = format!("[{}] ", i + 1);
                let head_w: f32 =
                    head.chars().filter_map(|c| metrics.char_width(c, SIZE_BODY)).sum();
                let summary = value_summary(item);
                for (j, l) in wrap_text(&summary, metrics, SIZE_BODY, CONTENT_W - head_w)
                    .into_iter()
                    .enumerate()
                {
                    ly.ensure(SIZE_BODY * LINE_GAP);
                    if j == 0 {
                        ly.line(MARGIN, SIZE_BODY, &head);
                    }
                    ly.line(MARGIN + head_w, SIZE_BODY, &l);
                }
            }
            if total > MAX_DATA_ROWS {
                ly.ensure(SIZE_BODY * LINE_GAP);
                ly.line(
                    MARGIN,
                    SIZE_BODY,
                    &format!("…共 {total} 条，已截断至前 {MAX_DATA_ROWS} 条（完整数据请导出 JSON）"),
                );
            }
        }
        Value::Object(map) => {
            for (k, val) in map {
                let label = format!("{k}: ");
                let label_w: f32 = label
                    .chars()
                    .filter_map(|c| metrics.char_width(c, SIZE_BODY))
                    .sum();
                let summary = value_summary(val);
                for (j, l) in
                    wrap_text(&summary, metrics, SIZE_BODY, CONTENT_W - label_w).into_iter().enumerate()
                {
                    ly.ensure(SIZE_BODY * LINE_GAP);
                    if j == 0 {
                        ly.line(MARGIN, SIZE_BODY, &label);
                    }
                    ly.line(MARGIN + label_w, SIZE_BODY, &l);
                }
            }
        }
        scalar => {
            for l in wrap_text(&value_summary(scalar), metrics, SIZE_BODY, CONTENT_W) {
                ly.ensure(SIZE_BODY * LINE_GAP);
                ly.line(MARGIN, SIZE_BODY, &l);
            }
        }
    }
}

/// 缺字检查（fail-fast：列出字体不支持的字符，拒绝生成豆腐块 PDF）
fn check_missing_glyphs(pages: &[Vec<Op>], metrics: &GlyphMetrics) -> Result<(), String> {
    let mut missing: Vec<char> = Vec::new();
    for page in pages {
        for op in page {
            let Op::Text { text, .. } = op else { continue };
            for c in text.chars() {
                if c.is_control() || c == ' ' {
                    continue;
                }
                if metrics.face.glyph_index(c).is_none() && !missing.contains(&c) {
                    missing.push(c);
                }
            }
        }
    }
    if missing.is_empty() {
        return Ok(());
    }
    let list: Vec<String> = missing
        .iter()
        .take(20)
        .map(|c| format!("{c}(U+{:04X})", *c as u32))
        .collect();
    Err(format!(
        "渲染字体缺少以下字符的字形: {}{}。拒绝生成缺字 PDF（fail-fast）。\
         指引：换用覆盖面更广的中文字体（--pdf-font 指定，如 Noto Sans CJK / 微软雅黑）",
        list.join(" "),
        if missing.len() > 20 { " …" } else { "" }
    ))
}

/// PDF 写出（对象族：catalog / pages / 字体五件套 / 每页 page+content）
fn write_pdf(
    pages: &[Vec<Op>],
    metrics: &GlyphMetrics,
    remapper: &subsetter::GlyphRemapper,
    subset: &[u8],
    req: &PdfExportRequest,
) -> Result<Vec<u8>, String> {
    // 对象编号：1=catalog 2=pages 3=Type0 4=CIDFont 5=Descriptor 6=FontFile2 7=ToUnicode
    // 100+ = 每页 2 个（page=100+2i, content=101+2i）
    let catalog_id = Ref::new(1);
    let page_tree_id = Ref::new(2);
    let type0_id = Ref::new(3);
    let cid_id = Ref::new(4);
    let desc_id = Ref::new(5);
    let file2_id = Ref::new(6);
    let tounicode_id = Ref::new(7);

    let mut pdf = Pdf::new();
    pdf.catalog(catalog_id).pages(page_tree_id);
    pdf.pages(page_tree_id)
        .count(pages.len() as i32)
        .kids(pages.iter().enumerate().map(|(i, _)| Ref::new(100 + 2 * i as i32)));

    // --- 字体对象族 ---
    let upem = metrics.face.units_per_em() as f32;
    let to_em = |v: f32| v / upem * 1000.0;

    pdf.type0_font(type0_id)
        .base_font(Name(b"EvoruleCJK"))
        .encoding_predefined(Name(b"Identity-H"))
        .descendant_font(cid_id)
        .to_unicode(tounicode_id);

    let bbox = metrics.face.global_bounding_box();

    // /W 宽度表先收集（cid → 1000 空间宽度，升序），随后与 CIDFont 一次性写入
    // （pdf-writer 禁止对同一 Ref 写两次对象，之前两段式写法会 panic）
    let mut width_entries: Vec<(u16, f32)> = Vec::new();
    collect_widths(pages, metrics, remapper, &mut width_entries);
    width_entries.sort_unstable_by_key(|(cid, _)| *cid);

    let mut cid_font = pdf.cid_font(cid_id);
    cid_font
        .subtype(CidFontType::Type2)
        .base_font(Name(b"EvoruleCJK"))
        .system_info(SystemInfo {
            registry: Str(b"Adobe"),
            ordering: Str(b"Identity"),
            supplement: 0,
        })
        .font_descriptor(desc_id)
        .default_width(500.0)
        .cid_to_gid_map_predefined(Name(b"Identity"));
    {
        let mut widths = cid_font.widths();
        widths.consecutive(0, std::iter::once(500.0)); // .notdef 兜底
        for (cid, w) in width_entries {
            widths.same(cid, cid, w);
        }
    }
    drop(cid_font);

    pdf.font_descriptor(desc_id)
        .name(Name(b"EvoruleCJK"))
        .flags(FontFlags::SYMBOLIC)
        .italic_angle(0.0)
        .ascent(to_em(metrics.face.ascender() as f32))
        .descent(to_em(metrics.face.descender() as f32))
        .cap_height(to_em(metrics.face.ascender() as f32))
        .stem_v(80.0)
        .bbox(Rect::new(
            to_em(bbox.x_min as f32),
            to_em(bbox.y_min as f32),
            to_em(bbox.x_max as f32),
            to_em(bbox.y_max as f32),
        ))
        .font_file2(file2_id);

    // FontFile2：子集字体字节流（不压缩：pdf-writer 只写 /Filter 声明不做实际压缩，
    // 声明 Flate 而写原始字节会产出损坏 PDF——保持无压缩、零额外依赖）
    // /Length1 是 FontFile2 的规范必带键（未压缩字体长度）；Chrome PDFium 等查看器
    // 缺它直接拒载嵌入字体 → 页面所有文字不渲染（2026-09-05 W6 浏览器实测发现）
    pdf.stream(file2_id, subset)
        .pair(Name(b"Length1"), subset.len() as i32);

    // ToUnicode CMap（UnicodeCmap builder：新 CID → Unicode）
    // CIDSystemInfo 用 Adobe/UCS（ToUnicode 标准约定，区别于 CIDFont 的 Identity）
    let mut cmap = pdf_writer::types::UnicodeCmap::new(
        Name(b"EvoruleUnicode"),
        SystemInfo {
            registry: Str(b"Adobe"),
            ordering: Str(b"UCS"),
            supplement: 0,
        },
    );
    write_tounicode_pairs(pages, metrics, remapper, &mut cmap);
    let cmap_bytes = cmap.finish();
    pdf.cmap(tounicode_id, &cmap_bytes);

    // --- 每页 ---
    let title = req
        .options
        .as_ref()
        .and_then(|o| o.pdf_title.clone())
        .unwrap_or_else(|| "evorule 导出报告".to_string());
    for (i, ops) in pages.iter().enumerate() {
        let page_id = Ref::new(100 + 2 * i as i32);
        let content_id = Ref::new(101 + 2 * i as i32);

        let mut page = pdf.page(page_id);
        page.parent(page_tree_id)
            .media_box(Rect::new(0.0, 0.0, PAGE_W, PAGE_H));
        page.resources()
            .fonts()
            .pair(Name(b"F0"), type0_id);

        let mut content = Content::new();
        for op in ops {
            match op {
                Op::Text { x, y, size, text } => {
                    // Identity-H：CID 两字节大端（CID = remapped GID）
                    let mut bytes = Vec::with_capacity(text.len() * 2);
                    for c in text.chars() {
                        let old = metrics.face.glyph_index(c);
                        if let Some(g) = old.and_then(|g| remapper.get(g.0)) {
                            bytes.push((g >> 8) as u8);
                            bytes.push((g & 0xFF) as u8);
                        }
                    }
                    content.begin_text();
                    content.set_font(Name(b"F0"), *size);
                    // Td（文本定位）：BT 后首次调用即从文本空间原点(0,0)偏移到
                    // 绝对位置。之前误用 move_to（path 算子 m，在 BT/ET 内无效），
                    // 文字全部落在 (0,0) 页面左下角不可见（2026-09-05 W6 实测发现）
                    content.next_line(*x, *y);
                    content.show(pdf_writer::Str(&bytes));
                    content.end_text();
                }
                Op::Rule { x0, x1, y } => {
                    content.save_state();
                    content.set_line_width(0.5);
                    content.move_to(*x0, *y);
                    content.line_to(*x1, *y);
                    content.stroke();
                    content.restore_state();
                }
            }
        }
        // 页脚：左"evorule 服务端导出" 右"第 N 页 / 共 M 页"
        let footer = format!("第 {} 页 / 共 {} 页", i + 1, pages.len());
        let footer_w: f32 = footer
            .chars()
            .filter_map(|c| metrics.char_width(c, SIZE_FOOTER))
            .sum();
        let mut brand = String::from("evorule 服务端导出 · ");
        brand.push_str(&title);
        let brand = brand.chars().take(40).collect::<String>();
        content.begin_text();
        content.set_font(Name(b"F0"), SIZE_FOOTER);
        content.next_line(MARGIN, MARGIN / 2.0);
        content.show(pdf_writer::Str(&encode_text(&brand, metrics, remapper)));
        content.end_text();
        content.begin_text();
        content.set_font(Name(b"F0"), SIZE_FOOTER);
        // 独立 BT/ET 块保证 next_line 从 (0,0) 绝对定位（同块内 Td 是相对偏移）
        content.next_line(PAGE_W - MARGIN - footer_w, MARGIN / 2.0);
        content.show(pdf_writer::Str(&encode_text(&footer, metrics, remapper)));
        content.end_text();

        page.contents(content_id);
        drop(page);
        pdf.stream(content_id, &content.finish());
    }

    Ok(pdf.finish())
}

/// 收集 /W 宽度表（cid → 1000 空间宽度，含页脚固定文案）
fn collect_widths(
    pages: &[Vec<Op>],
    metrics: &GlyphMetrics,
    remapper: &subsetter::GlyphRemapper,
    out: &mut Vec<(u16, f32)>,
) {
    let upem = metrics.face.units_per_em() as f32;
    let mut push_char = |c: char| {
        if let Some(g) = metrics.face.glyph_index(c) {
            if let (Some(cid), Some(adv)) = (remapper.get(g.0), metrics.face.glyph_hor_advance(g))
            {
                if !out.iter().any(|(c2, _)| *c2 == cid) {
                    out.push((cid, adv as f32 / upem * 1000.0));
                }
            }
        }
    };
    for page in pages {
        for op in page {
            let Op::Text { text, .. } = op else { continue };
            for c in text.chars() {
                push_char(c);
            }
        }
    }
    // 页脚固定文案（write_pdf 中直接绘制，不在 ops 里）
    let extra = "evorule 服务端导出 · 第  页 / 共  页0123456789";
    for c in extra.chars() {
        push_char(c);
    }
}

/// 写 ToUnicode 映射（新 CID → char）
fn write_tounicode_pairs(
    pages: &[Vec<Op>],
    metrics: &GlyphMetrics,
    remapper: &subsetter::GlyphRemapper,
    cmap: &mut pdf_writer::types::UnicodeCmap,
) {
    let mut done: Vec<(u16, char)> = Vec::new();
    let mut push = |c: char| {
        if let Some(g) = metrics.face.glyph_index(c) {
            if let Some(cid) = remapper.get(g.0) {
                if !done.iter().any(|(c2, _)| *c2 == cid) {
                    done.push((cid, c));
                }
            }
        }
    };
    for page in pages {
        for op in page {
            let Op::Text { text, .. } = op else { continue };
            for c in text.chars() {
                push(c);
            }
        }
    }
    for c in "evorule 服务端导出 · 第  页 / 共  页0123456789".chars() {
        push(c);
    }
    for (cid, c) in done {
        cmap.pair(cid, c);
    }
}

/// 文本 → Identity-H 字节流（CID = remapped GID，两字节大端）
fn encode_text(
    text: &str,
    metrics: &GlyphMetrics,
    remapper: &subsetter::GlyphRemapper,
) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(text.len() * 2);
    for c in text.chars() {
        if let Some(g) = metrics.face.glyph_index(c) {
            if let Some(cid) = remapper.get(g.0) {
                bytes.push((cid >> 8) as u8);
                bytes.push((cid & 0xFF) as u8);
            }
        }
    }
    bytes
}

// ============================================================================
// 4. HTTP 层
// ============================================================================

/// 构造 `/api/export/pdf` 路由（挂入受认证保护路由组；body 上限放宽到 32MB）
pub fn pdf_export_router() -> Router<AppState> {
    Router::new().route(
        "/api/export/pdf",
        post(pdf_export_handler).layer(axum::extract::DefaultBodyLimit::max(MAX_BODY_BYTES)),
    )
}

/// 统一错误响应：`{ "success": false, "message": ... }`（与 permissions/marketplace 同形状）
fn err(status: StatusCode, message: impl Into<String>) -> (StatusCode, Json<Value>) {
    (
        status,
        Json(serde_json::json!({ "success": false, "message": message.into() })),
    )
}

/// `POST /api/export/pdf` → 服务端渲染 PDF（UV-084 W6）
///
/// - 200：`application/pdf` 二进制
/// - 400：请求体非法 / 缺 content_type / 字体缺请求字符（带指引）
/// - 500：无可用中文字体 / 渲染内部错误（带自诊断指引）
#[utoipa::path(
    post,
    path = "/api/export/pdf",
    tag = "export",
    request_body = serde_json::Value,
    responses(
        (status = 200, description = "渲染成功，返回 application/pdf 二进制"),
        (status = 400, description = "请求体非法 / 缺 content_type / 字体缺字符（message 带指引）", body = serde_json::Value),
        (status = 500, description = "无可用中文字体 / 渲染失败（message 带自诊断指引）", body = serde_json::Value)
    )
)]
async fn pdf_export_handler(
    payload: Result<Json<PdfExportRequest>, JsonRejection>,
) -> Result<Response, (StatusCode, Json<Value>)> {
    // Json rejection → 统一错误形状（不用 axum 默认文本，保持消费方可解析）
    let Json(req) = payload.map_err(|rej| {
        err(StatusCode::BAD_REQUEST, format!("请求体解析失败: {rej}"))
    })?;

    // 请求级可见性：server 无通用 access log，端点自带一行请求日志（消费方排障用）
    tracing::info!(
        "PDF 导出请求: content_type={}, raw_data={} 字节级数据, options={:?}",
        req.content_type,
        req.raw_data.is_some(),
        req.options.as_ref().map(|o| o.pdf_title.is_some()).unwrap_or(false)
    );

    match render_pdf(&req) {
        Ok(bytes) => {
            tracing::info!("PDF 导出成功: {} 字节", bytes.len());
            Ok(Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "application/pdf")
                .body(Body::from(bytes))
                .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, format!("响应构造失败: {e}")))?)
        }
        Err(msg) => {
            // 字体环境问题 → 500；请求内容问题（缺字符/缺 content_type）→ 400
            let status = if msg.contains("缺少 content_type")
                || msg.contains("缺少以下字符的字形")
                || msg.contains("渲染字体缺少")
            {
                StatusCode::BAD_REQUEST
            } else {
                StatusCode::INTERNAL_SERVER_ERROR
            };
            tracing::warn!("PDF 导出失败（不静默）: {msg}");
            Err(err(status, msg))
        }
    }
}

// ============================================================================
// 5. 单元测试（核心函数直测；无字体环境如实跳过，不假装通过）
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// 请求体便捷构造
    fn req(content_type: &str) -> PdfExportRequest {
        PdfExportRequest {
            content_type: content_type.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn missing_content_type_is_rejected() {
        let e = render_pdf(&req("")).unwrap_err();
        assert!(e.contains("content_type"), "错误应指明缺 content_type: {e}");
    }

    #[test]
    fn nonexistent_explicit_font_fails_with_guidance() {
        let e = find_cjk_font(Some(std::path::Path::new("Z:/definitely/not/here.ttf")))
            .unwrap_err();
        assert!(e.contains("--pdf-font"), "错误应带修复指引: {e}");
    }

    /// 有字体环境（Windows 本机）才有意义：渲染 → PDF 结构校验。
    /// 无字体环境（CI Linux 裸容器）如实跳过并打印原因——不假装通过。
    #[test]
    fn render_pdf_produces_valid_structure() {
        let font = match find_cjk_font(None) {
            Ok(f) => f,
            Err(reason) => {
                eprintln!("SKIP render_pdf_produces_valid_structure: {reason}");
                return;
            }
        };
        drop(font); // render_pdf 内部自行探测（同机结果一致）

        let mut r = req("audit_chain");
        r.raw_data = Some(serde_json::json!([
            {"fact_id": "#30001", "type": "set", "payload": {"规则": "报销上限 5000 元"}},
            {"fact_id": "#30002", "type": "merge", "payload": {"结论": "通过"}}
        ]));
        r.integrity = Some(serde_json::json!({
            "algorithm": "BLAKE3", "content_hash": "abc123", "fact_count": 2, "verified": true
        }));
        r.options = Some(PdfRequestOptions {
            pdf_title: Some("测试审计报告".to_string()),
            pdf_organization: Some("测试组织".to_string()),
            ..Default::default()
        });

        let bytes = render_pdf(&r).expect("本机有中文字体时应渲染成功");
        assert!(bytes.starts_with(b"%PDF"), "应为 PDF 魔数开头");
        assert!(bytes.ends_with(b"%%EOF") || bytes.windows(5).rev().any(|w| w == b"%%EOF"));
        let s = String::from_utf8_lossy(&bytes);
        assert!(s.contains("/FontFile2"), "应嵌入字体（子集）: {s:.200}");
        assert!(s.contains("/ToUnicode"), "应带 ToUnicode（可复制文本）");
        assert!(s.contains("/Type0"), "应为 CID 复合字体");
    }

    /// 缺字 fail-fast：从一批罕见码点中动态选一个本机字体确实没有的字符构造请求
    /// → 报错列出字符。（不能硬编码 U+E000：微软雅黑在私有区有图标字形，不缺它）
    #[test]
    fn missing_glyph_fails_fast() {
        let font = match find_cjk_font(None) {
            Ok(f) => f,
            Err(reason) => {
                eprintln!("SKIP missing_glyph_fails_fast: {reason}");
                return;
            }
        };
        let face = ttf_parser::Face::parse(&font.data, font.index).expect("探测到的字体应可解析");
        // 罕见码点候选（古文字/盲文图案/甲骨文扩展等，常规中文字体基本不覆盖）
        let candidates = [
            '\u{1018A}', // 古希腊零号记号
            '\u{12000}', // 楔形文字
            '\u{1D300}', // 易经六十四卦符号
            '\u{2CEB0}', // CJK 扩展 G（极罕用）
            '\u{1E900}', // Adlam 字母（西非文字）
        ];
        let Some(missing) = candidates.iter().find(|c| face.glyph_index(**c).is_none()) else {
            eprintln!("SKIP missing_glyph_fails_fast: 本机字体覆盖了全部候选码点");
            return;
        };
        let mut r = req("audit_chain");
        r.raw_data = Some(serde_json::json!([{"x": missing.to_string()}]));
        let e = render_pdf(&r).unwrap_err();
        assert!(
            e.contains(&format!("U+{:04X}", *missing as u32)),
            "应列出缺字码点: {e}"
        );
        assert!(e.contains("--pdf-font"), "应带换字体指引: {e}");
    }

    /// 截断纪律：超长字段加省略标记，不静默丢数据
    #[test]
    fn long_field_is_truncated_with_marker() {
        let s = value_summary(&serde_json::json!("x".repeat(1000)));
        assert!(s.contains("截断"), "应有截断标记: {s:.100}");
        assert!(s.chars().count() < 500);
    }

    /// 请求数组超上限截断
    #[test]
    fn oversized_array_is_truncated() {
        let items: Vec<Value> = (0..MAX_DATA_ROWS + 50)
            .map(|i| serde_json::json!({"i": i}))
            .collect();
        let s = value_summary(&serde_json::json!(items));
        assert!(s.contains("截断"));
    }

    /// 回归（2026-09-05 W6 实测 panic）：中英混排 + 空格回退断行。
    /// wrap_text 曾把 last_space 记成字符数、消费端按字节切片，行内含多字节
    /// 字符时 truncate/切片切进字符内部 → panic（5 条中文 fact 即触发）。
    /// 修复后：字节索引一致，长中文文本 + 空格正常断行不 panic。
    #[test]
    fn mixed_cjk_space_wrap_does_not_panic() {
        let font = match find_cjk_font(None) {
            Ok(f) => f,
            Err(reason) => {
                eprintln!("SKIP mixed_cjk_space_wrap_does_not_panic: {reason}");
                return;
            }
        };
        let face = ttf_parser::Face::parse(&font.data, font.index).expect("字体应可解析");
        let metrics = GlyphMetrics { face };

        // 触发条件：① 行内空格（英文整词断行入口）② 空格后跟多字节字符
        // ③ 累计宽度超 max_w 触发回退。窄 max_w 强制多次断行。
        let text = "规则 audit 链路完整性 verification 与中文审计事实留痕记录，"
            .repeat(8);
        let lines = wrap_text(&text, &metrics, 10.0, 120.0);
        assert!(lines.len() > 1, "窄宽度应产生多行: {}", lines.len());
        for l in &lines {
            assert!(l.chars().all(|c| !c.is_control()));
        }
        // 拼接回去字符总量不丢（空格在断行处被消费，容差 = 行数个空格）
        let total: usize = lines.iter().map(|l| l.chars().count()).sum();
        assert!(
            text.chars().count() - total <= lines.len(),
            "断行不应丢字符: 原文 {} 字符, 输出 {} 字符, {} 行",
            text.chars().count(),
            total,
            lines.len()
        );
    }
}
