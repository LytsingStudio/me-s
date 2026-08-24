use std::{
    env, fs,
    io::{self, BufRead, Write},
    path::Path,
    time::{Duration, Instant},
};

use me::{
    Result, codex_oauth,
    config::{
        GlobalConfig, WorkspaceConfig, global_config_path, workspace_config_path,
        workspace_edb_path,
    },
    desktop_toolbox, diag,
    event::{EventBase, EventDataBase},
    managed_child,
    model::{
        ModelApi, ModelContext, ModelRuntime, ModelUsage, OpenAiStreamEvent, openai_stream_event,
        openai_stream_usage,
    },
    model_transfer,
    orchestrator::{AVAILABLE_ORCHESTRATORS, apply_model_selection, latest_effort, latest_model},
    termination::TerminationSignals,
    toolbox, tui,
    ui_backend::workspace_ui_ports,
    updater, webui,
    workspace::Workspace,
    workspace_bootstrap,
};
use unicode_width::UnicodeWidthStr;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UiLaunchMode {
    TuiAndWeb,
    WebOnly,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct UiLaunchOptions {
    mode: UiLaunchMode,
    webui_passkey: Option<String>,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let arguments: Vec<String> = env::args().skip(1).collect();
    let workspace = env::current_dir()?;
    if arguments.as_slice() == ["__toolbox-terminal-worker"] {
        let stdin = io::stdin();
        let stdout = io::stdout();
        return toolbox::run_default_terminal_toolbox(stdin.lock(), stdout, &workspace);
    }
    if arguments.as_slice() == ["__toolbox-desktop-worker"] {
        let stdin = io::stdin();
        let stdout = io::stdout();
        return desktop_toolbox::run(stdin.lock(), stdout, &workspace);
    }
    if arguments.as_slice() == ["__gateway-child"] {
        return managed_child::run(&workspace);
    }
    if is_version_command(&arguments) {
        println!("{}", version_string());
        return Ok(());
    }
    if is_update_command(&arguments) {
        updater::update()?;
        return Ok(());
    }
    let workspace_path = workspace_config_path(&workspace);

    if arguments.as_slice() == ["workspace", "reset"] {
        return reset_workspace(&workspace);
    }

    match arguments.as_slice() {
        [command] if command == "init" => {
            let stdin = io::stdin();
            let mut input = stdin.lock();
            let stdout = io::stdout();
            let mut output = stdout.lock();
            model_transfer::initialize_global(&mut input, &mut output)?;
            return Ok(());
        }
        [command, action, file, password] if command == "model" && action == "import" => {
            let result = model_transfer::import_models(Path::new(file), password)?;
            println!(
                "imported models: added={} overwritten={} credentials={} codex={} default={}\nconfig: {}",
                result.added,
                result.overwritten,
                result.model_credentials,
                if result.codex_credential {
                    "restored"
                } else {
                    "unchanged"
                },
                result.default_model,
                result.config_file.display()
            );
            return Ok(());
        }
        [command, action] if command == "codex" && action == "status" => {
            print_codex_status(&codex_oauth::status()?);
            return Ok(());
        }
        [command, action] if command == "codex" && action == "login" => {
            let status = codex_oauth::login()?;
            print_codex_status(&status);
            return Ok(());
        }
        [command, action] if command == "codex" && action == "logout" => {
            let result = codex_oauth::logout()?;
            if result.removed {
                println!("Codex OAuth credential deleted");
            } else {
                println!("Codex OAuth is not logged in");
            }
            if let Some(warning) = result.revoke_warning {
                eprintln!("warning: {warning}");
            }
            return Ok(());
        }
        [command, action] if command == "diag" && action == "upload" => {
            let result = diag::upload_workspace(&workspace)?;
            println!(
                "uploaded diagnostic archive\nrepository: {}\narchive: {}\nbytes: {}\nurl: {}\ncontent: complete unfiltered .me directory",
                diag::DIAG_REPOSITORY,
                result.archive_name,
                result.archive_bytes,
                result.url
            );
            return Ok(());
        }
        _ => {}
    }

    let global_path = global_config_path()?;
    let mut global = GlobalConfig::load(&global_path)?;
    if let [command, action, password] = arguments.as_slice()
        && command == "model"
        && action == "export"
    {
        let result = model_transfer::export_models(&global, password)?;
        println!(
            "exported models: models={} credentials={} codex={}\nfile: {}",
            result.models,
            result.model_credentials,
            if result.codex_credential {
                "included"
            } else {
                "not exported"
            },
            result.file.display()
        );
        return Ok(());
    }
    if let [command, action, name] = arguments.as_slice()
        && command == "model"
        && action == "select-default"
    {
        select_default_model(&mut global, &global_path, name)?;
        return Ok(());
    }
    codex_oauth::add_models_if_logged_in(&mut global)?;

    if let Some(options) = ui_launch_options(&arguments)? {
        let local = {
            let stdin = io::stdin();
            let mut input = stdin.lock();
            let stdout = io::stdout();
            let mut output = stdout.lock();
            load_or_offer_workspace(
                &workspace,
                &workspace_path,
                &global.default_model,
                &mut input,
                &mut output,
            )?
        };
        let Some(local) = local else {
            return Ok(());
        };
        return run_user_interfaces(
            &workspace,
            local,
            &global.default_model,
            global.models.clone(),
            options,
        );
    }

    match arguments.as_slice() {
        [command] if command == "create" => {
            workspace_bootstrap::create(&workspace, &global.default_model)?;
            println!("created workspace {}", workspace.display());
        }
        [command, action] if command == "model" && action == "list" => {
            let local = workspace_path
                .exists()
                .then(|| WorkspaceConfig::load(&workspace_path))
                .transpose()?;
            let selected = local
                .as_ref()
                .map(|local| selected_workspace_model(&workspace, local))
                .transpose()?;
            list_models(&global, selected.as_deref());
        }
        [command, action, name] if command == "model" && action == "select" => {
            let mut local = WorkspaceConfig::load(&workspace_path)?;
            select_model(&global, &mut local, &workspace_path, &workspace, name, None)?;
        }
        [command, action, name] if command == "model" && action == "test" => {
            test_model(&global, name)?;
        }
        [command, action, name, effort] if command == "model" && action == "select" => {
            let mut local = WorkspaceConfig::load(&workspace_path)?;
            select_model(
                &global,
                &mut local,
                &workspace_path,
                &workspace,
                name,
                Some(effort),
            )?;
        }
        [command, action, prompt @ ..] if command == "model" && action == "request" => {
            if prompt.is_empty() {
                return Err("model request requires a prompt".into());
            }
            let local = WorkspaceConfig::load(&workspace_path)?;
            request(&global, &local, &workspace, &prompt.join(" "))?;
        }
        [command] if command == "orch" => {
            let local = WorkspaceConfig::load(&workspace_path)?;
            list_orchestrators(&local);
        }
        [command, name] if command == "orch" => {
            let mut local = WorkspaceConfig::load(&workspace_path)?;
            select_orchestrator(&mut local, &workspace_path, name)?;
        }
        [command] if command == "edb" => {
            WorkspaceConfig::load(&workspace_path)?;
            show_edbs(&workspace, false)?;
        }
        [command, action] if command == "edb" && action == "detail" => {
            WorkspaceConfig::load(&workspace_path)?;
            show_edbs(&workspace, true)?;
        }
        _ => print_usage(),
    }
    Ok(())
}

fn ui_launch_options(arguments: &[String]) -> Result<Option<UiLaunchOptions>> {
    if arguments.is_empty() {
        return Ok(Some(UiLaunchOptions {
            mode: UiLaunchMode::TuiAndWeb,
            webui_passkey: None,
        }));
    }
    if !arguments.iter().any(|argument| argument.starts_with("--")) {
        return Ok(None);
    }

    let mut mode = UiLaunchMode::TuiAndWeb;
    let mut saw_no_tui = false;
    let mut webui_passkey = None;
    let mut index = 0;
    while index < arguments.len() {
        match arguments[index].as_str() {
            "--no-tui" => {
                if saw_no_tui {
                    return Err("--no-tui may only be specified once".into());
                }
                saw_no_tui = true;
                mode = UiLaunchMode::WebOnly;
                index += 1;
            }
            "--webui-passkey" => {
                if webui_passkey.is_some() {
                    return Err("--webui-passkey may only be specified once".into());
                }
                let passkey = arguments
                    .get(index + 1)
                    .ok_or("--webui-passkey requires a password")?;
                if passkey.is_empty() {
                    return Err("--webui-passkey password must not be empty".into());
                }
                webui_passkey = Some(passkey.clone());
                index += 2;
            }
            argument => return Err(format!("unknown UI option: {argument}").into()),
        }
    }
    Ok(Some(UiLaunchOptions {
        mode,
        webui_passkey,
    }))
}

fn is_update_command(arguments: &[String]) -> bool {
    arguments == ["update"]
}

fn is_version_command(arguments: &[String]) -> bool {
    arguments == ["version"]
}

fn version_string() -> String {
    format!("me-s {}", env!("CARGO_PKG_VERSION"))
}

fn run_user_interfaces(
    workspace_root: &Path,
    local: WorkspaceConfig,
    default_model: &str,
    models: Vec<me::config::ModelConfig>,
    options: UiLaunchOptions,
) -> Result<()> {
    let termination = TerminationSignals::install()?;
    let workspace =
        Workspace::open_with_default_model(workspace_root, local, models, default_model)?;
    let (ui_backend, ui_commands) = workspace_ui_ports(workspace);
    match options.mode {
        UiLaunchMode::TuiAndWeb => {
            let webui = match webui::start(
                ui_backend.clone(),
                ui_commands.clone(),
                options.webui_passkey.as_deref(),
            ) {
                Ok(server) => {
                    eprintln!("WebUI: {}", server.address());
                    Some(server)
                }
                Err(error) => {
                    eprintln!("warning: WebUI 未启动：{error}");
                    None
                }
            };
            tui::run(&ui_backend, &ui_commands, termination.flag())?;
            drop(webui);
        }
        UiLaunchMode::WebOnly => {
            let server = webui::start(ui_backend, ui_commands, options.webui_passkey.as_deref())?;
            eprintln!("WebUI: {}", server.address());
            eprintln!("WebUI-only mode · press Ctrl+C to stop");
            while !termination.requested() {
                std::thread::park_timeout(Duration::from_millis(100));
            }
            drop(server);
        }
    }
    Ok(())
}

fn load_or_offer_workspace(
    workspace: &Path,
    workspace_path: &Path,
    default_model: &str,
    input: &mut impl BufRead,
    output: &mut impl Write,
) -> Result<Option<WorkspaceConfig>> {
    if workspace_path.exists() {
        return WorkspaceConfig::load(workspace_path).map(Some);
    }

    write!(output, "当前目录还不是 me 工作区，是否立即创建？[y/N] ")?;
    output.flush()?;
    let mut answer = String::new();
    input.read_line(&mut answer)?;
    if !answer.trim().eq_ignore_ascii_case("y") {
        writeln!(output, "未创建工作区。")?;
        return Ok(None);
    }

    let local = workspace_bootstrap::create(workspace, default_model)?;
    writeln!(output, "created workspace {}", workspace.display())?;
    Ok(Some(local))
}

fn show_edbs(workspace: &Path, detail: bool) -> Result<()> {
    let paths = workspace_edb_paths(workspace)?;
    println!("agents: {}", paths.len());

    for path in paths {
        let agent = path
            .file_stem()
            .map(|value| value.to_string_lossy())
            .unwrap_or_default();
        let bytes = path.metadata()?.len();
        let edb = EventDataBase::open(&path)?;
        let display_path = path.strip_prefix(workspace).unwrap_or(&path);
        println!(
            "\nagent: {agent}\npath: {}\nrecords: {}\nbytes: {bytes}",
            display_path.display(),
            edb.len()
        );

        for (order, event) in edb.events().iter().enumerate() {
            if detail {
                println!(
                    "\n  event order={order} id={}\n    timestamp_ms: {}\n    kind: {}\n    hash: {}\n    detail: {}",
                    event.getID(),
                    event.getTimestamp(),
                    event.getEventKind(),
                    event.getHash(),
                    event.getDetailString()
                );
            } else {
                println!(
                    "  order={order} id={} timestamp_ms={} kind={} hash={} {}",
                    event.getID(),
                    event.getTimestamp(),
                    event.getEventKind(),
                    event.getHash(),
                    event.getBriefString()
                );
            }
        }
    }
    Ok(())
}

fn workspace_edb_paths(workspace: &Path) -> Result<Vec<std::path::PathBuf>> {
    let directory = workspace.join(".me/edb");
    let mut paths = Vec::new();
    if directory.is_dir() {
        for entry in fs::read_dir(&directory)? {
            let entry = entry?;
            let path = entry.path();
            if entry.file_type()?.is_file() && path.extension().is_some_and(|value| value == "edb")
            {
                paths.push(path);
            }
        }
    }
    paths.sort();
    Ok(paths)
}

fn list_orchestrators(local: &WorkspaceConfig) {
    println!("default: {}", local.orchestrator);
    println!("available:");
    for name in AVAILABLE_ORCHESTRATORS {
        let marker = if *name == local.orchestrator {
            "*"
        } else {
            " "
        };
        println!("{marker} {name}");
    }
}

fn select_orchestrator(
    local: &mut WorkspaceConfig,
    path: &std::path::Path,
    name: &str,
) -> Result<()> {
    if !AVAILABLE_ORCHESTRATORS.contains(&name) {
        return Err(format!("orchestrator {name} does not exist").into());
    }
    local.orchestrator = name.to_owned();
    local.save(path)?;
    println!("selected default orchestrator {name}");
    Ok(())
}

fn list_models(global: &GlobalConfig, selected: Option<&str>) {
    println!("{}", format_model_table(&model_list_rows(global, selected)));
}

fn model_list_rows(global: &GlobalConfig, selected: Option<&str>) -> Vec<ModelListRow> {
    global
        .models
        .iter()
        .filter(|model| !codex_oauth::is_hidden_legacy_model(model))
        .map(|model| {
            let marker = if Some(model.name.as_str()) == selected {
                '●'
            } else {
                ' '
            };
            let efforts = if model.capabilities.reasoning_efforts.is_empty() {
                "-".to_owned()
            } else {
                model.capabilities.reasoning_efforts.join(",")
            };
            ModelListRow {
                marker,
                columns: [
                    model.name.clone(),
                    model.provider.to_string(),
                    model.model.clone(),
                    model.capabilities.context_window.to_string(),
                    model.capabilities.input_modalities.join(","),
                    efforts,
                ],
            }
        })
        .collect()
}

struct ModelListRow {
    marker: char,
    columns: [String; 6],
}

fn format_model_table(rows: &[ModelListRow]) -> String {
    let headers = [
        "NAME",
        "PROVIDER",
        "API MODEL",
        "CONTEXT",
        "MODALITIES",
        "EFFORT",
    ]
    .map(str::to_owned);
    let mut widths = std::array::from_fn(|index| display_width(&headers[index]));
    for row in rows {
        for (index, column) in row.columns.iter().enumerate() {
            widths[index] = widths[index].max(display_width(column));
        }
    }

    std::iter::once(format_model_row(' ', &headers, &widths))
        .chain(
            rows.iter()
                .map(|row| format_model_row(row.marker, &row.columns, &widths)),
        )
        .collect::<Vec<_>>()
        .join("\n")
}

fn format_model_row(marker: char, columns: &[String; 6], widths: &[usize; 6]) -> String {
    let cells: [String; 5] = std::array::from_fn(|index| {
        if index == 3 {
            pad_left(&columns[index], widths[index])
        } else {
            pad_right(&columns[index], widths[index])
        }
    });
    format!(
        "{marker} {}  {}  {}  {}  {}  {}",
        cells[0], cells[1], cells[2], cells[3], cells[4], columns[5]
    )
}

fn pad_left(value: &str, width: usize) -> String {
    format!(
        "{}{value}",
        " ".repeat(width.saturating_sub(display_width(value)))
    )
}

fn pad_right(value: &str, width: usize) -> String {
    format!(
        "{value}{}",
        " ".repeat(width.saturating_sub(display_width(value)))
    )
}

fn display_width(value: &str) -> usize {
    UnicodeWidthStr::width(value)
}

fn select_model(
    global: &GlobalConfig,
    local: &mut WorkspaceConfig,
    path: &std::path::Path,
    workspace: &std::path::Path,
    name: &str,
    effort: Option<&str>,
) -> Result<()> {
    let model = global
        .model(name)
        .ok_or_else(|| format!("model {name} does not exist"))?;
    if let Some(effort) = effort {
        model.validate_effort(effort)?;
    }
    let edb_path = workspace_edb_path(workspace);
    if !edb_path.exists() {
        local.model = name.to_owned();
        local.effort = effort.unwrap_or(me::config::UNSET_EFFORT).to_owned();
    } else {
        let mut edb = EventDataBase::open(&edb_path)?;
        if edb.is_empty() {
            local.model = name.to_owned();
            local.effort = effort.unwrap_or(me::config::UNSET_EFFORT).to_owned();
        } else {
            let active = latest_model(&edb).ok_or("EDB has no model state")?;
            let mut models = ModelRuntime::new(global.models.clone(), active)?;
            apply_model_selection(&mut edb, &mut models, name, effort)?;
            local.model = latest_model(&edb)
                .ok_or("EDB has no model state")?
                .to_owned();
            local.effort = latest_effort(&edb)
                .ok_or("EDB has no reasoning effort state")?
                .to_owned();
        }
    }
    local.save(path)?;
    println!("selected {} effort={}", local.model, local.effort);
    Ok(())
}

fn select_default_model(global: &mut GlobalConfig, path: &Path, name: &str) -> Result<()> {
    if global.model(name).is_none() {
        return Err(format!("model {name} does not exist").into());
    }
    global.default_model = name.to_owned();
    global.save(path)?;
    println!("selected default model {name}");
    Ok(())
}

fn selected_workspace_model(workspace: &Path, local: &WorkspaceConfig) -> Result<String> {
    let path = workspace_edb_path(workspace);
    if !path.exists() {
        return Ok(local.model.clone());
    }
    let edb = EventDataBase::open(&path)?;
    Ok(latest_model(&edb).unwrap_or(&local.model).to_owned())
}

fn request(
    global: &GlobalConfig,
    local: &WorkspaceConfig,
    workspace: &Path,
    prompt: &str,
) -> Result<()> {
    let path = workspace_edb_path(workspace);
    let edb = path
        .exists()
        .then(|| EventDataBase::open(&path))
        .transpose()?;
    let model_name = edb.as_ref().and_then(latest_model).unwrap_or(&local.model);
    let effort = edb
        .as_ref()
        .and_then(latest_effort)
        .unwrap_or(&local.effort);
    let model = global
        .model(model_name)
        .ok_or_else(|| format!("model {model_name} does not exist"))?;
    let api = ModelApi::new(model.clone())?;
    let context = ModelContext::user(prompt);
    api.complete_stream(&context, Some(effort), |line| {
        if let OpenAiStreamEvent::Delta {
            content: Some(content),
            ..
        } = openai_stream_event(line)?
        {
            print!("{content}");
            io::stdout().flush()?;
        }
        Ok(())
    })?;
    println!();
    Ok(())
}

const MODEL_TEST_MAX_OUTPUT_TOKENS: i64 = 128;
const MODEL_TEST_PROMPT: &str = "Reply with exactly OK and nothing else.";

struct ModelTestResult {
    response: String,
    ttft: Duration,
    elapsed: Duration,
    usage: Option<ModelUsage>,
}

fn test_model(global: &GlobalConfig, name: &str) -> Result<()> {
    let source = global
        .model(name)
        .ok_or_else(|| format!("model {name} does not exist"))?;
    let effort = lowest_effort(source);
    let mut model = source.clone();
    apply_test_parameters(&mut model);
    let api = ModelApi::new(model.clone())?;
    let context = ModelContext::user(MODEL_TEST_PROMPT);
    let started = Instant::now();
    let mut first_token_at = None;
    let mut response = String::new();
    let mut usage = None;
    let result = api.complete_stream(&context, effort, |line| {
        if let Some(current) = openai_stream_usage(line)? {
            usage = Some(current);
        }
        if let OpenAiStreamEvent::Delta {
            content: Some(content),
            ..
        } = openai_stream_event(line)?
            && !content.is_empty()
        {
            first_token_at.get_or_insert_with(Instant::now);
            response.push_str(&content);
        }
        Ok(())
    });
    let elapsed = started.elapsed();
    if let Err(error) = result {
        return Err(model_test_error(
            &model,
            effort,
            elapsed,
            "request failed",
            &error.to_string(),
        )
        .into());
    }
    let Some(first_token_at) = first_token_at else {
        return Err(model_test_error(
            &model,
            effort,
            elapsed,
            "no visible model reply",
            "the stream completed without a non-empty text delta",
        )
        .into());
    };
    let result = ModelTestResult {
        response,
        ttft: first_token_at.duration_since(started),
        elapsed,
        usage,
    };
    if result.response.trim() != "OK" {
        return Err(model_test_error(
            &model,
            effort,
            elapsed,
            "unexpected model reply",
            &format!("expected exactly OK, received {:?}", result.response),
        )
        .into());
    }
    print_model_test_success(&model, effort, &result);
    Ok(())
}

fn lowest_effort(model: &me::config::ModelConfig) -> Option<&str> {
    fn rank(effort: &str) -> usize {
        match effort {
            "none" => 0,
            "low" => 1,
            "medium" => 2,
            "high" => 3,
            "xhigh" => 4,
            "max" => 5,
            "ultra" => 6,
            _ => usize::MAX,
        }
    }

    model
        .capabilities
        .reasoning_efforts
        .iter()
        .enumerate()
        .min_by_key(|(index, effort)| (rank(effort), *index))
        .map(|(_, effort)| effort.as_str())
}

fn apply_test_parameters(model: &mut me::config::ModelConfig) {
    if model.provider == me::config::ProviderType::CodexOauth {
        return;
    }

    let limit = toml::Value::Integer(MODEL_TEST_MAX_OUTPUT_TOKENS);
    let parameter = if model.parameters.contains_key("max_output_tokens") {
        "max_output_tokens"
    } else if model.parameters.contains_key("max_completion_tokens") {
        "max_completion_tokens"
    } else {
        "max_tokens"
    };
    model.parameters.insert(parameter.into(), limit);
    model.parameters.insert(
        "stream_options".into(),
        toml::Value::Table(toml::Table::from_iter([(
            "include_usage".into(),
            toml::Value::Boolean(true),
        )])),
    );
}

fn print_model_test_success(
    model: &me::config::ModelConfig,
    effort: Option<&str>,
    result: &ModelTestResult,
) {
    println!("model: {}", model.name);
    println!("provider: {}", model.provider);
    println!("effort: {}", effort.unwrap_or("-"));
    println!("max output tokens: {}", model_test_output_limit(model));
    println!("status: passed");
    println!("ttft: {}", format_duration(result.ttft));
    println!("elapsed: {}", format_duration(result.elapsed));
    if let Some(usage) = &result.usage {
        println!("input tokens: {}", usage.input_tokens);
        println!("output tokens: {}", usage.output_tokens);
        println!("total tokens: {}", usage.total_tokens);
        if usage.output_tokens > 0 {
            println!(
                "token speed: {:.2} tok/s",
                usage.output_tokens as f64 / result.elapsed.as_secs_f64()
            );
        } else {
            println!("token speed: unavailable (provider returned no output token count)");
        }
    } else {
        println!("token speed: unavailable (provider returned no usage)");
    }
    println!("response:\n{}", result.response);
}

fn model_test_error(
    model: &me::config::ModelConfig,
    effort: Option<&str>,
    elapsed: Duration,
    summary: &str,
    detail: &str,
) -> String {
    format!(
        "model test failed\n  model: {}\n  provider: {}\n  endpoint: {}/{}\n  effort: {}\n  max output tokens: {}\n  elapsed: {}\n  problem: {summary}\n  detail: {detail}",
        model.name,
        model.provider,
        model.base_url.trim_end_matches('/'),
        model.endpoint.trim_start_matches('/'),
        effort.unwrap_or("-"),
        model_test_output_limit(model),
        format_duration(elapsed),
    )
}

fn model_test_output_limit(model: &me::config::ModelConfig) -> String {
    if model.provider == me::config::ProviderType::CodexOauth {
        "provider-managed".into()
    } else {
        MODEL_TEST_MAX_OUTPUT_TOKENS.to_string()
    }
}

fn format_duration(duration: Duration) -> String {
    format!("{:.3}s", duration.as_secs_f64())
}

fn print_codex_status(status: &codex_oauth::CodexStatus) {
    println!(
        "status: {}",
        if status.logged_in {
            "logged in"
        } else {
            "not logged in"
        }
    );
    println!("credential: {}", status.credential_file.display());
    if let Some(mode) = &status.auth_mode {
        println!("auth mode: {mode}");
    }
    if let Some(email) = &status.email {
        println!("email: {email}");
    }
    if let Some(plan) = &status.plan {
        println!("plan: {plan}");
    }
    if let Some(account_id) = &status.account_id {
        println!("account: {account_id}");
    }
    if let Some(expires_at) = status.expires_at {
        let expires_at = i64::try_from(expires_at)
            .ok()
            .and_then(|seconds| chrono::DateTime::from_timestamp(seconds, 0))
            .map(|time| time.to_rfc3339())
            .unwrap_or_else(|| expires_at.to_string());
        println!("access token expires at: {expires_at}");
    }
    if let Some(error) = &status.error {
        println!("credential error: {error}");
    }
}

fn reset_workspace(workspace: &Path) -> Result<()> {
    let directory = workspace.join(".me");
    if !directory.exists() {
        println!("workspace is not initialized: {}", directory.display());
        return Ok(());
    }
    fs::remove_dir_all(&directory)?;
    println!("removed workspace directory: {}", directory.display());
    Ok(())
}

fn print_usage() {
    eprintln!(
        "usage:\n  me-s [--no-tui] [--webui-passkey <password>]\n  me-s init\n  me-s version\n  me-s update\n  me-s create\n  me-s workspace reset\n  me-s codex status\n  me-s codex login\n  me-s codex logout\n  me-s model list\n  me-s model select <name> [effort]\n  me-s model select-default <name>\n  me-s model test <name>\n  me-s model request <prompt>\n  me-s model export <password>\n  me-s model import <file> <password>\n  me-s orch [name]\n  me-s edb [detail]\n  me-s diag upload"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model_config(name: &str, efforts: &[&str]) -> me::config::ModelConfig {
        me::config::ModelConfig {
            name: name.into(),
            provider: me::config::ProviderType::OpenaiCompatible,
            reserve_output_context: true,
            base_url: "https://example.invalid/v1".into(),
            endpoint: "/chat/completions".into(),
            api_key: Some("key".into()),
            api_key_env: None,
            credential_file: None,
            model: name.into(),
            source_url: None,
            timeout_seconds: 1,
            capabilities: me::config::ModelCapabilities {
                reasoning_efforts: efforts.iter().map(|effort| (*effort).into()).collect(),
                ..me::config::ModelCapabilities::default()
            },
            parameters: toml::Table::new(),
            effort_parameters: Default::default(),
        }
    }

    #[test]
    fn ui_launch_options_combine_web_only_and_passkey_modes() {
        assert_eq!(
            ui_launch_options(&[]).unwrap(),
            Some(UiLaunchOptions {
                mode: UiLaunchMode::TuiAndWeb,
                webui_passkey: None,
            })
        );
        assert_eq!(
            ui_launch_options(&["--no-tui".into()]).unwrap(),
            Some(UiLaunchOptions {
                mode: UiLaunchMode::WebOnly,
                webui_passkey: None,
            })
        );
        assert_eq!(
            ui_launch_options(&["--webui-passkey".into(), "secret".into()]).unwrap(),
            Some(UiLaunchOptions {
                mode: UiLaunchMode::TuiAndWeb,
                webui_passkey: Some("secret".into()),
            })
        );
        assert_eq!(
            ui_launch_options(&["--webui-passkey".into(), "secret".into(), "--no-tui".into(),])
                .unwrap(),
            Some(UiLaunchOptions {
                mode: UiLaunchMode::WebOnly,
                webui_passkey: Some("secret".into()),
            })
        );
        assert!(ui_launch_options(&["--webui-passkey".into()]).is_err());
        assert!(ui_launch_options(&["--webui-passkey".into(), String::new()]).is_err());
        assert!(ui_launch_options(&["--no-tui".into(), "--no-tui".into()]).is_err());
        assert!(ui_launch_options(&["create".into()]).unwrap().is_none());
    }

    #[test]
    fn update_is_an_exact_standalone_command() {
        assert!(is_update_command(&["update".into()]));
        assert!(!is_update_command(&[]));
        assert!(!is_update_command(&["update".into(), "extra".into()]));
    }

    #[test]
    fn version_is_an_exact_configuration_independent_command() {
        assert!(is_version_command(&["version".into()]));
        assert!(!is_version_command(&[]));
        assert!(!is_version_command(&["version".into(), "extra".into()]));
        assert_eq!(
            version_string(),
            format!("me-s {}", env!("CARGO_PKG_VERSION"))
        );
    }

    #[test]
    fn workspace_reset_removes_only_me_directory() {
        let workspace = env::temp_dir().join(format!("me-workspace-reset-{}", std::process::id()));
        let me_directory = workspace.join(".me");
        fs::create_dir_all(&me_directory).unwrap();
        fs::write(me_directory.join("config.toml"), "test").unwrap();
        fs::write(workspace.join("keep.txt"), "keep").unwrap();

        reset_workspace(&workspace).unwrap();

        assert!(!me_directory.exists());
        assert!(workspace.join("keep.txt").exists());
        reset_workspace(&workspace).unwrap();
        fs::remove_dir_all(workspace).unwrap();
    }

    #[test]
    fn missing_workspace_can_be_created_from_startup_prompt() {
        let workspace = env::temp_dir().join(format!("me-startup-create-{}", std::process::id()));
        let config_path = workspace_config_path(&workspace);
        let mut input = io::Cursor::new(b"y\n");
        let mut output = Vec::new();

        let local = load_or_offer_workspace(
            &workspace,
            &config_path,
            "test-model",
            &mut input,
            &mut output,
        )
        .unwrap()
        .unwrap();

        assert_eq!(local.model, "test-model");
        assert_eq!(
            WorkspaceConfig::load(&config_path).unwrap().model,
            "test-model"
        );
        assert_eq!(
            EventDataBase::open(&workspace_edb_path(&workspace))
                .unwrap()
                .len(),
            0
        );
        assert!(workspace.join(".me/tools/Terminal.py").is_file());
        assert!(workspace.join(".me/tools/File.py").is_file());
        assert!(workspace.join(".me/tools/WebBrowser.py").is_file());
        assert!(workspace.join(".me/tmp").is_dir());
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("是否立即创建"));
        assert!(output.contains("created workspace"));

        let mut input = io::Cursor::new([]);
        let mut output = Vec::new();
        assert!(
            load_or_offer_workspace(
                &workspace,
                &config_path,
                "ignored-model",
                &mut input,
                &mut output,
            )
            .unwrap()
            .is_some()
        );
        assert!(output.is_empty());
        fs::remove_dir_all(workspace).unwrap();
    }

    #[test]
    fn declining_startup_prompt_leaves_workspace_untouched() {
        let workspace = env::temp_dir().join(format!("me-startup-decline-{}", std::process::id()));
        let config_path = workspace_config_path(&workspace);
        let mut input = io::Cursor::new(b"n\n");
        let mut output = Vec::new();

        assert!(
            load_or_offer_workspace(
                &workspace,
                &config_path,
                "test-model",
                &mut input,
                &mut output,
            )
            .unwrap()
            .is_none()
        );
        assert!(!workspace.join(".me").exists());
        assert!(String::from_utf8(output).unwrap().contains("未创建工作区"));
    }

    #[test]
    fn orchestrator_selection_changes_only_the_workspace_default() {
        let workspace =
            env::temp_dir().join(format!("me-orchestrator-select-{}", std::process::id()));
        let config_path = workspace_config_path(&workspace);
        let mut local = WorkspaceConfig {
            version: 2,
            model: "test".into(),
            effort: me::config::UNSET_EFFORT.into(),
            orchestrator: "chatbot".into(),
        };
        local.save(&config_path).unwrap();
        let mut edb = EventDataBase::open(&workspace_edb_path(&workspace)).unwrap();
        edb.append_agent_kind_def(me::event::AgentKind::Primary, "chatbot", None, None)
            .unwrap();
        edb.append_initial_model("test").unwrap();
        edb.append_initial_reasoning_effort(me::config::UNSET_EFFORT)
            .unwrap();
        edb.append_user_prompt("chatbot history").unwrap();
        drop(edb);

        select_orchestrator(&mut local, &config_path, "main-agent").unwrap();
        assert_eq!(local.orchestrator, "main-agent");
        assert_eq!(
            WorkspaceConfig::load(&config_path).unwrap().orchestrator,
            "main-agent"
        );
        assert_eq!(
            EventDataBase::open(&workspace_edb_path(&workspace))
                .unwrap()
                .len(),
            4
        );
        assert!(select_orchestrator(&mut local, &config_path, "worker-agent").is_err());
        fs::remove_dir_all(workspace).unwrap();
    }

    #[test]
    fn model_list_keeps_custom_legacy_names_and_hides_only_codex_aliases() {
        let custom_same_name = model_config("gpt-5.6-sol", &["unset"]);
        let mut codex_legacy = model_config("gpt-5.6-terra", &["unset"]);
        codex_legacy.provider = me::config::ProviderType::CodexOauth;
        let mut codex_current = model_config("gpt-5.6-luna-512k", &["unset"]);
        codex_current.provider = me::config::ProviderType::CodexOauth;
        let global = GlobalConfig {
            version: 1,
            default_model: custom_same_name.name.clone(),
            models: vec![custom_same_name, codex_legacy, codex_current],
        };

        let names = model_list_rows(&global, Some("gpt-5.6-sol"))
            .into_iter()
            .map(|row| row.columns[0].clone())
            .collect::<Vec<_>>();
        assert_eq!(names, ["gpt-5.6-sol", "gpt-5.6-luna-512k"]);
    }

    #[test]
    fn model_list_is_a_unicode_width_aligned_table() {
        let rows = [
            ModelListRow {
                marker: ' ',
                columns: [
                    "模型".into(),
                    "anthropic".into(),
                    "alpha".into(),
                    "200000".into(),
                    "text".into(),
                    "-".into(),
                ],
            },
            ModelListRow {
                marker: '●',
                columns: [
                    "much-longer".into(),
                    "openai-compatible".into(),
                    "long-model".into(),
                    "1000000".into(),
                    "text,image".into(),
                    "low,high".into(),
                ],
            },
        ];
        let table = format_model_table(&rows);
        let lines = table.lines().collect::<Vec<_>>();
        let headers = lines[0];
        let header_columns = [
            "NAME",
            "PROVIDER",
            "API MODEL",
            "CONTEXT",
            "MODALITIES",
            "EFFORT",
        ]
        .map(|heading| display_width(&headers[..headers.find(heading).unwrap()]));

        assert_eq!(lines.len(), 3);
        assert!(lines[1].starts_with("  模型"));
        assert!(lines[2].starts_with("● much-longer"));
        for (line, values) in [
            (
                lines[1],
                ["模型", "anthropic", "alpha", "200000", "text", "-"],
            ),
            (
                lines[2],
                [
                    "much-longer",
                    "openai-compatible",
                    "long-model",
                    "1000000",
                    "text,image",
                    "low,high",
                ],
            ),
        ] {
            for index in [0, 1, 2, 4, 5] {
                assert_eq!(
                    display_width(&line[..line.find(values[index]).unwrap()]),
                    header_columns[index]
                );
            }
            assert_eq!(
                display_width(&line[..line.find(values[3]).unwrap()]) + display_width(values[3]),
                header_columns[3] + display_width("CONTEXT")
            );
            assert!(!line.ends_with(' '));
        }
    }

    #[test]
    fn model_select_uses_edb_state_and_persists_unsupported_effort_fallback() {
        let workspace =
            env::temp_dir().join(format!("me-model-select-state-{}", std::process::id()));
        let config_path = workspace_config_path(&workspace);
        let mut local = WorkspaceConfig {
            version: 2,
            model: "first".into(),
            effort: "low".into(),
            orchestrator: "main-agent".into(),
        };
        local.save(&config_path).unwrap();
        let mut edb = EventDataBase::open(&workspace_edb_path(&workspace)).unwrap();
        edb.append_initial_model("first").unwrap();
        edb.append_initial_reasoning_effort("low").unwrap();
        drop(edb);
        let global = GlobalConfig {
            version: 1,
            default_model: "first".into(),
            models: vec![
                model_config("first", &["unset", "low"]),
                model_config("second", &["unset", "high"]),
            ],
        };

        select_model(
            &global,
            &mut local,
            &config_path,
            &workspace,
            "second",
            None,
        )
        .unwrap();

        let edb = EventDataBase::open(&workspace_edb_path(&workspace)).unwrap();
        assert_eq!(latest_model(&edb), Some("second"));
        assert_eq!(latest_effort(&edb), Some(me::config::UNSET_EFFORT));
        assert!(matches!(
            edb.events().get(2),
            Some(me::event::Event::ModelChanged(event))
                if event.cause == me::event::ModelChangeCause::User
        ));
        assert!(matches!(
            edb.events().get(3),
            Some(me::event::Event::ReasoningEffortChanged(event))
                if event.cause == me::event::ReasoningEffortChangeCause::ModelUnsupported
        ));
        let saved = WorkspaceConfig::load(&config_path).unwrap();
        assert_eq!(saved.model, "second");
        assert_eq!(saved.effort, me::config::UNSET_EFFORT);
        fs::remove_dir_all(workspace).unwrap();
    }

    #[test]
    fn model_select_changes_main_without_changing_its_worker() {
        let workspace = env::temp_dir().join(format!(
            "me-manager-model-select-sync-{}",
            std::process::id()
        ));
        let config_path = workspace_config_path(&workspace);
        let mut local = WorkspaceConfig {
            version: 2,
            model: "first".into(),
            effort: "low".into(),
            orchestrator: "manager-agent".into(),
        };
        local.save(&config_path).unwrap();
        let mut manager = EventDataBase::open(&workspace_edb_path(&workspace)).unwrap();
        manager.append_initial_model("first").unwrap();
        manager.append_initial_reasoning_effort("low").unwrap();
        drop(manager);
        let worker_path = workspace.join(".me/edb/agent-worker.edb");
        let mut worker = EventDataBase::open(&worker_path).unwrap();
        worker
            .append_agent_kind_def(
                me::event::AgentKind::SubAgent,
                "worker-agent",
                Some("main".into()),
                None,
            )
            .unwrap();
        worker.append_initial_model("first").unwrap();
        worker.append_initial_reasoning_effort("low").unwrap();
        drop(worker);
        let global = GlobalConfig {
            version: 1,
            default_model: "first".into(),
            models: vec![
                model_config("first", &["unset", "low"]),
                model_config("second", &["unset", "high"]),
            ],
        };

        select_model(
            &global,
            &mut local,
            &config_path,
            &workspace,
            "second",
            None,
        )
        .unwrap();

        let manager = EventDataBase::open(&workspace_edb_path(&workspace)).unwrap();
        let worker = EventDataBase::open(&worker_path).unwrap();
        assert_eq!(latest_model(&manager), Some("second"));
        assert_eq!(latest_effort(&manager), Some(me::config::UNSET_EFFORT));
        assert_eq!(latest_model(&worker), Some("first"));
        assert_eq!(latest_effort(&worker), Some("low"));
        fs::remove_dir_all(workspace).unwrap();
    }

    #[test]
    fn model_select_default_persists_global_default_without_touching_models() {
        let directory =
            env::temp_dir().join(format!("me-model-select-default-{}", std::process::id()));
        let path = directory.join("models.toml");
        let mut global = GlobalConfig {
            version: 1,
            default_model: "first".into(),
            models: vec![
                model_config("first", &["unset"]),
                model_config("second", &["unset"]),
            ],
        };
        global.save(&path).unwrap();

        select_default_model(&mut global, &path, "second").unwrap();

        let loaded = GlobalConfig::load(&path).unwrap();
        assert_eq!(loaded.default_model, "second");
        assert_eq!(loaded.models.len(), 2);
        assert_eq!(loaded.models[0].name, "first");
        assert_eq!(loaded.models[1].name, "second");

        let error = select_default_model(&mut global, &path, "missing").unwrap_err();
        assert!(error.to_string().contains("model missing does not exist"));
        assert_eq!(GlobalConfig::load(&path).unwrap().default_model, "second");
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn workspace_commands_do_not_recreate_main_in_an_empty_workspace() {
        let workspace = env::temp_dir().join(format!("me-empty-workspace-{}", std::process::id()));
        let config_path = workspace_config_path(&workspace);
        let mut local = WorkspaceConfig {
            version: 2,
            model: "first".into(),
            effort: "unset".into(),
            orchestrator: "main-agent".into(),
        };
        local.save(&config_path).unwrap();
        let global = GlobalConfig {
            version: 1,
            default_model: "first".into(),
            models: vec![
                model_config("first", &["unset"]),
                model_config("second", &["unset"]),
            ],
        };

        assert_eq!(
            selected_workspace_model(&workspace, &local).unwrap(),
            "first"
        );
        select_model(
            &global,
            &mut local,
            &config_path,
            &workspace,
            "second",
            None,
        )
        .unwrap();
        select_orchestrator(&mut local, &config_path, "main-agent").unwrap();

        assert!(!workspace_edb_path(&workspace).exists());
        assert!(workspace_edb_paths(&workspace).unwrap().is_empty());
        fs::remove_dir_all(workspace).unwrap();
    }

    #[test]
    fn model_test_uses_lowest_effort_and_provider_specific_limit() {
        let mut model = me::config::ModelConfig {
            name: "test".into(),
            provider: me::config::ProviderType::OpenaiCompatible,
            reserve_output_context: true,
            base_url: "https://example.com/v1".into(),
            endpoint: "/chat/completions".into(),
            api_key: Some("key".into()),
            api_key_env: None,
            credential_file: None,
            model: "api-model".into(),
            source_url: None,
            timeout_seconds: 1,
            capabilities: me::config::ModelCapabilities {
                reasoning_efforts: vec!["max".into(), "high".into(), "none".into()],
                ..me::config::ModelCapabilities::default()
            },
            parameters: toml::from_str("max_completion_tokens = 8192").unwrap(),
            effort_parameters: Default::default(),
        };
        assert_eq!(lowest_effort(&model), Some("none"));
        apply_test_parameters(&mut model);
        assert_eq!(
            model.parameters["max_completion_tokens"].as_integer(),
            Some(MODEL_TEST_MAX_OUTPUT_TOKENS)
        );
        assert_eq!(
            model.parameters["stream_options"]
                .get("include_usage")
                .and_then(toml::Value::as_bool),
            Some(true)
        );
        assert!(!model.parameters.contains_key("max_tokens"));

        model.provider = me::config::ProviderType::CodexOauth;
        model.parameters.clear();
        apply_test_parameters(&mut model);
        assert!(model.parameters.is_empty());
        assert_eq!(model_test_output_limit(&model), "provider-managed");

        let error = model_test_error(
            &model,
            Some("low"),
            Duration::from_millis(1250),
            "request failed",
            "401 Unauthorized",
        );
        assert!(error.contains("model: test"));
        assert!(error.contains("1.250s"));
        assert!(error.contains("401 Unauthorized"));
        assert!(!error.contains("key"));
    }
}
