//! 温度与显卡读数。
//!
//! 本地客户端的系统监控面板与远程服务端（`GetSystemStatsResponse`）共用这里的
//! 采样逻辑，因此本模块只负责采集与归一化：展示文案、配色和协议字段都由调用方决定。
//!
//! 各平台的取数方式：
//!
//! - 温度：统一走 `sysinfo` 的传感器列表（Linux 读 `hwmon`，macOS 读 SMC，
//!   Windows 读 WMI）；Windows 在没有任何传感器读数时才用 `powershell` 兜底读
//!   ACPI 热区。
//! - 显卡：优先用 `nvidia-smi`（有 NVIDIA 驱动的机器信息最全），否则读平台自身的
//!   来源（Linux 读 `/sys/class/drm`，Windows 读 WMI `Win32_VideoController`，
//!   macOS 读 `system_profiler`）。

use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use sysinfo::{Component, Components};

/// 温度传感器所属的部件；展示方按它分组、排序和着色。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum TemperatureSource {
    Cpu,
    Gpu,
    Storage,
    Board,
    Network,
    Other,
}

/// 面板里的展示顺序：CPU、显卡、存储、主板、网络，最后是其它。
pub const TEMPERATURE_SOURCE_ORDER: [TemperatureSource; 6] = [
    TemperatureSource::Cpu,
    TemperatureSource::Gpu,
    TemperatureSource::Storage,
    TemperatureSource::Board,
    TemperatureSource::Network,
    TemperatureSource::Other,
];

impl TemperatureSource {
    /// 跨进程传输用的稳定标识；只能追加新取值，不能改动已有取值。
    pub fn wire_name(self) -> &'static str {
        match self {
            Self::Cpu => "cpu",
            Self::Gpu => "gpu",
            Self::Storage => "storage",
            Self::Board => "board",
            Self::Network => "network",
            Self::Other => "other",
        }
    }

    /// 无法识别的标识一律归入 [`TemperatureSource::Other`]，
    /// 这样新版服务端新增部件时旧客户端仍能正常显示。
    pub fn from_wire_name(name: &str) -> Self {
        TEMPERATURE_SOURCE_ORDER
            .into_iter()
            .find(|source| source.wire_name() == name)
            .unwrap_or(Self::Other)
    }
}

/// 单个温度传感器读数。
#[derive(Clone, Debug, PartialEq)]
pub struct TemperatureReading {
    /// 硬件或内核给出的原始传感器名，例如 `coretemp Package id 0`。
    pub label: String,
    pub source: TemperatureSource,
    pub celsius: f32,
}

/// 单张显卡读数。字段为 `None` 表示当前平台或驱动没有提供这项数据。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct GpuReading {
    pub name: String,
    pub vendor: String,
    pub driver_version: Option<String>,
    pub memory_total_bytes: Option<u64>,
    pub memory_used_bytes: Option<u64>,
    pub utilization_percent: Option<f32>,
    pub temperature_celsius: Option<f32>,
}

/// 一次采样的完整结果。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct HardwareReadings {
    pub temperatures: Vec<TemperatureReading>,
    pub gpus: Vec<GpuReading>,
}

/// 超过这个温度的读数必然是解析或驱动错误。
const MAX_PLAUSIBLE_CELSIUS: f32 = 150.;
/// `nvidia-smi` 单次调用约几百毫秒，显卡读数不像温度那么敏感。
const NVIDIA_SMI_REFRESH_INTERVAL: Duration = Duration::from_secs(3);
/// Windows 上启动 `powershell` 很贵，ACPI 温度只在完全没有其它温度来源时才兜底。
#[cfg(target_os = "windows")]
const WINDOWS_THERMAL_REFRESH_INTERVAL: Duration = Duration::from_secs(15);
/// 外部探测命令的最长等待时间；超时即杀掉，避免采样任务被一直卡住。
const COMMAND_TIMEOUT: Duration = Duration::from_millis(1500);

/// 硬件采样器。创建时做一次较贵的发现，之后每次 [`Self::sample`] 只刷新动态读数。
pub struct HardwareSampler {
    components: Components,
    gpus: Vec<GpuBackend>,
    #[cfg(target_os = "windows")]
    windows_thermal: WindowsThermalFallback,
}

impl HardwareSampler {
    pub fn new() -> Self {
        Self {
            components: Components::new_with_refreshed_list(),
            gpus: discover_gpu_backends(),
            #[cfg(target_os = "windows")]
            windows_thermal: WindowsThermalFallback::default(),
        }
    }

    pub fn sample(&mut self) -> HardwareReadings {
        self.components.refresh(true);
        let mut temperatures = select_temperatures(read_component_temperatures(&self.components));
        if temperatures.is_empty() {
            temperatures = self.platform_temperatures();
        }
        HardwareReadings {
            temperatures,
            gpus: self.sample_gpus(),
        }
    }

    fn sample_gpus(&mut self) -> Vec<GpuReading> {
        self.gpus.iter_mut().map(GpuBackend::refresh).collect()
    }

    /// 没有 `hwmon`/SMC 读数时的兜底温度来源；目前只有 Windows 的 ACPI 热区需要它。
    fn platform_temperatures(&mut self) -> Vec<TemperatureReading> {
        #[cfg(target_os = "windows")]
        {
            self.windows_thermal.sample()
        }
        #[cfg(not(target_os = "windows"))]
        {
            Vec::new()
        }
    }
}

impl Default for HardwareSampler {
    fn default() -> Self {
        Self::new()
    }
}

/// Windows 上 `sysinfo` 拿不到温度（常见于普通权限进程）时的兜底状态。
#[cfg(target_os = "windows")]
#[derive(Default)]
struct WindowsThermalFallback {
    temperatures: Vec<TemperatureReading>,
    last_refresh_at: Option<Instant>,
}

#[cfg(target_os = "windows")]
impl WindowsThermalFallback {
    fn sample(&mut self) -> Vec<TemperatureReading> {
        let expired = self
            .last_refresh_at
            .is_none_or(|last| last.elapsed() >= WINDOWS_THERMAL_REFRESH_INTERVAL);
        if expired {
            self.last_refresh_at = Some(Instant::now());
            self.temperatures = select_temperatures(read_windows_thermal_zones());
        }
        self.temperatures.clone()
    }
}

/// 一张显卡的读数来源；`refresh` 决定后续采样从哪里更新动态字段。
struct GpuBackend {
    reading: GpuReading,
    refresh: GpuRefresh,
    last_refresh_at: Option<Instant>,
}

enum GpuRefresh {
    /// 发现时采集一次，之后不再变化。
    #[cfg(not(any(target_os = "linux", target_os = "freebsd")))]
    Static,
    NvidiaSmi {
        index: String,
    },
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    LinuxDrm {
        device: PathBuf,
    },
}

impl GpuBackend {
    fn refresh(&mut self) -> GpuReading {
        match &self.refresh {
            #[cfg(not(any(target_os = "linux", target_os = "freebsd")))]
            GpuRefresh::Static => {}
            GpuRefresh::NvidiaSmi { index } => {
                let due = self
                    .last_refresh_at
                    .is_none_or(|last| last.elapsed() >= NVIDIA_SMI_REFRESH_INTERVAL);
                if due {
                    self.last_refresh_at = Some(Instant::now());
                    if let Some(gpu) = query_nvidia_smi()
                        .into_iter()
                        .find(|gpu| gpu.index == *index)
                    {
                        self.reading = gpu.reading;
                    }
                }
            }
            #[cfg(any(target_os = "linux", target_os = "freebsd"))]
            GpuRefresh::LinuxDrm { device } => {
                let dynamic = sample_linux_gpu(device);
                // 单项读取失败时保留上一次的值，避免界面在反复闪动。
                if let Some(percent) = dynamic.utilization_percent {
                    self.reading.utilization_percent = Some(percent);
                }
                if let Some(bytes) = dynamic.memory_total_bytes {
                    self.reading.memory_total_bytes = Some(bytes);
                }
                if let Some(bytes) = dynamic.memory_used_bytes {
                    self.reading.memory_used_bytes = Some(bytes);
                }
                if let Some(celsius) = dynamic.temperature_celsius {
                    self.reading.temperature_celsius = Some(celsius);
                }
            }
        }
        self.reading.clone()
    }
}

fn discover_gpu_backends() -> Vec<GpuBackend> {
    let nvidia_smi_gpus = query_nvidia_smi();
    let has_nvidia_smi = !nvidia_smi_gpus.is_empty();
    let mut backends: Vec<GpuBackend> = nvidia_smi_gpus
        .into_iter()
        .map(|gpu| GpuBackend {
            reading: gpu.reading,
            refresh: GpuRefresh::NvidiaSmi { index: gpu.index },
            last_refresh_at: Some(Instant::now()),
        })
        .collect();
    backends.extend(discover_platform_gpus(has_nvidia_smi));
    backends
}

// ---------------------------------------------------------------- 温度

fn read_component_temperatures(components: &Components) -> Vec<TemperatureReading> {
    components
        .iter()
        .filter_map(temperature_from_component)
        .collect()
}

fn temperature_from_component(component: &Component) -> Option<TemperatureReading> {
    let celsius = component.temperature()?;
    if !is_plausible_celsius(celsius) {
        return None;
    }
    let label = component.label().trim();
    if label.is_empty() {
        return None;
    }
    Some(TemperatureReading {
        label: label.to_string(),
        source: classify_temperature(label)?,
        celsius,
    })
}

/// 每个部件只保留一个代表读数：CPU 优先封装温度，其余取该部件里最高的一项。
/// 一台机器可能有几十个 `coretemp` 核心传感器，全部列出反而看不清。
pub fn select_temperatures(
    readings: impl IntoIterator<Item = TemperatureReading>,
) -> Vec<TemperatureReading> {
    let readings: Vec<TemperatureReading> = readings.into_iter().collect();
    // 旧式 ACPI 热区（`acpitz`）在多数机器上并不对应真实传感器；只有在没有
    // 任何其它传感器时才保留它，避免把错误的低值当成主板温度。
    let readings: Vec<TemperatureReading> = if readings
        .iter()
        .any(|reading| !is_acpi_thermal_zone(&reading.label))
    {
        readings
            .into_iter()
            .filter(|reading| !is_acpi_thermal_zone(&reading.label))
            .collect()
    } else {
        readings
    };
    let mut selected = Vec::with_capacity(TEMPERATURE_SOURCE_ORDER.len());
    for source in TEMPERATURE_SOURCE_ORDER {
        let candidates: Vec<&TemperatureReading> = readings
            .iter()
            .filter(|reading| reading.source == source)
            .collect();
        let preferred: Vec<&TemperatureReading> = if source == TemperatureSource::Cpu {
            candidates
                .iter()
                .copied()
                .filter(|reading| is_package_sensor(&reading.label))
                .collect()
        } else {
            Vec::new()
        };
        let pool = if preferred.is_empty() {
            candidates
        } else {
            preferred
        };
        if let Some(hottest) = pool
            .into_iter()
            .max_by(|left, right| left.celsius.total_cmp(&right.celsius))
        {
            selected.push(hottest.clone());
        }
    }
    selected
}

fn is_plausible_celsius(celsius: f32) -> bool {
    celsius.is_finite() && (0.0..=MAX_PLAUSIBLE_CELSIUS).contains(&celsius)
}

/// 把传感器名归入部件；返回 `None` 表示这个读数不值得展示（例如电池温度）。
fn classify_temperature(label: &str) -> Option<TemperatureSource> {
    let label = label.to_ascii_lowercase();
    if label.starts_with("bat") || label.contains("battery") {
        return None;
    }
    let source = if contains_any(&label, &["nvme", "drivetemp", "ssd", "hdd", "harddisk"]) {
        TemperatureSource::Storage
    } else if contains_any(
        &label,
        &["amdgpu", "radeon", "nouveau", "nvidia", "i915", "gpu"],
    ) {
        TemperatureSource::Gpu
    } else if contains_any(
        &label,
        &[
            "coretemp",
            "k10temp",
            "k8temp",
            "zenpower",
            "x86_pkg_temp",
            "cpu_thermal",
            "cpu-thermal",
            "soc_thermal",
            "peci",
            "package",
            "tctl",
            "tdie",
            "cpu",
        ],
    ) {
        TemperatureSource::Cpu
    } else if contains_any(&label, &["wifi", "wlan", "wireless", "ethernet"]) {
        TemperatureSource::Network
    } else if contains_any(
        &label,
        &[
            "acpitz",
            "acpi",
            "thermal",
            "zone",
            "computer",
            "pch",
            "board",
            "chipset",
            "mainboard",
            "motherboard",
        ],
    ) {
        TemperatureSource::Board
    } else {
        TemperatureSource::Other
    };
    Some(source)
}

fn is_package_sensor(label: &str) -> bool {
    contains_any(
        &label.to_ascii_lowercase(),
        &["package", "tctl", "tdie", "cpu", "soc"],
    )
}

fn is_acpi_thermal_zone(label: &str) -> bool {
    label.to_ascii_lowercase().contains("acpitz")
}

fn contains_any(haystack: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| haystack.contains(needle))
}

// ---------------------------------------------------------------- 显卡

struct NvidiaGpu {
    index: String,
    reading: GpuReading,
}

/// 查询 `nvidia-smi`。命令不存在或执行失败时返回空列表，由各平台自身的探测兜底。
fn query_nvidia_smi() -> Vec<NvidiaGpu> {
    const DUMP_FIELDS: &str =
        "index,name,driver_version,memory.total,memory.used,utilization.gpu,temperature.gpu";
    let mut command = new_command("nvidia-smi");
    command.args(["--query-gpu", DUMP_FIELDS, "--format=csv,noheader,nounits"]);
    let Some(output) = run_command_with_timeout(command) else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    parse_nvidia_smi_csv(&String::from_utf8_lossy(&output.stdout))
}

fn parse_nvidia_smi_csv(output: &str) -> Vec<NvidiaGpu> {
    output
        .lines()
        .filter_map(|line| {
            let mut fields = line.split(',').map(str::trim);
            let index = fields.next()?;
            let name = fields.next()?;
            let driver_version = fields.next()?;
            let memory_total = fields.next()?;
            let memory_used = fields.next()?;
            let utilization = fields.next()?;
            let temperature = fields.next()?;
            if index.is_empty() || name.is_empty() {
                return None;
            }
            Some(NvidiaGpu {
                index: index.to_string(),
                reading: GpuReading {
                    name: name.to_string(),
                    vendor: "NVIDIA".to_string(),
                    driver_version: optional_text(driver_version),
                    memory_total_bytes: parse_mebibytes(memory_total),
                    memory_used_bytes: parse_mebibytes(memory_used),
                    utilization_percent: parse_number(utilization)
                        .map(|percent| percent as f32)
                        .filter(|percent| (0.0..=100.0).contains(percent)),
                    temperature_celsius: parse_number(temperature)
                        .map(|celsius| celsius as f32)
                        .filter(|celsius| is_plausible_celsius(*celsius)),
                },
            })
        })
        .collect()
}

#[cfg(any(target_os = "linux", target_os = "freebsd"))]
fn discover_platform_gpus(has_nvidia_smi: bool) -> Vec<GpuBackend> {
    let Ok(gpus) = crate::read_drm_gpus() else {
        return Vec::new();
    };
    gpus.into_iter()
        // `nvidia-smi` 已经给出完整信息时不再重复列出同一张卡。
        .filter(|gpu| !(has_nvidia_smi && gpu.info.driver_name.as_deref() == Some("nvidia")))
        .map(|gpu| {
            let info = gpu.info;
            let vendor = info
                .vendor_name
                .unwrap_or_else(|| vendor_name_from_pci_id(info.vendor_pci_id).to_string());
            let name = info.device_name.unwrap_or_else(|| {
                fallback_gpu_name(&vendor, info.vendor_pci_id, info.device_pci_id)
            });
            GpuBackend {
                reading: GpuReading {
                    name,
                    vendor,
                    driver_version: info.driver_version,
                    ..Default::default()
                },
                refresh: GpuRefresh::LinuxDrm {
                    device: gpu.sysfs_path,
                },
                last_refresh_at: None,
            }
        })
        .collect()
}

#[cfg(any(target_os = "linux", target_os = "freebsd"))]
fn sample_linux_gpu(device: &Path) -> GpuReading {
    GpuReading {
        utilization_percent: read_whole_number(device.join("gpu_busy_percent"))
            .map(|percent| percent as f32)
            .filter(|percent| (0.0..=100.0).contains(percent)),
        memory_total_bytes: read_whole_number(device.join("mem_info_vram_total")),
        memory_used_bytes: read_whole_number(device.join("mem_info_vram_used")),
        temperature_celsius: read_gpu_hwmon_temperature(device),
        ..Default::default()
    }
}

/// `amdgpu`、`nouveau` 等驱动的显卡温度挂在显卡自己的 `hwmon` 下；
/// 同一张卡有多个传感器（edge/junction/mem）时取最高的一个。
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
fn read_gpu_hwmon_temperature(device: &Path) -> Option<f32> {
    let Ok(folders) = std::fs::read_dir(device.join("hwmon")) else {
        return None;
    };
    let mut hottest = None;
    for folder in folders.flatten() {
        let folder = folder.path();
        let Ok(name) = std::fs::read_to_string(folder.join("name")) else {
            continue;
        };
        if !matches!(
            name.trim(),
            "amdgpu" | "radeon" | "nouveau" | "nvidia" | "i915" | "xe"
        ) {
            continue;
        }
        let Ok(files) = std::fs::read_dir(&folder) else {
            continue;
        };
        for file in files.flatten() {
            let Some(file_name) = file.file_name().to_str().map(str::to_string) else {
                continue;
            };
            if !file_name.starts_with("temp") || !file_name.ends_with("_input") {
                continue;
            }
            let Some(celsius) = read_whole_number(file.path()).map(|milli| milli as f32 / 1000.)
            else {
                continue;
            };
            if !is_plausible_celsius(celsius) {
                continue;
            }
            hottest = Some(hottest.map_or(celsius, |current: f32| current.max(celsius)));
        }
    }
    hottest
}

#[cfg(target_os = "windows")]
fn discover_platform_gpus(has_nvidia_smi: bool) -> Vec<GpuBackend> {
    let mut discovered = read_windows_video_controllers();
    if has_nvidia_smi {
        discovered.retain(|gpu| !gpu.name.to_ascii_uppercase().contains("NVIDIA"));
    }
    discovered
        .into_iter()
        .map(|reading| GpuBackend {
            reading,
            refresh: GpuRefresh::Static,
            last_refresh_at: None,
        })
        .collect()
}

#[cfg(target_os = "windows")]
fn read_windows_video_controllers() -> Vec<GpuReading> {
    const SCRIPT: &str = r"Get-CimInstance Win32_VideoController | ForEach-Object { $_.Name + [char]9 + $_.DriverVersion + [char]9 + $_.AdapterRAM }";
    let Some(output) = run_powershell(SCRIPT) else {
        return Vec::new();
    };
    parse_windows_video_controllers(&output)
}

#[cfg(target_os = "windows")]
fn read_windows_thermal_zones() -> Vec<TemperatureReading> {
    const SCRIPT: &str = r"Get-CimInstance Win32_PerfFormattedData_Counters_ThermalZoneInformation | ForEach-Object { $_.Name + [char]9 + $_.HighPrecisionTemperature }";
    let Some(output) = run_powershell(SCRIPT) else {
        return Vec::new();
    };
    parse_windows_thermal_zones(&output)
}

/// 脚本刻意不含双引号：Rust 为 Windows 转义命令行引号的方式会与 PowerShell 的
/// 解析规则互相干扰，用 `[char]9` 拼分隔符可以避开这个问题。
#[cfg(target_os = "windows")]
fn run_powershell(script: &str) -> Option<String> {
    let mut command = new_command("powershell");
    command.args(["-NoProfile", "-NonInteractive", "-Command", script]);
    let output = run_command_with_timeout(command)?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(any(test, target_os = "windows"))]
fn parse_windows_video_controllers(output: &str) -> Vec<GpuReading> {
    output
        .lines()
        .filter_map(|line| {
            let mut fields = line.split('\t');
            let name = fields.next()?.trim();
            let driver_version = fields.next().unwrap_or("").trim();
            let adapter_memory = fields.next().unwrap_or("").trim();
            if name.is_empty() {
                return None;
            }
            Some(GpuReading {
                name: name.to_string(),
                vendor: vendor_from_gpu_name(name).to_string(),
                driver_version: optional_text(driver_version),
                memory_total_bytes: parse_number(adapter_memory)
                    .map(|bytes| bytes.max(0.) as u64)
                    .filter(is_plausible_adapter_memory),
                ..Default::default()
            })
        })
        .collect()
}

#[cfg(any(test, target_os = "windows"))]
fn parse_windows_thermal_zones(output: &str) -> Vec<TemperatureReading> {
    output
        .lines()
        .filter_map(|line| {
            let mut fields = line.split('\t');
            let label = fields.next()?.trim();
            let tenths_kelvin = fields.next().unwrap_or("").trim();
            if label.is_empty() {
                return None;
            }
            let celsius = parse_number(tenths_kelvin).map(|tenths| tenths as f32 / 10. - 273.15)?;
            if !is_plausible_celsius(celsius) {
                return None;
            }
            // ACPI 热区通常覆盖 CPU/主板，按主板归类并在行内展示热区原始名称。
            Some(TemperatureReading {
                label: label.to_string(),
                source: TemperatureSource::Board,
                celsius,
            })
        })
        .collect()
}

#[cfg(target_os = "macos")]
fn discover_platform_gpus(_has_nvidia_smi: bool) -> Vec<GpuBackend> {
    let mut command = new_command("system_profiler");
    command.arg("SPDisplaysDataType");
    let Some(output) = run_command_with_timeout(command) else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    parse_system_profiler_gpus(&String::from_utf8_lossy(&output.stdout))
        .into_iter()
        .map(|reading| GpuBackend {
            reading,
            refresh: GpuRefresh::Static,
            last_refresh_at: None,
        })
        .collect()
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "freebsd",
    target_os = "windows",
    target_os = "macos"
)))]
fn discover_platform_gpus(_has_nvidia_smi: bool) -> Vec<GpuBackend> {
    Vec::new()
}

/// 解析 `system_profiler SPDisplaysDataType` 的文本输出：四缩进且以冒号结尾的行是
/// 显卡标题，其下更深缩进的 `键: 值` 行属于这张显卡。
#[cfg(any(test, target_os = "macos"))]
const GPU_HEADER_INDENT: usize = 4;

#[cfg(any(test, target_os = "macos"))]
fn parse_system_profiler_gpus(output: &str) -> Vec<GpuReading> {
    let mut gpus: Vec<GpuReading> = Vec::new();
    for line in output.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let indent = line.len() - line.trim_start().len();
        if indent == GPU_HEADER_INDENT && trimmed.ends_with(':') {
            if trimmed == "Graphics/Displays:" {
                continue;
            }
            gpus.push(GpuReading {
                name: trimmed.trim_end_matches(':').trim().to_string(),
                ..Default::default()
            });
            continue;
        }
        let Some(gpu) = gpus.last_mut() else {
            continue;
        };
        let Some((key, value)) = trimmed.split_once(':') else {
            continue;
        };
        let value = value.trim();
        match key.trim() {
            "Chipset Model" => gpu.name = value.to_string(),
            "Vendor" => {
                let vendor = value.split('(').next().unwrap_or(value).trim();
                gpu.vendor = vendor_from_gpu_name(vendor).to_string();
            }
            "VRAM (Total)" | "VRAM (Dynamic, Max)" | "VRAM (Total, Dynamic Max)" => {
                gpu.memory_total_bytes = parse_size(value);
            }
            _ => {}
        }
    }
    gpus
}

// ---------------------------------------------------------------- 通用工具

#[cfg(any(test, target_os = "linux", target_os = "freebsd"))]
fn vendor_name_from_pci_id(vendor_id: u16) -> &'static str {
    match vendor_id {
        0x10de => "NVIDIA",
        0x1002 => "AMD",
        0x8086 => "Intel",
        0x106b => "Apple",
        0x13b5 => "ARM",
        0x1af4 | 0x1b36 => "Red Hat",
        0x15ad => "VMware",
        0x1234 => "Bochs",
        0x1414 => "Microsoft",
        _ => "",
    }
}

#[cfg(any(test, target_os = "linux", target_os = "freebsd"))]
fn fallback_gpu_name(vendor: &str, vendor_id: u16, device_id: u16) -> String {
    if vendor.is_empty() {
        format!("GPU {vendor_id:04x}:{device_id:04x}")
    } else {
        format!("{vendor} GPU")
    }
}

#[cfg(any(test, target_os = "windows", target_os = "macos"))]
fn vendor_from_gpu_name(name: &str) -> &'static str {
    let lowered = name.to_ascii_lowercase();
    if lowered.contains("nvidia") {
        "NVIDIA"
    } else if lowered.contains("amd")
        || lowered.contains("radeon")
        || lowered.contains("ati")
        || lowered.contains("advanced micro devices")
    {
        "AMD"
    } else if lowered.contains("intel") {
        "Intel"
    } else if lowered.contains("apple") {
        "Apple"
    } else if lowered.contains("qualcomm") || lowered.contains("adreno") {
        "Qualcomm"
    } else if lowered.contains("microsoft") {
        "Microsoft"
    } else {
        ""
    }
}

/// `Win32_VideoController.AdapterRAM` 是 32 位字段，4 GiB 及以上的显卡会回绕成
/// 接近 4 GiB 的假值；与其显示错误容量，不如当作未知。
#[cfg(any(test, target_os = "windows"))]
fn is_plausible_adapter_memory(bytes: &u64) -> bool {
    *bytes > 0 && *bytes < 4 * 1024 * 1024 * 1024 - 1024 * 1024
}

fn parse_number(value: &str) -> Option<f64> {
    let value = value.trim();
    if value.is_empty() || value == "N/A" || value == "[N/A]" {
        return None;
    }
    value.parse::<f64>().ok()
}

fn parse_mebibytes(value: &str) -> Option<u64> {
    parse_number(value).map(|mebibytes| (mebibytes.max(0.) as u64).saturating_mul(1024 * 1024))
}

#[cfg(any(test, target_os = "macos"))]
fn parse_size(value: &str) -> Option<u64> {
    let mut parts = value.split_whitespace();
    let amount = parse_number(parts.next()?)?;
    let multiplier = match parts.next().unwrap_or("").to_ascii_lowercase().as_str() {
        "tb" => 1024u64.pow(4),
        "gb" => 1024u64.pow(3),
        "mb" => 1024u64.pow(2),
        "kb" => 1024,
        _ => return None,
    };
    Some((amount.max(0.) * multiplier as f64) as u64)
}

fn optional_text(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() || value == "N/A" || value == "[N/A]" {
        None
    } else {
        Some(value.to_string())
    }
}

#[cfg(any(test, target_os = "linux", target_os = "freebsd"))]
fn read_whole_number(path: impl AsRef<Path>) -> Option<u64> {
    let text = std::fs::read_to_string(path).ok()?;
    text.trim().parse::<u64>().ok()
}

fn new_command(program: &str) -> std::process::Command {
    #[cfg_attr(not(target_os = "windows"), allow(unused_mut))]
    let mut command = std::process::Command::new(program);
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt as _;
        // 探测命令不应弹出控制台窗口。
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command
}

/// 有界运行外部命令：等待时间超过 [`COMMAND_TIMEOUT`] 就杀掉子进程并返回 `None`。
///
/// 采样始终发生在 `gpui` 后台执行器里，这里的同步等待只会占用一个后台线程。
#[allow(
    clippy::disallowed_methods,
    reason = "hardware probes run off the UI thread and are bounded by a timeout"
)]
fn run_command_with_timeout(mut command: std::process::Command) -> Option<std::process::Output> {
    use std::process::Stdio;

    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = command.spawn().ok()?;
    let deadline = Instant::now() + COMMAND_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Ok(None) => {
                if let Err(error) = child.kill() {
                    log::debug!("failed to kill a timed out hardware probe: {error}");
                }
                if let Err(error) = child.wait() {
                    log::debug!("failed to reap a timed out hardware probe: {error}");
                }
                return None;
            }
            Err(error) => {
                log::debug!("failed to wait for a hardware probe: {error}");
                return None;
            }
        }
    }
    child.wait_with_output().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reading(label: &str, celsius: f32) -> TemperatureReading {
        TemperatureReading {
            label: label.to_string(),
            source: classify_temperature(label).expect("测试标签应可归类"),
            celsius,
        }
    }

    #[test]
    fn classifies_common_sensor_labels() {
        assert_eq!(
            classify_temperature("coretemp Package id 0"),
            Some(TemperatureSource::Cpu)
        );
        assert_eq!(
            classify_temperature("k10temp Tctl"),
            Some(TemperatureSource::Cpu)
        );
        assert_eq!(
            classify_temperature("amdgpu edge"),
            Some(TemperatureSource::Gpu)
        );
        assert_eq!(
            classify_temperature("nvme Composite"),
            Some(TemperatureSource::Storage)
        );
        assert_eq!(
            classify_temperature("iwlwifi_1 temp1"),
            Some(TemperatureSource::Network)
        );
        assert_eq!(
            classify_temperature("acpitz temp1"),
            Some(TemperatureSource::Board)
        );
        assert_eq!(
            classify_temperature("Computer"),
            Some(TemperatureSource::Board)
        );
        assert_eq!(classify_temperature("BAT0 temp1"), None);
        assert_eq!(
            classify_temperature("something odd"),
            Some(TemperatureSource::Other)
        );
    }

    #[test]
    fn keeps_one_reading_per_component() {
        let selected = select_temperatures([
            reading("coretemp Core 0", 61.),
            reading("coretemp Core 1", 72.),
            reading("coretemp Package id 0", 66.),
            reading("amdgpu edge", 55.),
            reading("amdgpu junction", 71.),
            reading("nvme Composite", 41.),
            reading("iwlwifi_1 temp1", 39.),
            reading("pch_thermal temp1", 44.),
        ]);
        let labels: Vec<&str> = selected
            .iter()
            .map(|reading| reading.label.as_str())
            .collect();
        // CPU 取封装温度而不是更热的某个核心。
        assert_eq!(labels.first(), Some(&"coretemp Package id 0"));
        assert_eq!(
            selected
                .iter()
                .map(|reading| reading.source)
                .collect::<Vec<_>>(),
            [
                TemperatureSource::Cpu,
                TemperatureSource::Gpu,
                TemperatureSource::Storage,
                TemperatureSource::Board,
                TemperatureSource::Network,
            ]
        );
    }

    #[test]
    fn falls_back_to_hottest_core_without_a_package_sensor() {
        let selected = select_temperatures([
            reading("coretemp Core 0", 61.),
            reading("coretemp Core 1", 72.),
        ]);
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].label, "coretemp Core 1");
    }

    #[test]
    fn drops_battery_and_implausible_readings() {
        assert_eq!(classify_temperature("BAT1 temp1"), None);
        assert!(!is_plausible_celsius(f32::NAN));
        assert!(!is_plausible_celsius(-5.));
        assert!(!is_plausible_celsius(200.));
        assert!(is_plausible_celsius(66.));
    }

    #[test]
    fn drops_the_acpi_thermal_zone_when_real_sensors_exist() {
        let with_cpu = select_temperatures([
            reading("coretemp Package id 0", 66.),
            reading("acpitz temp1", 27.),
        ]);
        assert_eq!(with_cpu.len(), 1);
        assert_eq!(with_cpu[0].label, "coretemp Package id 0");

        // 只有 ACPI 热区时仍然展示，例如没有 hwmon 的虚拟机或笔记本。
        let only_acpi = select_temperatures([reading("acpitz temp1", 27.)]);
        assert_eq!(only_acpi.len(), 1);
        assert_eq!(only_acpi[0].source, TemperatureSource::Board);
    }

    #[test]
    fn parses_nvidia_smi_output() {
        let parsed = parse_nvidia_smi_csv(
            "0, NVIDIA GeForce RTX 4090, 550.120, 24564, 1024, 12, 45\n\
             1, NVIDIA GeForce RTX 3090, 550.120, 24576, 0, 0, [N/A]\n",
        );
        assert_eq!(parsed.len(), 2);
        let first = &parsed[0].reading;
        assert_eq!(parsed[0].index, "0");
        assert_eq!(first.name, "NVIDIA GeForce RTX 4090");
        assert_eq!(first.vendor, "NVIDIA");
        assert_eq!(first.driver_version.as_deref(), Some("550.120"));
        assert_eq!(first.memory_total_bytes, Some(24564 * 1024 * 1024));
        assert_eq!(first.memory_used_bytes, Some(1024 * 1024 * 1024));
        assert_eq!(first.utilization_percent, Some(12.));
        assert_eq!(first.temperature_celsius, Some(45.));
        assert_eq!(parsed[1].reading.temperature_celsius, None);
    }

    #[test]
    fn skips_unusable_nvidia_smi_lines() {
        assert!(parse_nvidia_smi_csv("").is_empty());
        assert!(parse_nvidia_smi_csv("0, , 550.120, 1, 1, 1, 1").is_empty());
        assert!(parse_nvidia_smi_csv("0, GPU").is_empty());
    }

    #[test]
    fn parses_windows_video_controllers() {
        let parsed = parse_windows_video_controllers(
            "NVIDIA GeForce RTX 4080\t31.0.15.3699\t4293918720\n\
             Intel(R) UHD Graphics\t31.0.101.4502\t1073741824\n\
             Microsoft Basic Display Adapter\t\t0\n",
        );
        assert_eq!(parsed.len(), 3);
        assert_eq!(parsed[0].vendor, "NVIDIA");
        assert_eq!(parsed[0].driver_version.as_deref(), Some("31.0.15.3699"));
        // 32 位字段回绕后的假容量按未知处理。
        assert_eq!(parsed[0].memory_total_bytes, None);
        assert_eq!(parsed[1].vendor, "Intel");
        assert_eq!(parsed[1].memory_total_bytes, Some(1024 * 1024 * 1024));
        assert_eq!(parsed[2].memory_total_bytes, None);
    }

    #[test]
    fn parses_windows_thermal_zones() {
        let parsed = parse_windows_thermal_zones("\\_TZ.TZ00\t3002\n\\_TZ.TZ01\t0\n");
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].label, "\\_TZ.TZ00");
        assert_eq!(parsed[0].source, TemperatureSource::Board);
        assert!((parsed[0].celsius - 27.05).abs() < 0.001);
    }

    #[test]
    fn parses_system_profiler_output() {
        let output = concat!(
            "Graphics/Displays:\n",
            "\n",
            "    Apple M2:\n",
            "\n",
            "      Chipset Model: Apple M2\n",
            "      Type: GPU\n",
            "      Vendor: Apple (0x106b)\n",
            "      Metal Support: Metal 3\n",
            "      Displays:\n",
            "        Color LCD:\n",
            "          Resolution: 2560 x 1664\n",
        );
        let parsed = parse_system_profiler_gpus(output);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].name, "Apple M2");
        assert_eq!(parsed[0].vendor, "Apple");
        assert_eq!(parsed[0].memory_total_bytes, None);
    }

    #[test]
    fn parses_system_profiler_vram_for_intel_gpus() {
        let output = concat!(
            "Graphics/Displays:\n",
            "\n",
            "    Intel HD Graphics 4000:\n",
            "\n",
            "      Chipset Model: Intel HD Graphics 4000\n",
            "      Vendor: Intel (0x8086)\n",
            "      VRAM (Dynamic, Max): 1536 MB\n",
        );
        let parsed = parse_system_profiler_gpus(output);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].vendor, "Intel");
        assert_eq!(parsed[0].memory_total_bytes, Some(1536 * 1024 * 1024));
    }

    #[test]
    fn parses_human_readable_sizes() {
        assert_eq!(parse_size("512 MB"), Some(512 * 1024 * 1024));
        assert_eq!(parse_size("8 GB"), Some(8 * 1024 * 1024 * 1024));
        assert_eq!(parse_size("1.5 GB"), Some(1610612736));
        assert_eq!(parse_size("nonsense"), None);
    }

    #[test]
    fn maps_wire_names_round_trip() {
        for source in TEMPERATURE_SOURCE_ORDER {
            assert_eq!(
                TemperatureSource::from_wire_name(source.wire_name()),
                source
            );
        }
        assert_eq!(
            TemperatureSource::from_wire_name("future-source"),
            TemperatureSource::Other
        );
    }

    /// 采样真实硬件：只要求不 panic，读数必须落在可信范围内。
    #[test]
    fn samples_the_current_machine() {
        let mut sampler = HardwareSampler::new();
        let readings = sampler.sample();
        for reading in &readings.temperatures {
            assert!(is_plausible_celsius(reading.celsius));
            assert!(!reading.label.is_empty());
        }
        for gpu in &readings.gpus {
            assert!(!gpu.name.is_empty());
            if let Some(percent) = gpu.utilization_percent {
                assert!((0.0..=100.0).contains(&percent));
            }
            if let Some(celsius) = gpu.temperature_celsius {
                assert!(is_plausible_celsius(celsius));
            }
            if let (Some(used), Some(total)) = (gpu.memory_used_bytes, gpu.memory_total_bytes) {
                assert!(used <= total);
            }
        }
    }
}
