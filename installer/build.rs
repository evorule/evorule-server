//! 将 `payload/` 目录整树内嵌进安装器可执行文件。
//!
//! 由发布流程（或 installer/prepare-payload.ps1）预先把分发包内容
//! 填入 payload/，本脚本递归扫描后生成 `PAYLOAD` 静态表（include_bytes!），
//! 因此最终产物是**单文件安装器**，双击即可安装，无需随附任何目录。
//!
//! 防御性排除：`data/`、`logs/`、`*.log`（用户数据/日志绝不入包）。
//!
//! Payload manifest gate: `prepare-payload.ps1` writes `payload/manifest.json`
//! (relative path + SHA-256 per file, generation time, version stamps and
//! source repo HEADs). This build script requires the manifest and
//! cross-checks it both ways against the actual payload tree, including a
//! full content-hash pass, so a stale, cross-batch or hand-modified payload
//! can never be embedded silently — every mismatch fails the build with
//! guidance to re-run prepare-payload.ps1.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

#[path = "src/sha256.rs"]
mod sha256;

const MANIFEST_NAME: &str = "manifest.json";

fn main() {
    println!("cargo:rerun-if-changed=payload");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src/sha256.rs");

    let manifest_dir = match std::env::var("CARGO_MANIFEST_DIR") {
        Ok(v) => PathBuf::from(v),
        Err(e) => {
            emit_compile_error(&format!("CARGO_MANIFEST_DIR unavailable: {e}"));
            return;
        }
    };
    let payload_dir = manifest_dir.join("payload");
    if !payload_dir.is_dir() {
        emit_compile_error(&format!(
            "payload/ 目录不存在：{}。请先运行 installer/prepare-payload.ps1 填充分发包内容。",
            payload_dir.display()
        ));
        return;
    }

    let manifest_path = payload_dir.join(MANIFEST_NAME);
    let manifest = match load_and_verify(&manifest_path) {
        Ok(m) => m,
        Err(e) => {
            emit_compile_error(&format!(
                "{e}\npayload 与 manifest 必须同批：请重跑 installer/prepare-payload.ps1，勿手工修改 payload/manifest.json。"
            ));
            return;
        }
    };

    let mut files: Vec<(String, PathBuf)> = Vec::new();
    if let Err(e) = walk(&payload_dir, &payload_dir, &mut files) {
        emit_compile_error(&format!("扫描 payload/ 失败: {e}"));
        return;
    }
    files.sort_by(|a, b| a.0.cmp(&b.0));

    if let Err(e) = cross_check(&files, &manifest) {
        emit_compile_error(&format!(
            "payload 与 manifest 对拍不齐（{e}）。\npayload 已被改动或非同批产物：请重跑 installer/prepare-payload.ps1。"
        ));
        return;
    }

    emit_payload_rs(&files, &manifest, &manifest_path);
}

/// Load + structurally validate the manifest (presence, shape, checksums).
fn load_and_verify(manifest_path: &Path) -> Result<Manifest, String> {
    if !manifest_path.is_file() {
        return Err(format!(
            "payload manifest 缺失：{}。请先运行 installer/prepare-payload.ps1 生成 manifest.json。",
            manifest_path.display()
        ));
    }
    let bytes = fs::read(manifest_path)
        .map_err(|e| format!("payload manifest 不可读 {}: {e}", manifest_path.display()))?;
    let bytes = bytes
        .strip_prefix(&[0xEF, 0xBB, 0xBF][..])
        .unwrap_or(&bytes);
    let text = String::from_utf8(bytes.to_vec()).map_err(|_| {
        format!(
            "payload manifest 不是合法 UTF-8: {}",
            manifest_path.display()
        )
    })?;
    let manifest = parse_manifest(&text)?;

    // The manifest must never ship as an installed file: it is metadata.
    if manifest.files.iter().any(|(rel, _)| rel == MANIFEST_NAME) {
        return Err("manifest 把自身列为载荷文件".to_string());
    }
    Ok(manifest)
}

struct Manifest {
    generated_at: String,
    versions: Vec<(String, String)>,
    files: Vec<(String, String)>, // (relative path with '/', lowercase sha256 hex)
}

/// Strict parser for the controlled manifest shape emitted by
/// prepare-payload.ps1 (one file entry per line, no nested objects besides
/// `versions`). Anything outside the known shape is rejected — a hand-edited
/// manifest therefore fails the build instead of passing silently.
fn parse_manifest(text: &str) -> Result<Manifest, String> {
    let mut manifest = Manifest {
        generated_at: String::new(),
        versions: Vec::new(),
        files: Vec::new(),
    };
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty()
            || line == "{"
            || line == "}"
            || line == "},"
            || line == "["
            || line == "]"
            || line == "],"
        {
            continue;
        }
        if let Some(rest) = line.strip_prefix("\"generated_at\":") {
            manifest.generated_at = json_string_value(rest)?;
            continue;
        }
        if line.starts_with("\"versions\":") || line.starts_with("\"files\":") {
            continue;
        }
        if line.contains("\"path\":") && line.contains("\"sha256\":") {
            let path = extract_string_field(line, "\"path\": \"")?;
            let sha = extract_string_field(line, "\"sha256\": \"")?;
            if sha.len() != 64 || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(format!("manifest 文件条目 sha256 非 64 位十六进制: {path}"));
            }
            manifest
                .files
                .push((path.to_string(), sha.to_ascii_lowercase()));
            continue;
        }
        if let Some((k, v)) = parse_kv_line(line) {
            manifest.versions.push((k, v));
            continue;
        }
        return Err(format!("manifest 含未知行形态: {line}"));
    }
    if manifest.generated_at.is_empty() {
        return Err("manifest 缺 generated_at".to_string());
    }
    if manifest.versions.is_empty() {
        return Err("manifest 缺 versions 段".to_string());
    }
    if manifest.files.is_empty() {
        return Err("manifest files 段为空".to_string());
    }
    Ok(manifest)
}

fn json_string_value(s: &str) -> Result<String, String> {
    let s = s.trim().trim_end_matches(',');
    if s.len() < 2 || !s.starts_with('"') || !s.ends_with('"') {
        return Err(format!("非 JSON 字符串值: {s}"));
    }
    Ok(s[1..s.len() - 1].to_string())
}

fn parse_kv_line(line: &str) -> Option<(String, String)> {
    // trailing comma optional: the last entry of the versions block has none
    let line = line.trim_end_matches(',');
    let (k, v) = line.split_once("\": \"")?;
    let k = k.trim().strip_prefix('"')?.to_string();
    let v = v.strip_suffix('"')?.to_string();
    if k.is_empty() || v.is_empty() {
        return None;
    }
    Some((k, v))
}

fn extract_string_field<'a>(line: &'a str, marker: &str) -> Result<&'a str, String> {
    let start = line
        .find(marker)
        .ok_or_else(|| format!("manifest 缺字段 {marker}"))?
        + marker.len();
    let rest = &line[start..];
    let end = rest
        .find('"')
        .ok_or_else(|| format!("manifest 字符串未闭合: {marker}"))?;
    Ok(&rest[..end])
}

/// Bidirectional set comparison plus a full content-hash pass.
fn cross_check(files: &[(String, PathBuf)], manifest: &Manifest) -> Result<(), String> {
    let mut tree: BTreeMap<&str, &PathBuf> = BTreeMap::new();
    for (rel, abs) in files {
        tree.insert(rel.as_str(), abs);
    }
    let mut listed: BTreeMap<&str, &str> = BTreeMap::new();
    for (rel, sha) in &manifest.files {
        listed.insert(rel.as_str(), sha);
    }

    for rel in tree.keys() {
        if !listed.contains_key(rel) {
            return Err(format!("载荷内文件不在 manifest 中: {rel}"));
        }
    }
    for rel in listed.keys() {
        if !tree.contains_key(rel) {
            return Err(format!("manifest 条目在载荷中无对应文件: {rel}"));
        }
    }
    for (rel, sha) in &manifest.files {
        let abs = tree[rel.as_str()];
        let bytes = fs::read(abs).map_err(|e| format!("读取失败 {}: {e}", abs.display()))?;
        let actual = sha256::sha256_hex(&bytes);
        if actual != *sha {
            return Err(format!("内容与 manifest 不符: {rel}"));
        }
    }
    Ok(())
}

fn emit_payload_rs(files: &[(String, PathBuf)], manifest: &Manifest, manifest_path: &Path) {
    let mut out = String::new();
    out.push_str("// @generated by build.rs — do not edit\n");
    out.push_str("#[rustfmt::skip]\n");
    out.push_str("pub static PAYLOAD: &[(&str, &[u8])] = &[\n");
    for (rel, abs) in files {
        let rel_esc = escape_literal(rel);
        let abs_str = escape_literal(&abs.to_string_lossy().replace('\\', "/"));
        out.push_str(&format!(
            "    (\"{rel_esc}\", include_bytes!(\"{abs_str}\")),\n"
        ));
    }
    out.push_str("];\n");
    // The manifest text ships inside every distributed installer binary as its
    // own provenance receipt (recoverable for after-sales verification); the
    // runtime version stamps below are relayed from the same manifest.
    let manifest_ref = escape_literal(&manifest_path.to_string_lossy().replace('\\', "/"));
    out.push_str(
        "#[allow(dead_code)] // provenance receipt, intentionally embedded for forensics\n",
    );
    out.push_str(&format!(
        "pub static PAYLOAD_MANIFEST: &str = include_str!(\"{manifest_ref}\");\n"
    ));
    out.push_str(&format!(
        "pub static PAYLOAD_GENERATED_AT: &str = \"{}\";\n",
        escape_literal(&manifest.generated_at)
    ));
    out.push_str("#[rustfmt::skip]\n");
    out.push_str("pub static PAYLOAD_VERSIONS: &[(&str, &str)] = &[\n");
    for (k, v) in &manifest.versions {
        out.push_str(&format!(
            "    (\"{}\", \"{}\"),\n",
            escape_literal(k),
            escape_literal(v)
        ));
    }
    out.push_str("];\n");

    let out_path = PathBuf::from(std::env::var("OUT_DIR").unwrap_or_default()).join("payload.rs");
    if let Err(e) = fs::write(&out_path, out) {
        // 写失败时 include!(payload.rs) 会在主编译期报错，无需在此 panic。
        println!("cargo:warning=evorule-setup build.rs: 写 payload.rs 失败: {e}");
    }
}

/// Rust string-literal escaping for generated `include_bytes!`/`include_str!`
/// paths and values (robustness: Windows filenames cannot carry quotes, but
/// the generator must never emit a malformed literal regardless).
fn escape_literal(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

fn walk(root: &Path, dir: &Path, files: &mut Vec<(String, PathBuf)>) -> Result<(), String> {
    let entries = fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("{}: {e}", dir.display()))?;
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().trim().to_string();
        if path.is_dir() {
            // 防御性排除：用户数据与日志不入包
            if name.eq_ignore_ascii_case("data") || name.eq_ignore_ascii_case("logs") {
                continue;
            }
            walk(root, &path, files)?;
        } else {
            if name.to_ascii_lowercase().ends_with(".log") {
                continue;
            }
            let rel = path
                .strip_prefix(root)
                .map_err(|e| format!("strip_prefix {}: {e}", path.display()))?
                .to_string_lossy()
                .replace('\\', "/");
            if rel == MANIFEST_NAME {
                // manifest 是元数据，非安装内容：单独 include_str! 内嵌。
                continue;
            }
            files.push((rel, path));
        }
    }
    Ok(())
}

fn emit_compile_error(msg: &str) {
    // The message (which may embed Windows paths with backslashes) must be a
    // valid Rust string literal inside the generated compile_error! — escape
    // it exactly like the include_bytes!/include_str! paths.
    let lit = escape_literal(msg);
    let mut out = String::from("// @generated by build.rs — do not edit\n");
    // Stub statics keep main.rs name resolution clean so the only reported
    // error is ours (the build still fails fail-fast on the compile_error!).
    out.push_str("pub static PAYLOAD: &[(&str, &[u8])] = &[];\n");
    out.push_str("#[allow(dead_code)]\npub static PAYLOAD_MANIFEST: &str = \"\";\n");
    out.push_str("pub static PAYLOAD_GENERATED_AT: &str = \"\";\n");
    out.push_str("pub static PAYLOAD_VERSIONS: &[(&str, &str)] = &[];\n");
    out.push_str(&format!("compile_error!(\"{lit}\");\n"));
    let out_path = PathBuf::from(std::env::var("OUT_DIR").unwrap_or_default()).join("payload.rs");
    if let Err(e) = fs::write(&out_path, out) {
        println!("cargo:warning=evorule-setup build.rs: 写 payload.rs 失败: {e}");
    }
}
