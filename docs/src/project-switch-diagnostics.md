# 项目切换异常诊断日志

桌面客户端会在普通日志所在目录自动保存 `Zed.diagnostics.log`，无需启用 Debug 日志。从终端启动也会保留文件日志。

发生卡住或闪退后，优先复制以下文件（不必等待再次复现）：

- `Zed.diagnostics.log`
- `Zed.diagnostics.log.old`（如果存在）
- `Zed.log` 与 `Zed.log.old`

通过命令面板的「在文件管理器中显示日志」（`workspace: reveal log in file manager`）打开日志目录。应用无法启动时，Windows 通常位于 `%LOCALAPPDATA%\Zed\logs`，Linux 通常位于 `~/.local/share/zed/logs`，macOS 通常位于 `~/Library/Logs/Zed`。自定义数据目录会改变位置。

诊断日志单份约 4 MiB，保留一份轮转旧文件；持续使用仍会覆盖更早的记录，异常后应尽早复制。记录包含时间、进程 ID、操作 ID、阶段、耗时、工作区／标签页内部 ID 和标签页类型，不包含文件正文、密码或项目绝对路径。日志不会自动上传。

主要操作：`project_picker_confirm`、`find_or_create_workspace`（仅入口）、`open_local_workspace`、`activate_workspace`、`restore_workspace`、`restore_item`。`complete` 表示该操作执行结束，恢复项另有 `result=ok/error`；`incomplete` 表示提前退出（可能是普通错误、取消或 panic，不能一律解释为崩溃）。异常强退时可能只有 `begin` 或最后一个阶段，没有结束标记。操作 ID 在同一进程内关联；不同启动需结合时间和进程 ID 区分。

每条检查点同步写入本机磁盘，远程项目的诊断也保存在客户端，不访问远程文件或更改服务端协议。低速或异常磁盘可能增加切换耗时。断电、磁盘写满、系统强杀和内存耗尽仍不能保证生成最终原因或堆栈；本日志不是崩溃转储，也不会在卡死时自动弹窗。

人工验证建议：本地与 SSH 项目各切换一次，关闭后恢复包含多个标签页的项目，检查上述阶段；从终端启动后检查文件日志存在；在测试环境终止进程并重启，确认先前检查点仍在当前或旧文件。Windows/macOS、真实 SSH 与原生 GUI 需在对应平台验证。
