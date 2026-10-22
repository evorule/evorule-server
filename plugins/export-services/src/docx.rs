// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! docx 文档渲染器（确定性纯函数）。
//!
//! B0 spike 结论（2026-10-22 实测）：docx-rs 0.4 缺省不内嵌创建时间（core.xml 无
//! dcterms:created），跨进程字节一致——零旋钮即确定性。同输入逐字节同输出；
//! 非确定量（业务时间）由调用方写入正文内容。

use docx_rs::{Docx, Paragraph, Run, Table, TableCell, TableRow};

/// 段落文本（复用渲染器值域：字符串；数字以字符串入文——出自服务信封出参）
pub struct DocxSpec {
    pub title: String,
    /// 正文段落（标题后依序渲染）
    pub paragraphs: Vec<String>,
    /// 表格：表头 + 数据行（全部字符串单元格）
    pub tables: Vec<TableSpec>,
}

pub struct TableSpec {
    pub header: Vec<String>,
    pub rows: Vec<Vec<String>>,
}

/// 渲染 docx → 字节流（确定性纯函数；失败带指引）。
pub fn render_docx(spec: &DocxSpec) -> Result<Vec<u8>, String> {
    let mut doc = Docx::new().add_paragraph(
        Paragraph::new().add_run(Run::new().add_text(spec.title.as_str()).bold()),
    );
    for p in &spec.paragraphs {
        doc = doc.add_paragraph(Paragraph::new().add_run(Run::new().add_text(p.as_str())));
    }
    for t in &spec.tables {
        let header = TableRow::new(
            t.header
                .iter()
                .map(|h| cell_of(h))
                .collect::<Vec<TableCell>>(),
        );
        let mut rows = vec![header];
        for r in &t.rows {
            rows.push(TableRow::new(
                r.iter().map(|c| cell_of(c)).collect::<Vec<TableCell>>(),
            ));
        }
        doc = doc.add_table(Table::new(rows));
    }

    // save_to_buffer 式出口：pack 直写内存包
    let mut buf: Vec<u8> = Vec::new();
    doc.build()
        .pack(&mut std::io::Cursor::new(&mut buf))
        .map_err(|e| format!("docx 序列化失败: {e}"))?;
    Ok(buf)
}

fn cell_of(text: &str) -> TableCell {
    TableCell::new().add_paragraph(Paragraph::new().add_run(Run::new().add_text(text)))
}
