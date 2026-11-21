//! Hardware knowledge base: what each CPU architecture and GPU vendor needs, from the
//! names Windows reports. Every profile has its own memory-profile name, driver tool,
//! known issues and fixes.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CpuArch {
    AmdZen1,  // Ryzen 1000/2000 (Zen, Zen+), AM4
    AmdZen2,  // Ryzen 3000/4000, AM4
    AmdZen3,  // Ryzen 5000, AM4
    AmdZen4,  // Ryzen 7000/8000, AM5
    AmdZen5,  // Ryzen 9000 / Ryzen AI 300, AM5
    AmdOther, // Threadripper, Athlon, EPYC...
    IntelLegacy, // Core 6th-11th gen
    IntelAlder,  // Core 12th gen
    IntelRaptor, // Core 13th/14th gen
    IntelUltra,  // Core Ultra
    IntelOther,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GpuKind {
    Nvidia,
    Amd,
    IntelArc,
    IntelIntegrated,
    Other,
}

pub struct Profile {
    pub name: &'static str,
    pub tool: (&'static str, &'static str), // what to update drivers with, and where
    pub tips: &'static [&'static str],
}

/// First run of digits in `s` that is `len` long.
fn digits(s: &str, len: usize) -> Option<String> {
    s.split(|c: char| !c.is_ascii_digit()).find(|t| t.len() == len).map(String::from)
}

pub fn cpu_arch(name: &str) -> CpuArch {
    let n = name.to_lowercase();
    if n.contains("amd") || n.contains("ryzen") {
        if !n.contains("ryzen") || n.contains("threadripper") {
            return CpuArch::AmdOther;
        }
        if n.contains("ryzen ai") {
            return CpuArch::AmdZen5;
        }
        return match digits(&n, 4).and_then(|d| d.chars().next()) {
            Some('1' | '2') => CpuArch::AmdZen1,
            Some('3' | '4') => CpuArch::AmdZen2,
            Some('5') => CpuArch::AmdZen3,
            Some('7' | '8') => CpuArch::AmdZen4,
            Some('9') => CpuArch::AmdZen5,
            _ => CpuArch::AmdOther,
        };
    }
    if n.contains("intel") {
        if n.contains("ultra") {
            return CpuArch::IntelUltra;
        }
        // i7-13700K -> 13; i7-9700K -> 9
        let generation = digits(&n, 5).and_then(|d| d[..2].parse::<u32>().ok()).or_else(|| digits(&n, 4).and_then(|d| d[..1].parse().ok()));
        return match generation {
            Some(13 | 14) => CpuArch::IntelRaptor,
            Some(12) => CpuArch::IntelAlder,
            Some(6..=11) => CpuArch::IntelLegacy,
            _ => CpuArch::IntelOther,
        };
    }
    CpuArch::Unknown
}

impl CpuArch {
    pub fn is_amd(self) -> bool {
        matches!(self, CpuArch::AmdZen1 | CpuArch::AmdZen2 | CpuArch::AmdZen3 | CpuArch::AmdZen4 | CpuArch::AmdZen5 | CpuArch::AmdOther)
    }
    pub fn is_intel(self) -> bool {
        matches!(self, CpuArch::IntelLegacy | CpuArch::IntelAlder | CpuArch::IntelRaptor | CpuArch::IntelUltra | CpuArch::IntelOther)
    }
    /// AM4 Ryzen: where "Power Supply Idle Control" and the FCLK 1:1 rule apply.
    pub fn is_am4(self) -> bool {
        matches!(self, CpuArch::AmdZen1 | CpuArch::AmdZen2 | CpuArch::AmdZen3)
    }
    /// The BIOS name for the memory's rated-speed profile.
    pub fn memory_profile(self) -> &'static str {
        match self {
            CpuArch::AmdZen4 | CpuArch::AmdZen5 => "EXPO",
            a if a.is_amd() => "DOCP / EXPO",
            _ => "XMP",
        }
    }
}

const AMD_CHIPSET: (&str, &str) = ("AMD chipset driver", "https://www.amd.com/en/support/download/drivers.html");
const INTEL_DSA: (&str, &str) = ("Intel Driver & Support Assistant", "https://www.intel.com/content/www/us/en/support/detect.html");

pub fn cpu_profile(arch: CpuArch) -> Profile {
    match arch {
        CpuArch::AmdZen1 => Profile { name: "AMD Zen / Zen+ (Ryzen 1000-2000, AM4)", tool: AMD_CHIPSET, tips: &[
            "Memory support is the weakest of any Ryzen: DDR4-3200 is a realistic ceiling, and a BIOS update often improves memory compatibility.",
            "If the PC crashes or reboots at idle, set Power Supply Idle Control to Typical Current Idle in the BIOS.",
            "Install AMD's chipset driver for the right power plan and USB/SATA drivers.",
        ]},
        CpuArch::AmdZen2 => Profile { name: "AMD Zen 2 (Ryzen 3000/4000, AM4)", tool: AMD_CHIPSET, tips: &[
            "Turn on DOCP/EXPO, and keep the Infinity Fabric (FCLK) at half the memory speed, up to about 1800 MHz (DDR4-3600).",
            "If the PC crashes at idle, set Power Supply Idle Control to Typical Current Idle.",
            "Install AMD's chipset driver; newer BIOS versions fix boost and USB dropout issues.",
        ]},
        CpuArch::AmdZen3 => Profile { name: "AMD Zen 3 (Ryzen 5000, AM4)", tool: AMD_CHIPSET, tips: &[
            "Idle reboots and WHEA 18/19 errors are common on Ryzen 5000: set Power Supply Idle Control to Typical Current Idle and update the BIOS.",
            "Turn on DOCP/EXPO and keep FCLK 1:1 with memory, up to about 1800-1900 MHz.",
            "Curve Optimizer undervolts that pass benchmarks can still crash at idle: back them off a few steps if you see WHEA errors.",
            "BIOS AGESA 1.2.0.7 or newer fixes the fTPM stutter some Ryzen 5000 systems have.",
        ]},
        CpuArch::AmdZen4 => Profile { name: "AMD Zen 4 (Ryzen 7000/8000, AM5)", tool: AMD_CHIPSET, tips: &[
            "Turn on EXPO. The first boot after changing memory settings can train memory for a few minutes: that's normal.",
            "Enable Memory Context Restore in the BIOS to cut boot times after training.",
            "Keep the BIOS updated: newer AM5 BIOS versions cap SoC voltage to protect the CPU.",
            "On X3D chips, install AMD's chipset driver so games run on the cores with the extra cache.",
        ]},
        CpuArch::AmdZen5 => Profile { name: "AMD Zen 5 (Ryzen 9000 / Ryzen AI 300)", tool: AMD_CHIPSET, tips: &[
            "Turn on EXPO, and keep the BIOS (AGESA) current: early Zen 5 BIOS updates brought real performance and stability fixes.",
            "Install AMD's chipset driver, which X3D and dual-CCD chips need for correct core scheduling in games.",
            "Use Windows 11 24H2 or newer for AMD's branch prediction scheduling improvements.",
        ]},
        CpuArch::AmdOther => Profile { name: "AMD processor", tool: AMD_CHIPSET, tips: &[
            "Install AMD's chipset driver and keep the BIOS updated.",
        ]},
        CpuArch::IntelRaptor => Profile { name: "Intel Core 13th/14th gen (Raptor Lake)", tool: INTEL_DSA, tips: &[
            "Update the BIOS to Intel microcode 0x12B or newer. Older microcode lets the CPU request too much voltage, which can permanently damage it over time.",
            "Use the 'Intel Default Settings' profile in the BIOS instead of the board's unlimited power defaults.",
            "Crashes while decompressing files or loading games are a known symptom of affected chips: Intel extended the warranty for them.",
        ]},
        CpuArch::IntelAlder => Profile { name: "Intel Core 12th gen (Alder Lake)", tool: INTEL_DSA, tips: &[
            "Use Windows 11 so Thread Director puts games on the performance cores.",
            "Turn on XMP for the memory's rated speed; DDR5 boards benefit most from BIOS updates.",
        ]},
        CpuArch::IntelUltra => Profile { name: "Intel Core Ultra", tool: INTEL_DSA, tips: &[
            "Keep the BIOS and Intel's graphics and NPU drivers current with Intel Driver & Support Assistant.",
            "Turn on XMP for the memory's rated speed.",
        ]},
        CpuArch::IntelLegacy => Profile { name: "Intel Core 6th-11th gen", tool: INTEL_DSA, tips: &[
            "Turn on XMP for the memory's rated speed.",
            "Install Intel's chipset and Management Engine drivers with Intel Driver & Support Assistant.",
        ]},
        CpuArch::IntelOther => Profile { name: "Intel processor", tool: INTEL_DSA, tips: &[
            "Keep drivers current with Intel Driver & Support Assistant, and the BIOS from your PC or board maker.",
        ]},
        CpuArch::Unknown => Profile { name: "Processor", tool: ("Windows Update", "ms-settings:windowsupdate"), tips: &[] },
    }
}

pub fn gpu_kind(name: &str) -> GpuKind {
    let n = name.to_lowercase();
    if n.contains("nvidia") || n.contains("geforce") || n.contains("quadro") {
        GpuKind::Nvidia
    } else if n.contains("radeon") || n.contains("amd") {
        GpuKind::Amd
    } else if n.contains("intel") && n.contains("arc") {
        GpuKind::IntelArc
    } else if n.contains("intel") {
        GpuKind::IntelIntegrated
    } else {
        GpuKind::Other
    }
}

pub fn gpu_profile(kind: GpuKind) -> Profile {
    match kind {
        GpuKind::Nvidia => Profile { name: "NVIDIA GeForce", tool: ("NVIDIA App or nvidia.com", "https://www.nvidia.com/en-us/software/nvidia-app/"), tips: &[
            "If games crash with 'display driver stopped responding' (nvlddmkm), do a clean install: tick 'Perform a clean installation' in the NVIDIA installer, or use DDU in Safe Mode.",
            "Check both PCIe power cables are fully seated; loose cables cause driver resets under load.",
            "Flash compares your driver with the latest Game Ready driver automatically.",
        ]},
        GpuKind::Amd => Profile { name: "AMD Radeon", tool: ("AMD Software: Adrenalin Edition", "https://www.amd.com/en/support/download/drivers.html"), tips: &[
            "If you see 'driver timeout' errors, reset Radeon tuning to defaults, then reinstall with the Factory Reset option in the Adrenalin installer.",
            "Turn on Smart Access Memory (Resizable BAR and Above 4G Decoding in the BIOS) for extra performance.",
            "Windows Update sometimes swaps in an older Radeon driver; Flash's Updates page shows these as vendor-driver swaps so you can skip them.",
        ]},
        GpuKind::IntelArc => Profile { name: "Intel Arc", tool: ("Intel Graphics Software", "https://www.intel.com/content/www/us/en/download/785597/intel-arc-iris-xe-graphics-windows.html"), tips: &[
            "Arc needs Resizable BAR: turn on 'Re-Size BAR Support' and 'Above 4G Decoding' in the BIOS, or performance drops sharply.",
            "Keep the driver current: Arc drivers have brought large gains for older DirectX 9 and 11 games.",
        ]},
        GpuKind::IntelIntegrated => Profile { name: "Intel integrated graphics", tool: INTEL_DSA, tips: &[
            "On laptops, the maker's driver may be required: if Intel's installer refuses, get the graphics driver from the laptop maker's support page.",
        ]},
        GpuKind::Other => Profile { name: "Graphics", tool: ("Windows Update", "ms-settings:windowsupdate"), tips: &[] },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_architectures() {
        assert_eq!(cpu_arch("AMD Ryzen 5 5600 6-Core Processor"), CpuArch::AmdZen3);
        assert_eq!(cpu_arch("AMD Ryzen 7 7800X3D 8-Core Processor"), CpuArch::AmdZen4);
        assert_eq!(cpu_arch("AMD Ryzen 9 9950X 16-Core Processor"), CpuArch::AmdZen5);
        assert_eq!(cpu_arch("AMD Ryzen 7 3700X 8-Core Processor"), CpuArch::AmdZen2);
        assert_eq!(cpu_arch("AMD Ryzen AI 9 HX 370"), CpuArch::AmdZen5);
        assert_eq!(cpu_arch("AMD Ryzen Threadripper 3970X 32-Core Processor"), CpuArch::AmdOther);
        assert_eq!(cpu_arch("13th Gen Intel(R) Core(TM) i7-13700K"), CpuArch::IntelRaptor);
        assert_eq!(cpu_arch("Intel(R) Core(TM) i9-14900K"), CpuArch::IntelRaptor);
        assert_eq!(cpu_arch("12th Gen Intel(R) Core(TM) i5-12400F"), CpuArch::IntelAlder);
        assert_eq!(cpu_arch("Intel(R) Core(TM) i7-9700K CPU @ 3.60GHz"), CpuArch::IntelLegacy);
        assert_eq!(cpu_arch("Intel(R) Core(TM) Ultra 7 155H"), CpuArch::IntelUltra);
        assert_eq!(cpu_arch("Qualcomm Snapdragon X Elite"), CpuArch::Unknown);
        assert_eq!(CpuArch::AmdZen4.memory_profile(), "EXPO");
        assert_eq!(CpuArch::IntelRaptor.memory_profile(), "XMP");
        assert!(CpuArch::AmdZen3.is_am4() && !CpuArch::AmdZen4.is_am4());
        assert_eq!(gpu_kind("NVIDIA GeForce RTX 2060"), GpuKind::Nvidia);
        assert_eq!(gpu_kind("AMD Radeon RX 7800 XT"), GpuKind::Amd);
        assert_eq!(gpu_kind("Intel(R) Arc(TM) A770 Graphics"), GpuKind::IntelArc);
        assert_eq!(gpu_kind("Intel(R) UHD Graphics 770"), GpuKind::IntelIntegrated);
        for a in [CpuArch::AmdZen1, CpuArch::AmdZen2, CpuArch::AmdZen3, CpuArch::AmdZen4, CpuArch::AmdZen5, CpuArch::IntelLegacy, CpuArch::IntelAlder, CpuArch::IntelRaptor, CpuArch::IntelUltra] {
            assert!(!cpu_profile(a).tips.is_empty());
        }
        for g in [GpuKind::Nvidia, GpuKind::Amd, GpuKind::IntelArc, GpuKind::IntelIntegrated] {
            assert!(!gpu_profile(g).tips.is_empty());
        }
    }
}
