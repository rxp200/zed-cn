use anyhow::{Context as _, bail};
use editor::{Editor, RunCode, RunFile, RunSelection};
use gpui::{Context, Window};
use project::{TaskSourceKind, trusted_worktrees::TrustedWorktrees};
use task::{RevealStrategy, SaveStrategy, Shell, TaskContext, TaskTemplate, VariableName};
use util::ResultExt as _;
use workspace::Workspace;

pub(super) fn run_file(
    workspace: &mut Workspace,
    _: &RunFile,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    run(workspace, false, false, window, cx);
}

pub(super) fn run_code(
    workspace: &mut Workspace,
    _: &RunCode,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    run(workspace, true, false, window, cx);
}

pub(super) fn run_selection(
    workspace: &mut Workspace,
    _: &RunSelection,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    run(workspace, true, true, window, cx);
}

fn run(
    workspace: &mut Workspace,
    use_selection: bool,
    require_selection: bool,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let project = workspace.project().clone();
    if project.read(cx).is_disconnected(cx) {
        workspace.show_error("无法运行代码：远程连接已断开，请先重新连接。", cx);
        return;
    }
    if project.read(cx).is_via_collab()
        || TrustedWorktrees::has_restricted_worktrees(&project.read(cx).worktree_store(), cx)
    {
        workspace.show_error("无法运行代码：请在受信任的本地或远程项目中运行。", cx);
        return;
    }
    let Some(buffer) = workspace
        .active_item_as::<Editor>(cx)
        .and_then(|editor| editor.read(cx).buffer().read(cx).as_singleton())
    else {
        workspace.show_error("请先打开一个代码文件；多文件视图不支持直接运行。", cx);
        return;
    };
    if buffer.read(cx).file().is_none() {
        workspace.show_error("请先保存文件，再运行代码。", cx);
        return;
    }
    let language = buffer.read(cx).language().cloned();
    let windows = project.read(cx).path_style(cx).is_windows();
    let contexts = super::task_contexts(workspace, window, cx);
    let save = project.update(cx, |project, cx| project.save_buffer(buffer.clone(), cx));
    cx.spawn_in(window, async move |workspace, cx| {
        let result: anyhow::Result<()> = async {
            save.await.context("运行前保存文件失败")?;
            if buffer.read_with(cx, |buffer, _| buffer.is_dirty()) {
                bail!("文件在保存期间发生变化，请重新运行。");
            }
            let contexts = contexts.await;
            let context = contexts
                .active_item_context
                .as_ref()
                .map(|(_, _, context)| context)
                .context("无法获取当前文件的运行环境")?;
            let tasks = project.update(cx, |project, cx| {
                project
                    .task_store()
                    .read(cx)
                    .task_inventory()
                    .map(|inventory| {
                        inventory.read(cx).list_tasks(
                            Some(buffer.clone()),
                            language.clone(),
                            contexts.worktree(),
                            cx,
                        )
                    })
            });
            let selection = use_selection
                .then(|| context.task_variables.get(&VariableName::SelectedText))
                .flatten()
                .filter(|text| !text.trim().is_empty());
            if require_selection && selection.is_none() {
                bail!("请先选中需要运行的代码。");
            }
            let mut overrides = match tasks {
                Some(tasks) => tasks.await,
                None => Vec::new(),
            };
            let tag = if selection.is_some() {
                "run-selection"
            } else {
                "run-current"
            };
            overrides
                .retain(|(_, template)| template.tags.iter().any(|candidate| candidate == tag));
            // Project tasks take precedence over global and language defaults.
            overrides.sort_by_key(|(source, _)| match source {
                TaskSourceKind::Worktree { .. } => 0,
                TaskSourceKind::AbsPath { .. } => 1,
                _ => 2,
            });
            if let Some((source, _)) = overrides.first() {
                let priority = std::mem::discriminant(source);
                overrides.retain(|(source, _)| std::mem::discriminant(source) == priority);
            }
            if overrides.len() > 1 {
                let previous = project.read_with(cx, |project, cx| {
                    project
                        .task_store()
                        .read(cx)
                        .task_inventory()
                        .and_then(|inventory| inventory.read(cx).last_scheduled_task(None))
                        .map(|(_, task)| task.original_task().label.clone())
                });
                if let Some(index) = previous.and_then(|label| {
                    overrides
                        .iter()
                        .position(|(_, template)| template.label == label)
                }) {
                    let chosen = overrides.remove(index);
                    overrides.clear();
                    overrides.push(chosen);
                }
            }
            if overrides.len() > 1 {
                let labels = overrides
                    .iter()
                    .map(|(_, task)| task.label.as_str())
                    .chain(std::iter::once("取消"))
                    .collect::<Vec<_>>();
                let choice = workspace
                    .update_in(cx, |_, window, cx| {
                        window.prompt(
                            gpui::PromptLevel::Info,
                            "选择运行方式",
                            Some("检测到多个运行任务，请选择本次需要运行的目标。"),
                            &labels,
                            cx,
                        )
                    })?
                    .await?;
                if choice >= overrides.len() {
                    return Ok(());
                }
                let chosen = overrides.remove(choice);
                overrides.clear();
                overrides.push(chosen);
            }
            let (source, mut resolved) = if let Some((source, mut template)) = overrides.pop() {
                template.save = SaveStrategy::None;
                template.reveal = RevealStrategy::NoFocus;
                template
                    .env
                    .insert("CODE_RUNNER_MANAGED".into(), "1".into());
                let resolved = template
                    .resolve_task(&source.to_id_base(), context)
                    .context("自定义运行任务缺少必要变量，请检查 tasks.json")?;
                (source, resolved)
            } else {
                let name = language.as_ref().map(|language| language.name());
                let name = name.as_ref().map(|name| name.as_ref()).unwrap_or("");
                let resolved = if let Some(selection) = selection {
                    selection_task(name, windows, context, selection)?
                } else {
                    builtin_task(name, windows, context)?
                };
                (TaskSourceKind::UserInput, resolved)
            };
            resolved.resolved.save = SaveStrategy::None;
            resolved
                .resolved
                .env
                .insert("CODE_RUNNER_MANAGED".into(), "1".into());
            resolved.resolved.allow_concurrent_runs = false;
            resolved.resolved.use_new_terminal = false;
            workspace.update_in(cx, |workspace, window, cx| {
                let project = workspace.project().read(cx);
                if project.is_disconnected(cx) {
                    workspace.show_error("无法运行代码：远程连接已断开。", cx);
                } else if TrustedWorktrees::has_restricted_worktrees(&project.worktree_store(), cx)
                {
                    workspace.show_error("无法运行代码：项目已进入受限模式。", cx);
                } else if buffer.read(cx).is_dirty() {
                    workspace.show_error("文件在准备运行期间发生变化，请重新运行。", cx);
                } else {
                    workspace.schedule_resolved_task(source, resolved, false, window, cx);
                }
            })?;
            Ok(())
        }
        .await;
        if let Err(error) = result {
            workspace
                .update(cx, |workspace, cx| {
                    workspace.show_error(format!("运行代码失败：{error:#}"), cx);
                })
                .log_err();
        }
    })
    .detach();
}

fn selection_task(
    language: &str,
    windows: bool,
    context: &TaskContext,
    selection: &str,
) -> anyhow::Result<task::ResolvedTask> {
    if selection.len() > 16 * 1024 {
        bail!("选区超过 16 KiB，请保存为文件后运行。");
    }
    let mut resolved = builtin_task(language, windows, context)?;
    let (program, flags) = match language {
        "Python" => ("", "-u -c"),
        "JavaScript" => ("node", "-e"),
        "Ruby" => ("ruby", "-e"),
        "PHP" => ("php", "-r"),
        "Perl" => ("perl", "-e"),
        "Lua" => ("lua", "-e"),
        "Shell Script" | "Bash" => ("bash", "-c"),
        "PowerShell" => ("pwsh", "-NoProfile -Command"),
        "Julia" => ("julia", "-e"),
        "R" => ("Rscript", "-e"),
        "Elixir" => ("elixir", "-e"),
        _ => bail!("{language} 暂不支持独立选区执行，请运行整个文件或配置 run-selection 任务。"),
    };
    let command = if language == "Python" {
        let source = if windows {
            windows_script(Recipe::Python)
        } else {
            posix_script(Recipe::Python)
        };
        if windows {
            source.replace(
                "& $runner -u $env:CODE_RUNNER_FILE",
                "& $runner -u -c $env:CODE_RUNNER_SELECTION",
            )
        } else {
            source.replace(
                "exec \"$runner\" -u \"$CODE_RUNNER_FILE\"",
                "exec \"$runner\" -u -c \"$CODE_RUNNER_SELECTION\"",
            )
        }
    } else if windows {
        format!(
            "$ErrorActionPreference = 'Stop'\n& {program} {flags} $env:CODE_RUNNER_SELECTION\nexit $LASTEXITCODE"
        )
    } else {
        format!("exec {program} {flags} \"$CODE_RUNNER_SELECTION\"")
    };
    // Pass source as environment data so quotes and shell metacharacters stay literal.
    let mut template = resolved.original_task().clone();
    template.command = command;
    template.label = format!("运行选中代码 · {language}");
    template.env.insert(
        "CODE_RUNNER_SELECTION".into(),
        VariableName::SelectedText.template_value(),
    );
    resolved = template
        .resolve_task("code-runner-selection", context)
        .context("无法生成选区运行任务")?;
    Ok(resolved)
}

fn builtin_task(
    language: &str,
    windows: bool,
    context: &TaskContext,
) -> anyhow::Result<task::ResolvedTask> {
    let recipe = recipe(language).with_context(|| format!(
        "暂不支持直接运行 {language} 文件。请在 tasks.json 中定义带 run-current 标签的运行任务。"
    ))?;
    let file = context
        .task_variables
        .get(&VariableName::File)
        .context("请先保存当前文件")?;
    context
        .task_variables
        .get(&VariableName::Dirname)
        .context("无法确定文件目录")?;
    let mut script = if windows {
        windows_script(recipe)
    } else {
        posix_script(recipe)
    };
    if let Some(manifests) = project_manifests(language) {
        script = format!("{}\n{script}", project_guard(manifests, language, windows));
    }
    if language == "Kotlin" && !file.ends_with(".kts") {
        bail!("Kotlin 单文件运行需要 .kts 脚本；.kt 项目请配置 run-current 任务。");
    }
    if [".jsx", ".h", ".hpp", ".hh", ".hxx"]
        .iter()
        .any(|extension| file.ends_with(extension))
    {
        bail!("当前文件需要项目运行入口，请配置 run-current 任务。");
    }
    let mut env = collections::HashMap::default();
    env.insert("CODE_RUNNER_MANAGED".into(), "1".into());
    env.insert("CODE_RUNNER_ROOT".into(), "${ZED_WORKTREE_ROOT:}".into());
    env.insert("GOPROXY".into(), "off".into());
    env.insert("GOTOOLCHAIN".into(), "local".into());
    env.insert(
        "CODE_RUNNER_FILE".into(),
        VariableName::File.template_value(),
    );
    env.insert(
        "CODE_RUNNER_DIRECTORY".into(),
        VariableName::Dirname.template_value(),
    );
    env.insert(
        "CODE_RUNNER_PYTHON".into(),
        "${ZED_CUSTOM_PYTHON_ACTIVE_ZED_TOOLCHAIN:}".into(),
    );
    let template = TaskTemplate {
        label: format!("运行当前文件 · {language}"),
        command: script,
        cwd: Some(VariableName::Dirname.template_value()),
        env,
        shell: Shell::WithArguments {
            program: if windows { "powershell" } else { "sh" }.into(),
            title_override: None,
            args: if windows {
                vec!["-NoProfile".into()]
            } else {
                Vec::new()
            },
        },
        reveal: RevealStrategy::NoFocus,
        show_summary: true,
        show_command: false,
        ..TaskTemplate::default()
    };
    template
        .resolve_task("code-runner", context)
        .context("无法生成运行命令")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Recipe {
    Interpreter(&'static str, &'static [&'static str]),
    Python,
    TypeScript,
    JavaScript,
    CSharp,
    Compile(&'static str),
}

fn recipe(language: &str) -> Option<Recipe> {
    Some(match language {
        "Python" => Recipe::Python,
        "JavaScript" => Recipe::JavaScript,
        "C#" | "CSharp" => Recipe::CSharp,
        "TypeScript" => Recipe::TypeScript,
        "Ruby" => Recipe::Interpreter("ruby", &[]),
        "PHP" => Recipe::Interpreter("php", &[]),
        "Perl" => Recipe::Interpreter("perl", &[]),
        "Lua" => Recipe::Interpreter("lua", &[]),
        "Shell Script" | "Bash" => Recipe::Interpreter("bash", &[]),
        "PowerShell" => Recipe::Interpreter("pwsh", &["-NoProfile", "-File"]),
        "R" => Recipe::Interpreter("Rscript", &[]),
        "Julia" => Recipe::Interpreter("julia", &[]),
        "Dart" => Recipe::Interpreter("dart", &["run"]),
        "Elixir" => Recipe::Interpreter("elixir", &[]),
        "Haskell" => Recipe::Interpreter("runghc", &[]),
        "OCaml" => Recipe::Interpreter("ocaml", &[]),
        "Groovy" => Recipe::Interpreter("groovy", &[]),
        "Kotlin" => Recipe::Interpreter("kotlinc", &["-script"]),
        "Swift" => Recipe::Interpreter("swift", &[]),
        "Java" => Recipe::Interpreter("java", &[]),
        "Go" => Recipe::Interpreter("go", &["run"]),
        "C" => Recipe::Compile("cc"),
        "C++" => Recipe::Compile("c++"),
        "Rust" => Recipe::Compile("rustc"),
        _ => return None,
    })
}

fn project_manifests(language: &str) -> Option<&'static [&'static str]> {
    match language {
        "Rust" => Some(&["Cargo.toml"]),
        "Go" => Some(&["go.mod", "go.work"]),
        "Java" | "Kotlin" => Some(&["pom.xml", "build.gradle", "build.gradle.kts"]),
        "C" | "C++" => Some(&["CMakeLists.txt", "Makefile", "meson.build"]),
        "Swift" => Some(&["Package.swift"]),
        _ => None,
    }
}

fn project_guard(manifests: &[&str], language: &str, windows: bool) -> String {
    let (posix_run, windows_run) = match language {
        "Rust" => (
            "cd \"$directory\" || exit 1; exec cargo run --offline",
            "Set-Location -LiteralPath $directory; & cargo run --offline; exit $LASTEXITCODE",
        ),
        "Go" => ("exec go run .", "& go run .; exit $LASTEXITCODE"),
        "Swift" => (
            "cd \"$directory\" || exit 1; exec swift run --disable-automatic-resolution --skip-update",
            "Set-Location -LiteralPath $directory; & swift run --disable-automatic-resolution --skip-update; exit $LASTEXITCODE",
        ),
        _ => (
            "printf '%s\\n' '检测到工程构建配置，请通过带 run-current 标签的任务指定运行目标。' >&2; exit 2",
            "throw '检测到工程构建配置，请通过带 run-current 标签的任务指定运行目标。'",
        ),
    };
    if windows {
        let names = manifests
            .iter()
            .map(|name| format!("'{name}'"))
            .collect::<Vec<_>>()
            .join(",");
        format!(
            r#"$directory = $env:CODE_RUNNER_DIRECTORY
while ($directory) {{
    foreach ($name in @({names})) {{
        if (Test-Path -LiteralPath (Join-Path $directory $name)) {{
            {windows_run}
        }}
    }}
    if ($directory -eq $env:CODE_RUNNER_ROOT) {{ break }}
    $parent = [IO.Directory]::GetParent($directory)
    if (!$parent -or $parent.FullName -eq $directory) {{ break }}
    $directory = $parent.FullName
}}"#
        )
    } else {
        let names = manifests.join(" ");
        format!(
            r#"directory=$CODE_RUNNER_DIRECTORY
while :; do
    for name in {names}; do
        if [ -f "$directory/$name" ]; then
            {posix_run}
        fi
    done
    [ "$directory" = "$CODE_RUNNER_ROOT" ] && break
    parent=$(dirname "$directory")
    [ "$parent" = "$directory" ] && break
    directory=$parent
done"#
        )
    }
}

fn posix_script(recipe: Recipe) -> String {
    let body = match recipe {
        Recipe::JavaScript => r#"if [ -f bun.lock ] || [ -f bun.lockb ] || [ -f "$CODE_RUNNER_ROOT/bun.lock" ] || [ -f "$CODE_RUNNER_ROOT/bun.lockb" ]; then runner=bun
elif [ -f deno.json ] || [ -f deno.jsonc ] || [ -f "$CODE_RUNNER_ROOT/deno.json" ] || [ -f "$CODE_RUNNER_ROOT/deno.jsonc" ]; then need deno; exec deno run --cached-only "$CODE_RUNNER_FILE"
elif command -v node >/dev/null 2>&1; then runner=node
else runner=bun; fi
need "$runner"
exec "$runner" "$CODE_RUNNER_FILE""#.into(),
        Recipe::CSharp => r#"need dotnet
directory=$CODE_RUNNER_DIRECTORY
while :; do
    set -- "$directory"/*.csproj
    if [ -f "$1" ]; then
        [ "$#" -eq 1 ] || { printf '%s\n' '找到多个 C# 项目，请使用 run-current 任务指定项目。' >&2; exit 2; }
        exec dotnet run --no-restore --project "$1"
    fi
    [ "$directory" = "$CODE_RUNNER_ROOT" ] && break
    parent=$(dirname "$directory")
    [ "$parent" = "$directory" ] && break
    directory=$parent
done
printf '%s\n' '未找到 .csproj 项目，请创建项目或配置 run-current 任务。' >&2
exit 2"#.into(),
        Recipe::Interpreter(program, arguments) => format!(
            "runner={program}\nneed \"$runner\"\nexec \"$runner\" {} \"$CODE_RUNNER_FILE\"", arguments.join(" ")
        ),
        Recipe::Python => r#"if [ -n "$CODE_RUNNER_PYTHON" ] && [ "$CODE_RUNNER_PYTHON" != python3 ]; then
    runner=$CODE_RUNNER_PYTHON
    need "$runner"
elif [ -x .venv/bin/python ]; then runner=$CODE_RUNNER_DIRECTORY/.venv/bin/python
elif [ -n "$CODE_RUNNER_ROOT" ] && [ -x "$CODE_RUNNER_ROOT/.venv/bin/python" ]; then runner=$CODE_RUNNER_ROOT/.venv/bin/python
elif command -v python3 >/dev/null 2>&1; then runner=python3
else runner=python; fi
need "$runner"
exec "$runner" -u "$CODE_RUNNER_FILE""#.into(),
        Recipe::TypeScript => r#"if [ -f bun.lock ] || [ -f bun.lockb ] || [ -f "$CODE_RUNNER_ROOT/bun.lock" ] || [ -f "$CODE_RUNNER_ROOT/bun.lockb" ]; then need bun; exec bun "$CODE_RUNNER_FILE"; fi
if [ -f deno.json ] || [ -f deno.jsonc ] || [ -f "$CODE_RUNNER_ROOT/deno.json" ] || [ -f "$CODE_RUNNER_ROOT/deno.jsonc" ]; then need deno; exec deno run --cached-only "$CODE_RUNNER_FILE"; fi
if [ -x node_modules/.bin/tsx ]; then exec node_modules/.bin/tsx "$CODE_RUNNER_FILE"; fi
if [ -n "$CODE_RUNNER_ROOT" ] && [ -x "$CODE_RUNNER_ROOT/node_modules/.bin/tsx" ]; then exec "$CODE_RUNNER_ROOT/node_modules/.bin/tsx" "$CODE_RUNNER_FILE"; fi
if command -v tsx >/dev/null 2>&1; then exec tsx "$CODE_RUNNER_FILE"; fi
printf '%s\n' '未找到 tsx。请安装 TypeScript 运行工具，或配置 run-current 任务选择 Bun/Deno。' >&2
exit 127"#.into(),
        Recipe::Compile(compiler) => {
            let flags = if compiler == "rustc" { "--crate-name zed_code_runner --edition 2024" } else { "" };
            format!(r#"need {compiler}
output=$(mktemp -d) || exit 1
trap 'rm -rf "$output"' EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
{compiler} {flags} "$CODE_RUNNER_FILE" -o "$output/program" || exit $?
"$output/program"
exit $?"#)
        }
    };
    format!(
        "need() {{ command -v \"$1\" >/dev/null 2>&1 || {{ printf '未找到运行工具：%s。请安装工具或配置 run-current 任务。\\n' \"$1\" >&2; exit 127; }}; }}\n{body}"
    )
}

fn windows_script(recipe: Recipe) -> String {
    let body = match recipe {
        Recipe::JavaScript => r#"if ((Test-Path -LiteralPath 'bun.lock') -or (Test-Path -LiteralPath 'bun.lockb') -or ($env:CODE_RUNNER_ROOT -and ((Test-Path -LiteralPath (Join-Path $env:CODE_RUNNER_ROOT 'bun.lock')) -or (Test-Path -LiteralPath (Join-Path $env:CODE_RUNNER_ROOT 'bun.lockb'))))) { $runner = 'bun' }
elseif ((Test-Path -LiteralPath 'deno.json') -or (Test-Path -LiteralPath 'deno.jsonc') -or ($env:CODE_RUNNER_ROOT -and ((Test-Path -LiteralPath (Join-Path $env:CODE_RUNNER_ROOT 'deno.json')) -or (Test-Path -LiteralPath (Join-Path $env:CODE_RUNNER_ROOT 'deno.jsonc'))))) { Need deno; & deno run --cached-only $env:CODE_RUNNER_FILE; exit $LASTEXITCODE }
elseif (Get-Command node -ErrorAction SilentlyContinue) { $runner = 'node' }
else { $runner = 'bun' }
Need $runner
& $runner $env:CODE_RUNNER_FILE
exit $LASTEXITCODE"#.into(),
        Recipe::CSharp => r#"Need dotnet
$directory = $env:CODE_RUNNER_DIRECTORY
while ($directory) {
    $projects = @(Get-ChildItem -LiteralPath $directory -Filter '*.csproj' -File)
    if ($projects.Count -gt 1) { throw '找到多个 C# 项目，请使用 run-current 任务指定项目。' }
    if ($projects.Count -eq 1) { & dotnet run --no-restore --project $projects[0].FullName; exit $LASTEXITCODE }
    if ($directory -eq $env:CODE_RUNNER_ROOT) { break }
    $parent = [IO.Directory]::GetParent($directory)
    if (!$parent -or $parent.FullName -eq $directory) { break }
    $directory = $parent.FullName
}
throw '未找到 .csproj 项目，请创建项目或配置 run-current 任务。'"#.into(),
        Recipe::Interpreter("pwsh", _) => r#"$runner = if (Get-Command pwsh -ErrorAction SilentlyContinue) { 'pwsh' } else { 'powershell' }
Need $runner
& $runner -NoProfile -File $env:CODE_RUNNER_FILE
exit $LASTEXITCODE"#.into(),
        Recipe::Interpreter(program, arguments) => format!(
            "$runner = '{program}'\nNeed $runner\n& $runner {} $env:CODE_RUNNER_FILE\nexit $LASTEXITCODE", arguments.join(" ")
        ),
        Recipe::Python => r#"$runner = $env:CODE_RUNNER_PYTHON
if (!$runner -or $runner -eq 'python3') {
    if (Test-Path -LiteralPath '.venv\Scripts\python.exe') { $runner = Join-Path $env:CODE_RUNNER_DIRECTORY '.venv\Scripts\python.exe' }
    elseif ($env:CODE_RUNNER_ROOT -and (Test-Path -LiteralPath (Join-Path $env:CODE_RUNNER_ROOT '.venv\Scripts\python.exe'))) { $runner = Join-Path $env:CODE_RUNNER_ROOT '.venv\Scripts\python.exe' }
    elseif (Get-Command python -ErrorAction SilentlyContinue) { $runner = 'python' }
    else { $runner = 'py' }
}
Need $runner
& $runner -u $env:CODE_RUNNER_FILE
exit $LASTEXITCODE"#.into(),
        Recipe::TypeScript => r#"if ((Test-Path -LiteralPath 'bun.lock') -or (Test-Path -LiteralPath 'bun.lockb') -or ($env:CODE_RUNNER_ROOT -and ((Test-Path -LiteralPath (Join-Path $env:CODE_RUNNER_ROOT 'bun.lock')) -or (Test-Path -LiteralPath (Join-Path $env:CODE_RUNNER_ROOT 'bun.lockb'))))) { Need bun; & bun $env:CODE_RUNNER_FILE; exit $LASTEXITCODE }
if ((Test-Path -LiteralPath 'deno.json') -or (Test-Path -LiteralPath 'deno.jsonc') -or ($env:CODE_RUNNER_ROOT -and ((Test-Path -LiteralPath (Join-Path $env:CODE_RUNNER_ROOT 'deno.json')) -or (Test-Path -LiteralPath (Join-Path $env:CODE_RUNNER_ROOT 'deno.jsonc'))))) { Need deno; & deno run --cached-only $env:CODE_RUNNER_FILE; exit $LASTEXITCODE }
if (Test-Path -LiteralPath 'node_modules\.bin\tsx.cmd') { $runner = '.\node_modules\.bin\tsx.cmd' }
elseif ($env:CODE_RUNNER_ROOT -and (Test-Path -LiteralPath (Join-Path $env:CODE_RUNNER_ROOT 'node_modules\.bin\tsx.cmd'))) { $runner = Join-Path $env:CODE_RUNNER_ROOT 'node_modules\.bin\tsx.cmd' }
else { $runner = 'tsx' }
Need $runner
& $runner $env:CODE_RUNNER_FILE
exit $LASTEXITCODE"#.into(),
        Recipe::Compile(compiler) => {
            let compiler = match compiler { "cc" => "gcc", "c++" => "g++", other => other };
            let flags = if compiler == "rustc" { "--crate-name zed_code_runner --edition 2024" } else { "" };
            format!(r#"Need {compiler}
$output = Join-Path ([IO.Path]::GetTempPath()) ([Guid]::NewGuid().ToString())
[IO.Directory]::CreateDirectory($output) | Out-Null
try {{
    $binary = Join-Path $output 'program.exe'
    & {compiler} {flags} $env:CODE_RUNNER_FILE -o $binary
    if ($LASTEXITCODE -ne 0) {{ exit $LASTEXITCODE }}
    & $binary
    exit $LASTEXITCODE
}} finally {{ Remove-Item -LiteralPath $output -Recurse -Force }}"#)
        }
    };
    format!(
        "$ErrorActionPreference = 'Stop'\nfunction Need($program) {{ if (!(Get-Command $program -ErrorAction SilentlyContinue)) {{ throw \"未找到运行工具：$program。请安装工具或配置 run-current 任务。\" }} }}\n{body}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[gpui::test]
    async fn plain_file_action_saves_and_schedules_without_runnables(
        cx: &mut gpui::TestAppContext,
    ) {
        use language::{Language, LanguageConfig};
        use project::{FakeFs, Project};
        use std::sync::Arc;
        use ui::VisualContext as _;
        use util::{path, rel_path::rel_path};
        crate::tests::init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/runner"),
            serde_json::json!({"test.py": "print('Hello, World!')\n"}),
        )
        .await;
        let project = Project::test(fs, [path!("/runner").as_ref()], cx).await;
        let id = project.update(cx, |project, cx| {
            project
                .worktrees(cx)
                .next()
                .expect("worktree")
                .read(cx)
                .id()
        });
        let buffer = project
            .update(cx, |project, cx| {
                project.open_buffer((id, rel_path("test.py")), cx)
            })
            .await
            .expect("buffer");
        buffer.update(cx, |buffer, cx| {
            buffer.set_language(
                Some(Arc::new(Language::new(
                    LanguageConfig {
                        name: "Python".into(),
                        ..Default::default()
                    },
                    None,
                ))),
                cx,
            )
        });
        let (multi_workspace, cx) = cx.add_window_view(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });
        let workspace = multi_workspace.read_with(cx, |multi, _| multi.workspace().clone());
        let editor = cx.new_window_entity(|window, cx| {
            Editor::for_buffer(buffer.clone(), Some(project.clone()), window, cx)
        });
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.add_item_to_center(Box::new(editor), window, cx);
            run_file(workspace, &RunFile, window, cx);
        });
        cx.run_until_parked();
        let scheduled = project.read_with(cx, |project, cx| {
            project
                .task_store()
                .read(cx)
                .task_inventory()
                .expect("inventory")
                .read(cx)
                .last_scheduled_task(None)
        });
        let (_, scheduled) = scheduled.expect("plain file schedules a task");
        assert_eq!(
            scheduled
                .resolved
                .env
                .get("CODE_RUNNER_FILE")
                .map(String::as_str),
            Some(path!("/runner/test.py"))
        );
        assert_eq!(
            scheduled
                .resolved
                .env
                .get("CODE_RUNNER_MANAGED")
                .map(String::as_str),
            Some("1")
        );
        assert!(!buffer.read_with(cx, |buffer, _| buffer.is_dirty()));
    }

    #[test]
    #[ignore = "requires pwsh on PATH; run explicitly for PowerShell syntax validation"]
    #[allow(
        clippy::disallowed_methods,
        reason = "Synchronous parser validation outside the UI executor"
    )]
    fn powershell_recipes_parse() -> anyhow::Result<()> {
        for language in [
            "Python",
            "JavaScript",
            "TypeScript",
            "C#",
            "C",
            "C++",
            "Rust",
            "Go",
            "Java",
            "Swift",
            "PowerShell",
        ] {
            let mut context = TaskContext::default();
            context
                .task_variables
                .insert(VariableName::File, "C:\\project\\test.py".into());
            context
                .task_variables
                .insert(VariableName::Dirname, "C:\\project".into());
            let task = builtin_task(language, true, &context)?;
            let output = std::process::Command::new("pwsh").args(["-NoProfile", "-Command", "$tokens = $null; $errors = $null; [System.Management.Automation.Language.Parser]::ParseInput($env:RUNNER_SCRIPT, [ref]$tokens, [ref]$errors) | Out-Null; if ($errors.Count) { $errors | Out-String | Write-Error; exit 1 }"])
                .env("RUNNER_SCRIPT", task.resolved.command.as_deref().expect("command")).output()?;
            assert!(
                output.status.success(),
                "{language}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    #[ignore = "requires pwsh on PATH"]
    #[allow(
        clippy::disallowed_methods,
        reason = "Real PowerShell smoke test outside the UI executor"
    )]
    fn powershell_executes_python_with_literal_paths() -> anyhow::Result<()> {
        let root = std::env::temp_dir().join(format!("zed-pwsh-runner-{}", std::process::id()));
        std::fs::create_dir_all(&root)?;
        let result = (|| -> anyhow::Result<()> {
            let file = root.join("中文 ' $() & test.py");
            std::fs::write(&file, "print('PowerShell runner')\n")?;
            let mut context = TaskContext::default();
            context
                .task_variables
                .insert(VariableName::File, file.to_string_lossy().into_owned());
            context
                .task_variables
                .insert(VariableName::Dirname, root.to_string_lossy().into_owned());
            context.task_variables.insert(
                VariableName::Custom("PYTHON_ACTIVE_ZED_TOOLCHAIN".into()),
                "/usr/bin/python3".into(),
            );
            let task = builtin_task("Python", true, &context)?;
            let output = std::process::Command::new("pwsh")
                .args([
                    "-NoProfile",
                    "-Command",
                    task.resolved.command.as_deref().expect("command"),
                ])
                .envs(&task.resolved.env)
                .current_dir(&root)
                .output()?;
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(String::from_utf8(output.stdout)?, "PowerShell runner\n");
            let selection = "print(\"$HOME $(touch INJECTION) 'literal'\")";
            context
                .task_variables
                .insert(VariableName::SelectedText, selection.into());
            let task = selection_task("Python", true, &context, selection)?;
            let output = std::process::Command::new("pwsh")
                .args([
                    "-NoProfile",
                    "-Command",
                    task.resolved.command.as_deref().expect("command"),
                ])
                .envs(&task.resolved.env)
                .current_dir(&root)
                .output()?;
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(
                String::from_utf8(output.stdout)?,
                "$HOME $(touch INJECTION) 'literal'\n"
            );
            assert!(!root.join("INJECTION").exists());
            Ok(())
        })();
        std::fs::remove_dir_all(root)?;
        result
    }

    #[test]
    fn supported_languages_have_both_platform_recipes() {
        for language in [
            "Python",
            "JavaScript",
            "TypeScript",
            "Ruby",
            "PHP",
            "Perl",
            "Lua",
            "Shell Script",
            "PowerShell",
            "R",
            "Julia",
            "Dart",
            "Elixir",
            "Haskell",
            "OCaml",
            "Groovy",
            "Kotlin",
            "Swift",
            "Java",
            "Go",
            "C",
            "C++",
            "Rust",
        ] {
            let recipe = recipe(language).expect("supported language");
            assert!(!posix_script(recipe).is_empty());
            assert!(!windows_script(recipe).is_empty());
        }
        for language in ["JSON", "Markdown", "HTML", "TSX", "JSX", "Plain Text"] {
            assert!(recipe(language).is_none());
        }
    }

    #[test]
    fn paths_are_environment_data_not_shell_source() {
        for windows in [false, true] {
            let file = if windows {
                r#"C:\代码 空格\$(evil) & 'test.py"#
            } else {
                "/代码 空格/$(evil) & 'test.py"
            };
            let mut context = TaskContext::default();
            context
                .task_variables
                .insert(VariableName::File, file.into());
            context
                .task_variables
                .insert(VariableName::Dirname, "/project".into());
            let resolved = builtin_task("Python", windows, &context).expect("valid task");
            assert_eq!(
                resolved
                    .resolved
                    .env
                    .get("CODE_RUNNER_FILE")
                    .map(String::as_str),
                Some(file)
            );
            assert!(
                !resolved
                    .resolved
                    .command
                    .as_ref()
                    .expect("command")
                    .contains(file)
            );
            assert_eq!(resolved.resolved.save, SaveStrategy::None);
            assert_eq!(resolved.resolved.reveal, RevealStrategy::NoFocus);
            assert_eq!(resolved.original_task().reveal, RevealStrategy::NoFocus);
        }
    }

    #[cfg(unix)]
    #[test]
    #[allow(
        clippy::disallowed_methods,
        reason = "Synchronous subprocess smoke test runs outside the UI executor"
    )]
    fn scripts_run_literal_paths_and_guard_compilation() -> anyhow::Result<()> {
        let root = std::env::temp_dir().join(format!(
            "zed-runner-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos()
        ));
        std::fs::create_dir(&root)?;
        let result = (|| -> anyhow::Result<()> {
            let file = root.join("中文 空格 $(touch SHOULD_NOT_EXIST) 'test.py");
            std::fs::write(&file, "print('Hello, World!')\n")?;
            let mut context = TaskContext::default();
            context
                .task_variables
                .insert(VariableName::File, file.to_string_lossy().into_owned());
            context
                .task_variables
                .insert(VariableName::Dirname, root.to_string_lossy().into_owned());
            let resolved = builtin_task("Python", false, &context)?;
            // Exercise the same no-quote task boundary used by the terminal panel.
            let (program, args) = task::ShellBuilder::new(&resolved.resolved.shell, false)
                .non_interactive()
                .build_no_quote(resolved.resolved.command.clone(), &resolved.resolved.args);
            let run = || {
                std::process::Command::new(&program)
                    .args(&args)
                    .envs(&resolved.resolved.env)
                    .current_dir(&root)
                    .output()
            };
            let output = run()?;
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(String::from_utf8(output.stdout)?, "Hello, World!\n");
            assert!(!root.join("SHOULD_NOT_EXIST").exists());
            std::fs::write(&file, "raise SystemExit(7)\n")?;
            assert_eq!(run()?.status.code(), Some(7));
            let rerun = resolved
                .original_task()
                .resolve_task("code-runner", &context)
                .expect("rerunnable task");
            assert_eq!(rerun.resolved.command, resolved.resolved.command);
            let selected = "print(\"$HOME $(touch SELECTION_INJECTION) 'literal'\")";
            context
                .task_variables
                .insert(VariableName::SelectedText, selected.into());
            let selected_task = selection_task("Python", false, &context, selected)?;
            let output = std::process::Command::new("sh")
                .args([
                    "-c",
                    selected_task.resolved.command.as_deref().expect("command"),
                ])
                .envs(&selected_task.resolved.env)
                .current_dir(&root)
                .output()?;
            assert!(output.status.success());
            assert_eq!(
                String::from_utf8(output.stdout)?,
                "$HOME $(touch SELECTION_INJECTION) 'literal'\n"
            );
            assert!(!root.join("SELECTION_INJECTION").exists());
            assert!(selection_task("C", false, &context, "int x;").is_err());
            assert!(selection_task("Python", false, &context, &"x".repeat(16385)).is_err());

            let source = root.join("中文 test.c");
            std::fs::write(
                &source,
                "#include <stdio.h>\nint main(void) { puts(\"compiled\"); }\n",
            )?;
            context
                .task_variables
                .insert(VariableName::File, source.to_string_lossy().into_owned());
            let compiled = builtin_task("C", false, &context)?;
            let run_compiler = || {
                std::process::Command::new("sh")
                    .args(["-c", compiled.resolved.command.as_deref().expect("command")])
                    .envs(&compiled.resolved.env)
                    .current_dir(&root)
                    .output()
            };
            let output = run_compiler()?;
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(String::from_utf8(output.stdout)?, "compiled\n");
            std::fs::write(&source, "this is not valid C\n")?;
            let output = run_compiler()?;
            assert!(!output.status.success());
            assert!(output.stdout.is_empty());
            std::fs::write(root.join("CMakeLists.txt"), "")?;
            let output = run_compiler()?;
            assert_eq!(output.status.code(), Some(2));
            assert!(String::from_utf8(output.stderr)?.contains("检测到工程构建配置"));
            let package = root.join("rust project");
            std::fs::create_dir_all(package.join("src"))?;
            std::fs::write(
                package.join("Cargo.toml"),
                "[package]\nname = \"runner_smoke\"\nversion = \"0.1.0\"\nedition = \"2024\"\n[workspace]\n",
            )?;
            std::fs::write(
                package.join("src/main.rs"),
                "fn main() { println!(\"cargo project\"); }\n",
            )?;
            context.task_variables.insert(
                VariableName::File,
                package.join("src/main.rs").to_string_lossy().into_owned(),
            );
            context.task_variables.insert(
                VariableName::Dirname,
                package.join("src").to_string_lossy().into_owned(),
            );
            context.task_variables.insert(
                VariableName::WorktreeRoot,
                package.to_string_lossy().into_owned(),
            );
            let rust = builtin_task("Rust", false, &context)?;
            let output = std::process::Command::new("sh")
                .args(["-c", rust.resolved.command.as_deref().expect("command")])
                .envs(&rust.resolved.env)
                .env("CARGO_TARGET_DIR", package.join("target"))
                .current_dir(package.join("src"))
                .output()?;
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(String::from_utf8(output.stdout)?, "cargo project\n");
            Ok(())
        })();
        std::fs::remove_dir_all(&root)?;
        result
    }

    #[test]
    fn compilation_failure_never_runs_previous_binary() {
        for compiler in ["cc", "c++", "rustc"] {
            let posix = posix_script(Recipe::Compile(compiler));
            assert!(posix.contains("mktemp -d"));
            assert!(posix.contains("|| exit $?"));
            let windows = windows_script(Recipe::Compile(compiler));
            assert!(windows.contains("NewGuid"));
            assert!(windows.contains("if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }"));
        }
    }
}
