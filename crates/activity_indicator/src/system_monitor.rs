use gpui::{
    Action, App, Context, Div, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, Hsla,
    InteractiveElement as _, PathBuilder, Render, StatefulInteractiveElement as _, Subscription,
    Task, WeakEntity, Window, actions, canvas, point, px,
};
use proto::GetSystemStatsResponse;
use std::{collections::VecDeque, time::Duration};
use sysinfo::{Disks, Networks, ProcessesToUpdate, System};
use terminal_view::port_forwarding::{ForwardDirection, ForwardSnapshot, ForwardSource, ForwardStatus, PortForwardManager};
use ui::{ProgressBar, prelude::*};
use workspace::{
    Panel, StatusItemView, Workspace,
    dock::{DockPosition, PanelEvent},
    item::ItemHandle,
};

const REFRESH_INTERVAL: Duration = Duration::from_secs(2);
const HISTORY_LENGTH: usize = 30;

pub const SYSTEM_MONITOR_PANEL_KEY: &str = "SystemMonitorPanel";

actions!(system_monitor, [ToggleFocus]);

#[derive(Clone, Default)]
pub struct SystemStats {
    pub hostname: String,
    pub os_name: String,
    pub kernel_version: String,
    pub uptime_seconds: u64,
    pub cpu_usage_percent: f32,
    pub cpu_core_usage_percent: Vec<f32>,
    pub memory_used_bytes: u64,
    pub memory_total_bytes: u64,
    pub swap_used_bytes: u64,
    pub swap_total_bytes: u64,
    pub disk_used_bytes: u64,
    pub disk_total_bytes: u64,
    pub network_received_bytes_per_second: u64,
    pub network_transmitted_bytes_per_second: u64,
    pub process_count: u32,
    pub load_average: [f64; 3],
}

impl From<GetSystemStatsResponse> for SystemStats {
    fn from(stats: GetSystemStatsResponse) -> Self {
        Self {
            hostname: stats.hostname,
            os_name: stats.os_name,
            kernel_version: stats.kernel_version,
            uptime_seconds: stats.uptime_seconds,
            cpu_usage_percent: stats.cpu_usage_percent,
            cpu_core_usage_percent: stats.cpu_core_usage_percent,
            memory_used_bytes: stats.memory_used_bytes,
            memory_total_bytes: stats.memory_total_bytes,
            swap_used_bytes: stats.swap_used_bytes,
            swap_total_bytes: stats.swap_total_bytes,
            disk_used_bytes: stats.disk_used_bytes,
            disk_total_bytes: stats.disk_total_bytes,
            network_received_bytes_per_second: stats.network_received_bytes_per_second,
            network_transmitted_bytes_per_second: stats.network_transmitted_bytes_per_second,
            process_count: stats.process_count,
            load_average: [
                stats.load_average_one,
                stats.load_average_five,
                stats.load_average_fifteen,
            ],
        }
    }
}

struct LocalSampler {
    system: System,
    disks: Disks,
    networks: Networks,
    last_sample: std::time::Instant,
}

impl LocalSampler {
    fn new() -> Self {
        let mut system = System::new();
        system.refresh_cpu_all();
        system.refresh_memory();
        system.refresh_processes(ProcessesToUpdate::All, true);
        Self {
            system,
            disks: Disks::new_with_refreshed_list(),
            networks: Networks::new_with_refreshed_list(),
            last_sample: std::time::Instant::now(),
        }
    }

    fn sample(&mut self) -> SystemStats {
        let elapsed = self.last_sample.elapsed().as_secs_f64().max(0.001);
        self.system.refresh_cpu_usage();
        self.system.refresh_memory();
        self.system.refresh_processes(ProcessesToUpdate::All, true);
        self.disks.refresh(true);
        self.networks.refresh(true);
        self.last_sample = std::time::Instant::now();

        let disk_total_bytes: u64 = self.disks.iter().map(|disk| disk.total_space()).sum();
        let disk_available_bytes: u64 = self.disks.iter().map(|disk| disk.available_space()).sum();
        let load = System::load_average();
        let received: u64 = self.networks.values().map(|data| data.received()).sum();
        let transmitted: u64 = self.networks.values().map(|data| data.transmitted()).sum();

        SystemStats {
            hostname: System::host_name().unwrap_or_else(|| "本机".into()),
            os_name: System::long_os_version()
                .or_else(System::name)
                .unwrap_or_else(|| "未知系统".into()),
            kernel_version: System::kernel_version().unwrap_or_default(),
            uptime_seconds: System::uptime(),
            cpu_usage_percent: self.system.global_cpu_usage(),
            cpu_core_usage_percent: self
                .system
                .cpus()
                .iter()
                .map(|cpu| cpu.cpu_usage())
                .collect(),
            memory_used_bytes: self.system.used_memory(),
            memory_total_bytes: self.system.total_memory(),
            swap_used_bytes: self.system.used_swap(),
            swap_total_bytes: self.system.total_swap(),
            disk_used_bytes: disk_total_bytes.saturating_sub(disk_available_bytes),
            disk_total_bytes,
            network_received_bytes_per_second: (received as f64 / elapsed) as u64,
            network_transmitted_bytes_per_second: (transmitted as f64 / elapsed) as u64,
            process_count: self.system.processes().len().try_into().unwrap_or(u32::MAX),
            load_average: [load.one, load.five, load.fifteen],
        }
    }
}

pub struct SystemMonitor {
    stats: Option<SystemStats>,
    cpu_history: VecDeque<f32>,
    download_history: VecDeque<u64>,
    upload_history: VecDeque<u64>,
    remote_name: Option<String>,
    error: Option<String>,
    _refresh_task: Task<()>,
    right_dock: Entity<workspace::dock::Dock>,
    _dock_subscription: Subscription,
}

impl SystemMonitor {
    pub fn new(workspace: &Workspace, cx: &mut App) -> Entity<Self> {
        let remote_client = workspace.project().read(cx).remote_client();
        let remote_name = remote_client
            .as_ref()
            .map(|client| client.read(cx).connection_options().host());
        let right_dock = workspace.right_dock().clone();
        cx.new(|cx| {
            let dock_subscription = cx.observe(&right_dock, |_, _, cx| cx.notify());
            let refresh_task = cx.spawn({
                let remote_client = remote_client.clone();
                async move |this: WeakEntity<SystemMonitor>, cx| {
                    let mut local_sampler = remote_client.is_none().then(LocalSampler::new);
                    loop {
                        let result = if let Some(remote_client) = remote_client.as_ref() {
                            if remote_client
                                .read_with(cx, |client, _| client.supports_system_stats())
                            {
                                let request =
                                    remote_client.read_with(cx, |client, _| client.system_stats());
                                request.await.map(SystemStats::from)
                            } else {
                                Err(anyhow::anyhow!(
                                    "远程服务不支持系统监控，请选择 Zed CN 远程服务并重新连接"
                                ))
                            }
                        } else if let Some(sampler) = local_sampler.take() {
                            let (sampler, stats) = cx
                                .background_spawn(async move {
                                    let mut sampler = sampler;
                                    let stats = sampler.sample();
                                    (sampler, stats)
                                })
                                .await;
                            local_sampler = Some(sampler);
                            Ok(stats)
                        } else {
                            Err(anyhow::anyhow!("本机监控采样器不可用"))
                        };
                        if this
                            .update(cx, |this, cx| this.apply_sample(result, cx))
                            .is_err()
                        {
                            break;
                        }
                        cx.background_executor().timer(REFRESH_INTERVAL).await;
                    }
                }
            });
            Self {
                stats: None,
                cpu_history: VecDeque::with_capacity(HISTORY_LENGTH),
                download_history: VecDeque::with_capacity(HISTORY_LENGTH),
                upload_history: VecDeque::with_capacity(HISTORY_LENGTH),
                remote_name,
                error: None,
                _refresh_task: refresh_task,
                right_dock,
                _dock_subscription: dock_subscription,
            }
        })
    }

    fn apply_sample(&mut self, result: anyhow::Result<SystemStats>, cx: &mut Context<Self>) {
        match result {
            Ok(stats) => {
                push_history(&mut self.cpu_history, stats.cpu_usage_percent);
                push_history(
                    &mut self.download_history,
                    stats.network_received_bytes_per_second,
                );
                push_history(
                    &mut self.upload_history,
                    stats.network_transmitted_bytes_per_second,
                );
                self.stats = Some(stats);
                self.error = None;
            }
            Err(error) => self.error = Some(format!("{error:#}")),
        }
        cx.notify();
    }

    fn target_label(&self) -> String {
        self.remote_name
            .as_ref()
            .map(|name| format!("远程 · {name}"))
            .unwrap_or_else(|| "本机".into())
    }

    fn tooltip_element(&self) -> gpui::AnyElement {
        let content = if let Some(stats) = self.stats.as_ref() {
            v_flex()
                .w(px(280.))
                .min_w_0()
                .gap_2()
                .child(
                    h_flex()
                        .min_w_0()
                        .justify_between()
                        .gap_2()
                        .child(Label::new(format!("{}运行状态", self.target_label())))
                        .child(
                            Label::new(stats.hostname.clone())
                                .size(LabelSize::Small)
                                .color(Color::Muted)
                                .truncate(),
                        ),
                )
                .child(metric_line(
                    "CPU",
                    format!("{:.0}%", stats.cpu_usage_percent),
                ))
                .child(metric_line(
                    "内存",
                    format!(
                        "{} / {}",
                        format_bytes(stats.memory_used_bytes),
                        format_bytes(stats.memory_total_bytes)
                    ),
                ))
                .child(metric_line(
                    "磁盘",
                    format!(
                        "{} / {}",
                        format_bytes(stats.disk_used_bytes),
                        format_bytes(stats.disk_total_bytes)
                    ),
                ))
                .child(metric_line(
                    "网络",
                    format!(
                        "↓ {}/s  ↑ {}/s",
                        format_bytes(stats.network_received_bytes_per_second),
                        format_bytes(stats.network_transmitted_bytes_per_second)
                    ),
                ))
                .child(
                    Label::new("点击打开右侧系统监控")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .into_any_element()
        } else {
            v_flex()
                .w(px(280.))
                .min_w_0()
                .gap_1()
                .child(Label::new(format!("{}运行状态", self.target_label())))
                .child(
                    Label::new(
                        self.error
                            .clone()
                            .unwrap_or_else(|| "正在读取系统状态…".into()),
                    )
                    .size(LabelSize::Small)
                    .color(if self.error.is_some() {
                        Color::Error
                    } else {
                        Color::Muted
                    }),
                )
                .into_any_element()
        };
        content
    }
}

impl Render for SystemMonitor {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let monitor = cx.entity();
        let dock = self.right_dock.read(cx);
        let open = dock
            .visible_panel()
            .is_some_and(|panel| panel.panel_key() == SYSTEM_MONITOR_PANEL_KEY);
        IconButton::new("system-monitor-status", IconName::Gauge)
            .icon_size(IconSize::Small)
            .icon_color(Color::Muted)
            .selected_icon_color(Color::Accent)
            .toggle_state(open)
            .aria_label("系统监控")
            .aria_expanded(open)
            .tooltip(ui::Tooltip::element(move |_, cx| {
                monitor.read(cx).tooltip_element()
            }))
            .on_click(|_, window, cx| {
                window.dispatch_action(ToggleFocus.boxed_clone(), cx);
            })
    }
}

impl StatusItemView for SystemMonitor {
    fn set_active_pane_item(
        &mut self,
        _: Option<&dyn ItemHandle>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) {
    }

    fn hide_setting(&self, _: &App) -> Option<workspace::HideStatusItem> {
        None
    }
}

pub struct SystemMonitorPanel {
    monitor: Entity<SystemMonitor>,
    port_forward_manager: Option<Entity<PortForwardManager>>,
    focus_handle: FocusHandle,
    _port_forward_subscription: Option<Subscription>,
}

impl SystemMonitorPanel {
    pub fn new(
        monitor: Entity<SystemMonitor>,
        port_forward_manager: Option<Entity<PortForwardManager>>,
        cx: &mut Context<Self>,
    ) -> Self {
        let port_forward_subscription = port_forward_manager
            .as_ref()
            .map(|manager| cx.observe(manager, |_, _, cx| cx.notify()));
        Self {
            monitor,
            port_forward_manager,
            focus_handle: cx.focus_handle(),
            _port_forward_subscription: port_forward_subscription,
        }
    }

    pub fn set_port_forward_manager(
        &mut self,
        port_forward_manager: Entity<PortForwardManager>,
        cx: &mut Context<Self>,
    ) {
        self._port_forward_subscription = Some(cx.observe(
            &port_forward_manager,
            |_, _, cx| cx.notify(),
        ));
        self.port_forward_manager = Some(port_forward_manager);
        cx.notify();
    }
}

impl EventEmitter<PanelEvent> for SystemMonitorPanel {}

impl Focusable for SystemMonitorPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Panel for SystemMonitorPanel {
    fn persistent_name() -> &'static str {
        "System Monitor"
    }

    fn panel_key() -> &'static str {
        SYSTEM_MONITOR_PANEL_KEY
    }

    fn position(&self, _: &Window, _: &App) -> DockPosition {
        DockPosition::Right
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        position == DockPosition::Right
    }

    fn set_position(&mut self, _: DockPosition, _: &mut Window, _: &mut Context<Self>) {}

    fn default_size(&self, _: &Window, _: &App) -> gpui::Pixels {
        px(340.)
    }

    fn icon(&self, _: &Window, _: &App) -> Option<IconName> {
        Some(IconName::Gauge)
    }

    fn icon_tooltip(&self, _: &Window, _: &App) -> Option<&'static str> {
        Some("系统监控")
    }

    fn toggle_action(&self) -> Box<dyn gpui::Action> {
        ToggleFocus.boxed_clone()
    }

    fn activation_priority(&self) -> u32 {
        7
    }
}

impl Render for SystemMonitorPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let monitor = self.monitor.read(cx);
        let stats = monitor.stats.as_ref();
        let forwarded_ports = self
            .port_forward_manager
            .as_ref()
            .map(|manager| manager.read(cx).snapshots())
            .unwrap_or_default();
        v_flex()
            .id("system-monitor-panel")
            .size_full()
            .track_focus(&self.focus_handle)
            .overflow_x_hidden()
            .overflow_y_scroll()
            .p_3()
            .gap_2()
            .bg(cx.theme().colors().panel_background)
            .child(
                h_flex()
                    .w_full()
                    .min_w_0()
                    .justify_between()
                    .gap_2()
                    .child(
                        h_flex()
                            .flex_none()
                            .gap_1p5()
                            .child(
                                Icon::new(IconName::Gauge)
                                    .size(IconSize::Small)
                                    .color(Color::Accent),
                            )
                            .child(Label::new("系统监控").weight(FontWeight::MEDIUM)),
                    )
                    .child(
                        Label::new(monitor.target_label())
                            .size(LabelSize::Small)
                            .color(Color::Muted)
                            .truncate(),
                    ),
            )
            .when_some(stats, |element, stats| {
                element
                    .child(system_card(stats, cx))
                    .child(resource_card(
                        "CPU",
                        IconName::Gauge,
                        stats.cpu_usage_percent,
                        format!("{:.1}%", stats.cpu_usage_percent),
                        Some(
                            sparkline(
                                &monitor.cpu_history,
                                100.,
                                24.,
                                usage_color(stats.cpu_usage_percent, cx),
                            )
                            .w_full()
                            .into_any_element(),
                        ),
                        cx,
                    ))
                    .child(resource_card(
                        "内存",
                        IconName::DatabaseZap,
                        percentage(stats.memory_used_bytes, stats.memory_total_bytes),
                        format!(
                            "{} / {}",
                            format_bytes(stats.memory_used_bytes),
                            format_bytes(stats.memory_total_bytes)
                        ),
                        None,
                        cx,
                    ))
                    .child(network_card(stats, monitor, cx))
                    .child(resource_card(
                        "磁盘",
                        IconName::Server,
                        percentage(stats.disk_used_bytes, stats.disk_total_bytes),
                        format!(
                            "{} / {}",
                            format_bytes(stats.disk_used_bytes),
                            format_bytes(stats.disk_total_bytes)
                        ),
                        None,
                        cx,
                    ))
            })
            .when(!forwarded_ports.is_empty(), |element| {
                element.child(port_forwarding_card(&forwarded_ports, cx))
            })
            .when(stats.is_none(), |element| {
                element.child(
                    card(cx).child(
                        Label::new(
                            monitor
                                .error
                                .clone()
                                .unwrap_or_else(|| "正在读取系统状态…".into()),
                        )
                        .color(if monitor.error.is_some() {
                            Color::Error
                        } else {
                            Color::Muted
                        }),
                    ),
                )
            })
    }
}

fn card(cx: &App) -> Div {
    v_flex()
        .w_full()
        .min_w_0()
        .p_3()
        .gap_2()
        .border_1()
        .border_color(cx.theme().colors().border_variant)
        .rounded_lg()
        .bg(cx.theme().colors().element_background)
}

fn usage_color(percent: f32, cx: &App) -> Hsla {
    let status = cx.theme().status();
    if percent >= 90. {
        status.error
    } else if percent >= 75. {
        status.warning
    } else {
        status.info
    }
}

fn system_card(stats: &SystemStats, cx: &App) -> impl IntoElement {
    card(cx)
        .gap_1()
        .child(
            h_flex()
                .w_full()
                .min_w_0()
                .justify_between()
                .gap_2()
                .child(
                    Label::new(stats.hostname.clone())
                        .weight(FontWeight::MEDIUM)
                        .truncate(),
                )
                .child(
                    Label::new(format_uptime(stats.uptime_seconds))
                        .size(LabelSize::Small)
                        .color(Color::Accent)
                        .flex_none(),
                ),
        )
        .child(
            Label::new(stats.os_name.clone())
                .size(LabelSize::Small)
                .color(Color::Muted)
                .truncate(),
        )
        .when(!stats.kernel_version.is_empty(), |element| {
            element.child(
                Label::new(format!("内核 {}", stats.kernel_version))
                    .size(LabelSize::Small)
                    .color(Color::Muted)
                    .truncate(),
            )
        })
        .child(metric_line("进程", stats.process_count.to_string()))
        .child(metric_line(
            "负载",
            format!(
                "{:.2} · {:.2} · {:.2}",
                stats.load_average[0], stats.load_average[1], stats.load_average[2]
            ),
        ))
}

fn resource_card(
    title: &'static str,
    icon: IconName,
    percent: f32,
    value: String,
    history: Option<gpui::AnyElement>,
    cx: &App,
) -> impl IntoElement {
    card(cx)
        .child(
            h_flex()
                .w_full()
                .min_w_0()
                .justify_between()
                .gap_2()
                .child(
                    h_flex()
                        .min_w_0()
                        .gap_1p5()
                        .child(Icon::new(icon).size(IconSize::Small).color(Color::Accent))
                        .child(Label::new(title).truncate()),
                )
                .child(
                    Label::new(value)
                        .size(LabelSize::Small)
                        .color(Color::Muted)
                        .flex_none(),
                ),
        )
        .when_some(history, |element, history| element.child(history))
        .child(ProgressBar::new(title, percent, 100., cx).fg_color(usage_color(percent, cx)))
}

fn port_forwarding_card(entries: &[ForwardSnapshot], cx: &App) -> impl IntoElement {
    card(cx)
        .child(
            h_flex()
                .w_full()
                .min_w_0()
                .gap_1p5()
                .child(
                    Icon::new(IconName::Link)
                        .size(IconSize::Small)
                        .color(Color::Accent),
                )
                .child(Label::new("端口转发")),
        )
        .children(entries.iter().map(|entry| {
            let direction = match entry.direction {
                ForwardDirection::RemoteToLocal => "远程 → 本地",
                ForwardDirection::LocalToRemote => "本地 → 远程",
            };
            let source = match entry.source {
                ForwardSource::Automatic => "自动",
                ForwardSource::Manual => "手动",
                ForwardSource::Preview => "网页预览",
            };
            let local_port = entry
                .local_port
                .map(|port| port.to_string())
                .unwrap_or_else(|| "待分配".into());
            let ports = match entry.direction {
                ForwardDirection::RemoteToLocal => {
                    format!("{} → {local_port}", entry.remote_port)
                }
                ForwardDirection::LocalToRemote => {
                    format!("{local_port} → {}", entry.remote_port)
                }
            };
            let (status, status_color) = match entry.status {
                ForwardStatus::Starting => ("启动中", Color::Muted),
                ForwardStatus::RunningUnconfirmed => ("运行中", Color::Success),
                ForwardStatus::Failed(_) => ("失败", Color::Error),
            };
            v_flex()
                .w_full()
                .min_w_0()
                .gap_0p5()
                .child(
                    h_flex()
                        .w_full()
                        .min_w_0()
                        .justify_between()
                        .gap_2()
                        .child(
                            Label::new(format!("{direction} · {source}"))
                                .size(LabelSize::XSmall)
                                .color(Color::Muted)
                                .truncate(),
                        )
                        .child(
                            Label::new(status)
                                .size(LabelSize::XSmall)
                                .color(status_color)
                                .flex_none(),
                        ),
                )
                .child(Label::new(ports).size(LabelSize::Small).truncate())
        }))
}

fn network_card(stats: &SystemStats, monitor: &SystemMonitor, cx: &App) -> impl IntoElement {
    let maximum = monitor
        .download_history
        .iter()
        .chain(monitor.upload_history.iter())
        .copied()
        .max()
        .unwrap_or(1)
        .max(1) as f32;
    card(cx)
        .child(
            h_flex()
                .w_full()
                .min_w_0()
                .gap_1p5()
                .child(
                    Icon::new(IconName::Public)
                        .size(IconSize::Small)
                        .color(Color::Accent),
                )
                .child(Label::new("网络")),
        )
        .child(network_row(
            "下载",
            &monitor.download_history,
            maximum,
            stats.network_received_bytes_per_second,
            cx.theme().status().info,
        ))
        .child(network_row(
            "上传",
            &monitor.upload_history,
            maximum,
            stats.network_transmitted_bytes_per_second,
            cx.theme().status().success,
        ))
}

fn network_row(
    label: &'static str,
    history: &VecDeque<u64>,
    maximum: f32,
    current: u64,
    color: Hsla,
) -> impl IntoElement {
    let values: VecDeque<f32> = history.iter().map(|value| *value as f32).collect();
    h_flex()
        .w_full()
        .min_w_0()
        .items_center()
        .gap_2()
        .child(
            Label::new(label)
                .size(LabelSize::Small)
                .color(Color::Muted)
                .flex_none(),
        )
        .child(
            sparkline(&values, maximum, 16., color)
                .flex_1()
                .min_w(px(24.)),
        )
        .child(
            Label::new(format!("{}/s", format_bytes(current)))
                .size(LabelSize::Small)
                .flex_none(),
        )
}

fn sparkline(values: &VecDeque<f32>, maximum: f32, height: f32, color: Hsla) -> Div {
    let values = values.iter().copied().collect::<Vec<_>>();
    div().h(px(height)).relative().child(
        canvas(
            |_, _, _| {},
            move |bounds, _, window, _| {
                if values.len() < 2 || maximum <= 0. {
                    return;
                }
                let last_index = (values.len() - 1) as f32;
                let mut builder = PathBuilder::stroke(px(1.5));
                for (index, value) in values.iter().enumerate() {
                    let x = bounds.origin.x + bounds.size.width * (index as f32 / last_index);
                    let normalized = (*value / maximum).clamp(0., 1.);
                    let y = bounds.origin.y + bounds.size.height * (1. - normalized);
                    if index == 0 {
                        builder.move_to(point(x, y));
                    } else {
                        builder.line_to(point(x, y));
                    }
                }
                if let Ok(path) = builder.build() {
                    window.paint_path(path, color);
                }
            },
        )
        .absolute()
        .size_full(),
    )
}

fn metric_line(label: &'static str, value: String) -> impl IntoElement {
    h_flex()
        .w_full()
        .min_w_0()
        .justify_between()
        .gap_2()
        .child(
            Label::new(label)
                .size(LabelSize::Small)
                .color(Color::Muted)
                .truncate(),
        )
        .child(Label::new(value).size(LabelSize::Small).flex_none())
}

fn push_history<T>(history: &mut VecDeque<T>, value: T) {
    if history.len() == HISTORY_LENGTH {
        history.pop_front();
    }
    history.push_back(value);
}

fn percentage(used: u64, total: u64) -> f32 {
    if total == 0 {
        0.
    } else {
        used as f32 / total as f32 * 100.
    }
}

fn format_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.;
    const MIB: f64 = KIB * 1024.;
    const GIB: f64 = MIB * 1024.;
    let bytes = bytes as f64;
    if bytes >= GIB {
        format!("{:.1} GiB", bytes / GIB)
    } else if bytes >= MIB {
        format!("{:.1} MiB", bytes / MIB)
    } else if bytes >= KIB {
        format!("{:.1} KiB", bytes / KIB)
    } else {
        format!("{} B", bytes as u64)
    }
}

fn format_uptime(seconds: u64) -> String {
    let days = seconds / 86_400;
    let hours = seconds % 86_400 / 3_600;
    let minutes = seconds % 3_600 / 60;
    if days > 0 {
        format!("已运行 {days} 天 {hours} 小时")
    } else if hours > 0 {
        format!("已运行 {hours} 小时 {minutes} 分")
    } else {
        format!("已运行 {minutes} 分钟")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_uptime_and_bytes_for_compact_surfaces() {
        assert_eq!(format_uptime(90), "已运行 1 分钟");
        assert_eq!(format_uptime(3_900), "已运行 1 小时 5 分");
        assert_eq!(format_uptime(180_000), "已运行 2 天 2 小时");
        assert_eq!(format_bytes(1_536), "1.5 KiB");
        assert_eq!(format_bytes(2 * 1024 * 1024 * 1024), "2.0 GiB");
    }

    #[test]
    fn history_stays_bounded() {
        let mut history = VecDeque::new();
        for value in 0..HISTORY_LENGTH + 5 {
            push_history(&mut history, value);
        }
        assert_eq!(history.len(), HISTORY_LENGTH);
        assert_eq!(history.front(), Some(&5));
        assert_eq!(history.back(), Some(&(HISTORY_LENGTH + 4)));
    }

    #[test]
    fn maps_remote_statistics_without_losing_fields() {
        let stats = SystemStats::from(GetSystemStatsResponse {
            hostname: "dev-host".into(),
            os_name: "Linux".into(),
            kernel_version: "6.8".into(),
            uptime_seconds: 42,
            cpu_usage_percent: 37.5,
            cpu_core_usage_percent: vec![25., 50.],
            memory_used_bytes: 4,
            memory_total_bytes: 8,
            swap_used_bytes: 1,
            swap_total_bytes: 2,
            disk_used_bytes: 10,
            disk_total_bytes: 20,
            network_received_bytes_per_second: 30,
            network_transmitted_bytes_per_second: 40,
            process_count: 50,
            load_average_one: 0.5,
            load_average_five: 0.25,
            load_average_fifteen: 0.125,
            sampled_at_unix_seconds: 60,
        });
        assert_eq!(stats.hostname, "dev-host");
        assert_eq!(stats.cpu_core_usage_percent, [25., 50.]);
        assert_eq!(stats.load_average, [0.5, 0.25, 0.125]);
        assert_eq!(stats.network_transmitted_bytes_per_second, 40);
    }
}
