#![doc = "Provides the Unclean command-line contract and converts shared failures into stable process results."]

use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Arg, ArgAction, ArgGroup, ArgMatches, Command};
use serde::Serialize;
use unclean_core::apply::{
    OperationReport, ProjectRestorePlan, RestorePlan, TemplateRestorePlan, WriteConfirmation,
    apply_engine_plan, apply_project_plan, apply_template_plan, build_project_restore_plan,
    build_restore_plan, build_template_restore_plan, restore_engine_plan, restore_project_plan,
    restore_template_plan, write_confirmation,
};
use unclean_core::dependencies::{DependencyWarning, analyze_plugins};
use unclean_core::descriptors::{PluginDescriptor, PluginScanWarning, scan_engine_plugins};
use unclean_core::discovery::{
    DiscoveryOptions, DiscoveryReport, DiscoverySource, DiscoveryWarning, EngineInstallation,
    discover_engines, select_engine_by_version,
};
use unclean_core::elevation::{
    ActiveUnrealProcess, ELEVATED_REQUEST_OPTION, ELEVATED_WORKER_COMMAND, ElevatedRequest,
    find_active_unreal_processes, run_elevated_request, run_elevated_worker,
    template_write_access_requires_elevation, write_access_requires_elevation,
};
use unclean_core::journal::{
    EngineStatus, JournalOperation, ProjectStatus, default_journal_path, engine_history,
    inspect_engine_status, inspect_project_status, inspect_template_status, project_history,
    template_history,
};
use unclean_core::plans::{EnginePlan, PlanBuildOptions, build_engine_plan};
use unclean_core::presets::{
    Preset, PresetFile, PresetRuleSource, default_preset_directory, list_available_presets,
    load_preset,
};
use unclean_core::project_plans::{
    ProjectPlan, build_project_edit_plan, build_project_preset_plan,
};
use unclean_core::project_state::{ProjectWorkspace, load_project_workspace};
use unclean_core::projects::{
    ProjectDescriptorEdit, ProjectPluginEdit, ProjectPluginEditAction, ProjectSuppressionEdit,
};
use unclean_core::templates::{
    TemplateCatalog, TemplatePlan, build_template_plan, resolve_template_selection,
    scan_engine_templates,
};
use unclean_core::{Error, ErrorCode, PRODUCT_NAME, Result};

const OUTPUT_FORMAT: &str = "format";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OutputFormat {
    Table,
    Json,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CommandOutcome {
    Success,
    Drift,
}

impl CommandOutcome {
    fn exit_code(self) -> ExitCode {
        match self {
            Self::Success => ExitCode::SUCCESS,
            Self::Drift => ExitCode::from(ErrorCode::Drift.exit_code()),
        }
    }
}

#[derive(Serialize)]
struct FailureEnvelope<'a> {
    schema: u8,
    ok: bool,
    error: FailureBody<'a>,
}

#[derive(Serialize)]
struct FailureBody<'a> {
    code: &'static str,
    message: &'a str,
    exit_code: u8,
}

#[derive(Serialize)]
struct EngineEnvelope<'a> {
    schema: u8,
    ok: bool,
    engines: &'a [EngineInstallation],
    warnings: &'a [DiscoveryWarning],
}

#[derive(Serialize)]
struct PluginEnvelope<'a> {
    schema: u8,
    ok: bool,
    engine: &'a EngineInstallation,
    plugins: &'a [PluginDescriptor],
    warnings: &'a [PluginScanWarning],
    dependency_warnings: &'a [DependencyWarning],
    discovery_warnings: &'a [DiscoveryWarning],
}

#[derive(Serialize)]
struct PresetListEnvelope<'a> {
    schema: u8,
    ok: bool,
    directory: Option<&'a std::path::Path>,
    presets: &'a [PresetFile],
}

#[derive(Serialize)]
struct PresetEnvelope<'a> {
    schema: u8,
    ok: bool,
    path: &'a std::path::Path,
    preset: &'a Preset,
}

#[derive(Serialize)]
struct PlanEnvelope<'a> {
    schema: u8,
    ok: bool,
    plan: &'a EnginePlan,
    discovery_warnings: &'a [DiscoveryWarning],
}

#[derive(Serialize)]
struct StatusEnvelope<'a> {
    schema: u8,
    ok: bool,
    engine: &'a EngineInstallation,
    status: &'a EngineStatus,
    discovery_warnings: &'a [DiscoveryWarning],
}

#[derive(Serialize)]
struct ApplyEnvelope<'a> {
    schema: u8,
    ok: bool,
    plan: &'a EnginePlan,
    result: &'a OperationReport,
    discovery_warnings: &'a [DiscoveryWarning],
}

#[derive(Serialize)]
struct HistoryEnvelope<'a> {
    schema: u8,
    ok: bool,
    engine: &'a EngineInstallation,
    operations: &'a [JournalOperation],
    discovery_warnings: &'a [DiscoveryWarning],
}

#[derive(Serialize)]
struct RestoreEnvelope<'a> {
    schema: u8,
    ok: bool,
    plan: &'a RestorePlan,
    result: &'a OperationReport,
    discovery_warnings: &'a [DiscoveryWarning],
}

#[derive(Serialize)]
struct ProjectWorkspaceEnvelope<'a> {
    schema: u8,
    ok: bool,
    workspace: &'a ProjectWorkspace,
    discovery_warnings: &'a [DiscoveryWarning],
}

#[derive(Serialize)]
struct ProjectPlanEnvelope<'a> {
    schema: u8,
    ok: bool,
    plan: &'a ProjectPlan,
    discovery_warnings: &'a [DiscoveryWarning],
}

#[derive(Serialize)]
struct ProjectApplyEnvelope<'a> {
    schema: u8,
    ok: bool,
    plan: &'a ProjectPlan,
    result: &'a OperationReport,
    discovery_warnings: &'a [DiscoveryWarning],
}

#[derive(Serialize)]
struct ProjectStatusEnvelope<'a> {
    schema: u8,
    ok: bool,
    project: &'a Path,
    status: &'a ProjectStatus,
    discovery_warnings: &'a [DiscoveryWarning],
}

#[derive(Serialize)]
struct ProjectHistoryEnvelope<'a> {
    schema: u8,
    ok: bool,
    project: &'a Path,
    operations: &'a [JournalOperation],
    discovery_warnings: &'a [DiscoveryWarning],
}

#[derive(Serialize)]
struct ProjectRestoreEnvelope<'a> {
    schema: u8,
    ok: bool,
    plan: &'a ProjectRestorePlan,
    result: &'a OperationReport,
    discovery_warnings: &'a [DiscoveryWarning],
}

#[derive(Serialize)]
struct TemplateCatalogEnvelope<'a> {
    schema: u8,
    ok: bool,
    catalog: &'a TemplateCatalog,
    discovery_warnings: &'a [DiscoveryWarning],
}

#[derive(Serialize)]
struct TemplatePlanEnvelope<'a> {
    schema: u8,
    ok: bool,
    plan: &'a TemplatePlan,
    discovery_warnings: &'a [DiscoveryWarning],
}

#[derive(Serialize)]
struct TemplateApplyEnvelope<'a> {
    schema: u8,
    ok: bool,
    plan: &'a TemplatePlan,
    result: &'a OperationReport,
    discovery_warnings: &'a [DiscoveryWarning],
}

#[derive(Serialize)]
struct TemplateRestoreEnvelope<'a> {
    schema: u8,
    ok: bool,
    plan: &'a TemplateRestorePlan,
    result: &'a OperationReport,
    discovery_warnings: &'a [DiscoveryWarning],
}

fn main() -> ExitCode {
    let matches = command().get_matches();
    if let Some(worker) = matches.subcommand_matches(ELEVATED_WORKER_COMMAND) {
        let Some(request_path) = worker.get_one::<String>("request") else {
            let error = Error::Internal {
                message: "the elevated worker request path is missing".to_owned(),
            };
            render_error(OutputFormat::Table, &error);
            return ExitCode::from(error.code().exit_code());
        };
        return ExitCode::from(run_elevated_worker(Path::new(request_path)));
    }
    let format = output_format(&matches);

    match dispatch(&matches, format) {
        Ok(outcome) => outcome.exit_code(),
        Err(error) => {
            render_error(format, &error);
            ExitCode::from(error.code().exit_code())
        }
    }
}

fn command() -> Command {
    Command::new("unclean")
        .version(unclean_core::version())
        .about("Review and manage Unreal Engine plugin defaults.")
        .arg_required_else_help(true)
        .subcommand_required(true)
        .arg(
            Arg::new(OUTPUT_FORMAT)
                .long(OUTPUT_FORMAT)
                .global(true)
                .default_value("table")
                .value_parser(["table", "json"])
                .help("Choose human-readable table output or versioned JSON output."),
        )
        .subcommand(engine_selector(
            Command::new("engines").about("List discovered engine installations and their health."),
        ))
        .subcommand(engine_selector(
            Command::new("plugins").about("List plugin state for one engine."),
        ))
        .subcommand(Command::new("presets").about("List available presets."))
        .subcommand(
            Command::new("preset")
                .about("Inspect or validate one preset.")
                .subcommand_required(true)
                .arg_required_else_help(true)
                .subcommand(
                    Command::new("show")
                        .about("Show the resolved contents of one preset.")
                        .arg(preset_argument()),
                )
                .subcommand(
                    Command::new("validate")
                        .about("Validate one preset without scanning or writing.")
                        .arg(preset_argument()),
                ),
        )
        .subcommand(
            engine_selector(Command::new("plan").about("Print the reviewed plan without writing."))
                .arg(preset_option()),
        )
        .subcommand(
            engine_selector(Command::new("apply").about("Review and apply one preset."))
                .arg(preset_option())
                .arg(yes_option()),
        )
        .subcommand(engine_selector(
            Command::new("status").about("Report recorded drift for one engine."),
        ))
        .subcommand(engine_selector(
            Command::new("history").about("List recorded operations for one engine."),
        ))
        .subcommand(
            engine_selector(
                Command::new("restore").about("Review and restore one recorded snapshot."),
            )
            .arg(
                Arg::new("snapshot")
                    .long("snapshot")
                    .required(true)
                    .value_name("SNAPSHOT")
                    .help("Select the snapshot identifier to restore."),
            )
            .arg(yes_option()),
        )
        .subcommand(engine_selector(
            Command::new("templates").about("List project templates for one engine."),
        ))
        .subcommand(template_command())
        .subcommand(project_command())
        .subcommand(Command::new("gui").about("Open the desktop interface."))
        .subcommand(
            Command::new(ELEVATED_WORKER_COMMAND).hide(true).arg(
                Arg::new("request")
                    .long(ELEVATED_REQUEST_OPTION.trim_start_matches('-'))
                    .required(true)
                    .value_name("PATH"),
            ),
        )
}

fn template_command() -> Command {
    Command::new("template")
        .about("Review and manage suppression in new-project templates.")
        .subcommand_required(true)
        .arg_required_else_help(true)
        .subcommand(template_edit_arguments(engine_selector(
            Command::new("plan").about("Print a template suppression plan without writing."),
        )))
        .subcommand(
            template_edit_arguments(engine_selector(
                Command::new("apply").about("Review and apply template suppression."),
            ))
            .arg(yes_option()),
        )
        .subcommand(engine_selector(
            Command::new("status").about("Report recorded drift for engine templates."),
        ))
        .subcommand(engine_selector(
            Command::new("history").about("List recorded template operations for one engine."),
        ))
        .subcommand(
            engine_selector(
                Command::new("restore").about("Review and restore one template snapshot."),
            )
            .arg(
                Arg::new("snapshot")
                    .long("snapshot")
                    .required(true)
                    .value_name("SNAPSHOT")
                    .help("Select the template snapshot identifier to restore."),
            )
            .arg(yes_option()),
        )
}

fn template_edit_arguments(command: Command) -> Command {
    command
        .group(
            ArgGroup::new("template-selection")
                .args(["template", "all"])
                .required(true)
                .multiple(false),
        )
        .arg(
            Arg::new("template")
                .long("template")
                .value_name("TEMPLATE")
                .action(ArgAction::Append)
                .help("Select a template by exact name or engine-relative path."),
        )
        .arg(
            Arg::new("all")
                .long("all")
                .action(ArgAction::SetTrue)
                .help("Select every valid project template in this engine."),
        )
        .arg(
            Arg::new("suppression")
                .long("suppression")
                .required(true)
                .value_name("STATE")
                .value_parser(["enabled", "disabled", "clear"])
                .help("Set or clear DisableEnginePluginsByDefault in the selected templates."),
        )
}

fn project_command() -> Command {
    Command::new("project")
        .about("Review and manage plugin overrides in one Unreal project.")
        .subcommand_required(true)
        .arg_required_else_help(true)
        .subcommand(project_selector(
            Command::new("plugins")
                .about("List engine defaults and project-specific plugin state."),
        ))
        .subcommand(project_edit_arguments(project_selector(
            Command::new("plan").about("Print a project plugin plan without writing."),
        )))
        .subcommand(
            project_edit_arguments(project_selector(
                Command::new("apply").about("Review and apply project plugin overrides."),
            ))
            .arg(yes_option()),
        )
        .subcommand(project_selector(
            Command::new("status").about("Report recorded drift for one project."),
        ))
        .subcommand(project_selector(
            Command::new("history").about("List recorded operations for one project."),
        ))
        .subcommand(
            project_selector(
                Command::new("restore").about("Review and restore one project snapshot."),
            )
            .arg(
                Arg::new("snapshot")
                    .long("snapshot")
                    .required(true)
                    .value_name("SNAPSHOT")
                    .help("Select the project snapshot identifier to restore."),
            )
            .arg(yes_option()),
        )
}

fn project_selector(command: Command) -> Command {
    command
        .arg(
            Arg::new("project")
                .long("project")
                .required(true)
                .value_name("PATH")
                .help("Select one .uproject file."),
        )
        .arg(
            Arg::new("engine-path")
                .long("engine-path")
                .value_name("PATH")
                .help("Limit engine discovery to one installation path."),
        )
}

fn project_edit_arguments(command: Command) -> Command {
    command
        .arg(
            Arg::new("preset")
                .long("preset")
                .value_name("PRESET")
                .conflicts_with_all(["enable", "disable", "clear"])
                .help("Map one saved preset to project plugin references."),
        )
        .arg(
            Arg::new("enable")
                .long("enable")
                .value_name("PLUGIN")
                .action(ArgAction::Append)
                .help("Write an explicit enabled project reference."),
        )
        .arg(
            Arg::new("disable")
                .long("disable")
                .value_name("PLUGIN")
                .action(ArgAction::Append)
                .help("Write an explicit disabled project reference."),
        )
        .arg(
            Arg::new("clear")
                .long("clear")
                .value_name("PLUGIN")
                .action(ArgAction::Append)
                .help("Remove an explicit project reference."),
        )
        .arg(
            Arg::new("suppression")
                .long("suppression")
                .value_name("STATE")
                .value_parser(["enabled", "disabled", "clear"])
                .help("Set or clear DisableEnginePluginsByDefault in the project."),
        )
}

fn engine_selector(command: Command) -> Command {
    command
        .arg(
            Arg::new("engine")
                .long("engine")
                .value_name("VERSION")
                .conflicts_with("engine-path")
                .help("Select an engine by its reported version."),
        )
        .arg(
            Arg::new("engine-path")
                .long("engine-path")
                .value_name("PATH")
                .conflicts_with("engine")
                .help("Select an engine by its installation path."),
        )
}

fn preset_argument() -> Arg {
    Arg::new("preset")
        .required(true)
        .value_name("PRESET")
        .help("Select a preset by name or path.")
}

fn preset_option() -> Arg {
    Arg::new("preset")
        .long("preset")
        .required(true)
        .value_name("PRESET")
        .help("Select a preset by name or path.")
}

fn yes_option() -> Arg {
    Arg::new("yes")
        .long("yes")
        .action(ArgAction::SetTrue)
        .help("Confirm the reviewed plan in a noninteractive session.")
}

fn output_format(matches: &ArgMatches) -> OutputFormat {
    match matches.get_one::<String>(OUTPUT_FORMAT).map(String::as_str) {
        Some("json") => OutputFormat::Json,
        _ => OutputFormat::Table,
    }
}

fn dispatch(matches: &ArgMatches, format: OutputFormat) -> Result<CommandOutcome> {
    let Some((name, subcommand_matches)) = matches.subcommand() else {
        return Err(Error::Internal {
            message: "the parsed command has no subcommand".to_owned(),
        });
    };

    if name == "engines" {
        run_engines(subcommand_matches, format)?;
        return Ok(CommandOutcome::Success);
    }
    if name == "plugins" {
        run_plugins(subcommand_matches, format)?;
        return Ok(CommandOutcome::Success);
    }
    if name == "presets" {
        run_presets(format)?;
        return Ok(CommandOutcome::Success);
    }
    if name == "preset" {
        run_preset(subcommand_matches, format)?;
        return Ok(CommandOutcome::Success);
    }
    if name == "plan" {
        run_plan(subcommand_matches, format)?;
        return Ok(CommandOutcome::Success);
    }
    if name == "apply" {
        run_apply(subcommand_matches, format)?;
        return Ok(CommandOutcome::Success);
    }
    if name == "status" {
        return run_status(subcommand_matches, format);
    }
    if name == "history" {
        run_history(subcommand_matches, format)?;
        return Ok(CommandOutcome::Success);
    }
    if name == "restore" {
        run_restore(subcommand_matches, format)?;
        return Ok(CommandOutcome::Success);
    }
    if name == "templates" {
        run_templates(subcommand_matches, format)?;
        return Ok(CommandOutcome::Success);
    }
    if name == "template" {
        return run_template(subcommand_matches, format);
    }
    if name == "project" {
        return run_project(subcommand_matches, format);
    }

    let command = match name {
        "gui" => "gui",
        _ => {
            return Err(Error::Internal {
                message: "the parsed command is unknown".to_owned(),
            });
        }
    };

    Err(Error::Unavailable { command })
}

fn run_template(matches: &ArgMatches, format: OutputFormat) -> Result<CommandOutcome> {
    let Some((name, command_matches)) = matches.subcommand() else {
        return Err(Error::Internal {
            message: "the parsed template command has no subcommand".to_owned(),
        });
    };
    match name {
        "plan" => {
            run_template_plan(command_matches, format)?;
            Ok(CommandOutcome::Success)
        }
        "apply" => {
            run_template_apply(command_matches, format)?;
            Ok(CommandOutcome::Success)
        }
        "status" => run_template_status(command_matches, format),
        "history" => {
            run_template_history(command_matches, format)?;
            Ok(CommandOutcome::Success)
        }
        "restore" => {
            run_template_restore(command_matches, format)?;
            Ok(CommandOutcome::Success)
        }
        _ => Err(Error::Internal {
            message: "the parsed template command is unknown".to_owned(),
        }),
    }
}

fn run_project(matches: &ArgMatches, format: OutputFormat) -> Result<CommandOutcome> {
    let Some((name, command_matches)) = matches.subcommand() else {
        return Err(Error::Internal {
            message: "the parsed project command has no subcommand".to_owned(),
        });
    };
    match name {
        "plugins" => {
            run_project_plugins(command_matches, format)?;
            Ok(CommandOutcome::Success)
        }
        "plan" => {
            run_project_plan(command_matches, format)?;
            Ok(CommandOutcome::Success)
        }
        "apply" => {
            run_project_apply(command_matches, format)?;
            Ok(CommandOutcome::Success)
        }
        "status" => run_project_status(command_matches, format),
        "history" => {
            run_project_history(command_matches, format)?;
            Ok(CommandOutcome::Success)
        }
        "restore" => {
            run_project_restore(command_matches, format)?;
            Ok(CommandOutcome::Success)
        }
        _ => Err(Error::Internal {
            message: "the parsed project command is unknown".to_owned(),
        }),
    }
}

fn run_engines(matches: &ArgMatches, format: OutputFormat) -> Result<()> {
    let requested_path = matches.get_one::<String>("engine-path");
    let requested_version = matches.get_one::<String>("engine");
    let options = if let Some(path) = requested_path {
        DiscoveryOptions {
            explicit_paths: vec![path.into()],
            current_dir: None,
            launcher_manifest: None,
            include_registry: false,
        }
    } else {
        DiscoveryOptions::default()
    };
    let mut report = discover_engines(&options);

    if let Some(version) = requested_version {
        report.engines = vec![select_engine_by_version(&report.engines, version)?.clone()];
    }

    render_engines(format, &report)
}

fn run_plugins(matches: &ArgMatches, format: OutputFormat) -> Result<()> {
    let (engine, discovery_warnings) = selected_engine(matches)?;
    let scan = scan_engine_plugins(&engine)?;
    let report = analyze_plugins(scan.plugins);
    render_plugins(
        format,
        &engine,
        &report.plugins,
        &scan.warnings,
        &report.warnings,
        &discovery_warnings,
    )
}

fn run_presets(format: OutputFormat) -> Result<()> {
    let directory = default_preset_directory();
    let presets = list_available_presets(directory.as_deref())?;
    render_presets(format, directory.as_deref(), &presets)
}

fn run_preset(matches: &ArgMatches, format: OutputFormat) -> Result<()> {
    let Some((command, command_matches)) = matches.subcommand() else {
        return Err(Error::Internal {
            message: "the parsed preset command has no subcommand".to_owned(),
        });
    };
    let selector = command_matches
        .get_one::<String>("preset")
        .ok_or_else(|| Error::Internal {
            message: "the parsed preset command has no selector".to_owned(),
        })?;
    let directory = default_preset_directory();
    let (path, document) = load_preset(selector, directory.as_deref())?;
    render_preset(format, command, &path, document.preset())
}

fn run_plan(matches: &ArgMatches, format: OutputFormat) -> Result<()> {
    let (engine, discovery_warnings) = selected_engine(matches)?;
    let selector = matches
        .get_one::<String>("preset")
        .ok_or_else(|| Error::Internal {
            message: "the parsed plan command has no preset selector".to_owned(),
        })?;
    let preset_directory = default_preset_directory();
    let (preset_path, document) = load_preset(selector, preset_directory.as_deref())?;
    let options = PlanBuildOptions::for_current_process()?;
    let plan = build_engine_plan(&engine, &preset_path, document.preset(), &options)?;
    render_plan(format, &plan, &discovery_warnings)
}

fn run_apply(matches: &ArgMatches, format: OutputFormat) -> Result<()> {
    let (engine, discovery_warnings) = selected_engine(matches)?;
    let selector = matches
        .get_one::<String>("preset")
        .ok_or_else(|| Error::Internal {
            message: "the parsed apply command has no preset selector".to_owned(),
        })?;
    let preset_directory = default_preset_directory();
    let (preset_path, document) = load_preset(selector, preset_directory.as_deref())?;
    let options = PlanBuildOptions::for_current_process()?;
    let plan = build_engine_plan(&engine, &preset_path, document.preset(), &options)?;
    if format == OutputFormat::Table {
        render_plan(format, &plan, &discovery_warnings)?;
    }
    confirm_write(
        matches,
        format,
        plan.changes().is_empty(),
        "Apply this plan",
    )?;
    confirm_active_processes(format, plan.changes().is_empty(), &engine.path)?;
    let journal_path = default_journal_path()?;
    let relative_paths = plan
        .changes()
        .iter()
        .map(|change| change.relative_path.clone())
        .collect::<Vec<_>>();
    let report = if write_access_requires_elevation(&engine, &relative_paths)? {
        run_elevated_request(&ElevatedRequest::from_engine_plan(&plan)?)?
    } else {
        apply_engine_plan(&plan, &journal_path)?
    };
    render_apply(format, &plan, &report, &discovery_warnings)
}

fn run_status(matches: &ArgMatches, format: OutputFormat) -> Result<CommandOutcome> {
    let (engine, discovery_warnings) = selected_engine(matches)?;
    let journal_path = default_journal_path()?;
    let status = inspect_engine_status(&engine, &journal_path)?;
    render_status(format, &engine, &status, &discovery_warnings)?;
    if status.drifted {
        Ok(CommandOutcome::Drift)
    } else {
        Ok(CommandOutcome::Success)
    }
}

fn run_history(matches: &ArgMatches, format: OutputFormat) -> Result<()> {
    let (engine, discovery_warnings) = selected_engine(matches)?;
    let journal_path = default_journal_path()?;
    let operations = engine_history(&engine, &journal_path)?;
    render_history(format, &engine, &operations, &discovery_warnings)
}

fn run_restore(matches: &ArgMatches, format: OutputFormat) -> Result<()> {
    let (engine, discovery_warnings) = selected_engine(matches)?;
    let snapshot = matches
        .get_one::<String>("snapshot")
        .ok_or_else(|| Error::Internal {
            message: "the parsed restore command has no snapshot selector".to_owned(),
        })?;
    let journal_path = default_journal_path()?;
    let options = PlanBuildOptions::for_current_process()?;
    let plan = build_restore_plan(&engine, snapshot, &journal_path, &options)?;
    if format == OutputFormat::Table {
        render_restore_plan(&plan, &discovery_warnings);
    }
    confirm_write(
        matches,
        format,
        plan.changes().is_empty(),
        "Restore this snapshot",
    )?;
    confirm_active_processes(format, plan.changes().is_empty(), &engine.path)?;
    let relative_paths = plan
        .changes()
        .iter()
        .map(|change| change.relative_path.clone())
        .collect::<Vec<_>>();
    let report = if write_access_requires_elevation(&engine, &relative_paths)? {
        run_elevated_request(&ElevatedRequest::from_restore_plan(&plan)?)?
    } else {
        restore_engine_plan(&plan, &journal_path)?
    };
    render_restore(format, &plan, &report, &discovery_warnings)
}

fn run_templates(matches: &ArgMatches, format: OutputFormat) -> Result<()> {
    let (engine, discovery_warnings) = selected_engine(matches)?;
    let catalog = scan_engine_templates(&engine)?;
    render_templates(format, &catalog, &discovery_warnings)
}

fn run_template_plan(matches: &ArgMatches, format: OutputFormat) -> Result<()> {
    let (engine, discovery_warnings) = selected_engine(matches)?;
    let options = PlanBuildOptions::for_current_process()?;
    let plan = requested_template_plan(matches, &engine, &options)?;
    render_template_plan(format, &plan, &discovery_warnings)
}

fn run_template_apply(matches: &ArgMatches, format: OutputFormat) -> Result<()> {
    let (engine, discovery_warnings) = selected_engine(matches)?;
    let options = PlanBuildOptions::for_current_process()?;
    let plan = requested_template_plan(matches, &engine, &options)?;
    if format == OutputFormat::Table {
        render_template_plan(format, &plan, &discovery_warnings)?;
    }
    confirm_write(
        matches,
        format,
        plan.changes().is_empty(),
        "Apply suppression to these templates",
    )?;
    confirm_active_processes(format, plan.changes().is_empty(), &engine.path)?;
    let relative_paths = plan
        .changes()
        .iter()
        .map(|change| change.relative_path.clone())
        .collect::<Vec<_>>();
    let journal_path = default_journal_path()?;
    let report = if template_write_access_requires_elevation(&engine, &relative_paths)? {
        run_elevated_request(&ElevatedRequest::from_template_plan(&plan)?)?
    } else {
        apply_template_plan(&plan, &journal_path)?
    };
    render_template_apply(format, &plan, &report, &discovery_warnings)
}

fn run_template_status(matches: &ArgMatches, format: OutputFormat) -> Result<CommandOutcome> {
    let (engine, discovery_warnings) = selected_engine(matches)?;
    let journal_path = default_journal_path()?;
    let status = inspect_template_status(&engine, &journal_path)?;
    render_template_status(format, &engine, &status, &discovery_warnings)?;
    if status.drifted {
        Ok(CommandOutcome::Drift)
    } else {
        Ok(CommandOutcome::Success)
    }
}

fn run_template_history(matches: &ArgMatches, format: OutputFormat) -> Result<()> {
    let (engine, discovery_warnings) = selected_engine(matches)?;
    let journal_path = default_journal_path()?;
    let operations = template_history(&engine, &journal_path)?;
    render_template_history(format, &engine, &operations, &discovery_warnings)
}

fn run_template_restore(matches: &ArgMatches, format: OutputFormat) -> Result<()> {
    let (engine, discovery_warnings) = selected_engine(matches)?;
    let snapshot = matches
        .get_one::<String>("snapshot")
        .ok_or_else(|| Error::Internal {
            message: "the parsed template restore command has no snapshot selector".to_owned(),
        })?;
    let journal_path = default_journal_path()?;
    let options = PlanBuildOptions::for_current_process()?;
    let plan = build_template_restore_plan(&engine, snapshot, &journal_path, &options)?;
    if format == OutputFormat::Table {
        render_template_restore_plan(&plan, &discovery_warnings);
    }
    confirm_write(
        matches,
        format,
        plan.changes().is_empty(),
        "Restore this template snapshot",
    )?;
    confirm_active_processes(format, plan.changes().is_empty(), &engine.path)?;
    let relative_paths = plan
        .changes()
        .iter()
        .map(|change| change.relative_path.clone())
        .collect::<Vec<_>>();
    let report = if template_write_access_requires_elevation(&engine, &relative_paths)? {
        run_elevated_request(&ElevatedRequest::from_template_restore_plan(&plan)?)?
    } else {
        restore_template_plan(&plan, &journal_path)?
    };
    render_template_restore(format, &plan, &report, &discovery_warnings)
}

fn requested_template_plan(
    matches: &ArgMatches,
    engine: &EngineInstallation,
    options: &PlanBuildOptions,
) -> Result<TemplatePlan> {
    let catalog = scan_engine_templates(engine)?;
    let selected = if matches.get_flag("all") {
        if catalog.templates.is_empty() {
            return Err(Error::NotFound {
                item: format!(
                    "valid project templates under {}",
                    engine.path.join("Templates").display()
                ),
            });
        }
        catalog
            .templates
            .iter()
            .map(|template| template.relative_path.clone())
            .collect()
    } else {
        let selectors = matches
            .get_many::<String>("template")
            .into_iter()
            .flatten()
            .cloned()
            .collect::<Vec<_>>();
        resolve_template_selection(&catalog, &selectors)?
    };
    build_template_plan(engine, &selected, suppression_edit(matches), options)
}

fn run_project_plugins(matches: &ArgMatches, format: OutputFormat) -> Result<()> {
    let (project_path, report) = selected_project_context(matches)?;
    let workspace = load_project_workspace(&project_path, &report.engines).map_err(|error| {
        Error::InvalidInput {
            message: error.to_string(),
        }
    })?;
    render_project_plugins(format, &workspace, &report.warnings)
}

fn run_project_plan(matches: &ArgMatches, format: OutputFormat) -> Result<()> {
    let (project_path, report) = selected_project_context(matches)?;
    let options = PlanBuildOptions::for_current_process()?;
    let plan = requested_project_plan(matches, &project_path, &report.engines, &options)?;
    render_project_plan(format, &plan, &report.warnings)
}

fn run_project_apply(matches: &ArgMatches, format: OutputFormat) -> Result<()> {
    let (project_path, discovery) = selected_project_context(matches)?;
    let options = PlanBuildOptions::for_current_process()?;
    let plan = requested_project_plan(matches, &project_path, &discovery.engines, &options)?;
    if format == OutputFormat::Table {
        render_project_plan(format, &plan, &discovery.warnings)?;
    }
    confirm_write(
        matches,
        format,
        plan.change().is_none(),
        "Apply these project overrides",
    )?;
    confirm_active_processes(format, plan.change().is_none(), &plan.engine().path)?;
    let journal_path = default_journal_path()?;
    let report = apply_project_plan(&plan, &journal_path)?;
    render_project_apply(format, &plan, &report, &discovery.warnings)
}

fn run_project_status(matches: &ArgMatches, format: OutputFormat) -> Result<CommandOutcome> {
    let (project_path, discovery) = selected_project_context(matches)?;
    let journal_path = default_journal_path()?;
    let status = inspect_project_status(&project_path, &journal_path)?;
    render_project_status(format, &project_path, &status, &discovery.warnings)?;
    if status.drifted {
        Ok(CommandOutcome::Drift)
    } else {
        Ok(CommandOutcome::Success)
    }
}

fn run_project_history(matches: &ArgMatches, format: OutputFormat) -> Result<()> {
    let (project_path, discovery) = selected_project_context(matches)?;
    let journal_path = default_journal_path()?;
    let operations = project_history(&project_path, &journal_path)?;
    render_project_history(format, &project_path, &operations, &discovery.warnings)
}

fn run_project_restore(matches: &ArgMatches, format: OutputFormat) -> Result<()> {
    let (project_path, discovery) = selected_project_context(matches)?;
    let snapshot = matches
        .get_one::<String>("snapshot")
        .ok_or_else(|| Error::Internal {
            message: "the parsed project restore command has no snapshot selector".to_owned(),
        })?;
    let journal_path = default_journal_path()?;
    let options = PlanBuildOptions::for_current_process()?;
    let plan = build_project_restore_plan(
        &project_path,
        &discovery.engines,
        snapshot,
        &journal_path,
        &options,
    )?;
    if format == OutputFormat::Table {
        render_project_restore_plan(&plan, &discovery.warnings);
    }
    confirm_write(
        matches,
        format,
        plan.change().is_none(),
        "Restore this project snapshot",
    )?;
    confirm_active_processes(format, plan.change().is_none(), &plan.engine().path)?;
    let report = restore_project_plan(&plan, &journal_path)?;
    render_project_restore(format, &plan, &report, &discovery.warnings)
}

fn selected_project_context(matches: &ArgMatches) -> Result<(PathBuf, DiscoveryReport)> {
    let project = matches
        .get_one::<String>("project")
        .ok_or_else(|| Error::Internal {
            message: "the parsed project command has no project path".to_owned(),
        })?;
    let project_path = fs::canonicalize(project).map_err(|error| Error::NotFound {
        item: format!("project descriptor {project}: {error}"),
    })?;
    let options =
        matches
            .get_one::<String>("engine-path")
            .map_or_else(DiscoveryOptions::default, |path| DiscoveryOptions {
                explicit_paths: vec![path.into()],
                current_dir: None,
                launcher_manifest: None,
                include_registry: false,
            });
    Ok((project_path, discover_engines(&options)))
}

fn requested_project_plan(
    matches: &ArgMatches,
    project_path: &Path,
    engines: &[EngineInstallation],
    options: &PlanBuildOptions,
) -> Result<ProjectPlan> {
    let suppression = suppression_edit(matches);
    if let Some(selector) = matches.get_one::<String>("preset") {
        let preset_directory = default_preset_directory();
        let (preset_path, document) = load_preset(selector, preset_directory.as_deref())?;
        return build_project_preset_plan(
            project_path,
            engines,
            &preset_path,
            document.preset(),
            suppression,
            options,
        );
    }
    let mut plugins = Vec::new();
    append_project_edits(
        &mut plugins,
        matches.get_many::<String>("enable"),
        ProjectPluginEditAction::Enable,
    );
    append_project_edits(
        &mut plugins,
        matches.get_many::<String>("disable"),
        ProjectPluginEditAction::Disable,
    );
    append_project_edits(
        &mut plugins,
        matches.get_many::<String>("clear"),
        ProjectPluginEditAction::Clear,
    );
    if plugins.is_empty() && suppression == ProjectSuppressionEdit::Keep {
        return Err(Error::InvalidInput {
            message: "project plan has no requested change; pass --preset, --enable, --disable, --clear, or --suppression".to_owned(),
        });
    }
    build_project_edit_plan(
        project_path,
        engines,
        "Console project edit",
        ProjectDescriptorEdit {
            suppression,
            plugins,
        },
        options,
    )
}

fn suppression_edit(matches: &ArgMatches) -> ProjectSuppressionEdit {
    match matches.get_one::<String>("suppression").map(String::as_str) {
        Some("enabled") => ProjectSuppressionEdit::Set(true),
        Some("disabled") => ProjectSuppressionEdit::Set(false),
        Some("clear") => ProjectSuppressionEdit::Clear,
        _ => ProjectSuppressionEdit::Keep,
    }
}

fn append_project_edits(
    edits: &mut Vec<ProjectPluginEdit>,
    values: Option<clap::parser::ValuesRef<'_, String>>,
    action: ProjectPluginEditAction,
) {
    edits.extend(
        values
            .into_iter()
            .flatten()
            .map(|plugin| ProjectPluginEdit {
                plugin: plugin.clone(),
                action,
            }),
    );
}

fn confirm_write(
    matches: &ArgMatches,
    format: OutputFormat,
    no_changes: bool,
    prompt: &str,
) -> Result<()> {
    if no_changes {
        return Ok(());
    }
    let yes = matches.get_flag("yes");
    let interactive = format == OutputFormat::Table && io::stdin().is_terminal();
    match write_confirmation(interactive, yes)? {
        WriteConfirmation::Confirmed => Ok(()),
        WriteConfirmation::PromptRequired => prompt_confirmation(prompt),
    }
}

fn confirm_active_processes(
    format: OutputFormat,
    no_changes: bool,
    engine_path: &Path,
) -> Result<()> {
    if no_changes {
        return Ok(());
    }
    let processes = find_active_unreal_processes(engine_path)?;
    if processes.is_empty() {
        return Ok(());
    }
    let interactive = format == OutputFormat::Table && io::stdin().is_terminal();
    if !interactive {
        return Err(Error::Conflict {
            message: active_process_message(&processes),
        });
    }

    eprintln!("Unreal processes are using the selected engine:");
    for process in &processes {
        eprintln!("  {} (PID {})", process.executable, process.process_id);
    }
    prompt_confirmation("Continue while these processes are active")
}

fn active_process_message(processes: &[ActiveUnrealProcess]) -> String {
    let names = processes
        .iter()
        .map(|process| format!("{} (PID {})", process.executable, process.process_id))
        .collect::<Vec<_>>()
        .join(", ");
    format!("close active Unreal processes before a noninteractive write: {names}")
}

fn prompt_confirmation(prompt: &str) -> Result<()> {
    eprint!("{prompt}? [y/N]: ");
    io::stderr().flush().map_err(|error| Error::Internal {
        message: format!("confirmation prompt failed: {error}"),
    })?;
    let mut answer = String::new();
    io::stdin()
        .read_line(&mut answer)
        .map_err(|error| Error::Internal {
            message: format!("confirmation input failed: {error}"),
        })?;
    if answer.trim().eq_ignore_ascii_case("y") || answer.trim().eq_ignore_ascii_case("yes") {
        Ok(())
    } else {
        Err(Error::InvalidInput {
            message: "the reviewed write lacks confirmation".to_owned(),
        })
    }
}

fn selected_engine(matches: &ArgMatches) -> Result<(EngineInstallation, Vec<DiscoveryWarning>)> {
    let requested_path = matches.get_one::<String>("engine-path");
    let requested_version = matches.get_one::<String>("engine");
    let options = if let Some(path) = requested_path {
        DiscoveryOptions {
            explicit_paths: vec![path.into()],
            current_dir: None,
            launcher_manifest: None,
            include_registry: false,
        }
    } else {
        DiscoveryOptions::default()
    };
    let report = discover_engines(&options);
    let engine = if requested_path.is_some() {
        report
            .engines
            .first()
            .cloned()
            .ok_or_else(|| Error::NotFound {
                item: "engine installation supplied through --engine-path".to_owned(),
            })?
    } else if let Some(version) = requested_version {
        select_engine_by_version(&report.engines, version)?.clone()
    } else {
        report
            .engines
            .iter()
            .find(|engine| engine.source == DiscoverySource::WorkingDirectory)
            .cloned()
            .ok_or_else(|| Error::InvalidInput {
                message:
                    "an engine selector is required; pass --engine <VERSION> or --engine-path <PATH>"
                        .to_owned(),
            })?
    };
    Ok((engine, report.warnings))
}

fn render_plugins(
    format: OutputFormat,
    engine: &EngineInstallation,
    plugins: &[PluginDescriptor],
    warnings: &[PluginScanWarning],
    dependency_warnings: &[DependencyWarning],
    discovery_warnings: &[DiscoveryWarning],
) -> Result<()> {
    if format == OutputFormat::Json {
        let envelope = PluginEnvelope {
            schema: 1,
            ok: true,
            engine,
            plugins,
            warnings,
            dependency_warnings,
            discovery_warnings,
        };
        let json = serde_json::to_string(&envelope).map_err(|error| Error::Internal {
            message: format!("plugin report serialization failed: {error}"),
        })?;
        println!("{json}");
        return Ok(());
    }

    if plugins.is_empty() {
        println!(
            "No plugin descriptors parsed under {}.",
            engine.path.join("Engine").join("Plugins").display()
        );
    } else {
        println!(
            "{:<13} {:<9} {:>7} {:<30} {:<18} PATH",
            "DECLARED", "EFFECTIVE", "MODULES", "PLUGIN", "CATEGORY"
        );
        for plugin in plugins {
            println!(
                "{:<13} {:<9} {:>7} {:<30} {:<18} {}",
                plugin.declared_state.as_str(),
                if plugin.effective_enabled == Some(true) {
                    "enabled"
                } else {
                    "disabled"
                },
                plugin.module_count,
                plugin.name,
                plugin.category.as_deref().unwrap_or("-"),
                plugin.relative_path.display()
            );
            if !plugin.enabled_dependencies.is_empty() {
                println!(
                    "  depends on: {}",
                    plugin
                        .enabled_dependencies
                        .iter()
                        .map(|dependency| dependency.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
            if !plugin.reached_by.is_empty() {
                println!("  reached by: {}", plugin.reached_by.join(", "));
            }
            if plugin.effective_path.len() > 1 {
                println!("  cause: {}", plugin.effective_path.join(" -> "));
            }
        }
    }

    for warning in discovery_warnings {
        eprintln!("Warning: {}", warning.message);
    }
    for warning in warnings {
        eprintln!(
            "Warning [{}] {}: {}",
            warning.code.as_str(),
            warning.path.display(),
            warning.message
        );
    }
    for warning in dependency_warnings {
        eprintln!("Warning [{}] {}", warning.code.as_str(), warning.message);
    }
    Ok(())
}

fn render_project_plugins(
    format: OutputFormat,
    workspace: &ProjectWorkspace,
    discovery_warnings: &[DiscoveryWarning],
) -> Result<()> {
    if format == OutputFormat::Json {
        let envelope = ProjectWorkspaceEnvelope {
            schema: 1,
            ok: true,
            workspace,
            discovery_warnings,
        };
        let json = serde_json::to_string(&envelope).map_err(|error| Error::Internal {
            message: format!("project plugin report serialization failed: {error}"),
        })?;
        println!("{json}");
        return Ok(());
    }
    println!("Project: {}", workspace.project.path.display());
    println!(
        "Engine: {} ({})",
        workspace.engine.version.as_deref().unwrap_or("unknown"),
        workspace.engine.path.display()
    );
    println!(
        "Engine defaults suppressed: {}",
        workspace.project.suppression.as_str()
    );
    println!(
        "{:<9} {:<10} {:<9} {:<27} {:>7} PLUGIN",
        "ENGINE", "OVERRIDE", "PROJECT", "SOURCE", "MODULES"
    );
    for plugin in &workspace.plugins {
        println!(
            "{:<9} {:<10} {:<9} {:<27} {:>7} {}",
            enabled_label(plugin.plugin.effective_enabled == Some(true)),
            project_reference_label(plugin.project_reference),
            enabled_label(plugin.project_effective_enabled),
            plugin.project_origin.as_str(),
            plugin.plugin.module_count,
            plugin.plugin.name
        );
        if plugin.project_effective_path.len() > 1 {
            println!(
                "  project cause: {}",
                plugin.project_effective_path.join(" -> ")
            );
        }
    }
    render_project_warnings(
        discovery_warnings,
        &workspace.scan_warnings,
        &workspace.dependency_warnings,
        &workspace.project_warnings,
    );
    Ok(())
}

fn render_project_plan(
    format: OutputFormat,
    plan: &ProjectPlan,
    discovery_warnings: &[DiscoveryWarning],
) -> Result<()> {
    if format == OutputFormat::Json {
        let envelope = ProjectPlanEnvelope {
            schema: 1,
            ok: true,
            plan,
            discovery_warnings,
        };
        let json = serde_json::to_string(&envelope).map_err(|error| Error::Internal {
            message: format!("project plan serialization failed: {error}"),
        })?;
        println!("{json}");
        return Ok(());
    }
    println!("Project plan: {}", plan.operation_id());
    println!("Project: {}", plan.project_path().display());
    println!(
        "Engine: {} ({})",
        plan.engine().version.as_deref().unwrap_or("unknown"),
        plan.engine().path.display()
    );
    println!("Source: {}", plan.source().name);
    if let Some(path) = &plan.source().path {
        println!("Preset: {}", path.display());
    }
    println!("Backup: {}", plan.backup_directory().display());
    println!(
        "Engine default suppression: {}",
        suppression_edit_label(plan.edit().suppression)
    );
    let impact = plan.impact();
    println!(
        "Effective plugins: {} -> {}",
        impact.effective_plugins.before, impact.effective_plugins.after
    );
    println!(
        "Declared modules: {} -> {}",
        impact.declared_modules.before, impact.declared_modules.after
    );
    println!(
        "Explicit project references: {} -> {}",
        impact.explicit_references.before, impact.explicit_references.after
    );
    if let Some(change) = plan.change() {
        println!("Project file change: {}", change.relative_path.display());
        println!("  Source SHA-256: {}", change.sha256_before);
        println!("  Planned SHA-256: {}", change.sha256_after);
        println!(
            "  Bytes: source {}..{} -> planned {}..{} ({} bytes total)",
            change.byte_change.source.start,
            change.byte_change.source.end,
            change.byte_change.planned.start,
            change.byte_change.planned.end,
            change.planned_byte_count
        );
        for plugin in plan.plugins().iter().filter(|plugin| {
            plugin.reference_before != plugin.reference_after
                || plugin.effective_before != plugin.effective_after
        }) {
            println!(
                "  {}: override {} -> {}, project {} -> {}, engine {}",
                plugin.plugin,
                project_reference_label(plugin.reference_before),
                project_reference_label(plugin.reference_after),
                enabled_label(plugin.effective_before),
                enabled_label(plugin.effective_after),
                enabled_label(plugin.engine_effective_enabled)
            );
        }
    } else {
        println!("Project file change: none");
    }
    if !plan.pattern_expansions().is_empty() {
        println!("Pattern expansion:");
        for expansion in plan.pattern_expansions() {
            let matches = if expansion.matches.is_empty() {
                "none".to_owned()
            } else {
                expansion.matches.join(", ")
            };
            println!("  {}: {matches}", expansion.pattern);
        }
    }
    for unmatched in plan.unmatched_rules() {
        eprintln!(
            "Warning: preset rule did not match the associated engine: {} {}. Update the preset or select another project.",
            unmatched.action.as_str(),
            unmatched.rule
        );
    }
    render_project_warnings(
        discovery_warnings,
        plan.scan_warnings(),
        plan.dependency_warnings(),
        plan.project_warnings(),
    );
    Ok(())
}

fn render_project_apply(
    format: OutputFormat,
    plan: &ProjectPlan,
    report: &OperationReport,
    discovery_warnings: &[DiscoveryWarning],
) -> Result<()> {
    if format == OutputFormat::Json {
        let envelope = ProjectApplyEnvelope {
            schema: 1,
            ok: true,
            plan,
            result: report,
            discovery_warnings,
        };
        let json = serde_json::to_string(&envelope).map_err(|error| Error::Internal {
            message: format!("project apply result serialization failed: {error}"),
        })?;
        println!("{json}");
        return Ok(());
    }
    if report.recorded {
        println!(
            "Updated the project descriptor. Backup: {}",
            report
                .backup_directory
                .as_deref()
                .map_or_else(|| "-".to_owned(), |path| path.display().to_string())
        );
        if let Some(path) = &report.journal_path {
            println!("Journal: {}", path.display());
        }
    } else {
        println!("The project descriptor already matches the reviewed plan.");
    }
    Ok(())
}

fn render_project_status(
    format: OutputFormat,
    project_path: &Path,
    status: &ProjectStatus,
    discovery_warnings: &[DiscoveryWarning],
) -> Result<()> {
    if format == OutputFormat::Json {
        let envelope = ProjectStatusEnvelope {
            schema: 1,
            ok: true,
            project: project_path,
            status,
            discovery_warnings,
        };
        let json = serde_json::to_string(&envelope).map_err(|error| Error::Internal {
            message: format!("project status serialization failed: {error}"),
        })?;
        println!("{json}");
        return Ok(());
    }
    println!("Project: {}", project_path.display());
    if let Some(operation) = &status.operation {
        println!("Operation: {}", operation.id);
        println!("Source: {}", operation.preset);
        println!("Completed: {}", operation.completed);
        println!("Backup: {}", operation.backup_directory.display());
    }
    if status.recorded {
        for file in &status.files {
            println!(
                "{:<11} {}",
                file.state.as_str(),
                file.relative_path.display()
            );
            if let Some(message) = &file.message {
                eprintln!("Warning: {message}");
            }
        }
        if status.drifted {
            eprintln!(
                "Project drift detected. Run project plan to review the current descriptor state."
            );
        } else {
            println!("Status: the recorded project bytes match.");
        }
    } else {
        println!("Status: this project has no completed operation.");
    }
    render_discovery_warnings(discovery_warnings);
    Ok(())
}

fn render_project_history(
    format: OutputFormat,
    project_path: &Path,
    operations: &[JournalOperation],
    discovery_warnings: &[DiscoveryWarning],
) -> Result<()> {
    if format == OutputFormat::Json {
        let envelope = ProjectHistoryEnvelope {
            schema: 1,
            ok: true,
            project: project_path,
            operations,
            discovery_warnings,
        };
        let json = serde_json::to_string(&envelope).map_err(|error| Error::Internal {
            message: format!("project history serialization failed: {error}"),
        })?;
        println!("{json}");
        return Ok(());
    }
    println!("Project: {}", project_path.display());
    if operations.is_empty() {
        println!("History: this project has no completed operations.");
    } else {
        println!(
            "{:<9} {:<22} {:<24} SNAPSHOT",
            "KIND", "COMPLETED", "SOURCE"
        );
        for operation in operations {
            println!(
                "{:<9} {:<22} {:<24} {}",
                operation.kind.as_str(),
                operation.completed,
                operation.preset,
                operation.id
            );
        }
    }
    render_discovery_warnings(discovery_warnings);
    Ok(())
}

fn render_project_restore_plan(plan: &ProjectRestorePlan, discovery_warnings: &[DiscoveryWarning]) {
    println!("Project restore plan: {}", plan.operation_id());
    println!("Snapshot: {}", plan.source_snapshot());
    println!("Project: {}", plan.project_path().display());
    println!(
        "Engine: {} ({})",
        plan.engine().version.as_deref().unwrap_or("unknown"),
        plan.engine().path.display()
    );
    println!("Source: {}", plan.preset());
    println!("Backup: {}", plan.backup_directory().display());
    if let Some(change) = plan.change() {
        println!("Change: {}", change.relative_path.display());
        println!("  Source SHA-256: {}", change.sha256_before);
        println!("  Restored SHA-256: {}", change.sha256_after);
        println!("  Restored bytes: {}", change.planned_byte_count);
    } else {
        println!("Change: none");
    }
    render_discovery_warnings(discovery_warnings);
}

fn render_project_restore(
    format: OutputFormat,
    plan: &ProjectRestorePlan,
    report: &OperationReport,
    discovery_warnings: &[DiscoveryWarning],
) -> Result<()> {
    if format == OutputFormat::Json {
        let envelope = ProjectRestoreEnvelope {
            schema: 1,
            ok: true,
            plan,
            result: report,
            discovery_warnings,
        };
        let json = serde_json::to_string(&envelope).map_err(|error| Error::Internal {
            message: format!("project restore result serialization failed: {error}"),
        })?;
        println!("{json}");
        return Ok(());
    }
    if report.recorded {
        println!(
            "Restored the project descriptor. Backup: {}",
            report
                .backup_directory
                .as_deref()
                .map_or_else(|| "-".to_owned(), |path| path.display().to_string())
        );
        if let Some(path) = &report.journal_path {
            println!("Journal: {}", path.display());
        }
    } else {
        println!("The selected snapshot already matches the project.");
    }
    Ok(())
}

fn render_templates(
    format: OutputFormat,
    catalog: &TemplateCatalog,
    discovery_warnings: &[DiscoveryWarning],
) -> Result<()> {
    if format == OutputFormat::Json {
        let envelope = TemplateCatalogEnvelope {
            schema: 1,
            ok: true,
            catalog,
            discovery_warnings,
        };
        let json = serde_json::to_string(&envelope).map_err(|error| Error::Internal {
            message: format!("template catalog serialization failed: {error}"),
        })?;
        println!("{json}");
        return Ok(());
    }
    println!(
        "Engine: {} ({})",
        catalog.engine.version.as_deref().unwrap_or("unknown"),
        catalog.engine.path.display()
    );
    println!(
        "{:<12} {:>7} {:<28} PATH",
        "SUPPRESSION", "PLUGINS", "TEMPLATE"
    );
    for template in &catalog.templates {
        println!(
            "{:<12} {:>7} {:<28} {}",
            template.suppression.as_str(),
            template.plugin_reference_count,
            template.name,
            template.relative_path.display()
        );
    }
    if catalog.templates.is_empty() {
        println!("Unclean found no valid project templates.");
    }
    render_template_warnings(&catalog.warnings, discovery_warnings);
    Ok(())
}

fn render_template_plan(
    format: OutputFormat,
    plan: &TemplatePlan,
    discovery_warnings: &[DiscoveryWarning],
) -> Result<()> {
    if format == OutputFormat::Json {
        let envelope = TemplatePlanEnvelope {
            schema: 1,
            ok: true,
            plan,
            discovery_warnings,
        };
        let json = serde_json::to_string(&envelope).map_err(|error| Error::Internal {
            message: format!("template plan serialization failed: {error}"),
        })?;
        println!("{json}");
        return Ok(());
    }
    println!("Template plan: {}", plan.operation_id());
    println!(
        "Engine: {} ({})",
        plan.engine().version.as_deref().unwrap_or("unknown"),
        plan.engine().path.display()
    );
    println!(
        "Suppression: {}",
        suppression_edit_label(plan.suppression())
    );
    println!("Backup: {}", plan.backup_directory().display());
    println!(
        "Selected templates: {} ({} file changes)",
        plan.templates().len(),
        plan.changes().len()
    );
    for template in plan.templates() {
        println!(
            "  {}: {} -> {}{}",
            template.relative_path.display(),
            template.suppression_before.as_str(),
            template.suppression_after.as_str(),
            if template.changed { "" } else { " (no change)" }
        );
    }
    for change in plan.changes() {
        println!(
            "  Source SHA-256 [{}]: {}",
            change.template, change.sha256_before
        );
        println!(
            "  Planned SHA-256 [{}]: {}",
            change.template, change.sha256_after
        );
    }
    render_template_warnings(plan.warnings(), discovery_warnings);
    Ok(())
}

fn render_template_apply(
    format: OutputFormat,
    plan: &TemplatePlan,
    report: &OperationReport,
    discovery_warnings: &[DiscoveryWarning],
) -> Result<()> {
    if format == OutputFormat::Json {
        let envelope = TemplateApplyEnvelope {
            schema: 1,
            ok: true,
            plan,
            result: report,
            discovery_warnings,
        };
        let json = serde_json::to_string(&envelope).map_err(|error| Error::Internal {
            message: format!("template apply result serialization failed: {error}"),
        })?;
        println!("{json}");
        return Ok(());
    }
    if report.recorded {
        println!(
            "Updated {} template descriptor(s). Backup: {}",
            report.files_written,
            report
                .backup_directory
                .as_deref()
                .map_or_else(|| "-".to_owned(), |path| path.display().to_string())
        );
    } else {
        println!("The selected templates already match the reviewed plan.");
    }
    Ok(())
}

fn render_template_status(
    format: OutputFormat,
    engine: &EngineInstallation,
    status: &EngineStatus,
    discovery_warnings: &[DiscoveryWarning],
) -> Result<()> {
    if format == OutputFormat::Json {
        let envelope = StatusEnvelope {
            schema: 1,
            ok: true,
            engine,
            status,
            discovery_warnings,
        };
        println!(
            "{}",
            serde_json::to_string(&envelope).map_err(|error| Error::Internal {
                message: format!("template status serialization failed: {error}"),
            })?
        );
        return Ok(());
    }
    println!("Engine templates: {}", engine.path.display());
    if let Some(operation) = &status.operation {
        println!("Operation: {}", operation.id);
        println!("Completed: {}", operation.completed);
        println!("Backup: {}", operation.backup_directory.display());
    }
    for file in &status.files {
        println!(
            "{:<11} {}",
            file.state.as_str(),
            file.relative_path.display()
        );
        if let Some(message) = &file.message {
            eprintln!("Warning: {message}");
        }
    }
    if !status.recorded {
        println!("Status: this engine has no completed template operation.");
    } else if status.drifted {
        eprintln!("Template drift detected. Run template plan to review the current files.");
    } else {
        println!("Status: all recorded template files match.");
    }
    render_discovery_warnings(discovery_warnings);
    Ok(())
}

fn render_template_history(
    format: OutputFormat,
    engine: &EngineInstallation,
    operations: &[JournalOperation],
    discovery_warnings: &[DiscoveryWarning],
) -> Result<()> {
    if format == OutputFormat::Json {
        let envelope = HistoryEnvelope {
            schema: 1,
            ok: true,
            engine,
            operations,
            discovery_warnings,
        };
        println!(
            "{}",
            serde_json::to_string(&envelope).map_err(|error| Error::Internal {
                message: format!("template history serialization failed: {error}"),
            })?
        );
        return Ok(());
    }
    println!("Engine templates: {}", engine.path.display());
    if operations.is_empty() {
        println!("History: this engine has no completed template operations.");
    } else {
        println!("{:<9} {:<22} {:>5} SNAPSHOT", "KIND", "COMPLETED", "FILES");
        for operation in operations {
            println!(
                "{:<9} {:<22} {:>5} {}",
                operation.kind.as_str(),
                operation.completed,
                operation.files.len(),
                operation.id
            );
        }
    }
    render_discovery_warnings(discovery_warnings);
    Ok(())
}

fn render_template_restore_plan(
    plan: &TemplateRestorePlan,
    discovery_warnings: &[DiscoveryWarning],
) {
    println!("Template restore plan: {}", plan.operation_id());
    println!("Snapshot: {}", plan.source_snapshot());
    println!("Engine: {}", plan.engine().path.display());
    println!("Backup: {}", plan.backup_directory().display());
    if plan.changes().is_empty() {
        println!("Changes: none");
    } else {
        println!("Changes: {}", plan.changes().len());
        for change in plan.changes() {
            println!(
                "  {}: {} -> {}",
                change.relative_path.display(),
                change.sha256_before.as_deref().unwrap_or("missing"),
                change.sha256_after
            );
        }
    }
    render_discovery_warnings(discovery_warnings);
}

fn render_template_restore(
    format: OutputFormat,
    plan: &TemplateRestorePlan,
    report: &OperationReport,
    discovery_warnings: &[DiscoveryWarning],
) -> Result<()> {
    if format == OutputFormat::Json {
        let envelope = TemplateRestoreEnvelope {
            schema: 1,
            ok: true,
            plan,
            result: report,
            discovery_warnings,
        };
        println!(
            "{}",
            serde_json::to_string(&envelope).map_err(|error| Error::Internal {
                message: format!("template restore result serialization failed: {error}"),
            })?
        );
        return Ok(());
    }
    if report.recorded {
        println!(
            "Restored {} template descriptor(s). Backup: {}",
            report.files_written,
            report
                .backup_directory
                .as_deref()
                .map_or_else(|| "-".to_owned(), |path| path.display().to_string())
        );
    } else {
        println!("The selected snapshot already matches the engine templates.");
    }
    Ok(())
}

fn render_template_warnings(
    warnings: &[unclean_core::templates::TemplateScanWarning],
    discovery_warnings: &[DiscoveryWarning],
) {
    render_discovery_warnings(discovery_warnings);
    for warning in warnings {
        eprintln!(
            "Warning [{}] {}: {}",
            warning.code.as_str(),
            warning.path.display(),
            warning.message
        );
    }
}

fn render_project_warnings(
    discovery_warnings: &[DiscoveryWarning],
    scan_warnings: &[PluginScanWarning],
    dependency_warnings: &[DependencyWarning],
    project_warnings: &[unclean_core::project_state::ProjectStateWarning],
) {
    render_discovery_warnings(discovery_warnings);
    for warning in scan_warnings {
        eprintln!(
            "Warning [{}] {}: {}",
            warning.code.as_str(),
            warning.path.display(),
            warning.message
        );
    }
    for warning in dependency_warnings {
        eprintln!("Warning [{}] {}", warning.code.as_str(), warning.message);
    }
    for warning in project_warnings {
        let label = if warning.blocking {
            "Conflict"
        } else {
            "Warning"
        };
        eprintln!("{label} [{}] {}", warning.code.as_str(), warning.message);
    }
}

fn render_discovery_warnings(warnings: &[DiscoveryWarning]) {
    for warning in warnings {
        eprintln!("Warning: {}", warning.message);
    }
}

const fn enabled_label(enabled: bool) -> &'static str {
    if enabled { "enabled" } else { "disabled" }
}

const fn project_reference_label(reference: Option<bool>) -> &'static str {
    match reference {
        Some(true) => "enabled",
        Some(false) => "disabled",
        None => "inherit",
    }
}

const fn suppression_edit_label(edit: ProjectSuppressionEdit) -> &'static str {
    match edit {
        ProjectSuppressionEdit::Keep => "keep current value",
        ProjectSuppressionEdit::Set(true) => "enabled",
        ProjectSuppressionEdit::Set(false) => "disabled",
        ProjectSuppressionEdit::Clear => "clear field",
    }
}

fn render_presets(
    format: OutputFormat,
    directory: Option<&std::path::Path>,
    presets: &[PresetFile],
) -> Result<()> {
    if format == OutputFormat::Json {
        let envelope = PresetListEnvelope {
            schema: 1,
            ok: true,
            directory,
            presets,
        };
        let json = serde_json::to_string(&envelope).map_err(|error| Error::Internal {
            message: format!("preset list serialization failed: {error}"),
        })?;
        println!("{json}");
        return Ok(());
    }

    if presets.is_empty() {
        if let Some(directory) = directory {
            println!("No preset files found in {}.", directory.display());
        } else {
            println!("Unclean found no available preset directory. Pass an explicit preset path.");
        }
        return Ok(());
    }
    println!("{:<28} PATH", "PRESET");
    for preset in presets {
        println!("{:<28} {}", preset.name, preset.path.display());
    }
    Ok(())
}

fn render_preset(
    format: OutputFormat,
    command: &str,
    path: &std::path::Path,
    preset: &Preset,
) -> Result<()> {
    if format == OutputFormat::Json {
        let envelope = PresetEnvelope {
            schema: 1,
            ok: true,
            path,
            preset,
        };
        let json = serde_json::to_string(&envelope).map_err(|error| Error::Internal {
            message: format!("preset serialization failed: {error}"),
        })?;
        println!("{json}");
        return Ok(());
    }

    if command == "validate" {
        println!(
            "Preset is valid: {} uses schema {} at {}.",
            preset.name,
            preset.schema,
            path.display()
        );
        return Ok(());
    }
    println!("Preset: {}", preset.name);
    println!("Schema: {}", preset.schema);
    println!("Path: {}", path.display());
    println!(
        "Description: {}",
        preset.description.as_deref().unwrap_or("-")
    );
    render_rule_list("Enable", &preset.enable);
    render_rule_list("Disable", &preset.disable);
    render_rule_list("Clear", &preset.clear);
    render_rule_list("Disable matching", &preset.disable_matching);
    Ok(())
}

fn render_rule_list(label: &str, rules: &[String]) {
    if rules.is_empty() {
        println!("{label}: -");
    } else {
        println!("{label}: {}", rules.join(", "));
    }
}

fn render_plan(
    format: OutputFormat,
    plan: &EnginePlan,
    discovery_warnings: &[DiscoveryWarning],
) -> Result<()> {
    if format == OutputFormat::Json {
        let envelope = PlanEnvelope {
            schema: 1,
            ok: true,
            plan,
            discovery_warnings,
        };
        let json = serde_json::to_string(&envelope).map_err(|error| Error::Internal {
            message: format!("plan serialization failed: {error}"),
        })?;
        println!("{json}");
        return Ok(());
    }

    println!("Plan: {}", plan.operation_id());
    println!(
        "Engine: {} ({})",
        plan.engine().version.as_deref().unwrap_or("unknown"),
        plan.engine().path.display()
    );
    println!(
        "Preset: {} ({})",
        plan.preset().name,
        plan.preset().path.display()
    );
    println!("Backup: {}", plan.backup_directory().display());
    let impact = plan.impact();
    println!(
        "Default roots: {} -> {}",
        impact.default_roots.before, impact.default_roots.after
    );
    println!(
        "Effective plugins: {} -> {}",
        impact.effective_plugins.before, impact.effective_plugins.after
    );
    println!(
        "Declared modules: {} -> {}",
        impact.declared_modules.before, impact.declared_modules.after
    );

    if plan.changes().is_empty() {
        println!("Changes: none");
    } else {
        println!("Changes: {}", plan.changes().len());
        for edit in plan.changes() {
            println!(
                "  {}: {} {} -> {}",
                edit.plugin,
                edit.field.as_str(),
                edit.value_before.as_str(),
                edit.value_after.as_str()
            );
            println!("    Target: {}", edit.relative_path.display());
            println!("    Source SHA-256: {}", edit.sha256_before);
            println!("    Planned SHA-256: {}", edit.sha256_after);
            println!(
                "    Bytes: source {}..{} -> planned {}..{} ({} bytes total)",
                edit.byte_change.source.start,
                edit.byte_change.source.end,
                edit.byte_change.planned.start,
                edit.byte_change.planned.end,
                edit.planned_byte_count
            );
            println!("    Selected by: {}", render_matches(&edit.matched_by));
        }
    }

    if !plan.no_ops().is_empty() {
        println!("No change needed: {}", plan.no_ops().len());
        for no_op in plan.no_ops() {
            println!(
                "  {} {}: {}",
                no_op.action.as_str(),
                no_op.plugin,
                no_op.reason.message()
            );
        }
    }
    if !plan.pattern_expansions().is_empty() {
        println!("Pattern expansion:");
        for expansion in plan.pattern_expansions() {
            let matches = if expansion.matches.is_empty() {
                "none".to_owned()
            } else {
                expansion.matches.join(", ")
            };
            println!("  {}: {matches}", expansion.pattern);
        }
    }

    render_plan_warnings(plan, discovery_warnings);
    Ok(())
}

fn render_apply(
    format: OutputFormat,
    plan: &EnginePlan,
    report: &OperationReport,
    discovery_warnings: &[DiscoveryWarning],
) -> Result<()> {
    if format == OutputFormat::Json {
        let envelope = ApplyEnvelope {
            schema: 1,
            ok: true,
            plan,
            result: report,
            discovery_warnings,
        };
        let json = serde_json::to_string(&envelope).map_err(|error| Error::Internal {
            message: format!("apply result serialization failed: {error}"),
        })?;
        println!("{json}");
        return Ok(());
    }
    if report.recorded {
        println!(
            "Applied {} descriptor file(s). Backup: {}",
            report.files_written,
            report
                .backup_directory
                .as_deref()
                .map_or_else(|| "-".to_owned(), |path| path.display().to_string())
        );
        if let Some(path) = &report.journal_path {
            println!("Journal: {}", path.display());
        }
    } else {
        println!("No descriptor files required changes.");
    }
    Ok(())
}

fn render_history(
    format: OutputFormat,
    engine: &EngineInstallation,
    operations: &[JournalOperation],
    discovery_warnings: &[DiscoveryWarning],
) -> Result<()> {
    if format == OutputFormat::Json {
        let envelope = HistoryEnvelope {
            schema: 1,
            ok: true,
            engine,
            operations,
            discovery_warnings,
        };
        let json = serde_json::to_string(&envelope).map_err(|error| Error::Internal {
            message: format!("history serialization failed: {error}"),
        })?;
        println!("{json}");
        return Ok(());
    }
    println!(
        "Engine: {} ({})",
        engine.version.as_deref().unwrap_or("unknown"),
        engine.path.display()
    );
    if operations.is_empty() {
        println!("History: this engine has no completed operations.");
    } else {
        println!(
            "{:<9} {:<22} {:<24} {:>5} SNAPSHOT",
            "KIND", "COMPLETED", "PRESET", "FILES"
        );
        for operation in operations {
            println!(
                "{:<9} {:<22} {:<24} {:>5} {}",
                operation.kind.as_str(),
                operation.completed,
                operation.preset,
                operation.files.len(),
                operation.id
            );
        }
    }
    for warning in discovery_warnings {
        eprintln!("Warning: {}", warning.message);
    }
    Ok(())
}

fn render_restore_plan(plan: &RestorePlan, discovery_warnings: &[DiscoveryWarning]) {
    println!("Restore plan: {}", plan.operation_id());
    println!("Snapshot: {}", plan.source_snapshot());
    println!(
        "Engine: {} ({})",
        plan.engine().version.as_deref().unwrap_or("unknown"),
        plan.engine().path.display()
    );
    println!("Preset: {}", plan.preset());
    println!("Backup: {}", plan.backup_directory().display());
    if plan.changes().is_empty() {
        println!("Changes: none");
    } else {
        println!("Changes: {}", plan.changes().len());
        for edit in plan.changes() {
            println!(
                "  {}: {} -> {}",
                edit.relative_path.display(),
                edit.value_before.map_or("missing", |state| state.as_str()),
                edit.value_after.as_str()
            );
            println!(
                "    Source SHA-256: {}",
                edit.sha256_before.as_deref().unwrap_or("missing")
            );
            println!("    Restored SHA-256: {}", edit.sha256_after);
            println!("    Restored bytes: {}", edit.planned_byte_count);
        }
    }
    for warning in discovery_warnings {
        eprintln!("Warning: {}", warning.message);
    }
}

fn render_restore(
    format: OutputFormat,
    plan: &RestorePlan,
    report: &OperationReport,
    discovery_warnings: &[DiscoveryWarning],
) -> Result<()> {
    if format == OutputFormat::Json {
        let envelope = RestoreEnvelope {
            schema: 1,
            ok: true,
            plan,
            result: report,
            discovery_warnings,
        };
        let json = serde_json::to_string(&envelope).map_err(|error| Error::Internal {
            message: format!("restore result serialization failed: {error}"),
        })?;
        println!("{json}");
        return Ok(());
    }
    if report.recorded {
        println!(
            "Restored {} descriptor file(s). Backup: {}",
            report.files_written,
            report
                .backup_directory
                .as_deref()
                .map_or_else(|| "-".to_owned(), |path| path.display().to_string())
        );
        if let Some(path) = &report.journal_path {
            println!("Journal: {}", path.display());
        }
    } else {
        println!("The selected snapshot already matches the engine.");
    }
    Ok(())
}

fn render_matches(matches: &[unclean_core::presets::PresetRuleMatch]) -> String {
    matches
        .iter()
        .map(|matched| {
            let source = match matched.source {
                PresetRuleSource::Exact => "exact",
                PresetRuleSource::Pattern => "pattern",
            };
            format!("{source} {}", matched.rule)
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn render_plan_warnings(plan: &EnginePlan, discovery_warnings: &[DiscoveryWarning]) {
    for warning in discovery_warnings {
        eprintln!("Warning: {}", warning.message);
    }
    for warning in plan.scan_warnings() {
        eprintln!(
            "Warning [{}] {}: {}",
            warning.code.as_str(),
            warning.path.display(),
            warning.message
        );
    }
    for warning in plan.graph_warnings() {
        eprintln!("Warning [{}] {}", warning.code.as_str(), warning.message);
    }
    for warning in plan.dependency_warnings() {
        eprintln!("Warning: {}", warning.message);
    }
    for unmatched in plan.unmatched_rules() {
        eprintln!(
            "Warning: preset rule did not match this engine: {} {}. Update the preset or select another engine.",
            unmatched.action.as_str(),
            unmatched.rule
        );
    }
}

fn render_status(
    format: OutputFormat,
    engine: &EngineInstallation,
    status: &EngineStatus,
    discovery_warnings: &[DiscoveryWarning],
) -> Result<()> {
    if format == OutputFormat::Json {
        let envelope = StatusEnvelope {
            schema: 1,
            ok: true,
            engine,
            status,
            discovery_warnings,
        };
        let json = serde_json::to_string(&envelope).map_err(|error| Error::Internal {
            message: format!("status serialization failed: {error}"),
        })?;
        println!("{json}");
        return Ok(());
    }

    println!(
        "Engine: {} ({})",
        engine.version.as_deref().unwrap_or("unknown"),
        engine.path.display()
    );
    if status.recorded {
        if let Some(operation) = &status.operation {
            println!("Operation: {}", operation.id);
            println!("Preset: {}", operation.preset);
            println!("Completed: {}", operation.completed);
            println!("Backup: {}", operation.backup_directory.display());
        }
        println!("{:<11} TARGET", "STATE");
        for file in &status.files {
            println!(
                "{:<11} {}",
                file.state.as_str(),
                file.relative_path.display()
            );
            if let Some(message) = &file.message {
                eprintln!("Warning: {message}");
            }
        }
        if status.drifted {
            eprintln!(
                "Drift detected in recorded targets. Run plan to review the current descriptor state."
            );
        } else {
            println!("Status: all recorded targets match.");
        }
    } else {
        println!("Status: this engine has no completed operation.");
    }
    for warning in discovery_warnings {
        eprintln!("Warning: {}", warning.message);
    }
    Ok(())
}

fn render_engines(format: OutputFormat, report: &DiscoveryReport) -> Result<()> {
    if format == OutputFormat::Json {
        let envelope = EngineEnvelope {
            schema: 1,
            ok: true,
            engines: &report.engines,
            warnings: &report.warnings,
        };
        let json = serde_json::to_string(&envelope).map_err(|error| Error::Internal {
            message: format!("engine report serialization failed: {error}"),
        })?;
        println!("{json}");
        return Ok(());
    }

    if report.engines.is_empty() {
        println!("No engine installations found. Pass --engine-path with an installation folder.");
    } else {
        println!(
            "{:<12} {:<11} {:>11} {:<17} PATH",
            "VERSION", "HEALTH", "DESCRIPTORS", "SOURCE"
        );
        for engine in &report.engines {
            println!(
                "{:<12} {:<11} {:>11} {:<17} {}",
                engine.version.as_deref().unwrap_or("unknown"),
                engine.health.as_str(),
                engine.descriptor_count,
                engine.source.as_str(),
                engine.path.display()
            );
            for issue in &engine.issues {
                println!("  {}: {}", issue.code.as_str(), issue.message);
            }
        }
    }

    for warning in &report.warnings {
        eprintln!("Warning: {}", warning.message);
    }
    Ok(())
}

fn render_error(format: OutputFormat, error: &Error) {
    let message = error.to_string();
    if format == OutputFormat::Json {
        let envelope = FailureEnvelope {
            schema: 1,
            ok: false,
            error: FailureBody {
                code: error.code().as_str(),
                message: &message,
                exit_code: error.code().exit_code(),
            },
        };
        match serde_json::to_string(&envelope) {
            Ok(json) => eprintln!("{json}"),
            Err(serialization_error) => {
                eprintln!(
                    "Error output failed: {serialization_error}. Report this result with the {PRODUCT_NAME} version."
                );
            }
        }
    } else {
        eprintln!("{message}");
    }
}

#[cfg(test)]
mod tests {
    use super::command;

    #[test]
    fn command_contract_is_internally_consistent() {
        command().debug_assert();
    }
}
