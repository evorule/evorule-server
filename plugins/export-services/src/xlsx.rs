// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! xlsx 报表渲染器（确定性纯函数）。
//!
//! B0 spike 结论（2026-10-22 实测）：rust_xlsxwriter 缺省内嵌当前创建时间（docProps/core.xml
//! 的 dcterms:created/modified）→ 跨进程字节不一致；按官方 checksum 稳定例固定
//! [`FIXED_CREATION`] 后跨进程字节一致。本模块所有产物一律带固定文档属性——
//! 同输入逐字节同输出，非确定量（如业务时间）由调用方写入单元格内容。

use rust_xlsxwriter::{DocProperties, ExcelDateTime, Format, Workbook, XlsxError};

/// 固定文档创建时间（文档属性恒量；业务时间语义由单元格内容承载）
const FIXED_CREATION: (u16, u8, u8) = (2000, 1, 1);

/// 单元格值（TCB 值域对齐：字符串/整数——与 JsonValue 六变体无浮点一致；
/// 浮点需求走 numeric 服务的字符串出参，不由渲染器造数）
pub enum Cell {
    Text(String),
    Int(i64),
}

/// 一个数据表：表名 + 表头 + 数据行
pub struct SheetSpec {
    pub name: String,
    pub header: Vec<String>,
    pub rows: Vec<Vec<Cell>>,
}

/// 渲染 xlsx：sheets → 字节流（确定性纯函数；失败带指引）。
///
/// - 首表前插「溯源」表：每行 = claim/service/args(压缩 JSON)/digest（可复算清单入口）；
/// - 表头加粗；表名消毒由 rust_xlsxwriter 校验（非法名报错不静默）。
pub fn render_xlsx(
    title: &str,
    sheets: &[SheetSpec],
    provenance: &[(String, String, String, String)],
) -> Result<Vec<u8>, String> {
    let mut wb = Workbook::new();

    // 溯源表（恒为首表）：可复算清单——数字出自哪个服务、什么参数、信封哈希
    if !provenance.is_empty() {
        let ws = wb.add_worksheet();
        ws.set_name("溯源").map_err(|e| format!("表名非法: {e}"))?;
        let bold = Format::new().set_bold();
        for (c, h) in ["断言", "服务", "入参", "信封摘要"].iter().enumerate() {
            ws.write_with_format(0, c as u16, *h, &bold)
                .map_err(err_of)?;
        }
        for (r, (claim, service, args, digest)) in provenance.iter().enumerate() {
            let row = (r + 1) as u32;
            ws.write(row, 0, claim.as_str()).map_err(err_of)?;
            ws.write(row, 1, service.as_str()).map_err(err_of)?;
            ws.write(row, 2, args.as_str()).map_err(err_of)?;
            ws.write(row, 3, digest.as_str()).map_err(err_of)?;
        }
    }

    for (idx, spec) in sheets.iter().enumerate() {
        let ws = wb
            .add_worksheet()
            .set_name(spec.name.as_str())
            .map_err(|e| format!("第 {} 表表名非法 {:?}: {e}", idx + 1, spec.name))?;
        let bold = Format::new().set_bold();
        for (c, h) in spec.header.iter().enumerate() {
            ws.write_with_format(0, c as u16, h.as_str(), &bold)
                .map_err(err_of)?;
        }
        for (r, row) in spec.rows.iter().enumerate() {
            let row_no = (r + 1) as u32;
            for (c, cell) in row.iter().enumerate() {
                match cell {
                    Cell::Text(t) => {
                        ws.write(row_no, c as u16, t.as_str()).map_err(err_of)?;
                    }
                    Cell::Int(i) => {
                        ws.write(row_no, c as u16, *i).map_err(err_of)?;
                    }
                }
            }
        }
    }

    // 确定性旋钮（B0）：固定文档创建时间，否则跨进程字节漂移；标题入文档属性
    let (y, m, d) = FIXED_CREATION;
    let date = ExcelDateTime::from_ymd(y, m, d).map_err(|e| format!("固定文档时间非法: {e}"))?;
    let props = DocProperties::new()
        .set_title(title)
        .set_creation_datetime(&date);
    wb.set_properties(&props);

    let bytes = wb
        .save_to_buffer()
        .map_err(|e| format!("xlsx 序列化失败: {e}"))?;
    Ok(bytes)
}

fn err_of(e: XlsxError) -> String {
    format!("xlsx 写入失败: {e}")
}
