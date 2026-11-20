//! Microsoft Update Catalog: a public index of every driver Microsoft distributes,
//! including ones Windows Update doesn't offer this PC automatically.
use crate::inf::mdy_to_iso;
use std::path::Path;
use std::time::Duration;

const SEARCH: &str = "https://www.catalog.update.microsoft.com/Search.aspx?q=";
const DOWNLOAD: &str = "https://www.catalog.update.microsoft.com/DownloadDialog.aspx";

#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub guid: String,
    pub title: String,
    pub products: String,
    pub date: String, // yyyy-mm-dd
    pub version: String,
    pub size: String,
}

/// The catalog search term for a device: its bus ID without SUBSYS/REV, e.g.
/// `PCI\VEN_1022&DEV_790B`. Only real hardware buses with vendor IDs are searched.
pub fn query_id(hwids: &[String]) -> Option<String> {
    let up = hwids.first()?.to_ascii_uppercase();
    let bus_ok = ["PCI\\", "USB\\", "HDAUDIO\\"].iter().any(|b| up.starts_with(b));
    if !bus_ok || !(up.contains("VEN_") || up.contains("VID_")) {
        return None;
    }
    let cut = ["&SUBSYS_", "&REV_"].iter().filter_map(|m| up.find(m)).min().unwrap_or(up.len());
    Some(up[..cut].to_string())
}

pub fn is_guid(s: &str) -> bool {
    s.len() == 36 && s.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
}

fn url_encode(s: &str) -> String {
    s.bytes()
        .map(|b| if b.is_ascii_alphanumeric() || b"-_.".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") })
        .collect()
}

/// Strips tags, decodes the few entities the catalog uses and collapses whitespace.
fn clean(html: &str) -> String {
    let mut text = String::new();
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' => {
                in_tag = false;
                text.push(' ');
            }
            _ if !in_tag => text.push(c),
            _ => {}
        }
    }
    let text = text.replace("&amp;", "&").replace("&nbsp;", " ").replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&#39;", "'");
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Parses a catalog search results page.
pub fn parse(html: &str) -> Vec<Entry> {
    let mut out = Vec::new();
    for chunk in html.split("<tr id=\"").skip(1) {
        let Some(guid) = chunk.find("_R").map(|e| &chunk[..e]) else { continue };
        if !is_guid(guid) {
            continue;
        }
        let cell = |n: u32| -> String {
            let key = format!("id=\"{guid}_C{n}_R");
            chunk
                .find(&key)
                .and_then(|i| {
                    let rest = &chunk[i..];
                    let start = rest.find('>')? + 1;
                    let end = rest.find("</td>")?;
                    Some(clean(&rest[start..end]))
                })
                .unwrap_or_default()
        };
        out.push(Entry {
            guid: guid.to_string(),
            title: cell(1),
            products: cell(2),
            date: mdy_to_iso(&cell(4)),
            version: cell(5),
            size: cell(6).split_whitespace().take(2).collect::<Vec<_>>().join(" "),
        });
    }
    out
}

/// True when a catalog entry is for this Windows version and an x64 PC.
pub fn fits_os(e: &Entry, win11: bool) -> bool {
    let p = e.products.to_lowercase();
    let t = e.title.to_lowercase();
    if p.contains("arm64") || t.contains("arm64") {
        return false;
    }
    if win11 { p.contains("windows 11") || p.contains("and later") } else { p.contains("windows 10") }
}

pub fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new().timeout(Duration::from_secs(30)).build()
}

pub fn search(agent: &ureq::Agent, query: &str) -> Result<Vec<Entry>, String> {
    let html = agent.get(&format!("{SEARCH}{}", url_encode(query))).call().map_err(|e| e.to_string())?.into_string().map_err(|e| e.to_string())?;
    Ok(parse(&html))
}

/// Download links for a catalog update. Only Microsoft download hosts are accepted.
pub fn download_urls(agent: &ureq::Agent, guid: &str) -> Result<Vec<String>, String> {
    if !is_guid(guid) {
        return Err("invalid update id".into());
    }
    let body = format!("updateIDs=[{{\"size\":0,\"languages\":\"\",\"uidInfo\":\"{guid}\",\"updateID\":\"{guid}\"}}]");
    let page = agent
        .post(DOWNLOAD)
        .set("Content-Type", "application/x-www-form-urlencoded")
        .send_string(&body)
        .map_err(|e| e.to_string())?
        .into_string()
        .map_err(|e| e.to_string())?;
    let urls: Vec<String> = page.split(".url = '").skip(1).filter_map(|p| p.split('\'').next()).filter(|u| trusted(u)).map(String::from).collect();
    if urls.is_empty() { Err("no download link in catalog response".into()) } else { Ok(urls) }
}

fn trusted(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("https://").or_else(|| url.strip_prefix("http://")) else { return false };
    let host = rest.split('/').next().unwrap_or("").to_ascii_lowercase();
    host.ends_with(".windowsupdate.com") || host.ends_with(".microsoft.com")
}

/// Downloads a catalog driver into `dir` and unpacks it. Returns the folder holding the INFs.
pub fn fetch(agent: &ureq::Agent, guid: &str, dir: &Path) -> Result<std::path::PathBuf, String> {
    let out = dir.join(guid);
    std::fs::create_dir_all(&out).map_err(|e| e.to_string())?;
    for (i, url) in download_urls(agent, guid)?.iter().enumerate() {
        let cab = dir.join(format!("{guid}-{i}.cab"));
        let mut reader = agent.get(url).call().map_err(|e| e.to_string())?.into_reader();
        let mut file = std::fs::File::create(&cab).map_err(|e| e.to_string())?;
        std::io::copy(&mut reader, &mut file).map_err(|e| e.to_string())?;
        drop(file);
        crate::run_hidden("expand.exe", &["-F:*", &cab.to_string_lossy(), &out.to_string_lossy()]);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROW: &str = r##"<tr id="headerRow"><td>x</td></tr><tr id="b11318c2-6580-4502-931c-491427baf4df_R1" style="">
<td id="b11318c2-6580-4502-931c-491427baf4df_C1_R1">
  <a href="#">  Advanced Micro Devices, Inc System Driver Update (2.0.0.29) </a>
</td><td id="b11318c2-6580-4502-931c-491427baf4df_C2_R1">Windows 11 Client,  version 24H2 and later, Upgrade &amp; Servicing Drivers</td>
<td id="b11318c2-6580-4502-931c-491427baf4df_C3_R1">Drivers (Other Hardware)</td>
<td class="x " id="b11318c2-6580-4502-931c-491427baf4df_C4_R1"> 7/7/2026 </td>
<td id="b11318c2-6580-4502-931c-491427baf4df_C5_R1">2.0.0.29</td>
<td id="b11318c2-6580-4502-931c-491427baf4df_C6_R1"><span>17 KB</span> <span class="noDisplay">18194</span></td></tr>"##;

    #[test]
    fn parses_results() {
        let e = parse(ROW);
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].title, "Advanced Micro Devices, Inc System Driver Update (2.0.0.29)");
        assert_eq!((e[0].date.as_str(), e[0].version.as_str(), e[0].size.as_str()), ("2026-07-07", "2.0.0.29", "17 KB"));
        assert!(e[0].products.contains("Upgrade & Servicing"));
        assert!(fits_os(&e[0], true) && !fits_os(&e[0], false));
    }

    #[test]
    fn queries_and_hosts() {
        let ids = |s: &str| vec![s.to_string()];
        assert_eq!(query_id(&ids("PCI\\VEN_1022&DEV_790B&SUBSYS_88771043&REV_61")).as_deref(), Some("PCI\\VEN_1022&DEV_790B"));
        assert_eq!(query_id(&ids("USB\\VID_046D&PID_C52B&REV_1211&MI_00")).as_deref(), Some("USB\\VID_046D&PID_C52B"));
        assert_eq!(query_id(&ids("USB\\ROOT_HUB30")), None);
        assert_eq!(query_id(&ids("ACPI\\PNP0A08")), None);
        assert_eq!(url_encode("PCI\\VEN_1&DEV_2"), "PCI%5CVEN_1%26DEV_2");
        assert!(trusted("https://catalog.s.download.windowsupdate.com/d/x.cab"));
        assert!(!trusted("https://evil.example.com/windowsupdate.com/x.cab"));
        assert!(!trusted("file:///C:/x.cab"));
    }
}
