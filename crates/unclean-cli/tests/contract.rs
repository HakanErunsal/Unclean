#![doc = "Verifies the executable command contract, output schema, and stable process status."]

use std::fs;
use std::process::{Command, Output};

use tempfile::tempdir;
use unclean_core::discovery::{DiscoveryOptions, discover_engines};
use unclean_core::journal::{
    JournalFile, JournalOperation, JournalOperationKind, JournalState, OperationTargetKind,
};
use unclean_core::plans::PlanBuildOptions;
use unclean_core::presets::PresetDocument;
use unclean_gui::workflow::build_engine_review_with_options;

fn run(arguments: &[&str]) -> Result<Output, Box<dyn std::error::Error>> {
    Ok(Command::new(env!("CARGO_BIN_EXE_unclean"))
        .args(arguments)
        .output()?)
}

fn run_in(
    arguments: &[&str],
    current_dir: &std::path::Path,
) -> Result<Output, Box<dyn std::error::Error>> {
    Ok(Command::new(env!("CARGO_BIN_EXE_unclean"))
        .args(arguments)
        .current_dir(current_dir)
        .output()?)
}

fn run_with_appdata(
    arguments: &[&str],
    appdata: &std::path::Path,
) -> Result<Output, Box<dyn std::error::Error>> {
    Ok(Command::new(env!("CARGO_BIN_EXE_unclean"))
        .args(arguments)
        .env("APPDATA", appdata)
        .output()?)
}

fn gui_plan_for_cli_report(
    engine: &std::path::Path,
    preset_path: &std::path::Path,
    backup_root: &std::path::Path,
    cli_report: &serde_json::Value,
) -> Result<unclean_core::plans::EnginePlan, Box<dyn std::error::Error>> {
    let discovery = discover_engines(&DiscoveryOptions {
        explicit_paths: vec![engine.to_path_buf()],
        current_dir: None,
        launcher_manifest: None,
        include_registry: false,
    });
    let discovered_engine = discovery
        .engines
        .first()
        .ok_or("engine was not discovered")?;
    let preset_document = PresetDocument::parse(&fs::read_to_string(preset_path)?)?;
    let options = PlanBuildOptions::new(backup_root.to_path_buf(), "gui-contract-plan".to_owned())?;
    let plan = build_engine_review_with_options(
        discovered_engine,
        preset_path,
        &preset_document,
        &options,
    )?;
    let gui_report = serde_json::to_value(&plan)?;
    for field in [
        "impact",
        "changes",
        "no_ops",
        "dependency_warnings",
        "graph_warnings",
        "scan_warnings",
        "pattern_expansions",
        "unmatched_rules",
    ] {
        assert_eq!(
            gui_report[field], cli_report["plan"][field],
            "{field} differs"
        );
    }
    Ok(plan)
}

fn assert_cli_apply_matches_gui_plan(
    engine: &std::path::Path,
    preset_path: &std::path::Path,
    gamma: &std::path::Path,
    appdata: &std::path::Path,
    gui_plan: &unclean_core::plans::EnginePlan,
) -> Result<(), Box<dyn std::error::Error>> {
    let apply = run_with_appdata(
        &[
            "apply",
            "--engine-path",
            &engine.display().to_string(),
            "--preset",
            &preset_path.display().to_string(),
            "--yes",
            "--format",
            "json",
        ],
        appdata,
    )?;
    let report: serde_json::Value = serde_json::from_slice(&apply.stdout)?;
    assert!(apply.status.success());
    assert_eq!(report["result"]["files_written"], gui_plan.changes().len());
    let gui_gamma = gui_plan
        .changes()
        .iter()
        .find(|change| change.plugin == "Gamma")
        .ok_or("GUI plan has no Gamma change")?;
    assert_eq!(fs::read(gamma)?, gui_gamma.planned_bytes());
    Ok(())
}

#[test]
fn help_lists_the_frontend_contract() -> Result<(), Box<dyn std::error::Error>> {
    let output = run(&["--help"])?;
    let stdout = String::from_utf8(output.stdout)?;

    assert!(output.status.success());
    for command in [
        "engines",
        "plugins",
        "presets",
        "preset",
        "plan",
        "apply",
        "status",
        "history",
        "restore",
        "templates",
        "template",
        "project",
        "gui",
    ] {
        assert!(stdout.contains(command));
    }
    assert!(!stdout.contains("__elevated-worker"));

    Ok(())
}

#[test]
fn template_commands_select_apply_track_and_restore_descriptors()
-> Result<(), Box<dyn std::error::Error>> {
    let temp = tempdir()?;
    let engine = temp.path().join("UE_Templates");
    let build = engine.join("Engine/Build");
    let plugins = engine.join("Engine/Plugins");
    let template = engine.join("Templates/TP_Blank/TP_Blank.uproject");
    fs::create_dir_all(&build)?;
    fs::create_dir_all(&plugins)?;
    fs::create_dir_all(
        template
            .parent()
            .ok_or("template fixture has no parent directory")?,
    )?;
    fs::write(
        build.join("Build.version"),
        r#"{"MajorVersion":5,"MinorVersion":9,"PatchVersion":0}"#,
    )?;
    let original = br#"{"FileVersion":3,"Plugins":[{"Name":"KeepMe","Enabled":true}]}"#;
    fs::write(&template, original)?;
    let engine_value = engine.display().to_string();

    let list = run_with_appdata(
        &[
            "templates",
            "--engine-path",
            &engine_value,
            "--format",
            "json",
        ],
        temp.path(),
    )?;
    let list_report: serde_json::Value = serde_json::from_slice(&list.stdout)?;
    assert!(list.status.success());
    assert_eq!(list_report["catalog"]["templates"][0]["name"], "TP_Blank");
    assert_eq!(
        list_report["catalog"]["templates"][0]["suppression"],
        "unspecified"
    );

    let apply = run_with_appdata(
        &[
            "template",
            "apply",
            "--engine-path",
            &engine_value,
            "--template",
            "TP_Blank",
            "--suppression",
            "enabled",
            "--yes",
            "--format",
            "json",
        ],
        temp.path(),
    )?;
    let apply_report: serde_json::Value = serde_json::from_slice(&apply.stdout)?;
    assert!(apply.status.success());
    assert_eq!(apply_report["result"]["target_kind"], "template");
    assert_eq!(apply_report["result"]["files_written"], 1);
    let edited = fs::read_to_string(&template)?;
    assert!(edited.contains("\"DisableEnginePluginsByDefault\":true"));
    assert!(edited.contains("\"Name\":\"KeepMe\""));
    let snapshot = apply_report["result"]["operation_id"]
        .as_str()
        .ok_or("template apply result has no operation identifier")?;

    let history = run_with_appdata(
        &[
            "template",
            "history",
            "--engine-path",
            &engine_value,
            "--format",
            "json",
        ],
        temp.path(),
    )?;
    let history_report: serde_json::Value = serde_json::from_slice(&history.stdout)?;
    assert_eq!(history_report["operations"][0]["id"], snapshot);
    assert_eq!(history_report["operations"][0]["target_kind"], "template");

    let restore = run_with_appdata(
        &[
            "template",
            "restore",
            "--engine-path",
            &engine_value,
            "--snapshot",
            snapshot,
            "--yes",
            "--format",
            "json",
        ],
        temp.path(),
    )?;
    let restore_report: serde_json::Value = serde_json::from_slice(&restore.stdout)?;
    assert!(restore.status.success());
    assert_eq!(restore_report["result"]["target_kind"], "template");
    assert_eq!(fs::read(template)?, original);
    Ok(())
}

#[test]
fn project_commands_resolve_engine_defaults_apply_overrides_and_restore()
-> Result<(), Box<dyn std::error::Error>> {
    let temp = tempdir()?;
    let engine = temp.path().join("UE_5.8");
    let build = engine.join("Engine").join("Build");
    let plugin = engine
        .join("Engine")
        .join("Plugins")
        .join("Runtime")
        .join("DefaultPlugin")
        .join("DefaultPlugin.uplugin");
    fs::create_dir_all(&build)?;
    fs::create_dir_all(plugin.parent().ok_or("plugin fixture has no parent")?)?;
    fs::write(
        build.join("Build.version"),
        r#"{"MajorVersion":5,"MinorVersion":8,"PatchVersion":4}"#,
    )?;
    fs::write(
        &plugin,
        r#"{"FileVersion":3,"EnabledByDefault":true,"Modules":[{"Name":"DefaultRuntime"}]}"#,
    )?;
    let project = temp.path().join("Fixture.uproject");
    let original = b"{\r\n  \"FileVersion\": 3,\r\n  \"EngineAssociation\": \"5.8\",\r\n  \"Plugins\": [{\"Name\":\"DefaultPlugin\",\"Enabled\":false}]\r\n}\r\n";
    fs::write(&project, original)?;
    let project_arg = project.display().to_string();
    let engine_arg = engine.display().to_string();

    assert_project_listing_and_plan(temp.path(), &project, original, &project_arg, &engine_arg)?;
    apply_and_restore_project(temp.path(), &project, original, &project_arg, &engine_arg)?;
    Ok(())
}

fn assert_project_listing_and_plan(
    appdata: &std::path::Path,
    project: &std::path::Path,
    original: &[u8],
    project_arg: &str,
    engine_arg: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let plugins = run_with_appdata(
        &[
            "project",
            "plugins",
            "--project",
            project_arg,
            "--engine-path",
            engine_arg,
            "--format",
            "json",
        ],
        appdata,
    )?;
    let plugin_report: serde_json::Value = serde_json::from_slice(&plugins.stdout)?;
    assert!(plugins.status.success());
    assert_eq!(plugin_report["workspace"]["engine"]["version"], "5.8.4");
    assert_eq!(
        plugin_report["workspace"]["plugins"][0]["plugin"]["effective_enabled"],
        true
    );
    assert_eq!(
        plugin_report["workspace"]["plugins"][0]["project_reference"],
        false
    );
    assert_eq!(
        plugin_report["workspace"]["plugins"][0]["project_effective_enabled"],
        false
    );
    assert_eq!(
        plugin_report["workspace"]["plugins"][0]["project_origin"],
        "project_disabled"
    );

    let plan = run_with_appdata(
        &[
            "project",
            "plan",
            "--project",
            project_arg,
            "--engine-path",
            engine_arg,
            "--clear",
            "DefaultPlugin",
            "--format",
            "json",
        ],
        appdata,
    )?;
    let plan_report: serde_json::Value = serde_json::from_slice(&plan.stdout)?;
    assert!(plan.status.success());
    assert_eq!(
        plan_report["plan"]["plugins"][0]["engine_effective_enabled"],
        true
    );
    assert_eq!(plan_report["plan"]["plugins"][0]["reference_before"], false);
    assert!(plan_report["plan"]["plugins"][0]["reference_after"].is_null());
    assert_eq!(plan_report["plan"]["plugins"][0]["effective_after"], true);
    assert_eq!(fs::read(project)?, original);
    Ok(())
}

fn apply_and_restore_project(
    appdata: &std::path::Path,
    project: &std::path::Path,
    original: &[u8],
    project_arg: &str,
    engine_arg: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let apply = run_with_appdata(
        &[
            "project",
            "apply",
            "--project",
            project_arg,
            "--engine-path",
            engine_arg,
            "--clear",
            "DefaultPlugin",
            "--yes",
            "--format",
            "json",
        ],
        appdata,
    )?;
    let apply_report: serde_json::Value = serde_json::from_slice(&apply.stdout)?;
    assert!(apply.status.success());
    assert_eq!(apply_report["result"]["target_kind"], "project");
    assert_eq!(apply_report["result"]["files_written"], 1);
    assert_ne!(fs::read(project)?, original);
    let snapshot = apply_report["result"]["operation_id"]
        .as_str()
        .ok_or("project apply result has no operation identifier")?;

    let history = run_with_appdata(
        &[
            "project",
            "history",
            "--project",
            project_arg,
            "--engine-path",
            engine_arg,
            "--format",
            "json",
        ],
        appdata,
    )?;
    let history_report: serde_json::Value = serde_json::from_slice(&history.stdout)?;
    assert!(history.status.success());
    assert_eq!(history_report["operations"][0]["id"], snapshot);
    assert_eq!(history_report["operations"][0]["target_kind"], "project");

    let restore = run_with_appdata(
        &[
            "project",
            "restore",
            "--project",
            project_arg,
            "--engine-path",
            engine_arg,
            "--snapshot",
            snapshot,
            "--yes",
            "--format",
            "json",
        ],
        appdata,
    )?;
    let restore_report: serde_json::Value = serde_json::from_slice(&restore.stdout)?;
    assert!(restore.status.success());
    assert_eq!(restore_report["result"]["kind"], "restore");
    assert_eq!(restore_report["result"]["target_kind"], "project");
    assert_eq!(fs::read(project)?, original);
    Ok(())
}

#[test]
fn write_commands_expose_noninteractive_confirmation() -> Result<(), Box<dyn std::error::Error>> {
    for command in ["apply", "restore"] {
        let output = run(&[command, "--help"])?;
        let stdout = String::from_utf8(output.stdout)?;
        assert!(output.status.success());
        assert!(stdout.contains("--yes"));
    }
    let plan = run(&["plan", "--help"])?;
    let stdout = String::from_utf8(plan.stdout)?;
    assert!(plan.status.success());
    assert!(!stdout.contains("--yes"));
    Ok(())
}

#[test]
fn presets_lists_bundled_files_with_an_empty_user_directory()
-> Result<(), Box<dyn std::error::Error>> {
    let temp = tempdir()?;
    let output = run_with_appdata(&["presets"], temp.path())?;
    let stdout = String::from_utf8(output.stdout)?;

    assert!(output.status.success());
    assert!(stdout.contains("project-first"));
    assert!(stdout.contains("review-first"));
    assert!(stdout.contains("windows-desktop-lean"));

    Ok(())
}

#[test]
fn presets_returns_a_versioned_json_list() -> Result<(), Box<dyn std::error::Error>> {
    let temp = tempdir()?;
    let output = run_with_appdata(&["presets", "--format", "json"], temp.path())?;
    let report: serde_json::Value = serde_json::from_slice(&output.stdout)?;

    assert!(output.status.success());
    assert_eq!(report["schema"], 1);
    assert_eq!(report["ok"], true);
    assert_eq!(report["presets"].as_array().map(Vec::len), Some(3));
    assert_eq!(report["presets"][0]["name"], "project-first");
    assert_eq!(report["presets"][1]["name"], "review-first");
    assert_eq!(report["presets"][2]["name"], "windows-desktop-lean");

    Ok(())
}

#[test]
fn plugins_returns_declared_state_and_source_metadata() -> Result<(), Box<dyn std::error::Error>> {
    let temp = tempdir()?;
    let engine = temp.path().join("UE_Invented");
    let build = engine.join("Engine").join("Build");
    let plugins = engine.join("Engine").join("Plugins");
    fs::create_dir_all(&build)?;
    fs::create_dir_all(plugins.join("Runtime").join("Alpha"))?;
    fs::create_dir_all(plugins.join("Editor").join("Beta"))?;
    fs::write(
        build.join("Build.version"),
        r#"{"MajorVersion":5,"MinorVersion":8,"PatchVersion":2}"#,
    )?;
    fs::write(
        plugins.join("Runtime").join("Alpha").join("Alpha.uplugin"),
        "{\r\n  \"FriendlyName\": \"Invented Alpha\",\r\n  \"Category\": \"Testing\",\r\n  \"EnabledByDefault\": true,\r\n  \"Modules\": [{\"Name\":\"AlphaRuntime\"}],\r\n  \"Plugins\": [{\"Name\":\"Beta\",\"Enabled\":true}]\r\n}\r\n",
    )?;
    fs::write(
        plugins.join("Editor").join("Beta").join("Beta.uplugin"),
        "{\n\t\"Description\": \"Invented beta descriptor.\"\n}\n",
    )?;

    let output = run(&[
        "plugins",
        "--engine-path",
        &engine.display().to_string(),
        "--format",
        "json",
    ])?;
    let report: serde_json::Value = serde_json::from_slice(&output.stdout)?;

    assert!(output.status.success());
    assert_eq!(report["schema"], 1);
    assert_eq!(report["ok"], true);
    assert_eq!(report["engine"]["version"], "5.8.2");
    assert_eq!(report["plugins"].as_array().map(Vec::len), Some(2));
    assert_eq!(report["plugins"][0]["name"], "Alpha");
    assert_eq!(report["plugins"][0]["declared_state"], "enabled");
    assert_eq!(report["plugins"][0]["module_count"], 1);
    assert_eq!(report["plugins"][0]["source"]["encoding"], "utf-8");
    assert_eq!(report["plugins"][0]["source"]["line_ending"], "crlf");
    assert_eq!(report["plugins"][0]["effective_enabled"], true);
    assert_eq!(
        report["plugins"][0]["enabled_dependencies"][0]["name"],
        "Beta"
    );
    assert_eq!(report["plugins"][1]["name"], "Beta");
    assert_eq!(report["plugins"][1]["friendly_name"], "Beta");
    assert_eq!(report["plugins"][1]["declared_state"], "unspecified");
    assert_eq!(report["plugins"][1]["effective_enabled"], true);
    assert_eq!(report["plugins"][1]["effective_path"][0], "Alpha");
    assert_eq!(report["plugins"][1]["effective_path"][1], "Beta");
    assert_eq!(report["plugins"][1]["reached_by"][0], "Alpha");
    assert_eq!(report["warnings"].as_array().map(Vec::len), Some(0));
    assert_eq!(
        report["dependency_warnings"].as_array().map(Vec::len),
        Some(0)
    );
    Ok(())
}

#[test]
fn preset_commands_resolve_names_and_explicit_paths() -> Result<(), Box<dyn std::error::Error>> {
    let temp = tempdir()?;
    let directory = temp.path().join("Unclean").join("presets");
    fs::create_dir_all(&directory)?;
    let preset_path = directory.join("invented.toml");
    fs::write(
        &preset_path,
        "schema = 1\nname = \"Invented preset\"\ndescription = \"Synthetic command fixture.\"\nenable = [\"CorePlugin\"]\ndisable = []\nclear = []\ndisable_matching = [\"Android*\"]\n",
    )?;

    let list = run_with_appdata(&["presets", "--format", "json"], temp.path())?;
    let list_report: serde_json::Value = serde_json::from_slice(&list.stdout)?;
    assert!(list.status.success());
    assert_eq!(list_report["presets"][0]["name"], "invented");

    let show = run_with_appdata(
        &["preset", "show", "invented", "--format", "json"],
        temp.path(),
    )?;
    let show_report: serde_json::Value = serde_json::from_slice(&show.stdout)?;
    assert!(show.status.success());
    assert_eq!(show_report["preset"]["schema"], 1);
    assert_eq!(show_report["preset"]["name"], "Invented preset");
    assert_eq!(show_report["preset"]["enable"][0], "CorePlugin");

    let validate = run_with_appdata(
        &["preset", "validate", &preset_path.display().to_string()],
        temp.path(),
    )?;
    let stdout = String::from_utf8(validate.stdout)?;
    assert!(validate.status.success());
    assert!(stdout.contains("Preset is valid: Invented preset"));
    Ok(())
}

#[test]
fn preset_validation_returns_the_shared_json_failure() -> Result<(), Box<dyn std::error::Error>> {
    let temp = tempdir()?;
    let preset_path = temp.path().join("invalid.toml");
    fs::write(&preset_path, "schema = 2\nname = \"Invalid fixture\"\n")?;

    let output = run_with_appdata(
        &[
            "preset",
            "validate",
            &preset_path.display().to_string(),
            "--format",
            "json",
        ],
        temp.path(),
    )?;
    let report: serde_json::Value = serde_json::from_slice(&output.stderr)?;

    assert_eq!(output.status.code(), Some(3));
    assert_eq!(report["schema"], 1);
    assert_eq!(report["ok"], false);
    assert_eq!(report["error"]["code"], "invalid_input");
    assert!(
        report["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("file uses schema 2"))
    );
    Ok(())
}

#[test]
fn plugins_infers_the_engine_from_a_working_directory() -> Result<(), Box<dyn std::error::Error>> {
    let temp = tempdir()?;
    let engine = temp.path().join("UE_Invented");
    let build = engine.join("Engine").join("Build");
    let plugin_dir = engine
        .join("Engine")
        .join("Plugins")
        .join("Runtime")
        .join("Invented");
    fs::create_dir_all(&build)?;
    fs::create_dir_all(&plugin_dir)?;
    fs::write(
        build.join("Build.version"),
        r#"{"MajorVersion":5,"MinorVersion":9,"PatchVersion":0}"#,
    )?;
    fs::write(
        plugin_dir.join("Invented.uplugin"),
        r#"{"EnabledByDefault":false}"#,
    )?;

    let output = run_in(&["plugins", "--format", "json"], &plugin_dir)?;
    let report: serde_json::Value = serde_json::from_slice(&output.stdout)?;

    assert!(output.status.success());
    assert_eq!(report["engine"]["version"], "5.9.0");
    assert_eq!(report["engine"]["source"], "working_directory");
    assert_eq!(report["plugins"][0]["name"], "Invented");
    Ok(())
}

#[test]
fn engines_returns_the_versioned_machine_schema() -> Result<(), Box<dyn std::error::Error>> {
    let temp = tempdir()?;
    let engine = temp.path().join("UE_5.8");
    let build = engine.join("Engine").join("Build");
    let plugins = engine.join("Engine").join("Plugins");
    fs::create_dir_all(&build)?;
    fs::create_dir_all(&plugins)?;
    fs::write(
        build.join("Build.version"),
        r#"{"MajorVersion":5,"MinorVersion":8,"PatchVersion":1}"#,
    )?;
    fs::write(plugins.join("Test.uplugin"), "{}")?;

    let output = run(&[
        "engines",
        "--engine-path",
        &engine.display().to_string(),
        "--format",
        "json",
    ])?;
    let report: serde_json::Value = serde_json::from_slice(&output.stdout)?;

    assert!(output.status.success());
    assert_eq!(report["schema"], 1);
    assert_eq!(report["ok"], true);
    assert_eq!(report["engines"][0]["version"], "5.8.1");
    assert_eq!(report["engines"][0]["source"], "explicit");
    assert_eq!(report["engines"][0]["health"], "partial");
    assert_eq!(report["engines"][0]["descriptor_count"], 1);
    assert_eq!(
        report["engines"][0]["issues"][0]["code"],
        "low_descriptor_count"
    );
    Ok(())
}

#[test]
fn plan_returns_verified_edits_without_writing() -> Result<(), Box<dyn std::error::Error>> {
    let temp = tempdir()?;
    let engine = temp.path().join("UE_Invented");
    let build = engine.join("Engine").join("Build");
    let plugins = engine.join("Engine").join("Plugins");
    let alpha = plugins.join("Runtime").join("Alpha").join("Alpha.uplugin");
    let beta = plugins.join("Runtime").join("Beta").join("Beta.uplugin");
    let gamma = plugins.join("Runtime").join("Gamma").join("Gamma.uplugin");
    fs::create_dir_all(&build)?;
    fs::create_dir_all(alpha.parent().ok_or("Alpha fixture has no parent")?)?;
    fs::create_dir_all(beta.parent().ok_or("Beta fixture has no parent")?)?;
    fs::create_dir_all(gamma.parent().ok_or("Gamma fixture has no parent")?)?;
    fs::write(
        build.join("Build.version"),
        r#"{"MajorVersion":5,"MinorVersion":9,"PatchVersion":0}"#,
    )?;
    fs::write(
        &alpha,
        r#"{"EnabledByDefault":true,"Modules":[{"Name":"Alpha"}],"Plugins":[{"Name":"Beta","Enabled":true}]}"#,
    )?;
    fs::write(&beta, r#"{"Modules":[{"Name":"Beta"}]}"#)?;
    let gamma_source = r#"{"EnabledByDefault":true,"Modules":[{"Name":"Gamma"}]}"#;
    fs::write(&gamma, gamma_source)?;
    let preset_path = temp.path().join("plan.toml");
    fs::write(
        &preset_path,
        "schema = 1\nname = \"Invented plan\"\nenable = []\ndisable = [\"Beta\", \"Gamma\"]\nclear = []\ndisable_matching = []\n",
    )?;

    let output = run_with_appdata(
        &[
            "plan",
            "--engine-path",
            &engine.display().to_string(),
            "--preset",
            &preset_path.display().to_string(),
            "--format",
            "json",
        ],
        temp.path(),
    )?;
    let report: serde_json::Value = serde_json::from_slice(&output.stdout)?;

    assert!(output.status.success());
    assert_eq!(report["schema"], 1);
    assert_eq!(report["ok"], true);
    assert_eq!(report["plan"]["schema"], 1);
    assert_eq!(report["plan"]["changes"].as_array().map(Vec::len), Some(1));
    assert_eq!(report["plan"]["changes"][0]["plugin"], "Gamma");
    assert_eq!(report["plan"]["changes"][0]["field"], "EnabledByDefault");
    assert_eq!(
        report["plan"]["changes"][0]["sha256_before"]
            .as_str()
            .map(str::len),
        Some(64)
    );
    assert!(report["plan"]["changes"][0].get("planned_bytes").is_none());
    assert_eq!(report["plan"]["no_ops"][0]["plugin"], "Beta");
    assert_eq!(
        report["plan"]["no_ops"][0]["reason"],
        "unspecified_already_off"
    );
    assert_eq!(
        report["plan"]["dependency_warnings"][0]["roots"][0],
        "Alpha"
    );
    assert_eq!(fs::read_to_string(&gamma)?, gamma_source);

    let gui_plan = gui_plan_for_cli_report(
        &engine,
        &preset_path,
        &temp.path().join("gui-backups"),
        &report,
    )?;

    let table = run_with_appdata(
        &[
            "plan",
            "--engine-path",
            &engine.display().to_string(),
            "--preset",
            &preset_path.display().to_string(),
        ],
        temp.path(),
    )?;
    let stdout = String::from_utf8(table.stdout)?;
    let stderr = String::from_utf8(table.stderr)?;
    assert!(table.status.success());
    assert!(stdout.contains("Gamma: EnabledByDefault enabled -> disabled"));
    assert!(stdout.contains("Source SHA-256:"));
    assert!(stdout.contains("Backup:"));
    assert!(stdout.contains("Effective plugins:"));
    assert!(stderr.contains("Beta"));

    assert_cli_apply_matches_gui_plan(&engine, &preset_path, &gamma, temp.path(), &gui_plan)?;
    Ok(())
}

#[test]
fn status_returns_drift_data_with_exit_code_six() -> Result<(), Box<dyn std::error::Error>> {
    let temp = tempdir()?;
    let engine = temp.path().join("UE_Invented");
    let build = engine.join("Engine").join("Build");
    let plugin = engine
        .join("Engine")
        .join("Plugins")
        .join("Runtime")
        .join("Drifted")
        .join("Drifted.uplugin");
    fs::create_dir_all(&build)?;
    fs::create_dir_all(plugin.parent().ok_or("drift fixture has no parent")?)?;
    fs::write(
        build.join("Build.version"),
        r#"{"MajorVersion":5,"MinorVersion":9,"PatchVersion":0}"#,
    )?;
    fs::write(&plugin, b"current bytes")?;
    let state_directory = temp.path().join("Unclean");
    fs::create_dir_all(&state_directory)?;
    let state = JournalState {
        operations: vec![JournalOperation {
            id: "test-operation".to_owned(),
            kind: JournalOperationKind::Apply,
            target_kind: OperationTargetKind::Engine,
            engine_path: fs::canonicalize(&engine)?,
            engine_version: Some("5.9.0".to_owned()),
            project_path: None,
            preset: "Invented".to_owned(),
            completed: "test-time".to_owned(),
            backup_directory: fs::canonicalize(temp.path())?.join("backup"),
            source_snapshot: None,
            files: vec![JournalFile {
                relative_path: "Engine/Plugins/Runtime/Drifted/Drifted.uplugin".into(),
                sha256_after: unclean_core::plans::sha256_hex(b"recorded bytes"),
            }],
        }],
        ..JournalState::default()
    };
    fs::write(state_directory.join("state.toml"), state.render())?;

    let output = run_with_appdata(
        &[
            "status",
            "--engine-path",
            &engine.display().to_string(),
            "--format",
            "json",
        ],
        temp.path(),
    )?;
    let report: serde_json::Value = serde_json::from_slice(&output.stdout)?;

    assert_eq!(output.status.code(), Some(6));
    assert_eq!(report["schema"], 1);
    assert_eq!(report["ok"], true);
    assert_eq!(report["status"]["recorded"], true);
    assert_eq!(report["status"]["drifted"], true);
    assert_eq!(report["status"]["files"][0]["state"], "modified");
    assert!(output.stderr.is_empty());
    Ok(())
}

#[test]
fn status_without_history_is_clean() -> Result<(), Box<dyn std::error::Error>> {
    let temp = tempdir()?;
    let engine = temp.path().join("UE_Invented");
    let build = engine.join("Engine").join("Build");
    let plugins = engine.join("Engine").join("Plugins");
    fs::create_dir_all(&build)?;
    fs::create_dir_all(&plugins)?;
    fs::write(
        build.join("Build.version"),
        r#"{"MajorVersion":5,"MinorVersion":9,"PatchVersion":0}"#,
    )?;

    let output = run_with_appdata(
        &[
            "status",
            "--engine-path",
            &engine.display().to_string(),
            "--format",
            "json",
        ],
        temp.path(),
    )?;
    let report: serde_json::Value = serde_json::from_slice(&output.stdout)?;

    assert!(output.status.success());
    assert_eq!(report["status"]["recorded"], false);
    assert_eq!(report["status"]["drifted"], false);
    Ok(())
}

#[test]
fn apply_history_and_restore_share_the_transaction_record() -> Result<(), Box<dyn std::error::Error>>
{
    let temp = tempdir()?;
    let engine = temp.path().join("UE_Invented");
    let build = engine.join("Engine").join("Build");
    let plugin = engine
        .join("Engine")
        .join("Plugins")
        .join("Runtime")
        .join("Invented")
        .join("Invented.uplugin");
    fs::create_dir_all(&build)?;
    fs::create_dir_all(plugin.parent().ok_or("plugin fixture has no parent")?)?;
    fs::write(
        build.join("Build.version"),
        r#"{"MajorVersion":5,"MinorVersion":9,"PatchVersion":0}"#,
    )?;
    let original = br#"{"FileVersion":3,"EnabledByDefault":true}"#;
    fs::write(&plugin, original)?;
    let preset = temp.path().join("disable.toml");
    fs::write(
        &preset,
        "schema = 1\nname = \"Disable invented\"\nenable = []\ndisable = [\"Invented\"]\nclear = []\ndisable_matching = []\n",
    )?;

    let unconfirmed = run_with_appdata(
        &[
            "apply",
            "--engine-path",
            &engine.display().to_string(),
            "--preset",
            &preset.display().to_string(),
            "--format",
            "json",
        ],
        temp.path(),
    )?;
    assert_eq!(unconfirmed.status.code(), Some(3));
    assert_eq!(fs::read(&plugin)?, original);

    let apply = run_with_appdata(
        &[
            "apply",
            "--engine-path",
            &engine.display().to_string(),
            "--preset",
            &preset.display().to_string(),
            "--yes",
            "--format",
            "json",
        ],
        temp.path(),
    )?;
    let apply_report: serde_json::Value = serde_json::from_slice(&apply.stdout)?;
    assert!(apply.status.success());
    assert_eq!(apply_report["ok"], true);
    assert_eq!(apply_report["result"]["kind"], "apply");
    assert_eq!(apply_report["result"]["files_written"], 1);
    assert_ne!(fs::read(&plugin)?, original);
    let snapshot = apply_report["result"]["operation_id"]
        .as_str()
        .ok_or("apply result has no operation identifier")?;

    let history = run_with_appdata(
        &[
            "history",
            "--engine-path",
            &engine.display().to_string(),
            "--format",
            "json",
        ],
        temp.path(),
    )?;
    let history_report: serde_json::Value = serde_json::from_slice(&history.stdout)?;
    assert!(history.status.success());
    assert_eq!(history_report["operations"][0]["id"], snapshot);
    assert_eq!(history_report["operations"][0]["kind"], "apply");

    let restore = run_with_appdata(
        &[
            "restore",
            "--engine-path",
            &engine.display().to_string(),
            "--snapshot",
            snapshot,
            "--yes",
            "--format",
            "json",
        ],
        temp.path(),
    )?;
    let restore_report: serde_json::Value = serde_json::from_slice(&restore.stdout)?;
    assert!(restore.status.success());
    assert_eq!(restore_report["result"]["kind"], "restore");
    assert_eq!(restore_report["plan"]["source_snapshot"], snapshot);
    assert_eq!(fs::read(&plugin)?, original);

    assert_restored_history(temp.path(), &engine, snapshot)?;
    Ok(())
}

fn assert_restored_history(
    appdata: &std::path::Path,
    engine: &std::path::Path,
    snapshot: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let final_history = run_with_appdata(
        &[
            "history",
            "--engine-path",
            &engine.display().to_string(),
            "--format",
            "json",
        ],
        appdata,
    )?;
    let final_report: serde_json::Value = serde_json::from_slice(&final_history.stdout)?;
    assert_eq!(final_report["operations"].as_array().map(Vec::len), Some(2));
    assert_eq!(final_report["operations"][0]["kind"], "restore");
    assert_eq!(final_report["operations"][0]["source_snapshot"], snapshot);
    Ok(())
}

#[test]
fn invalid_invocations_keep_the_parser_exit_code() -> Result<(), Box<dyn std::error::Error>> {
    let output = run(&["plan"])?;

    assert_eq!(output.status.code(), Some(2));

    Ok(())
}
