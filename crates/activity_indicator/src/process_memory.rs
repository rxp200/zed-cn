//! 当前进程自身的内存读数。
//!
//! 面板要回答的是“Zed 自己占了多少内存”，而各平台任务管理器默认展示的都不是
//! 总工作集：Windows 的「内存」列是私有工作集，Linux 的常驻内存含共享页，
//! macOS 的活动监视器按 `phys_footprint` 统计。这里同时给出工作集与私有部分，
//! 面板据此把与其它进程共用的页面（系统 DLL、驱动映射、字体缓存等）单独展示。

/// 当前进程的一次内存读数。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ProcessMemory {
    /// 总工作集/常驻内存，包含与其它进程共享的页。
    pub working_set_bytes: u64,
    /// 私有（非共享）部分，即各平台任务管理器默认「内存」列的口径。
    pub private_bytes: u64,
}

impl ProcessMemory {
    /// 工作集中与其它进程共享、同时也计入它们工作集的那部分。
    pub fn shared_bytes(&self) -> u64 {
        self.working_set_bytes.saturating_sub(self.private_bytes)
    }
}

/// 读取当前进程的内存口径；平台不支持或读取失败时返回 `None`。
pub fn current_process_memory() -> Option<ProcessMemory> {
    platform::current_process_memory()
}

/// Windows：`PROCESS_MEMORY_COUNTERS_EX2` 一次给出工作集与私有工作集。
#[cfg(target_os = "windows")]
mod platform {
    use super::ProcessMemory;
    use std::mem::size_of;
    use windows::Win32::System::ProcessStatus::{
        GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX2,
    };
    use windows::Win32::System::Threading::GetCurrentProcess;

    pub(super) fn current_process_memory() -> Option<ProcessMemory> {
        let mut counters = PROCESS_MEMORY_COUNTERS_EX2 {
            cb: size_of::<PROCESS_MEMORY_COUNTERS_EX2>() as u32,
            ..Default::default()
        };
        // SAFETY: `counters` 以 EX2 的实际大小初始化了 `cb`，指针指向该结构
        // 自身，内核只会写入 `cb` 覆盖的字节；返回成功时所有字段都已填充。
        unsafe {
            GetProcessMemoryInfo(
                GetCurrentProcess(),
                std::ptr::addr_of_mut!(counters).cast::<PROCESS_MEMORY_COUNTERS>(),
                counters.cb,
            )
        }
        .ok()?;
        // Windows 8.1 之前没有 `PrivateWorkingSetSize`，`EX2` 会直接失败，
        // 此时返回 `None` 而不是把另一种口径冒充成任务管理器口径。
        Some(ProcessMemory {
            working_set_bytes: counters.WorkingSetSize as u64,
            private_bytes: counters.PrivateWorkingSetSize as u64,
        })
    }
}

/// Linux：`/proc/self/smaps_rollup` 同时给出 `Rss` 与共享页，二者相减即私有部分。
#[cfg(target_os = "linux")]
mod platform {
    use super::ProcessMemory;

    pub(super) fn current_process_memory() -> Option<ProcessMemory> {
        let rollup = std::fs::read_to_string("/proc/self/smaps_rollup").ok()?;
        parse_smaps_rollup(&rollup)
    }

    /// `smaps_rollup` 每行形如 `Rss:                1234 kB`，数值单位是 KiB。
    fn rollup_kib(text: &str, field: &str) -> Option<u64> {
        let line = text.lines().find(|line| line.starts_with(field))?;
        line.split_whitespace().nth(1)?.parse::<u64>().ok()
    }

    /// `Rss` 减去共享页就是私有常驻；内核 4.14 起才有 `smaps_rollup`。
    fn parse_smaps_rollup(text: &str) -> Option<ProcessMemory> {
        let resident_kib = rollup_kib(text, "Rss:")?;
        let shared_kib = rollup_kib(text, "Shared_Clean:").unwrap_or(0)
            + rollup_kib(text, "Shared_Dirty:").unwrap_or(0);
        Some(ProcessMemory {
            working_set_bytes: resident_kib.saturating_mul(1024),
            private_bytes: resident_kib.saturating_sub(shared_kib).saturating_mul(1024),
        })
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn splits_resident_memory_into_private_and_shared() {
            let rollup = "\
Rss:                1024 kB\n\
Pss:                 512 kB\n\
Shared_Clean:        256 kB\n\
Shared_Dirty:         64 kB\n\
Private_Clean:        64 kB\n\
Private_Dirty:       640 kB\n";
            let memory = parse_smaps_rollup(rollup).expect("完整的 rollup 应该可解析");
            assert_eq!(memory.working_set_bytes, 1024 * 1024);
            assert_eq!(memory.private_bytes, 704 * 1024);
            assert_eq!(memory.shared_bytes(), 320 * 1024);
        }

        #[test]
        fn treats_a_missing_shared_split_as_unavailable() {
            assert_eq!(parse_smaps_rollup("VmRSS: 10 kB\n"), None);
            let shared_free = parse_smaps_rollup("Rss: 100 kB\n").expect("Rss 足够解析");
            assert_eq!(shared_free.private_bytes, 100 * 1024);
            assert_eq!(shared_free.shared_bytes(), 0);
        }
    }
}

/// macOS：`rusage_info_v2` 一次给出 `ri_resident_size` 与 `ri_phys_footprint`，
/// 后者就是活动监视器「内存」列的口径（含压缩内存与 IOKit 映射，因此可能大于
/// 常驻内存，此时 [`ProcessMemory::shared_bytes`] 归零）。
#[cfg(target_os = "macos")]
mod platform {
    use super::ProcessMemory;
    use std::os::raw::c_int;

    const RUSAGE_INFO_V2: c_int = 2;

    unsafe extern "C" {
        fn proc_pid_rusage(pid: c_int, flavor: c_int, buffer: *mut libc::rusage_info_v2) -> c_int;
    }

    pub(super) fn current_process_memory() -> Option<ProcessMemory> {
        let mut info = std::mem::MaybeUninit::<libc::rusage_info_v2>::zeroed();
        // SAFETY: 缓冲区按 `rusage_info_v2` 的实际大小与对齐分配，flavor 与该
        // 结构匹配；返回 0 时内核已写入全部字段。
        let status = unsafe {
            proc_pid_rusage(
                std::process::id() as c_int,
                RUSAGE_INFO_V2,
                info.as_mut_ptr(),
            )
        };
        if status != 0 {
            return None;
        }
        // SAFETY: 上面的调用返回 0，表示缓冲区已被完整初始化。
        let info = unsafe { info.assume_init() };
        Some(ProcessMemory {
            working_set_bytes: info.ri_resident_size,
            private_bytes: info.ri_phys_footprint,
        })
    }
}

/// 其它平台没有可靠且低开销的私有内存口径，面板退回只显示工作集。
#[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
mod platform {
    use super::ProcessMemory;

    pub(super) fn current_process_memory() -> Option<ProcessMemory> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::ProcessMemory;

    #[test]
    fn shared_part_never_underflows() {
        let sample = ProcessMemory {
            working_set_bytes: 1024,
            private_bytes: 4096,
        };
        assert_eq!(sample.shared_bytes(), 0);
    }
}
