//! What the operating system will tell you about itself.
//!
//! Everything in this module is a direct Win32 call. There is no crate between
//! the window and `GetSystemTimes`, which is the point: a task manager is a
//! document about the platform it runs on, and one written against a
//! cross-platform abstraction would be a document about the abstraction.
//!
//! The shape is a [`Sampler`] that owns the *previous* reading and a
//! [`Snapshot`] that is one moment. Almost nothing the kernel reports is a
//! rate: `GetSystemTimes` returns totals since boot, `GetProcessTimes` returns
//! totals since the process started, and `MIB_IF_ROW2::InOctets` counts every
//! byte the adapter has ever seen. A percentage is a difference between two
//! readings divided by the wall-clock time between them, so somebody has to
//! remember the last one — and that somebody is the sampler, not the UI.
//!
//! The structs are platform-neutral on purpose. The non-Windows build returns
//! empty snapshots rather than failing to compile, so the workspace still
//! builds everywhere and the UI has exactly one code path.

use std::collections::HashMap;
use std::time::Instant;

// ---------------------------------------------------------------------------
// What a sample is
// ---------------------------------------------------------------------------

/// Fixed facts about the machine, read once at start-up.
///
/// Separate from [`Snapshot`] because none of it changes while the window is
/// open, and re-reading the registry sixty times a second to learn the same
/// CPU name would be silly.
#[derive(Clone, Debug, Default)]
pub struct MachineInfo {
    /// The processor's marketing name, from the registry.
    pub cpu_name: String,
    /// Logical processors, which is what a percentage is divided by.
    pub logical_cores: u32,
    /// Physical cores, where the kernel will say.
    pub physical_cores: u32,
    /// `x64`, `ARM64`, `x86`.
    pub arch: &'static str,
    /// The operating system's own name for itself.
    pub os_name: String,
    /// `24H2`-style release identifier, when the registry carries one.
    pub os_release: String,
    /// The build number, which is the only version anyone can act on.
    pub os_build: String,
    /// The machine's NetBIOS name.
    pub host: String,
    /// The signed-in account.
    pub user: String,
    /// Allocation granularity, reported because it explains the commit numbers.
    pub page_size: u64,
    /// Total physical memory, which never changes and is therefore not a rate.
    pub total_ram: u64,
}

/// One process, as of one sample.
#[derive(Clone, Debug, Default)]
pub struct ProcInfo {
    pub pid: u32,
    /// The process that created it. Not shown anywhere yet; sampled because it
    /// is free — `PROCESSENTRY32W` carries it — and a tree view is the obvious
    /// next thing a list of processes grows.
    #[allow(dead_code)]
    pub parent: u32,
    pub name: String,
    pub threads: u32,
    /// Share of the whole machine, `0..=100`, over the last interval.
    pub cpu: f32,
    /// Resident set: what the process has in physical memory right now.
    pub working_set: u64,
    /// What it has asked the pagefile for, which is the number that keeps
    /// growing when something leaks.
    pub pagefile: u64,
    /// Seconds since it was created.
    pub uptime: f32,
    /// True when the process could not be opened — a protected or elevated one.
    /// Its CPU and memory read as zero, and saying so is better than implying
    /// it is idle.
    pub restricted: bool,
}

/// The whole-machine memory picture.
#[derive(Clone, Debug, Default)]
pub struct MemInfo {
    pub total: u64,
    pub available: u64,
    pub used: u64,
    /// What the kernel calls the memory load, `0..=100`.
    pub load: u32,
    pub commit_total: u64,
    pub commit_limit: u64,
    pub cached: u64,
    pub paged_pool: u64,
    pub nonpaged_pool: u64,
    pub handles: u32,
    pub processes: u32,
    pub threads: u32,
}

/// One mounted volume.
#[derive(Clone, Debug, Default)]
pub struct DiskInfo {
    /// `C:`, with no trailing separator.
    pub letter: String,
    pub label: String,
    pub filesystem: String,
    /// `Fixed`, `Removable`, `Network`, `Optical`, `RAM`.
    pub kind: &'static str,
    pub total: u64,
    pub free: u64,
}

impl DiskInfo {
    /// How full the volume is, `0..=1`.
    pub fn used_fraction(&self) -> f32 {
        if self.total == 0 {
            return 0.0;
        }
        ((self.total - self.free) as f64 / self.total as f64) as f32
    }
}

/// One network interface, with its throughput over the last interval.
#[derive(Clone, Debug, Default)]
pub struct NetInfo {
    pub name: String,
    pub description: String,
    pub up: bool,
    /// Negotiated receive rate in bits per second, as the driver reports it.
    pub link_speed: u64,
    pub rx_total: u64,
    pub tx_total: u64,
    /// Bytes per second over the last interval.
    pub rx_rate: f64,
    pub tx_rate: f64,
}

/// One moment, as measured against the previous one.
#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    /// Whole-machine CPU use, `0..=100`.
    pub cpu: f32,
    pub memory: MemInfo,
    pub processes: Vec<ProcInfo>,
    pub disks: Vec<DiskInfo>,
    pub interfaces: Vec<NetInfo>,
    /// Seconds since the machine booted.
    pub uptime: u64,
    /// How long the sample itself took, in milliseconds. On screen because a
    /// monitor that costs more than what it monitors is worth knowing about.
    pub sample_ms: f32,
}

impl Snapshot {
    /// Total receive throughput across every interface, in bytes per second.
    pub fn rx_rate(&self) -> f64 {
        self.interfaces.iter().map(|n| n.rx_rate).sum()
    }

    /// The same for transmit.
    pub fn tx_rate(&self) -> f64 {
        self.interfaces.iter().map(|n| n.tx_rate).sum()
    }
}

// ---------------------------------------------------------------------------
// The sampler
// ---------------------------------------------------------------------------

/// Holds the previous reading, so a total can become a rate.
pub struct Sampler {
    /// `(idle, kernel, user)` in 100-nanosecond units, from `GetSystemTimes`.
    prev_system: Option<(u64, u64, u64)>,
    /// Per-process kernel+user time, keyed by pid.
    ///
    /// Rebuilt every sample rather than pruned: a pid that has gone is a pid
    /// that must not be matched against a *reused* one, and Windows reuses pids
    /// aggressively. Dropping the old map wholesale is both cheaper and safer
    /// than trying to notice.
    prev_proc: HashMap<u32, u64>,
    /// Per-interface `(in, out)` octet totals, keyed by interface index.
    prev_net: HashMap<u32, (u64, u64)>,
    /// When the previous sample was taken, for the wall-clock denominator.
    prev_at: Option<Instant>,
    /// Cached, because a percentage is divided by it every single time.
    logical_cores: u32,
}

impl Sampler {
    pub fn new(machine: &MachineInfo) -> Self {
        Self {
            prev_system: None,
            prev_proc: HashMap::new(),
            prev_net: HashMap::new(),
            prev_at: None,
            logical_cores: machine.logical_cores.max(1),
        }
    }

    /// Takes one reading.
    ///
    /// The first call has nothing to difference against, so every rate in the
    /// snapshot it returns is zero. That is honest rather than unfortunate: the
    /// machine's CPU use over an interval of zero length is not a number.
    pub fn sample(&mut self) -> Snapshot {
        let started = Instant::now();
        let elapsed = self.prev_at.map(|t| started.duration_since(t).as_secs_f64()).unwrap_or(0.0);
        let mut snap = self.sample_inner(elapsed);
        self.prev_at = Some(started);
        snap.sample_ms = started.elapsed().as_secs_f32() * 1000.0;
        snap
    }
}

// ---------------------------------------------------------------------------
// Windows
// ---------------------------------------------------------------------------

#[cfg(windows)]
mod win {
    use super::*;

    use std::ffi::c_void;
    use std::os::windows::ffi::OsStrExt;

    use windows_sys::Win32::Foundation::{CloseHandle, FILETIME, HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::NetworkManagement::IpHelper::{
        FreeMibTable, GetIfTable2, IF_TYPE_SOFTWARE_LOOPBACK, MIB_IF_TABLE2,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        GetDiskFreeSpaceExW, GetDriveTypeW, GetLogicalDrives, GetVolumeInformationW,
    };
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    };
    use windows_sys::Win32::System::ProcessStatus::{
        GetPerformanceInfo, GetProcessMemoryInfo, PERFORMANCE_INFORMATION, PROCESS_MEMORY_COUNTERS,
    };
    use windows_sys::Win32::System::Registry::{
        HKEY, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ, RegGetValueW,
    };
    use windows_sys::Win32::System::SystemInformation::{
        GetNativeSystemInfo, GetTickCount64, GlobalMemoryStatusEx, MEMORYSTATUSEX, SYSTEM_INFO,
    };
    use windows_sys::Win32::System::Threading::{
        GetProcessTimes, GetSystemTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        PROCESS_TERMINATE, TerminateProcess,
    };

    /// `GetDriveTypeW`'s answers, which windows-sys puts behind a feature this
    /// crate does not otherwise need.
    const DRIVE_REMOVABLE: u32 = 2;
    const DRIVE_FIXED: u32 = 3;
    const DRIVE_REMOTE: u32 = 4;
    const DRIVE_CDROM: u32 = 5;
    const DRIVE_RAMDISK: u32 = 6;
    /// `IF_OPER_STATUS::IfOperStatusUp`.
    const IF_OPER_UP: i32 = 1;

    /// A NUL-terminated UTF-16 buffer, for the `W` entry points.
    fn wide(s: &str) -> Vec<u16> {
        std::ffi::OsStr::new(s).encode_wide().chain(std::iter::once(0)).collect()
    }

    /// The other direction: a fixed-size buffer the kernel filled, up to its
    /// first NUL. Windows does not promise the tail is zeroed, so a plain
    /// `from_utf16_lossy` over the whole array yields the string plus garbage.
    fn from_wide(buf: &[u16]) -> String {
        let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        String::from_utf16_lossy(&buf[..end])
    }

    /// Splices a `FILETIME` back into the 64-bit integer it always was.
    ///
    /// The split into two 32-bit halves is a 1980s ABI detail, not a unit: the
    /// value is one count of 100-nanosecond intervals and every arithmetic use
    /// of it wants it whole.
    fn filetime(ft: FILETIME) -> u64 {
        ((ft.dwHighDateTime as u64) << 32) | ft.dwLowDateTime as u64
    }

    /// Reads one `REG_SZ` value, or `None` if it is absent.
    fn reg_string(root: HKEY, subkey: &str, value: &str) -> Option<String> {
        let subkey = wide(subkey);
        let value = wide(value);
        let mut buf = [0u16; 256];
        let mut size = std::mem::size_of_val(&buf) as u32;
        let rc = unsafe {
            RegGetValueW(
                root,
                subkey.as_ptr(),
                value.as_ptr(),
                RRF_RT_REG_SZ,
                std::ptr::null_mut(),
                buf.as_mut_ptr().cast::<c_void>(),
                &mut size,
            )
        };
        // `ERROR_SUCCESS`. Anything else — no such key, no such value, a value
        // of the wrong type — is the same answer here: nothing to show.
        (rc == 0).then(|| from_wide(&buf))
    }

    /// Everything that does not change while the window is open.
    pub fn machine_info() -> MachineInfo {
        let mut info = SYSTEM_INFO::default();
        unsafe { GetNativeSystemInfo(&mut info) };
        // The architecture lives in an anonymous union whose other arm is the
        // obsolete `dwOemId`; the struct arm is the one Windows has documented
        // as current since XP.
        let arch = match unsafe { info.Anonymous.Anonymous.wProcessorArchitecture } {
            9 => "x64",
            12 => "ARM64",
            5 => "ARM",
            0 => "x86",
            _ => "unknown",
        };

        let mut status = MEMORYSTATUSEX {
            dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32,
            ..Default::default()
        };
        let total_ram =
            if unsafe { GlobalMemoryStatusEx(&mut status) } != 0 { status.ullTotalPhys } else { 0 };

        const CPU_KEY: &str = r"HARDWARE\DESCRIPTION\System\CentralProcessor\0";
        const OS_KEY: &str = r"SOFTWARE\Microsoft\Windows NT\CurrentVersion";

        MachineInfo {
            cpu_name: reg_string(HKEY_LOCAL_MACHINE, CPU_KEY, "ProcessorNameString")
                .map(|s| s.trim().to_string())
                .unwrap_or_else(|| "Unknown processor".into()),
            logical_cores: info.dwNumberOfProcessors.max(1),
            physical_cores: physical_cores(),
            arch,
            os_name: reg_string(HKEY_LOCAL_MACHINE, OS_KEY, "ProductName")
                .unwrap_or_else(|| "Windows".into()),
            os_release: reg_string(HKEY_LOCAL_MACHINE, OS_KEY, "DisplayVersion")
                .unwrap_or_default(),
            os_build: reg_string(HKEY_LOCAL_MACHINE, OS_KEY, "CurrentBuildNumber")
                .unwrap_or_default(),
            host: std::env::var("COMPUTERNAME").unwrap_or_else(|_| "this machine".into()),
            user: std::env::var("USERNAME").unwrap_or_else(|_| "user".into()),
            page_size: info.dwPageSize as u64,
            total_ram,
        }
    }

    /// Physical cores, counted from the processor-relationship table.
    ///
    /// `GetLogicalProcessorInformationEx` returns a *variable-length* array —
    /// each record carries its own size — so it cannot be indexed and has to be
    /// walked by byte offset. Worth the trouble: logical and physical counts
    /// differ on every machine with SMT, and reporting one as the other is the
    /// most common way a system readout is quietly wrong.
    fn physical_cores() -> u32 {
        use windows_sys::Win32::System::SystemInformation::{
            GetLogicalProcessorInformationEx, RelationProcessorCore,
            SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX,
        };
        let mut len: u32 = 0;
        unsafe {
            // The documented two-call idiom: the first call is expected to fail
            // with `ERROR_INSUFFICIENT_BUFFER` and exists only to fill `len`.
            GetLogicalProcessorInformationEx(RelationProcessorCore, std::ptr::null_mut(), &mut len);
        }
        if len == 0 {
            return 0;
        }
        let mut buf = vec![0u8; len as usize];
        let ok = unsafe {
            GetLogicalProcessorInformationEx(
                RelationProcessorCore,
                buf.as_mut_ptr().cast::<SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX>(),
                &mut len,
            )
        };
        if ok == 0 {
            return 0;
        }
        let mut offset = 0usize;
        let mut cores = 0u32;
        while offset + std::mem::size_of::<SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX>()
            <= len as usize
        {
            let record = unsafe {
                &*buf.as_ptr().add(offset).cast::<SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX>()
            };
            if record.Size == 0 {
                break;
            }
            cores += 1;
            offset += record.Size as usize;
        }
        cores
    }

    impl Sampler {
        pub(super) fn sample_inner(&mut self, elapsed: f64) -> Snapshot {
            Snapshot {
                cpu: self.cpu_total(),
                memory: memory(),
                processes: self.processes(elapsed),
                disks: disks(),
                interfaces: self.interfaces(elapsed),
                uptime: unsafe { GetTickCount64() } / 1000,
                sample_ms: 0.0,
            }
        }

        /// Whole-machine CPU use, from the three counters `GetSystemTimes` keeps.
        ///
        /// `kernel` already *includes* idle — that is the documented behaviour
        /// and the single most common mistake made with this call. Busy time is
        /// therefore `kernel + user - idle`, and the denominator is
        /// `kernel + user`, not the wall clock: the counters already sum across
        /// every logical processor, so dividing by elapsed seconds would give a
        /// figure that is `cores` times too large.
        fn cpu_total(&mut self) -> f32 {
            let (mut idle, mut kernel, mut user) =
                (FILETIME::default(), FILETIME::default(), FILETIME::default());
            if unsafe { GetSystemTimes(&mut idle, &mut kernel, &mut user) } == 0 {
                return 0.0;
            }
            let now = (filetime(idle), filetime(kernel), filetime(user));
            let Some(prev) = self.prev_system.replace(now) else { return 0.0 };

            let idle_delta = now.0.saturating_sub(prev.0);
            let total_delta =
                now.1.saturating_sub(prev.1).saturating_add(now.2.saturating_sub(prev.2));
            if total_delta == 0 {
                return 0.0;
            }
            let busy = total_delta.saturating_sub(idle_delta);
            (busy as f64 / total_delta as f64 * 100.0) as f32
        }

        /// Every process, with its CPU share over the interval.
        fn processes(&mut self, elapsed: f64) -> Vec<ProcInfo> {
            let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
            if snapshot == INVALID_HANDLE_VALUE || snapshot.is_null() {
                return Vec::new();
            }
            let mut entry = PROCESSENTRY32W {
                dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
                ..Default::default()
            };
            let mut out = Vec::with_capacity(320);
            // The map is rebuilt rather than updated in place, so a pid that
            // has exited cannot lend its CPU total to a new process that the
            // kernel happened to give the same number.
            let mut times = HashMap::with_capacity(320);
            // One machine-wide denominator: the interval, times the number of
            // processors, expressed in the same 100-nanosecond units the kernel
            // counts process time in.
            let capacity = elapsed * self.logical_cores as f64 * 10_000_000.0;

            let mut ok = unsafe { Process32FirstW(snapshot, &mut entry) };
            while ok != 0 {
                let pid = entry.th32ProcessID;
                let mut proc = ProcInfo {
                    pid,
                    parent: entry.th32ParentProcessID,
                    name: from_wide(&entry.szExeFile),
                    threads: entry.cntThreads,
                    restricted: true,
                    ..Default::default()
                };

                // `PROCESS_QUERY_LIMITED_INFORMATION` rather than the full
                // query right: it is the one an unelevated process is allowed
                // to have on most of the system, and it is enough for times and
                // counters. Asking for more would make the common case fail.
                let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
                if !handle.is_null() {
                    proc.restricted = false;
                    let total = fill_process(handle, &mut proc);

                    if let Some(prev) = self.prev_proc.get(&pid).copied()
                        && capacity > 0.0
                    {
                        let delta = total.saturating_sub(prev);
                        proc.cpu = (delta as f64 / capacity * 100.0).min(100.0) as f32;
                    }
                    times.insert(pid, total);
                    unsafe { CloseHandle(handle) };
                }
                out.push(proc);
                ok = unsafe { Process32NextW(snapshot, &mut entry) };
            }
            unsafe { CloseHandle(snapshot) };
            self.prev_proc = times;
            out
        }

        /// Per-interface throughput, differenced against the last reading.
        fn interfaces(&mut self, elapsed: f64) -> Vec<NetInfo> {
            let mut table: *mut MIB_IF_TABLE2 = std::ptr::null_mut();
            if unsafe { GetIfTable2(&mut table) } != 0 || table.is_null() {
                return Vec::new();
            }
            let count = unsafe { (*table).NumEntries } as usize;
            // `Table` is declared as a one-element array and is really `count`
            // of them: the classic Win32 variable-length tail. Every row is the
            // same size, so unlike the processor table this one can be indexed.
            let rows = unsafe { (*table).Table.as_ptr() };

            let mut out = Vec::with_capacity(count);
            let mut totals = HashMap::with_capacity(count);
            for i in 0..count {
                let row = unsafe { &*rows.add(i) };
                totals.insert(row.InterfaceIndex, (row.InOctets, row.OutOctets));
                // Loopback is not a network; showing it would put every local
                // socket's traffic in a readout meant to describe the wire.
                if row.Type == IF_TYPE_SOFTWARE_LOOPBACK {
                    continue;
                }
                let (rx_rate, tx_rate) = match self.prev_net.get(&row.InterfaceIndex) {
                    Some(&(rx, tx)) if elapsed > 0.0 => (
                        row.InOctets.saturating_sub(rx) as f64 / elapsed,
                        row.OutOctets.saturating_sub(tx) as f64 / elapsed,
                    ),
                    _ => (0.0, 0.0),
                };
                out.push(NetInfo {
                    name: from_wide(&row.Alias),
                    description: from_wide(&row.Description),
                    up: row.OperStatus == IF_OPER_UP,
                    link_speed: row.ReceiveLinkSpeed,
                    rx_total: row.InOctets,
                    tx_total: row.OutOctets,
                    rx_rate,
                    tx_rate,
                });
            }
            // The table is the kernel's allocation, not ours.
            unsafe { FreeMibTable(table.cast::<c_void>()) };
            self.prev_net = totals;
            // Live adapters first, then by traffic: an idle virtual adapter
            // should never be the first thing in the list.
            out.sort_by(|a, b| {
                b.up.cmp(&a.up).then((b.rx_total + b.tx_total).cmp(&(a.rx_total + a.tx_total)))
            });
            out
        }
    }

    /// Everything one open process can be asked, in two calls.
    ///
    /// Returns its kernel-plus-user time, in 100-nanosecond units, because the
    /// caller needs that for the CPU difference and `GetProcessTimes` hands it
    /// over in the same call that reports the creation time. Splitting the two
    /// out into separate functions cost a second `GetProcessTimes` per process,
    /// which on a machine with three hundred of them is three hundred wasted
    /// system calls every interval.
    fn fill_process(handle: HANDLE, proc: &mut ProcInfo) -> u64 {
        let mut counters = PROCESS_MEMORY_COUNTERS {
            cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
            ..Default::default()
        };
        if unsafe {
            GetProcessMemoryInfo(
                handle,
                &mut counters,
                std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
            )
        } != 0
        {
            proc.working_set = counters.WorkingSetSize as u64;
            proc.pagefile = counters.PagefileUsage as u64;
        }

        let (mut created, mut exited, mut kernel, mut user) =
            (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
        if unsafe { GetProcessTimes(handle, &mut created, &mut exited, &mut kernel, &mut user) }
            == 0
        {
            return 0;
        }
        // Creation time is an absolute FILETIME, so its age needs the current
        // one to subtract from. Reading the system clock through the same 1601
        // epoch keeps the arithmetic in one unit.
        proc.uptime = system_time_100ns().saturating_sub(filetime(created)) as f32 / 10_000_000.0;
        filetime(kernel) + filetime(user)
    }

    /// The wall clock in the same units and epoch as a `FILETIME`.
    ///
    /// Sampled once per process rather than once per sweep because it is a
    /// register read through a shared page — `GetSystemTimeAsFileTime` does not
    /// enter the kernel — and threading one reading through the whole walk
    /// would buy nothing measurable.
    fn system_time_100ns() -> u64 {
        use windows_sys::Win32::System::SystemInformation::GetSystemTimeAsFileTime;
        let mut ft = FILETIME::default();
        unsafe { GetSystemTimeAsFileTime(&mut ft) };
        filetime(ft)
    }

    /// The whole-machine memory picture, from two calls that overlap slightly.
    ///
    /// `GlobalMemoryStatusEx` has the physical totals; `GetPerformanceInfo` has
    /// the commit charge, the pools and the object counts. Neither has all of
    /// it, so a monitor needs both.
    fn memory() -> MemInfo {
        let mut status = MEMORYSTATUSEX {
            dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32,
            ..Default::default()
        };
        let mut mem = MemInfo::default();
        if unsafe { GlobalMemoryStatusEx(&mut status) } != 0 {
            mem.total = status.ullTotalPhys;
            mem.available = status.ullAvailPhys;
            mem.used = status.ullTotalPhys.saturating_sub(status.ullAvailPhys);
            mem.load = status.dwMemoryLoad;
        }

        let mut perf = PERFORMANCE_INFORMATION {
            cb: std::mem::size_of::<PERFORMANCE_INFORMATION>() as u32,
            ..Default::default()
        };
        if unsafe {
            GetPerformanceInfo(&mut perf, std::mem::size_of::<PERFORMANCE_INFORMATION>() as u32)
        } != 0
        {
            // Every field in this struct is counted in pages, not bytes. The
            // page size is in the same struct precisely because the caller is
            // expected to multiply.
            let page = perf.PageSize as u64;
            mem.commit_total = perf.CommitTotal as u64 * page;
            mem.commit_limit = perf.CommitLimit as u64 * page;
            mem.cached = perf.SystemCache as u64 * page;
            mem.paged_pool = perf.KernelPaged as u64 * page;
            mem.nonpaged_pool = perf.KernelNonpaged as u64 * page;
            mem.handles = perf.HandleCount;
            mem.processes = perf.ProcessCount;
            mem.threads = perf.ThreadCount;
        }
        mem
    }

    /// Every mounted volume, from the drive-letter bitmask.
    fn disks() -> Vec<DiskInfo> {
        let mask = unsafe { GetLogicalDrives() };
        let mut out = Vec::new();
        for bit in 0..26u32 {
            if mask & (1 << bit) == 0 {
                continue;
            }
            let letter = char::from(b'A' + bit as u8);
            let root = wide(&format!("{letter}:\\"));
            let kind = match unsafe { GetDriveTypeW(root.as_ptr()) } {
                DRIVE_FIXED => "Fixed",
                DRIVE_REMOVABLE => "Removable",
                DRIVE_REMOTE => "Network",
                DRIVE_CDROM => "Optical",
                DRIVE_RAMDISK => "RAM",
                _ => continue,
            };

            let (mut free, mut total, mut total_free) = (0u64, 0u64, 0u64);
            // An empty card reader or a disconnected share fails here rather
            // than returning zeroes, which is the only way to tell "0 bytes
            // free" from "no medium".
            if unsafe { GetDiskFreeSpaceExW(root.as_ptr(), &mut free, &mut total, &mut total_free) }
                == 0
            {
                continue;
            }

            let mut label = [0u16; 64];
            let mut fs = [0u16; 32];
            unsafe {
                GetVolumeInformationW(
                    root.as_ptr(),
                    label.as_mut_ptr(),
                    label.len() as u32,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    fs.as_mut_ptr(),
                    fs.len() as u32,
                );
            }

            out.push(DiskInfo {
                letter: format!("{letter}:"),
                label: {
                    let name = from_wide(&label);
                    if name.is_empty() { "Local Disk".into() } else { name }
                },
                filesystem: from_wide(&fs),
                kind,
                total,
                // `total_free` rather than `free`: the first is the volume's,
                // the second is what this user's quota still allows, and a
                // capacity readout means the volume.
                free: total_free,
            });
        }
        out
    }

    /// Ends a process, by pid.
    ///
    /// The one call in this module that changes the machine rather than
    /// describing it, which is why it is the only one that reports an error
    /// string: a failure here has to be shown, not swallowed.
    pub fn terminate(pid: u32) -> Result<(), String> {
        let handle = unsafe { OpenProcess(PROCESS_TERMINATE, 0, pid) };
        if handle.is_null() {
            return Err("access denied — the process is protected or elevated".into());
        }
        let ok = unsafe { TerminateProcess(handle, 1) };
        unsafe { CloseHandle(handle) };
        if ok == 0 { Err("the process refused to end".into()) } else { Ok(()) }
    }
}

#[cfg(windows)]
pub use win::{machine_info, terminate};

// ---------------------------------------------------------------------------
// Everywhere else
// ---------------------------------------------------------------------------

#[cfg(not(windows))]
impl Sampler {
    fn sample_inner(&mut self, _elapsed: f64) -> Snapshot {
        Snapshot::default()
    }
}

/// A stand-in so the workspace builds off Windows. The window still opens; it
/// simply has nothing to report.
#[cfg(not(windows))]
pub fn machine_info() -> MachineInfo {
    MachineInfo {
        cpu_name: "unavailable on this platform".into(),
        logical_cores: 1,
        arch: std::env::consts::ARCH,
        os_name: std::env::consts::OS.into(),
        ..Default::default()
    }
}

#[cfg(not(windows))]
pub fn terminate(_pid: u32) -> Result<(), String> {
    Err("ending a process is implemented against the Win32 API only".into())
}

// ---------------------------------------------------------------------------
// Formatting
// ---------------------------------------------------------------------------

/// Bytes, at the largest unit that leaves a number a person can read.
///
/// Binary units, because every number it is formatting came from a kernel that
/// counts in pages. Reporting a 16 GiB machine as 17.2 GB would be arithmetic
/// nobody asked for.
pub fn bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = n as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    match unit {
        0 => format!("{n} B"),
        1 => format!("{value:.0} {}", UNITS[unit]),
        _ if value >= 100.0 => format!("{value:.0} {}", UNITS[unit]),
        _ => format!("{value:.1} {}", UNITS[unit]),
    }
}

/// A throughput, in bytes per second.
pub fn rate(bytes_per_second: f64) -> String {
    if bytes_per_second < 1.0 {
        return "0 B/s".into();
    }
    format!("{}/s", bytes(bytes_per_second as u64))
}

/// A duration as `3d 04:15:22`, dropping the day when there is not one.
pub fn duration(seconds: u64) -> String {
    let (d, h, m, s) =
        (seconds / 86_400, (seconds % 86_400) / 3600, (seconds % 3600) / 60, seconds % 60);
    if d > 0 { format!("{d}d {h:02}:{m:02}:{s:02}") } else { format!("{h:02}:{m:02}:{s:02}") }
}
