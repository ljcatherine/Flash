//! Quick component benchmarks: CPU (one core and all cores), memory bandwidth, and
//! disk sequential/random speed. Disk tests bypass the Windows file cache so they
//! measure the drive, not RAM.
use std::hint::black_box;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::windows::fs::OpenOptionsExt;
use std::path::Path;
use std::time::{Duration, Instant};

const NO_BUFFERING: u32 = 0x2000_0000; // FILE_FLAG_NO_BUFFERING
const WRITE_THROUGH: u32 = 0x8000_0000; // FILE_FLAG_WRITE_THROUGH
const CHUNK: usize = 4 << 20; // 4 MiB, a multiple of every sector size

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Test {
    CpuSingle,
    CpuMulti,
    Memory,
    DiskWrite,
    DiskRead,
    DiskRandom,
    GpuCompute,
    GpuFill,
}

impl Test {
    pub const ALL: [Test; 8] = [Test::CpuSingle, Test::CpuMulti, Test::Memory, Test::GpuCompute, Test::GpuFill, Test::DiskWrite, Test::DiskRead, Test::DiskRandom];

    pub fn name(self) -> &'static str {
        match self {
            Test::CpuSingle => "CPU, one core",
            Test::CpuMulti => "CPU, all cores",
            Test::Memory => "Memory bandwidth",
            Test::DiskWrite => "Disk sequential write",
            Test::DiskRead => "Disk sequential read",
            Test::DiskRandom => "Disk random 4K read",
            Test::GpuCompute => "GPU shader compute",
            Test::GpuFill => "GPU fill rate",
        }
    }

    pub fn unit(self) -> &'static str {
        match self {
            Test::CpuSingle | Test::CpuMulti => "M ops/s",
            Test::Memory => "GB/s",
            Test::DiskWrite | Test::DiskRead => "MB/s",
            Test::DiskRandom => "IOPS",
            Test::GpuCompute => "GFLOPS",
            Test::GpuFill => "Gpixels/s",
        }
    }

    pub fn about(self) -> &'static str {
        match self {
            Test::CpuSingle => "Integer work on one thread. Drives how snappy apps and most games feel.",
            Test::CpuMulti => "The same work on every thread at once. Matters for rendering, compiling and encoding.",
            Test::Memory => "Copies a 256 MB block in RAM. Higher means faster loading and multitasking.",
            Test::DiskWrite => "Writes a 1 GB file straight to the drive.",
            Test::DiskRead => "Reads that file back, skipping the Windows cache.",
            Test::DiskRandom => "Small reads at random spots. The best measure of how fast Windows and apps load.",
            Test::GpuCompute => "Runs heavy maths in a shader on every pixel of a 1080p frame. Tracks how well the GPU handles demanding games.",
            Test::GpuFill => "Blends full 4K frames over and over. Measures how fast the GPU writes pixels to its memory.",
        }
    }

    pub fn is_gpu(self) -> bool {
        matches!(self, Test::GpuCompute | Test::GpuFill)
    }

    pub fn is_disk(self) -> bool {
        matches!(self, Test::DiskWrite | Test::DiskRead | Test::DiskRandom)
    }

    /// A fast current machine scores about this; used to scale the bars.
    pub fn reference(self) -> f64 {
        match self {
            Test::CpuSingle => 650.0,
            Test::CpuMulti => 10000.0,
            Test::Memory => 30.0,
            Test::DiskWrite => 5000.0,
            Test::DiskRead => 6000.0,
            Test::DiskRandom => 20000.0,
            Test::GpuCompute => 18000.0,
            Test::GpuFill => 150.0,
        }
    }
}

/// What a result means in plain words.
pub fn rating(test: Test, v: f64) -> &'static str {
    match test {
        Test::DiskRead | Test::DiskWrite if v >= 3000.0 => "Fast NVMe",
        Test::DiskRead | Test::DiskWrite if v >= 1000.0 => "NVMe",
        Test::DiskRead | Test::DiskWrite if v >= 300.0 => "SATA SSD",
        Test::DiskRead | Test::DiskWrite => "Hard drive or USB",
        Test::DiskRandom if v >= 8000.0 => "NVMe SSD",
        Test::DiskRandom if v >= 2000.0 => "SSD",
        Test::DiskRandom => "Hard drive class",
        _ => {
            let r = v / test.reference();
            if r >= 0.85 { "Excellent" } else if r >= 0.6 { "Good" } else if r >= 0.35 { "Average" } else { "Slow" }
        }
    }
}

/// xorshift with a multiply: cheap, branch-free integer work the compiler can't remove.
fn spin(dur: Duration) -> (u64, f64) {
    let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut n = 0u64;
    let start = Instant::now();
    while start.elapsed() < dur {
        for _ in 0..200_000 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x = x.wrapping_mul(0x2545_F491_4F6C_DD1D);
        }
        n += 200_000;
    }
    black_box(x);
    (n, start.elapsed().as_secs_f64())
}

pub fn cpu_single() -> f64 {
    let (n, secs) = spin(Duration::from_secs(3));
    n as f64 / secs / 1e6
}

pub fn cpu_multi() -> f64 {
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
    std::thread::scope(|s| {
        let hs: Vec<_> = (0..threads).map(|_| s.spawn(|| spin(Duration::from_secs(3)))).collect();
        hs.into_iter().map(|h| h.join().map(|(n, secs)| n as f64 / secs / 1e6).unwrap_or(0.0)).sum()
    })
}

pub fn memory() -> f64 {
    let size = 256 << 20;
    let src = vec![7u8; size];
    let mut dst = vec![0u8; size];
    let start = Instant::now();
    let mut copied = 0u64;
    while start.elapsed() < Duration::from_secs(2) {
        dst.copy_from_slice(black_box(&src));
        copied += size as u64;
    }
    black_box(&dst);
    copied as f64 / start.elapsed().as_secs_f64() / 1e9
}

/// Page-aligned buffer, which unbuffered file I/O requires.
struct Aligned {
    ptr: *mut u8,
    layout: std::alloc::Layout,
}

impl Aligned {
    fn new(size: usize) -> Aligned {
        let layout = std::alloc::Layout::from_size_align(size, 4096).expect("valid layout");
        // SAFETY: layout has non-zero size; null is handled below.
        let ptr = unsafe { std::alloc::alloc_zeroed(layout) };
        if ptr.is_null() {
            std::alloc::handle_alloc_error(layout);
        }
        Aligned { ptr, layout }
    }
    fn slice(&mut self) -> &mut [u8] {
        // SAFETY: ptr is a live allocation of layout.size() bytes owned by self.
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.layout.size()) }
    }
}

impl Drop for Aligned {
    fn drop(&mut self) {
        // SAFETY: allocated in new() with this exact layout.
        unsafe { std::alloc::dealloc(self.ptr, self.layout) }
    }
}

pub fn bench_file(dir: &Path) -> std::path::PathBuf {
    dir.join("flash-benchmark.tmp")
}

/// Writes the test file (1 GiB, or 256 MiB when space is short). Returns MB/s.
pub fn disk_write(file: &Path, free: u64) -> Result<f64, String> {
    let total: usize = if free > 4 << 30 { 1 << 30 } else { 256 << 20 };
    let mut buf = Aligned::new(CHUNK);
    let mut x: u64 = 0x1234_5678;
    for b in buf.slice().iter_mut() {
        // Incompressible data so drives that compress don't cheat.
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        *b = x as u8;
    }
    let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).custom_flags(NO_BUFFERING | WRITE_THROUGH).open(file).map_err(|e| e.to_string())?;
    let start = Instant::now();
    let mut written = 0;
    while written < total {
        f.write_all(buf.slice()).map_err(|e| e.to_string())?;
        written += CHUNK;
    }
    f.sync_all().map_err(|e| e.to_string())?;
    Ok(written as f64 / start.elapsed().as_secs_f64() / 1e6)
}

/// Reads the test file sequentially without the cache. Returns MB/s.
pub fn disk_read(file: &Path) -> Result<f64, String> {
    let mut f = std::fs::OpenOptions::new().read(true).custom_flags(NO_BUFFERING).open(file).map_err(|e| e.to_string())?;
    let mut buf = Aligned::new(CHUNK);
    let start = Instant::now();
    let mut read = 0usize;
    loop {
        let n = f.read(buf.slice()).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        read += n;
    }
    Ok(read as f64 / start.elapsed().as_secs_f64() / 1e6)
}

/// 4 KiB reads at random aligned offsets for 3 seconds, one at a time. Returns IOPS.
pub fn disk_random(file: &Path) -> Result<f64, String> {
    let len = std::fs::metadata(file).map_err(|e| e.to_string())?.len();
    let blocks = (len / 4096).max(1);
    let mut f = std::fs::OpenOptions::new().read(true).custom_flags(NO_BUFFERING).open(file).map_err(|e| e.to_string())?;
    let mut buf = Aligned::new(4096);
    let mut x: u64 = 0x9E37_79B9;
    let start = Instant::now();
    let mut ops = 0u64;
    while start.elapsed() < Duration::from_secs(3) {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        f.seek(SeekFrom::Start((x % blocks) * 4096)).map_err(|e| e.to_string())?;
        f.read_exact(buf.slice()).map_err(|e| e.to_string())?;
        ops += 1;
    }
    Ok(ops as f64 / start.elapsed().as_secs_f64())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ratings_and_disk_roundtrip() {
        assert_eq!(rating(Test::DiskRead, 3500.0), "Fast NVMe");
        assert_eq!(rating(Test::DiskRead, 500.0), "SATA SSD");
        assert_eq!(rating(Test::DiskRandom, 100.0), "Hard drive class");
        assert_eq!(rating(Test::CpuSingle, 600.0), "Excellent");
        // Small real run through the unbuffered path.
        let file = std::env::temp_dir().join(format!("flash-bench-test-{}.tmp", std::process::id()));
        assert!(disk_write(&file, 0).unwrap() > 0.0);
        assert_eq!(std::fs::metadata(&file).unwrap().len(), 256 << 20);
        assert!(disk_read(&file).unwrap() > 0.0);
        assert!(disk_random(&file).unwrap() > 0.0);
        std::fs::remove_file(&file).unwrap();
    }
}
