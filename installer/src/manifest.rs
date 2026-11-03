//! Payload manifest parser — shared by the build script and the installer.
//!
//! `build.rs` mounts this module via `#[path]` to cross-check the payload
//! tree against `payload/manifest.json` at compile time; `main.rs` includes
//! it normally to re-verify the embedded manifest against the embedded
//! payload bytes at install time. Single implementation keeps the strict
//! shape rules identical on both sides: anything outside the controlled
//! manifest shape emitted by `prepare-payload.ps1` is rejected, never
//! silently accepted.

/// Parsed manifest: generation time, version stamps, per-file checksums.
pub struct Manifest {
    pub generated_at: String,
    pub versions: Vec<(String, String)>,
    /// (relative path with '/', lowercase sha256 hex)
    pub files: Vec<(String, String)>,
}

/// Strict parser for the controlled manifest shape emitted by
/// prepare-payload.ps1 (one file entry per line, no nested objects besides
/// `versions`). Anything outside the known shape is rejected — a hand-edited
/// manifest therefore fails instead of passing silently.
pub fn parse_manifest(text: &str) -> Result<Manifest, String> {
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

/// Lowercase hex encoding for SHA-256 digests (no dedicated crate needed).
pub fn to_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}
