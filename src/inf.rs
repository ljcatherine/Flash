//! Reads driver packages (.inf files) so Flash can offer drivers from the local
//! Driver Store or a folder the user picks, without going online.
use std::cmp::Ordering;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq)]
pub struct InfDriver {
    pub path: PathBuf,
    pub date: String, // yyyy-mm-dd
    pub version: String,
    pub provider: String,
    pub ids: Vec<String>, // hardware IDs the package installs on
}

/// INF files are often UTF-16LE with a BOM; otherwise treat as UTF-8/ANSI.
pub fn decode(bytes: &[u8]) -> String {
    if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        let units: Vec<u16> = rest.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        String::from_utf16_lossy(&units)
    } else {
        String::from_utf8_lossy(bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes)).into_owned()
    }
}

/// `7/7/2026` or `07/07/2026` -> `2026-07-07`. Returns "" when it isn't a date.
pub fn mdy_to_iso(s: &str) -> String {
    let p: Vec<&str> = s.trim().split('/').collect();
    match (p.first().and_then(|x| x.parse::<u32>().ok()), p.get(1).and_then(|x| x.parse::<u32>().ok()), p.get(2).and_then(|x| x.parse::<u32>().ok())) {
        (Some(m), Some(d), Some(y)) if p.len() == 3 => format!("{y:04}-{m:02}-{d:02}"),
        _ => String::new(),
    }
}

pub fn version_parts(v: &str) -> Vec<u64> {
    v.trim().split('.').map(|x| x.trim().parse().unwrap_or(0)).collect()
}

/// Orders two drivers the way Windows ranks equally-matching drivers: newer date first, then higher version.
pub fn cmp_driver(date_a: &str, ver_a: &str, date_b: &str, ver_b: &str) -> Ordering {
    date_a.cmp(date_b).then_with(|| version_parts(ver_a).cmp(&version_parts(ver_b)))
}

fn strip_comment(line: &str) -> &str {
    let mut quoted = false;
    for (i, c) in line.char_indices() {
        match c {
            '"' => quoted = !quoted,
            ';' if !quoted => return &line[..i],
            _ => {}
        }
    }
    line
}

/// Parses an INF's text. Returns None when it has no usable DriverVer.
pub fn parse(text: &str) -> Option<(String, String, String, Vec<String>)> {
    let mut sections: HashMap<String, Vec<&str>> = HashMap::new();
    let mut cur = String::new();
    for raw in text.lines() {
        let line = strip_comment(raw).trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') {
            if let Some(end) = line.find(']') {
                cur = line[1..end].trim().to_lowercase();
            }
            continue;
        }
        sections.entry(cur.clone()).or_default().push(line);
    }

    let strings: HashMap<String, String> = sections
        .get("strings")
        .map(|ls| {
            ls.iter()
                .filter_map(|l| l.split_once('='))
                .map(|(k, v)| (k.trim().to_lowercase(), v.trim().trim_matches('"').to_string()))
                .collect()
        })
        .unwrap_or_default();
    // Resolves `%Key%` tokens from [Strings].
    let sub = |s: &str| -> String {
        let s = s.trim();
        if s.len() > 2 && s.starts_with('%') && s.ends_with('%') {
            strings.get(&s[1..s.len() - 1].to_lowercase()).cloned().unwrap_or_else(|| s.to_string())
        } else {
            s.trim_matches('"').to_string()
        }
    };
    let value = |section: &str, key: &str| -> Option<String> {
        sections.get(section)?.iter().find_map(|l| {
            let (k, v) = l.split_once('=')?;
            k.trim().eq_ignore_ascii_case(key).then(|| v.trim().to_string())
        })
    };

    let dv = value("version", "driverver")?;
    let (date, version) = dv.split_once(',').map(|(a, b)| (sub(a), sub(b)))?;
    let date = mdy_to_iso(&date);
    if date.is_empty() {
        return None;
    }
    let provider = value("version", "provider").map(|p| sub(&p)).unwrap_or_default();

    let mut ids = Vec::new();
    for line in sections.get("manufacturer").into_iter().flatten() {
        let rhs = line.split_once('=').map_or(*line, |(_, r)| r);
        let mut parts = rhs.split(',').map(|p| p.trim().to_lowercase());
        let Some(models) = parts.next() else { continue };
        let mut names = vec![models.clone()];
        // Only this PC's architecture (x64) and undecorated sections apply.
        names.extend(parts.filter(|dec| !dec.contains("x86") && !dec.contains("arm") && !dec.contains("ia64")).map(|dec| format!("{models}.{dec}")));
        for name in names {
            for l in sections.get(&name).into_iter().flatten() {
                if let Some((_, r)) = l.split_once('=') {
                    ids.extend(r.split(',').skip(1).map(|id| id.trim().trim_matches('"').to_string()).filter(|id| !id.is_empty()));
                }
            }
        }
    }
    Some((date, version, provider, ids))
}

/// Finds and parses every .inf under `dir` (a few levels deep).
pub fn scan_dir(dir: &Path) -> Vec<InfDriver> {
    fn walk(dir: &Path, depth: u32, out: &mut Vec<InfDriver>) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for e in entries.flatten() {
            let path = e.path();
            if path.is_dir() {
                if depth < 4 {
                    walk(&path, depth + 1, out);
                }
            } else if path.extension().is_some_and(|x| x.eq_ignore_ascii_case("inf")) {
                let Ok(bytes) = std::fs::read(&path) else { continue };
                if let Some((date, version, provider, ids)) = parse(&decode(&bytes)) {
                    out.push(InfDriver { path, date, version, provider, ids });
                }
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, 0, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
[Version]
Signature = "$WINDOWS NT$"
Provider  = %Vendor%   ; who ships it
DriverVer = 07/07/2026,2.0.0.29

[Manufacturer]
%Vendor% = Models, NTamd64.10.0, NTarm64

[Models.NTamd64.10.0]
%Dev1% = Install, PCI\VEN_1022&DEV_790B, PCI\VEN_1022&DEV_790B&REV_61

[Models.NTarm64]
%Dev1% = Install, PCI\VEN_9999&DEV_0001

[Strings]
Vendor = "Advanced Micro Devices"
Dev1 = "SMBus"
"#;

    #[test]
    fn parses_inf() {
        let (date, ver, provider, ids) = parse(SAMPLE).unwrap();
        assert_eq!((date.as_str(), ver.as_str(), provider.as_str()), ("2026-07-07", "2.0.0.29", "Advanced Micro Devices"));
        assert_eq!(ids, vec!["PCI\\VEN_1022&DEV_790B", "PCI\\VEN_1022&DEV_790B&REV_61"]); // arm64 section skipped
        assert!(parse("[Version]\nSignature=x\n").is_none());
    }

    #[test]
    fn decodes_utf16_and_compares() {
        let utf16: Vec<u8> = [0xFF, 0xFE].into_iter().chain("[Version]".encode_utf16().flat_map(|u| u.to_le_bytes())).collect();
        assert_eq!(decode(&utf16), "[Version]");
        assert_eq!(mdy_to_iso("7/7/2026"), "2026-07-07");
        assert_eq!(mdy_to_iso("nope"), "");
        assert_eq!(cmp_driver("2026-01-01", "1.0", "2025-12-31", "9.0"), Ordering::Greater);
        assert_eq!(cmp_driver("2026-01-01", "1.10.0", "2026-01-01", "1.9.0"), Ordering::Greater);
        assert_eq!(cmp_driver("2026-01-01", "1.0", "2026-01-01", "1.0"), Ordering::Equal);
    }
}
