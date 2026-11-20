//! WHEA (Windows Hardware Error Architecture) events: reads them from the System log,
//! groups repeats, and explains each one from a built-in knowledge base.
//!
//! Every WHEA-Logger event describes itself with the same few documented fields:
//! the component (CPU core, memory, a PCI Express port...), the error source (machine
//! check, PCIe Advanced Error Reporting, NMI...), and for CPUs the machine-check error
//! type (cache, TLB, bus/interconnect, internal...). The knowledge base covers every
//! value of those fields, with causes and fixes tuned for AMD or Intel.
use crate::{ps, rows};
use std::collections::BTreeSet;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Event {
    pub time: String, // yyyy-mm-dd hh:mm
    pub id: u32,
    pub fatal: bool,
    pub fields: Vec<(String, String)>, // lower-case key, value
}

impl Event {
    pub fn get(&self, key: &str) -> &str {
        self.fields.iter().find(|(k, _)| k == key).map_or("", |(_, v)| v.as_str())
    }
}

/// Splits a WHEA message into fatal/corrected and its "Key: Value" lines.
pub fn parse_message(msg: &str) -> (bool, Vec<(String, String)>) {
    let mut lines = msg.lines().map(str::trim).filter(|l| !l.is_empty());
    let first = lines.next().unwrap_or("").to_lowercase();
    let fatal = first.contains("fatal") || first.contains("uncorrect");
    let fields = lines
        .filter_map(|l| {
            // "Primary Bus:Device:Function: 0x0:0x1:0x2" has colons in key and value; ": " splits them.
            let (k, v) = l.split_once(": ").or_else(|| l.split_once(':'))?;
            let k = k.trim().to_lowercase();
            let k = if k == "reported by component" { "component".to_string() } else { k };
            let v = v.trim().to_string();
            // Long "keys" are prose sentences that happen to contain a colon.
            (k.len() < 40 && !v.is_empty()).then_some((k, v))
        })
        .collect();
    (fatal, fields)
}

const READ: &str = r#"
$since = (Get-Date).AddDays(-DAYS)
Get-WinEvent -FilterHashtable @{LogName='System'; ProviderName='Microsoft-Windows-WHEA-Logger'; StartTime=$since} -MaxEvents 2000 -ErrorAction SilentlyContinue | ForEach-Object {
  $_.TimeCreated.ToString('yyyy-MM-dd HH:mm') + "`t" + $_.Id + "`t" + ($_.Message -replace "`r?`n", [string][char]31)
}
"#;

/// WHEA events from the last `days` days, newest first.
pub fn read(days: u32) -> Vec<Event> {
    let (out, _) = ps(&READ.replace("DAYS", &days.to_string()));
    rows(&out, 3)
        .into_iter()
        .filter_map(|r| {
            let (fatal, fields) = parse_message(&r[2].replace('\u{1F}', "\n"));
            Some(Event { time: r[0].clone(), id: r[1].parse().ok()?, fatal, fields })
        })
        .collect()
}

/// Repeats of the same error, folded together.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Group {
    pub id: u32,
    pub fatal: bool,
    pub component: String,
    pub source: String,
    pub error_type: String,
    pub device: String, // hardware ID of the PCIe device involved, if any
    pub bdf: String,    // PCI bus:device:function
    pub count: usize,
    pub first: String,
    pub last: String,
    pub apic: BTreeSet<String>, // CPU threads that reported it
}

pub fn group(events: &[Event]) -> Vec<Group> {
    let mut out: Vec<Group> = Vec::new();
    for e in events {
        // For PCIe, the device behind the port (secondary) is the interesting one.
        let device = [e.get("secondary device name"), e.get("primary device name"), e.get("device name")].into_iter().find(|s| !s.is_empty()).unwrap_or("").to_string();
        let bdf = [e.get("secondary bus:device:function"), e.get("primary bus:device:function"), e.get("bus:device:function")].into_iter().find(|s| !s.is_empty() && *s != "0x0:0x0:0x0").unwrap_or("").to_string();
        let key = (e.id, e.fatal, e.get("component").to_string(), e.get("error source").to_string(), e.get("error type").to_string(), device.clone());
        let g = match out.iter_mut().find(|g| (g.id, g.fatal, g.component.clone(), g.source.clone(), g.error_type.clone(), g.device.clone()) == key) {
            Some(g) => g,
            None => {
                out.push(Group { id: e.id, fatal: e.fatal, component: key.2, source: key.3, error_type: key.4, device, bdf, first: e.time.clone(), last: e.time.clone(), ..Default::default() });
                out.last_mut().expect("just pushed")
            }
        };
        g.count += 1;
        g.first = g.first.clone().min(e.time.clone());
        g.last = g.last.clone().max(e.time.clone());
        let apic = e.get("processor apic id");
        if !apic.is_empty() {
            g.apic.insert(apic.to_string());
        }
    }
    out.sort_by(|a, b| b.fatal.cmp(&a.fatal).then(b.count.cmp(&a.count)));
    out
}

// ---------------- Knowledge base ----------------

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Cpu {
    Amd,
    Intel,
    Other,
}

/// What we know about this PC that changes the advice.
#[derive(Clone, Copy, Debug)]
pub struct Context {
    pub cpu: Cpu,
    /// Some(false): memory runs at its default (non-XMP) speed. Some(true): overclocked/XMP.
    pub ram_overclocked: Option<bool>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Kind {
    CpuCache,
    CpuTlb,
    CpuBus,
    CpuTimer,
    CpuInternal,
    CpuExternal,
    CpuOther,
    Memory,
    Pcie,
    PciBus,
    Nmi,
    Generic,
}

pub fn classify(g: &Group) -> Kind {
    let comp = g.component.to_lowercase();
    let src = g.source.to_lowercase();
    let ty = g.error_type.to_lowercase();
    if comp.contains("pci express") || src.contains("pci express") || src.contains("advanced error reporting") {
        return Kind::Pcie;
    }
    if comp.contains("memory") || ty.contains("memory") {
        return Kind::Memory;
    }
    if comp.contains("processor") || src.contains("machine check") {
        return match () {
            _ if ty.contains("cache") => Kind::CpuCache,
            _ if ty.contains("lookaside") || ty.contains("tlb") => Kind::CpuTlb,
            _ if ty.contains("bus") || ty.contains("interconnect") => Kind::CpuBus,
            _ if ty.contains("timer") || ty.contains("watchdog") => Kind::CpuTimer,
            _ if ty.contains("external") => Kind::CpuExternal,
            _ if ["parity", "unclassified", "microcode", "internal", "frc", "micro-architecture"].iter().any(|w| ty.contains(w)) => Kind::CpuInternal,
            _ => Kind::CpuOther,
        };
    }
    if src.contains("non-maskable") || src.contains("nmi") {
        return Kind::Nmi;
    }
    if comp.contains("pci") {
        return Kind::PciBus;
    }
    Kind::Generic
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Advice {
    pub title: String,
    pub meaning: String,
    pub causes: Vec<String>,
    pub fixes: Vec<String>,
    pub note: Option<String>,
}

fn v(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}

const OUTCOME_FATAL: &str = "It couldn't be corrected, so Windows stopped with a blue screen (WHEA_UNCORRECTABLE_ERROR) or the PC restarted.";
const OUTCOME_CORRECTED: &str = "The hardware corrected it on the fly, so nothing crashed, but repeats are an early sign of instability.";

/// Explains one group of errors: what it means, why it happens, how to fix it.
pub fn explain(g: &Group, ctx: &Context) -> Advice {
    let outcome = if g.fatal { OUTCOME_FATAL } else { OUTCOME_CORRECTED };
    let amd = ctx.cpu == Cpu::Amd;
    let intel = ctx.cpu == Cpu::Intel;
    let core_causes_amd = &[
        "Curve Optimizer or an undervolt set too aggressively (the most common cause on Ryzen)",
        "Precision Boost Overdrive (PBO) or a manual CPU overclock",
        "Infinity Fabric (FCLK) clock set too high",
        "Old BIOS (AGESA) firmware",
        "A core that's slightly weaker than the rest, or rarely a faulty CPU",
    ][..];
    let core_fixes_amd = &[
        "Load optimized defaults in the BIOS to rule out any overclock or undervolt.",
        "If you use Curve Optimizer, make the affected core less negative (for example -30 to -20) or turn it off.",
        "Turn PBO off while testing.",
        "Update the BIOS to the latest version; AGESA updates fixed many WHEA errors on Ryzen.",
        "Check CPU temperatures under load (HWiNFO or the System monitor page).",
        "If errors continue at full stock settings with the latest BIOS, the CPU may be faulty: contact AMD for a replacement.",
    ][..];
    let core_causes_intel = &[
        "An undervolt or overclock that isn't stable",
        "13th/14th gen Core instability from elevated voltage (fixed by newer microcode)",
        "Old BIOS or CPU microcode",
        "Overheating or a cooler that isn't seated properly",
        "Rarely, a faulty CPU",
    ][..];
    let core_fixes_intel = &[
        "Load BIOS defaults, or the 'Intel Default Settings' profile on newer boards.",
        "Update the BIOS for the latest microcode (0x12B or newer on 13th/14th gen).",
        "Remove any undervolt or overclock.",
        "Check CPU temperatures under load and reseat the cooler if they're high.",
        "If errors continue at stock settings with the latest BIOS, contact Intel about a replacement.",
    ][..];
    let core_causes_other = &["An overclock or undervolt that isn't stable", "Old BIOS firmware", "Overheating", "Rarely, a faulty CPU"][..];
    let core_fixes_other = &["Load BIOS defaults.", "Update the BIOS.", "Check CPU temperatures under load.", "If errors continue at stock settings, the CPU may be faulty."][..];
    let (cc, cf) = if amd { (core_causes_amd, core_fixes_amd) } else if intel { (core_causes_intel, core_fixes_intel) } else { (core_causes_other, core_fixes_other) };

    let mut a = match classify(g) {
        Kind::CpuCache => Advice {
            title: "CPU cache error".into(),
            meaning: format!("A CPU core's cache returned data that failed its check. {outcome}"),
            causes: v(cc),
            fixes: v(cf),
            note: None,
        },
        Kind::CpuTlb => Advice {
            title: "CPU address translation (TLB) error".into(),
            meaning: format!("The part of a CPU core that maps memory addresses (the translation lookaside buffer) failed a check. {outcome}"),
            causes: v(cc),
            fixes: v(cf),
            note: None,
        },
        Kind::CpuBus if amd => Advice {
            title: "CPU interconnect error (Infinity Fabric)".into(),
            meaning: format!("An error on the Infinity Fabric, the link inside a Ryzen CPU between the cores, the memory controller and PCIe. {outcome}"),
            causes: v(&[
                "Memory or Infinity Fabric overclock that isn't stable (XMP/DOCP/EXPO, or FCLK above about 1800 MHz on Ryzen 5000)",
                "Crashes at idle or light load from deep power-saving states, a known Ryzen 5000 issue",
                "SoC voltage too low for the memory speed",
                "Old BIOS (AGESA) firmware",
                "A PCIe device or riser cable causing link errors",
                "An unstable power supply",
            ]),
            fixes: v(&[
                "Update the BIOS. AGESA updates fixed many Ryzen 5000 WHEA 18 crashes.",
                "In the BIOS, set 'Power Supply Idle Control' to 'Typical Current Idle' (it fixes crashes at idle or light load).",
                "If memory is overclocked, set FCLK to half the memory speed (1600 MHz for DDR4-3200) or try one step slower.",
                "Turn off PBO and Curve Optimizer while testing.",
                "Reseat the graphics card and NVMe drives; if you use a riser cable, set the slot to PCIe Gen 3.",
                "As a last test, disable 'Global C-state Control' in the BIOS (it uses a little more power at idle).",
            ]),
            note: None,
        },
        Kind::CpuBus => Advice {
            title: "CPU bus / interconnect error".into(),
            meaning: format!("An error on the links between the CPU cores and the rest of the system (the ring/mesh and system agent on Intel). {outcome}"),
            causes: v(&["Cache (ring) or memory overclock that isn't stable", "System agent / IO voltage too low for the memory speed", "Old BIOS or microcode", "A PCIe device causing link errors", "An unstable power supply"]),
            fixes: v(&["Load BIOS defaults.", "Update the BIOS.", "Lower any memory or ring overclock.", "Reseat the graphics card and NVMe drives.", "Test with another power supply if crashes continue."]),
            note: None,
        },
        Kind::CpuTimer => Advice {
            title: "CPU watchdog timeout".into(),
            meaning: format!("A CPU core stopped responding to the processor's internal watchdog timer. {outcome}"),
            causes: v(&["An overclock or undervolt that isn't stable", "Power delivery problems (power supply or motherboard VRM)", "Overheating", "Old BIOS firmware"]),
            fixes: v(&["Load BIOS defaults.", "Update the BIOS.", "Check CPU and motherboard temperatures under load.", "Make sure both CPU power cables (EPS 8-pin) are firmly connected.", "Test with another power supply if crashes continue."]),
            note: None,
        },
        Kind::CpuInternal => Advice {
            title: "CPU internal error".into(),
            meaning: format!("The processor detected an internal inconsistency (parity, microcode or an unclassified internal check). {outcome}"),
            causes: v(cc),
            fixes: v(cf),
            note: None,
        },
        Kind::CpuExternal => Advice {
            title: "Error reported to the CPU by another component".into(),
            meaning: format!("Something outside the CPU (often memory or a PCIe device) signalled an error that reached the processor. {outcome}"),
            causes: v(&["Memory instability or a faulty module", "A PCIe device misbehaving", "Old BIOS firmware"]),
            fixes: v(&["Update the BIOS and chipset driver.", "Test memory with Windows Memory Diagnostic (mdsched.exe).", "Reseat expansion cards and drives."]),
            note: None,
        },
        Kind::CpuOther => Advice {
            title: "CPU machine check".into(),
            meaning: format!("A CPU core reported a hardware error. {outcome}"),
            causes: v(cc),
            fixes: v(cf),
            note: None,
        },
        Kind::Memory => {
            let mut causes = v(&[
                "A memory overclock that isn't stable (XMP/DOCP/EXPO or manual timings)",
                "A failing RAM module",
                "A module not fully seated, or dust in the slot",
                if amd { "Memory controller or Infinity Fabric instability" } else { "Memory controller or system agent instability" },
                "Old BIOS firmware",
            ]);
            let mut note = None;
            if ctx.ram_overclocked == Some(false) {
                causes.swap(0, 1);
                note = Some("Your memory runs at its default speed, not overclocked, which makes a faulty module, slot or seating problem more likely than a settings problem.".into());
            }
            Advice {
                title: "Memory error".into(),
                meaning: if g.fatal {
                    "The memory system hit an error it couldn't correct, so Windows stopped (blue screen). PCs without ECC memory usually only see memory errors when they're fatal.".into()
                } else {
                    "ECC memory found and corrected a bad bit. A few are normal over years; regular ones mean a module is wearing out.".into()
                },
                causes,
                fixes: v(&[
                    "Run Windows Memory Diagnostic (press Win+R, type mdsched.exe), or MemTest86 from a USB stick for a thorough test.",
                    "Test one module at a time, and try other slots, to find a bad stick or slot.",
                    "Reseat the memory modules firmly.",
                    "Update the BIOS.",
                    "If XMP/EXPO is on, try one speed step lower or raise DRAM voltage slightly within the kit's rating.",
                    "Replace any module that fails testing (most RAM has a lifetime warranty).",
                ]),
                note,
            }
        }
        Kind::Pcie => Advice {
            title: "PCI Express link error".into(),
            meaning: if g.fatal {
                "A PCIe device (graphics card, NVMe drive, network card...) stopped responding or sent bad data, and the error couldn't be recovered.".into()
            } else {
                "The PCIe link between the CPU or chipset and a device had a transmission error that was corrected. Occasional ones are harmless; many point to a signal problem.".into()
            },
            causes: v(&[
                "The card or drive isn't fully seated",
                "A PCIe riser cable, especially at PCIe 4.0 speeds",
                "PCIe link power saving (ASPM) on some boards",
                "Old chipset driver or BIOS",
                "Device firmware (SSD firmware or graphics card vBIOS)",
                "Loose PCIe power cables to the graphics card",
            ]),
            fixes: v(&[
                "Update the chipset driver and the BIOS.",
                "Reseat the device named above; for graphics cards, check that both power cables are firmly in.",
                "Turn off PCIe power saving: Control Panel > Power Options > Change plan settings > Change advanced power settings > PCI Express > Link State Power Management > Off.",
                "Set that slot to PCIe Gen 3 in the BIOS, especially with a riser cable.",
                "Update the device's driver and firmware (SSD makers have firmware tools).",
                "Try a different slot.",
            ]),
            note: None,
        },
        Kind::PciBus => Advice {
            title: "PCI bus error".into(),
            meaning: format!("A device on the PCI bus reported an error. {outcome}"),
            causes: v(&["A card that isn't seated properly", "A failing expansion card", "Old BIOS or chipset driver"]),
            fixes: v(&["Reseat expansion cards.", "Update the BIOS and chipset driver.", "Remove cards one at a time to find the faulty one."]),
            note: None,
        },
        Kind::Nmi => Advice {
            title: "Non-maskable interrupt (critical hardware signal)".into(),
            meaning: format!("Hardware raised an interrupt that can't be ignored, usually for a memory, chipset or expansion card fault. {outcome}"),
            causes: v(&["Faulty memory", "A failing expansion card", "Motherboard or chipset problem", "Old BIOS"]),
            fixes: v(&["Test memory with Windows Memory Diagnostic or MemTest86.", "Remove expansion cards that aren't needed, then add them back one by one.", "Update the BIOS."]),
            note: None,
        },
        Kind::Generic => Advice {
            title: "Hardware error".into(),
            meaning: format!("Windows logged a hardware error without saying which part it came from. {outcome}"),
            causes: v(&["An overclock or undervolt that isn't stable", "Old BIOS or chipset driver", "Overheating", "Faulty memory", "Power supply problems"]),
            fixes: v(&["Load BIOS defaults.", "Update the BIOS and chipset driver.", "Check temperatures under load.", "Test memory with Windows Memory Diagnostic.", "Check power supply cables."]),
            note: None,
        },
    };

    // All repeats on one CPU thread point at one weaker core.
    let cpu_kind = matches!(classify(g), Kind::CpuCache | Kind::CpuTlb | Kind::CpuBus | Kind::CpuTimer | Kind::CpuInternal | Kind::CpuOther);
    if cpu_kind && g.count > 1 && g.apic.len() == 1 {
        let id = g.apic.iter().next().cloned().unwrap_or_default();
        let tip = if amd { " With Curve Optimizer, give that core a less negative value." } else { "" };
        a.note = Some(format!("Every one of these came from the same CPU thread (APIC ID {id}), so one core is less stable than the rest.{tip}"));
    }
    a
}

#[cfg(test)]
mod tests {
    use super::*;

    const MCE: &str = "A fatal hardware error has occurred.\r\n\r\nReported by component: Processor Core\r\nError Source: Machine Check Exception\r\nError Type: Bus/Interconnect Error\r\nProcessor APIC ID: 0\r\n\r\nThe details view of this entry contains further information.";
    const MEM: &str = "A fatal hardware error has occurred.\n\nComponent: Memory\nError Source: Machine Check Exception\n\nThe details view of this entry contains further information.";
    const PCIE: &str = "A corrected hardware error has occurred.\n\nComponent: PCI Express Root Port\nError Source: Advanced Error Reporting (PCI Express)\n\nPrimary Bus:Device:Function: 0x0:0x1:0x2\nSecondary Bus:Device:Function: 0x2:0x0:0x0\nPrimary Device Name:PCI\\VEN_1022&DEV_1483&SUBSYS_14531022\nSecondary Device Name:PCI\\VEN_10DE&DEV_1F08&SUBSYS_86771043";

    fn ev(time: &str, id: u32, msg: &str) -> Event {
        let (fatal, fields) = parse_message(msg);
        Event { time: time.into(), id, fatal, fields }
    }

    #[test]
    fn parses_and_groups() {
        let e = ev("2026-11-09 17:54", 18, MCE);
        assert!(e.fatal);
        assert_eq!(e.get("component"), "Processor Core");
        assert_eq!(e.get("error type"), "Bus/Interconnect Error");
        assert_eq!(e.get("processor apic id"), "0");
        let p = ev("2026-11-10 10:00", 17, PCIE);
        assert!(!p.fatal);
        assert_eq!(p.get("primary bus:device:function"), "0x0:0x1:0x2");
        let gs = group(&[e.clone(), ev("2026-11-12 09:00", 18, MCE), p, ev("2026-11-16 20:29", 46, MEM)]);
        assert_eq!(gs.len(), 3);
        assert_eq!((gs[0].id, gs[0].count, gs[0].first.as_str(), gs[0].last.as_str()), (18, 2, "2026-11-09 17:54", "2026-11-12 09:00"));
        let pcie = gs.iter().find(|g| g.id == 17).unwrap();
        assert!(pcie.device.contains("VEN_10DE")); // the GPU behind the port, not the port
        assert_eq!(pcie.bdf, "0x2:0x0:0x0");
    }

    /// Reads this PC's real log: `cargo test --lib live_log -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn live_log() {
        let ctx = Context { cpu: Cpu::Amd, ram_overclocked: Some(false) };
        for g in group(&read(90)) {
            let a = explain(&g, &ctx);
            println!("{} x{} id={} fatal={} [{} | {} | {}] dev={} apic={:?} {}..{}\n  => {} | first fix: {}", a.title, g.count, g.id, g.fatal, g.component, g.source, g.error_type, g.device, g.apic, g.first, g.last, a.meaning, a.fixes[0]);
        }
    }

    #[test]
    fn explains_by_kind_and_context() {
        let amd = Context { cpu: Cpu::Amd, ram_overclocked: Some(false) };
        let gs = group(&[ev("a", 18, MCE), ev("b", 18, MCE), ev("c", 46, MEM), ev("d", 17, PCIE)]);
        let bus = explain(&gs[0], &amd);
        assert_eq!(classify(&gs[0]), Kind::CpuBus);
        assert!(bus.title.contains("Infinity Fabric"));
        assert!(bus.fixes.iter().any(|f| f.contains("Typical Current Idle")));
        assert!(bus.note.unwrap().contains("APIC ID 0"));
        let mem = explain(gs.iter().find(|g| g.id == 46).unwrap(), &amd);
        assert_eq!(mem.causes[0], "A failing RAM module"); // stock-speed RAM: hardware first
        assert!(mem.note.is_some());
        let pcie = gs.iter().find(|g| g.id == 17).unwrap();
        assert_eq!(classify(pcie), Kind::Pcie);
        let intel = explain(&gs[0], &Context { cpu: Cpu::Intel, ram_overclocked: None });
        assert!(!intel.title.contains("Infinity"));
        // Every kind has advice.
        for (comp, src, ty) in [("Processor Core", "Corrected Machine Check", "Cache Hierarchy Error"), ("", "Non-Maskable Interrupt", ""), ("Unknown", "Generic", ""), ("Processor Core", "Machine Check Exception", "Internal Timer Error")] {
            let g = Group { component: comp.into(), source: src.into(), error_type: ty.into(), ..Default::default() };
            let a = explain(&g, &amd);
            assert!(!a.fixes.is_empty() && !a.causes.is_empty() && !a.meaning.is_empty());
        }
    }
}
