//! Owns the desktop state model and renders the engine workflow.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::Duration;

use eframe::egui::{
    self, Align, Button, Color32, Context, Frame, Id, Key, Layout, Margin, RichText, ScrollArea,
    Sense, Stroke, TextEdit, Ui, Vec2, WidgetInfo, WidgetType,
};
use unclean_core::apply::{ProjectRestorePlan, RestorePlan, TemplateRestorePlan};
use unclean_core::descriptors::DeclaredPluginState;
use unclean_core::discovery::{
    DiscoveryOptions, DiscoveryReport, EngineHealth, EngineInstallation, discover_engines,
};
use unclean_core::elevation::ActiveUnrealProcess;
use unclean_core::plans::EnginePlan;
use unclean_core::preset_catalog::{PresetCatalog, preset_filename_stem};
use unclean_core::presets::{
    PresetDocument, PresetFile, PresetRuleList, default_preset_directory, list_available_presets,
    load_preset,
};
use unclean_core::project_plans::ProjectPlan;
use unclean_core::projects::ProjectSuppressionEdit;
use unclean_core::templates::TemplatePlan;

use crate::theme;
use crate::workflow::{
    EngineWorkspace, ProjectWorkspaceView, ReviewedEnginePlan, TemplateWorkspaceView,
    active_engine_processes, active_multi_engine_processes, build_engine_review,
    build_multi_engine_review, build_project_restore_review, build_project_review,
    build_restore_review, build_template_restore_review, build_template_review,
    engine_review_requires_elevation, execute_engine_review, execute_project_restore_review,
    execute_project_review, execute_restore_review, execute_template_restore_review,
    execute_template_review, load_engine_workspace, load_project_workspace_view,
    load_template_workspace, plugin_matches, projected_effective_states,
    restore_review_requires_elevation, template_restore_requires_elevation,
    template_review_requires_elevation,
};

const LIST_ROW_HEIGHT: f32 = 28.0;
const TABLE_LABEL_HEIGHT: f32 = 20.0;
const TABLE_CONTROL_HEIGHT: f32 = 26.0;
const TABLE_BORDER_WIDTH: f32 = 1.0;
const CONTROL_COLUMN_WIDTH: f32 = 74.0;
const ENGINE_EFFECTIVE_COLUMN_WIDTH: f32 = 60.0;
const ENGINE_PLUGIN_COLUMN_WIDTH: f32 = 230.0;
const ENGINE_CATEGORY_COLUMN_WIDTH: f32 = 150.0;
const PROJECT_OVERRIDE_COLUMN_WIDTH: f32 = 104.0;
const PROJECT_STATE_COLUMN_WIDTH: f32 = 58.0;
const PROJECT_PLUGIN_COLUMN_WIDTH: f32 = 215.0;
const PROJECT_SOURCE_COLUMN_WIDTH: f32 = 155.0;
const ENGINE_RAIL_DEFAULT_WIDTH: f32 = 238.0;
const PROJECT_RAIL_DEFAULT_WIDTH: f32 = 258.0;
const RAIL_MIN_WIDTH: f32 = 200.0;
const WORKSPACE_MIN_WIDTH: f32 = 680.0;
const DETAILS_DEFAULT_HEIGHT: f32 = 190.0;
const DETAILS_MIN_HEIGHT: f32 = 130.0;
const WORKSPACE_MIN_HEIGHT: f32 = 220.0;
type WorkspaceLoad = (
    usize,
    std::result::Result<EngineWorkspace, unclean_core::Error>,
);
type ProjectWorkspaceLoad = (
    PathBuf,
    std::result::Result<ProjectWorkspaceView, unclean_core::Error>,
);
type TemplateWorkspaceLoad = (
    usize,
    std::result::Result<TemplateWorkspaceView, unclean_core::Error>,
);
type PlanLoad = (
    u64,
    bool,
    std::result::Result<BuiltPlan, unclean_core::Error>,
);
type MultiPlanLoad = (
    u64,
    std::result::Result<Vec<ReviewedEnginePlan>, unclean_core::Error>,
);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TargetMode {
    Engine,
    Project,
    Template,
}

#[derive(Clone, Debug)]
enum BuiltPlan {
    Engine(Box<EnginePlan>, Option<bool>),
    Project(Box<ProjectPlan>),
    Template(Box<TemplatePlan>, Option<bool>),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum View {
    Workspace,
    PresetEditor,
    ApplyReview,
    ProjectApplyReview,
    TemplateApplyReview,
    MultiEngineSelection,
    MultiEngineReview,
    History,
    ProjectHistory,
    TemplateHistory,
    RestoreReview,
    ProjectRestoreReview,
    TemplateRestoreReview,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum NoticeKind {
    Success,
    Warning,
    Error,
}

#[derive(Clone, Debug)]
struct Notice {
    kind: NoticeKind,
    text: String,
}

#[derive(Clone, Debug)]
struct PresetDraft {
    name: String,
    description: String,
    enable: String,
    disable: String,
    clear: String,
    disable_matching: String,
}

#[derive(Clone, Debug)]
struct PresetEditorBackup {
    path: Option<PathBuf>,
    document: PresetDocument,
    draft: PresetDraft,
}

impl PresetDraft {
    fn from_document(document: &PresetDocument) -> Self {
        let preset = document.preset();
        Self {
            name: preset.name.clone(),
            description: preset.description.clone().unwrap_or_default(),
            enable: preset.enable.join("\n"),
            disable: preset.disable.join("\n"),
            clear: preset.clear.join("\n"),
            disable_matching: preset.disable_matching.join("\n"),
        }
    }

    fn document(&self, base: &PresetDocument) -> Result<PresetDocument, String> {
        let mut document = base.clone();
        document
            .set_name(self.name.trim())
            .map_err(|error| error.to_string())?;
        let description = self.description.trim();
        document
            .set_description((!description.is_empty()).then_some(description))
            .map_err(|error| error.to_string())?;
        for list in [
            PresetRuleList::Enable,
            PresetRuleList::Disable,
            PresetRuleList::Clear,
            PresetRuleList::DisableMatching,
        ] {
            document
                .set_rules(list, &[])
                .map_err(|error| error.to_string())?;
        }
        for (list, text) in [
            (PresetRuleList::Enable, self.enable.as_str()),
            (PresetRuleList::Disable, self.disable.as_str()),
            (PresetRuleList::Clear, self.clear.as_str()),
            (
                PresetRuleList::DisableMatching,
                self.disable_matching.as_str(),
            ),
        ] {
            document
                .set_rules(list, &rules_from_text(text))
                .map_err(|error| error.to_string())?;
        }
        Ok(document)
    }

    fn set_plugin(&mut self, plugin: &str, state: DeclaredPluginState) {
        let mut enable = rules_from_text(&self.enable);
        let mut disable = rules_from_text(&self.disable);
        let mut clear = rules_from_text(&self.clear);
        remove_rule(&mut enable, plugin);
        remove_rule(&mut disable, plugin);
        remove_rule(&mut clear, plugin);
        match state {
            DeclaredPluginState::Enabled => enable.push(plugin.to_owned()),
            DeclaredPluginState::Disabled => disable.push(plugin.to_owned()),
            DeclaredPluginState::Unspecified => clear.push(plugin.to_owned()),
        }
        self.enable = enable.join("\n");
        self.disable = disable.join("\n");
        self.clear = clear.join("\n");
    }
}

#[derive(Clone, Debug)]
enum PendingWrite {
    Apply {
        plan: Box<EnginePlan>,
        processes: Vec<ActiveUnrealProcess>,
    },
    Restore {
        plan: Box<RestorePlan>,
        processes: Vec<ActiveUnrealProcess>,
    },
    ProjectApply {
        plan: Box<ProjectPlan>,
        processes: Vec<ActiveUnrealProcess>,
    },
    ProjectRestore {
        plan: Box<ProjectRestorePlan>,
        processes: Vec<ActiveUnrealProcess>,
    },
    TemplateApply {
        plan: Box<TemplatePlan>,
        processes: Vec<ActiveUnrealProcess>,
    },
    TemplateRestore {
        plan: Box<TemplateRestorePlan>,
        processes: Vec<ActiveUnrealProcess>,
    },
    MultiEngineApply {
        plans: Vec<ReviewedEnginePlan>,
        processes: Vec<ActiveUnrealProcess>,
    },
}

#[derive(Clone, Debug)]
struct SavedPreset {
    catalog: PresetCatalog,
    path: PathBuf,
    document: PresetDocument,
}

#[derive(Clone, Debug)]
enum Action {
    SelectEngine(usize),
    AddEngine,
    OpenProject,
    UseEngineTarget,
    UseTemplateTarget,
    ToggleTemplate(PathBuf),
    SelectAllTemplates,
    ClearTemplateSelection,
    SetTemplateSuppression(ProjectSuppressionEdit),
    ShowMultiEngineSelection,
    ToggleMultiEngine(usize),
    SelectAllMultiEngines,
    ClearMultiEngineSelection,
    BuildMultiEngineReview,
    BackToMultiEngineSelection,
    BeginMultiEngineApply,
    Refresh,
    NewPreset,
    ImportPreset,
    LoadPreset(PathBuf),
    SavePreset,
    ExportPreset,
    ShowPresetEditor,
    CommitPresetEditor,
    CancelPresetEditor,
    ReviewApply,
    BeginApply,
    BeginProjectApply,
    BeginTemplateApply,
    ShowHistory,
    ReviewRestore(String),
    BeginRestore,
    BeginProjectRestore,
    BeginTemplateRestore,
    ConfirmWrite,
    CancelWrite,
    Back,
}

/// Owns one desktop session without duplicating scan, plan, or write rules.
pub(crate) struct UncleanApp {
    target_mode: TargetMode,
    engines: Vec<EngineInstallation>,
    discovery_warnings: Vec<String>,
    discovery_receiver: Option<Receiver<DiscoveryReport>>,
    pending_engine_path: Option<PathBuf>,
    pending_project_path: Option<PathBuf>,
    selected_engine: Option<usize>,
    workspace: Option<EngineWorkspace>,
    workspace_receiver: Option<Receiver<WorkspaceLoad>>,
    loading_workspace: Option<usize>,
    project_path: Option<PathBuf>,
    project_workspace: Option<ProjectWorkspaceView>,
    project_workspace_receiver: Option<Receiver<ProjectWorkspaceLoad>>,
    loading_project_path: Option<PathBuf>,
    template_workspace: Option<TemplateWorkspaceView>,
    template_workspace_receiver: Option<Receiver<TemplateWorkspaceLoad>>,
    loading_template_engine: Option<usize>,
    selected_templates: BTreeSet<PathBuf>,
    selected_plugin: Option<String>,
    preset_catalog: Option<PresetCatalog>,
    preset_files: Vec<PresetFile>,
    preset_path: Option<PathBuf>,
    preset_document: PresetDocument,
    preset_draft: PresetDraft,
    preset_editor_backup: Option<PresetEditorBackup>,
    plan: Option<EnginePlan>,
    project_plan: Option<ProjectPlan>,
    template_plan: Option<TemplatePlan>,
    plan_receiver: Option<Receiver<PlanLoad>>,
    plan_generation: u64,
    plan_requires_elevation: Option<bool>,
    project_suppression: ProjectSuppressionEdit,
    template_suppression: ProjectSuppressionEdit,
    restore_plan: Option<RestorePlan>,
    project_restore_plan: Option<ProjectRestorePlan>,
    template_restore_plan: Option<TemplateRestorePlan>,
    selected_multi_engines: BTreeSet<usize>,
    multi_engine_plans: Vec<ReviewedEnginePlan>,
    multi_plan_receiver: Option<Receiver<MultiPlanLoad>>,
    multi_plan_generation: u64,
    restore_requires_elevation: Option<bool>,
    pending_write: Option<PendingWrite>,
    view: View,
    search: String,
    only_changed: bool,
    only_effective: bool,
    notice: Option<Notice>,
}

impl UncleanApp {
    pub(crate) fn new(context: &Context) -> Result<Self, unclean_core::presets::PresetError> {
        Self::new_with_preset_directory(context, default_preset_directory())
    }

    fn new_with_preset_directory(
        context: &Context,
        preset_directory: Option<PathBuf>,
    ) -> Result<Self, unclean_core::presets::PresetError> {
        theme::install(context);
        let preset_document = PresetDocument::new("New preset")?;
        let preset_catalog = preset_directory.map(PresetCatalog::new);
        let preset_files = preset_catalog
            .as_ref()
            .map_or_else(|| list_available_presets(None), PresetCatalog::list)
            .unwrap_or_default();
        Ok(Self {
            target_mode: TargetMode::Engine,
            engines: Vec::new(),
            discovery_warnings: Vec::new(),
            discovery_receiver: Some(spawn_discovery(DiscoveryOptions::default())),
            pending_engine_path: None,
            pending_project_path: None,
            selected_engine: None,
            workspace: None,
            workspace_receiver: None,
            loading_workspace: None,
            project_path: None,
            project_workspace: None,
            project_workspace_receiver: None,
            loading_project_path: None,
            template_workspace: None,
            template_workspace_receiver: None,
            loading_template_engine: None,
            selected_templates: BTreeSet::new(),
            selected_plugin: None,
            preset_catalog,
            preset_files,
            preset_path: None,
            preset_draft: PresetDraft::from_document(&preset_document),
            preset_document,
            preset_editor_backup: None,
            plan: None,
            project_plan: None,
            template_plan: None,
            plan_receiver: None,
            plan_generation: 0,
            plan_requires_elevation: None,
            project_suppression: ProjectSuppressionEdit::Keep,
            template_suppression: ProjectSuppressionEdit::Set(true),
            restore_plan: None,
            project_restore_plan: None,
            template_restore_plan: None,
            selected_multi_engines: BTreeSet::new(),
            multi_engine_plans: Vec::new(),
            multi_plan_receiver: None,
            multi_plan_generation: 0,
            restore_requires_elevation: None,
            pending_write: None,
            view: View::Workspace,
            search: String::new(),
            only_changed: false,
            only_effective: false,
            notice: None,
        })
    }

    fn selected_engine(&self) -> Option<&EngineInstallation> {
        self.selected_engine
            .and_then(|index| self.engines.get(index))
    }

    fn select_engine(&mut self, index: usize) {
        let Some(engine) = self.engines.get(index).cloned() else {
            return;
        };
        if !engine.health.is_selectable() {
            self.set_notice(
                NoticeKind::Warning,
                "Engine selection failed. Repair the installation and refresh discovery.",
            );
            return;
        }
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let result = load_engine_workspace(&engine);
            let _ = sender.send((index, result));
        });
        self.target_mode = TargetMode::Engine;
        self.selected_engine = Some(index);
        self.selected_plugin = None;
        self.workspace = None;
        self.workspace_receiver = Some(receiver);
        self.loading_workspace = Some(index);
        self.plan = None;
        self.project_plan = None;
        self.template_plan = None;
        self.plan_receiver = None;
        self.plan_generation = self.plan_generation.wrapping_add(1);
        self.plan_requires_elevation = None;
        self.project_path = None;
        self.project_workspace = None;
        self.project_workspace_receiver = None;
        self.loading_project_path = None;
        self.project_restore_plan = None;
        self.template_workspace = None;
        self.template_workspace_receiver = None;
        self.loading_template_engine = None;
        self.selected_templates.clear();
        self.template_restore_plan = None;
        self.selected_multi_engines.clear();
        self.multi_engine_plans.clear();
        self.multi_plan_receiver = None;
        self.multi_plan_generation = self.multi_plan_generation.wrapping_add(1);
    }

    fn select_template_engine(&mut self, index: usize) {
        let Some(engine) = self.engines.get(index).cloned() else {
            return;
        };
        if !engine.health.is_selectable() {
            self.set_notice(
                NoticeKind::Warning,
                "Engine selection failed. Repair the installation and refresh discovery.",
            );
            return;
        }
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let result = load_template_workspace(&engine);
            let _ = sender.send((index, result));
        });
        self.target_mode = TargetMode::Template;
        self.selected_engine = Some(index);
        self.workspace = None;
        self.workspace_receiver = None;
        self.loading_workspace = None;
        self.project_path = None;
        self.project_workspace = None;
        self.project_workspace_receiver = None;
        self.loading_project_path = None;
        self.template_workspace = None;
        self.template_workspace_receiver = Some(receiver);
        self.loading_template_engine = Some(index);
        self.selected_templates.clear();
        self.selected_plugin = None;
        self.plan = None;
        self.project_plan = None;
        self.template_plan = None;
        self.plan_receiver = None;
        self.plan_generation = self.plan_generation.wrapping_add(1);
        self.plan_requires_elevation = None;
        self.template_suppression = ProjectSuppressionEdit::Set(true);
        self.restore_plan = None;
        self.project_restore_plan = None;
        self.template_restore_plan = None;
        self.selected_multi_engines.clear();
        self.multi_engine_plans.clear();
        self.multi_plan_receiver = None;
        self.multi_plan_generation = self.multi_plan_generation.wrapping_add(1);
    }

    fn refresh(&mut self) {
        let selected_path = self.selected_engine().map(|engine| engine.path.clone());
        self.pending_project_path = (self.target_mode == TargetMode::Project)
            .then(|| self.project_path.clone())
            .flatten();
        self.start_discovery(DiscoveryOptions::default(), selected_path);
    }

    fn start_discovery(&mut self, options: DiscoveryOptions, pending_engine_path: Option<PathBuf>) {
        self.discovery_receiver = Some(spawn_discovery(options));
        self.pending_engine_path = pending_engine_path;
        self.workspace = None;
        self.workspace_receiver = None;
        self.loading_workspace = None;
        self.project_workspace = None;
        self.project_workspace_receiver = None;
        self.loading_project_path = self.pending_project_path.clone();
        self.template_workspace = None;
        self.template_workspace_receiver = None;
        self.loading_template_engine = None;
        self.selected_engine = None;
        self.selected_plugin = None;
        self.plan = None;
        self.project_plan = None;
        self.template_plan = None;
        self.plan_receiver = None;
        self.plan_generation = self.plan_generation.wrapping_add(1);
        self.plan_requires_elevation = None;
        self.selected_multi_engines.clear();
        self.multi_engine_plans.clear();
        self.multi_plan_receiver = None;
        self.multi_plan_generation = self.multi_plan_generation.wrapping_add(1);
    }

    fn add_engine(&mut self) {
        let Some(path) = rfd::FileDialog::new()
            .set_title("Select an Unreal Engine installation")
            .pick_folder()
        else {
            return;
        };
        let mut options = DiscoveryOptions::default();
        options.explicit_paths.push(path.clone());
        self.pending_project_path = None;
        self.start_discovery(options, Some(path));
    }

    fn open_project(&mut self) {
        let Some(path) = rfd::FileDialog::new()
            .set_title("Open Unreal project")
            .add_filter("Unreal project", &["uproject"])
            .pick_file()
        else {
            return;
        };
        self.select_project_path(&path);
    }

    fn select_project_path(&mut self, path: &Path) {
        if self.engines.is_empty() {
            self.set_error(
                "Project loading failed: no engine installations are available. Add the associated engine and retry.",
            );
            return;
        }
        let engines = self.engines.clone();
        let requested_path = path.to_path_buf();
        let worker_path = path.to_path_buf();
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let result = load_project_workspace_view(&worker_path, &engines);
            let _ = sender.send((worker_path, result));
        });
        self.target_mode = TargetMode::Project;
        self.project_path = Some(path.to_path_buf());
        self.project_workspace = None;
        self.project_workspace_receiver = Some(receiver);
        self.loading_project_path = Some(requested_path);
        self.workspace = None;
        self.workspace_receiver = None;
        self.loading_workspace = None;
        self.template_workspace = None;
        self.template_workspace_receiver = None;
        self.loading_template_engine = None;
        self.selected_templates.clear();
        self.selected_plugin = None;
        self.plan = None;
        self.project_plan = None;
        self.template_plan = None;
        self.plan_receiver = None;
        self.plan_generation = self.plan_generation.wrapping_add(1);
        self.plan_requires_elevation = None;
        self.project_suppression = ProjectSuppressionEdit::Keep;
        self.restore_plan = None;
        self.project_restore_plan = None;
        self.template_restore_plan = None;
        self.selected_multi_engines.clear();
        self.multi_engine_plans.clear();
        self.multi_plan_receiver = None;
        self.multi_plan_generation = self.multi_plan_generation.wrapping_add(1);
    }

    fn poll_background(&mut self, context: &Context) {
        self.poll_discovery();
        self.poll_workspace();
        self.poll_project_workspace();
        self.poll_template_workspace();
        self.poll_plan();
        self.poll_multi_plan();
        if self.discovery_receiver.is_some()
            || self.workspace_receiver.is_some()
            || self.project_workspace_receiver.is_some()
            || self.template_workspace_receiver.is_some()
            || self.plan_receiver.is_some()
            || self.multi_plan_receiver.is_some()
        {
            context.request_repaint_after(Duration::from_millis(50));
        }
    }

    fn poll_discovery(&mut self) {
        let discovery = self.discovery_receiver.as_ref().map(Receiver::try_recv);
        match discovery {
            Some(Ok(report)) => {
                self.discovery_receiver = None;
                self.engines = report.engines;
                self.discovery_warnings = report
                    .warnings
                    .into_iter()
                    .map(|warning| warning.message)
                    .collect();
                let selection = self
                    .pending_engine_path
                    .take()
                    .and_then(|path| self.engines.iter().position(|engine| engine.path == path))
                    .or_else(|| {
                        self.engines
                            .iter()
                            .position(|engine| engine.source.as_str() == "explicit")
                    })
                    .or_else(|| {
                        self.engines
                            .iter()
                            .position(|engine| engine.health.is_selectable())
                    });
                if let Some(project_path) = self.pending_project_path.take() {
                    self.select_project_path(&project_path);
                } else if let Some(index) = selection {
                    if self.target_mode == TargetMode::Template {
                        self.select_template_engine(index);
                    } else {
                        self.select_engine(index);
                    }
                } else {
                    self.set_notice(
                        NoticeKind::Warning,
                        "Unclean found no usable Unreal Engine installation. Add an engine folder to continue.",
                    );
                }
            }
            Some(Err(TryRecvError::Disconnected)) => {
                self.discovery_receiver = None;
                self.set_error(
                    "Engine discovery stopped before returning data. Refresh discovery to retry.",
                );
            }
            Some(Err(TryRecvError::Empty)) | None => {}
        }
    }

    fn poll_workspace(&mut self) {
        let workspace = self.workspace_receiver.as_ref().map(Receiver::try_recv);
        match workspace {
            Some(Ok((index, Ok(workspace)))) if self.loading_workspace == Some(index) => {
                self.workspace_receiver = None;
                self.loading_workspace = None;
                self.selected_plugin = workspace.plugins.first().map(|plugin| plugin.name.clone());
                self.workspace = Some(workspace);
                self.rebuild_plan(false);
            }
            Some(Ok((index, Err(error)))) if self.loading_workspace == Some(index) => {
                self.workspace_receiver = None;
                self.loading_workspace = None;
                self.set_error(format!(
                    "Engine data load failed: {error}. Check the installation and refresh."
                ));
            }
            Some(Ok(_)) => {
                self.workspace_receiver = None;
            }
            Some(Err(TryRecvError::Disconnected)) => {
                self.workspace_receiver = None;
                self.loading_workspace = None;
                self.set_error(
                    "Engine loading stopped before returning data. Select the engine to retry.",
                );
            }
            Some(Err(TryRecvError::Empty)) | None => {}
        }
    }

    fn poll_project_workspace(&mut self) {
        let project = self
            .project_workspace_receiver
            .as_ref()
            .map(Receiver::try_recv);
        match project {
            Some(Ok((path, Ok(workspace))))
                if self.loading_project_path.as_ref() == Some(&path) =>
            {
                self.project_workspace_receiver = None;
                self.loading_project_path = None;
                self.selected_engine = self
                    .engines
                    .iter()
                    .position(|engine| engine.path == workspace.workspace.engine.path);
                self.selected_plugin = workspace
                    .workspace
                    .plugins
                    .first()
                    .map(|plugin| plugin.plugin.name.clone());
                self.project_path = Some(path);
                self.project_workspace = Some(workspace);
                self.rebuild_plan(false);
            }
            Some(Ok((path, Err(error)))) if self.loading_project_path.as_ref() == Some(&path) => {
                self.project_workspace_receiver = None;
                self.loading_project_path = None;
                self.set_error(format!(
                    "Project loading failed: {error}. Check EngineAssociation and the project file."
                ));
            }
            Some(Ok(_)) => {
                self.project_workspace_receiver = None;
            }
            Some(Err(TryRecvError::Disconnected)) => {
                self.project_workspace_receiver = None;
                self.loading_project_path = None;
                self.set_error(
                    "Project loading stopped before returning data. Open the project again to retry.",
                );
            }
            Some(Err(TryRecvError::Empty)) | None => {}
        }
    }

    fn poll_template_workspace(&mut self) {
        let template = self
            .template_workspace_receiver
            .as_ref()
            .map(Receiver::try_recv);
        match template {
            Some(Ok((index, Ok(workspace)))) if self.loading_template_engine == Some(index) => {
                self.template_workspace_receiver = None;
                self.loading_template_engine = None;
                self.template_workspace = Some(workspace);
                self.rebuild_plan(false);
            }
            Some(Ok((index, Err(error)))) if self.loading_template_engine == Some(index) => {
                self.template_workspace_receiver = None;
                self.loading_template_engine = None;
                self.set_error(format!(
                    "Template data load failed: {error}. Check the installation and refresh."
                ));
            }
            Some(Ok(_)) => {
                self.template_workspace_receiver = None;
            }
            Some(Err(TryRecvError::Disconnected)) => {
                self.template_workspace_receiver = None;
                self.loading_template_engine = None;
                self.set_error(
                    "Template loading stopped before returning data. Select the engine to retry.",
                );
            }
            Some(Err(TryRecvError::Empty)) | None => {}
        }
    }

    fn poll_plan(&mut self) {
        let plan = self.plan_receiver.as_ref().map(Receiver::try_recv);
        match plan {
            Some(Ok((generation, _, Ok(BuiltPlan::Engine(plan, elevation)))))
                if generation == self.plan_generation =>
            {
                self.plan_receiver = None;
                self.plan_requires_elevation = elevation;
                self.plan = Some(*plan);
                self.project_plan = None;
                self.template_plan = None;
            }
            Some(Ok((generation, _, Ok(BuiltPlan::Project(plan)))))
                if generation == self.plan_generation =>
            {
                self.plan_receiver = None;
                self.plan_requires_elevation = None;
                self.plan = None;
                self.project_plan = Some(*plan);
                self.template_plan = None;
            }
            Some(Ok((generation, _, Ok(BuiltPlan::Template(plan, elevation)))))
                if generation == self.plan_generation =>
            {
                self.plan_receiver = None;
                self.plan_requires_elevation = elevation;
                self.plan = None;
                self.project_plan = None;
                self.template_plan = Some(*plan);
            }
            Some(Ok((generation, report_error, Err(error))))
                if generation == self.plan_generation =>
            {
                self.plan_receiver = None;
                self.plan = None;
                self.project_plan = None;
                self.template_plan = None;
                self.plan_requires_elevation = None;
                if report_error {
                    self.set_error(format!(
                        "Review build failed: {error}. Check the preset and selected target files."
                    ));
                }
            }
            Some(Ok(_)) => {
                self.plan_receiver = None;
            }
            Some(Err(TryRecvError::Disconnected)) => {
                self.plan_receiver = None;
                self.plan = None;
                self.project_plan = None;
                self.template_plan = None;
                self.plan_requires_elevation = None;
                self.set_error(
                    "Plan building stopped before returning data. Edit the preset or refresh the selected target.",
                );
            }
            Some(Err(TryRecvError::Empty)) | None => {}
        }
    }

    fn poll_multi_plan(&mut self) {
        let result = self.multi_plan_receiver.as_ref().map(Receiver::try_recv);
        match result {
            Some(Ok((generation, Ok(plans)))) if generation == self.multi_plan_generation => {
                self.multi_plan_receiver = None;
                self.multi_engine_plans = plans;
                self.view = View::MultiEngineReview;
            }
            Some(Ok((generation, Err(error)))) if generation == self.multi_plan_generation => {
                self.multi_plan_receiver = None;
                self.multi_engine_plans.clear();
                self.set_error(error.to_string());
            }
            Some(Ok(_)) => {
                self.multi_plan_receiver = None;
            }
            Some(Err(TryRecvError::Disconnected)) => {
                self.multi_plan_receiver = None;
                self.multi_engine_plans.clear();
                self.set_error(
                    "Multi-engine planning stopped before returning data. Build the review again.",
                );
            }
            Some(Err(TryRecvError::Empty)) | None => {}
        }
    }

    fn rebuild_plan(&mut self, report_error: bool) -> bool {
        self.plan_generation = self.plan_generation.wrapping_add(1);
        self.plan_receiver = None;
        if self.target_mode == TargetMode::Template {
            return self.rebuild_template_plan(report_error);
        }
        let document = match self.preset_draft.document(&self.preset_document) {
            Ok(document) => document,
            Err(error) => {
                self.plan = None;
                self.project_plan = None;
                self.template_plan = None;
                self.plan_requires_elevation = None;
                if report_error {
                    self.set_error(format!(
                        "Preset rules are invalid: {error}. Correct the preset before review."
                    ));
                }
                return false;
            }
        };
        self.preset_document = document;
        let preset_path = self.plan_preset_path();
        let document = self.preset_document.clone();
        let generation = self.plan_generation;
        let (sender, receiver) = mpsc::channel();
        match self.target_mode {
            TargetMode::Engine => {
                let Some(engine) = self.selected_engine().cloned() else {
                    self.plan = None;
                    self.project_plan = None;
                    return true;
                };
                std::thread::spawn(move || {
                    let result =
                        build_engine_review(&engine, &preset_path, &document).map(|plan| {
                            let elevation = engine_review_requires_elevation(&plan).ok();
                            BuiltPlan::Engine(Box::new(plan), elevation)
                        });
                    let _ = sender.send((generation, report_error, result));
                });
            }
            TargetMode::Project => {
                let Some(project_path) = self.project_path.clone() else {
                    self.plan = None;
                    self.project_plan = None;
                    return true;
                };
                let engines = self.engines.clone();
                let suppression = self.project_suppression;
                std::thread::spawn(move || {
                    let result = build_project_review(
                        &project_path,
                        &engines,
                        &preset_path,
                        &document,
                        suppression,
                    )
                    .map(|plan| BuiltPlan::Project(Box::new(plan)));
                    let _ = sender.send((generation, report_error, result));
                });
            }
            TargetMode::Template => unreachable!("template planning uses its dedicated branch"),
        }
        self.plan = None;
        self.project_plan = None;
        self.template_plan = None;
        self.plan_requires_elevation = None;
        self.plan_receiver = Some(receiver);
        true
    }

    fn rebuild_template_plan(&mut self, report_error: bool) -> bool {
        let Some(engine) = self.selected_engine().cloned() else {
            self.template_plan = None;
            self.plan_requires_elevation = None;
            return true;
        };
        if self.selected_templates.is_empty() {
            self.template_plan = None;
            self.plan_requires_elevation = None;
            return true;
        }
        let selected = self.selected_templates.iter().cloned().collect::<Vec<_>>();
        let suppression = self.template_suppression;
        let generation = self.plan_generation;
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let result = build_template_review(&engine, &selected, suppression).map(|plan| {
                let elevation = template_review_requires_elevation(&plan).ok();
                BuiltPlan::Template(Box::new(plan), elevation)
            });
            let _ = sender.send((generation, report_error, result));
        });
        self.plan = None;
        self.project_plan = None;
        self.template_plan = None;
        self.plan_requires_elevation = None;
        self.plan_receiver = Some(receiver);
        true
    }

    fn show_multi_engine_selection(&mut self) {
        self.selected_multi_engines.clear();
        if let Some(index) = self.selected_engine {
            self.selected_multi_engines.insert(index);
        }
        self.multi_engine_plans.clear();
        self.multi_plan_receiver = None;
        self.multi_plan_generation = self.multi_plan_generation.wrapping_add(1);
        self.view = View::MultiEngineSelection;
    }

    fn build_multi_engine_plan(&mut self) {
        if self.selected_multi_engines.is_empty() {
            self.set_notice(NoticeKind::Warning, "Select at least one usable engine.");
            return;
        }
        let document = match self.preset_draft.document(&self.preset_document) {
            Ok(document) => document,
            Err(error) => {
                self.set_error(format!(
                    "Multi-engine review build failed: {error}. Correct the preset and retry."
                ));
                return;
            }
        };
        self.preset_document = document;
        let engines = self
            .selected_multi_engines
            .iter()
            .filter_map(|index| self.engines.get(*index).cloned())
            .collect::<Vec<_>>();
        let preset_path = self.plan_preset_path();
        let document = self.preset_document.clone();
        self.multi_plan_generation = self.multi_plan_generation.wrapping_add(1);
        let generation = self.multi_plan_generation;
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let result = build_multi_engine_review(&engines, &preset_path, &document);
            let _ = sender.send((generation, result));
        });
        self.multi_engine_plans.clear();
        self.multi_plan_receiver = Some(receiver);
    }

    fn begin_multi_engine_apply(&mut self) {
        if self.multi_engine_plans.is_empty() {
            self.set_error(
                "No multi-engine review is available. Select engines and build the review.",
            );
            return;
        }
        match active_multi_engine_processes(&self.multi_engine_plans) {
            Ok(processes) if processes.is_empty() => self.execute_multi_engine_apply(),
            Ok(processes) => {
                self.pending_write = Some(PendingWrite::MultiEngineApply {
                    plans: self.multi_engine_plans.clone(),
                    processes,
                });
            }
            Err(error) => self.set_error(format!(
                "Active Unreal process check failed: {error}. Close Unreal applications and retry."
            )),
        }
    }

    fn execute_multi_engine_apply(&mut self) {
        let plans = self.multi_engine_plans.clone();
        let mut completed = 0usize;
        let mut files_written = 0usize;
        for reviewed in &plans {
            match execute_engine_review(&reviewed.plan) {
                Ok(report) => {
                    completed += 1;
                    files_written += report.files_written;
                }
                Err(error) => {
                    self.pending_write = None;
                    self.view = View::MultiEngineReview;
                    let version = reviewed
                        .plan
                        .engine()
                        .version
                        .as_deref()
                        .unwrap_or("unknown");
                    self.set_error(format!(
                        "Multi-engine apply stopped at UE {version}: {error}. {completed} earlier engine operations remain recorded."
                    ));
                    return;
                }
            }
        }
        self.pending_write = None;
        self.view = View::Workspace;
        let selected_engine = self.selected_engine;
        if let Some(index) = selected_engine {
            self.select_engine(index);
        }
        self.set_notice(
            NoticeKind::Success,
            format!(
                "Applied preset to {completed} engines and wrote {files_written} descriptor files."
            ),
        );
    }

    fn plan_preset_path(&self) -> PathBuf {
        if let Some(path) = &self.preset_path {
            return path.clone();
        }
        self.preset_catalog
            .as_ref()
            .map_or_else(
                || std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
                |catalog| catalog.directory().to_path_buf(),
            )
            .join("Unsaved.toml")
    }

    fn refresh_preset_catalog(&mut self) -> unclean_core::Result<()> {
        self.preset_files = self
            .preset_catalog
            .as_ref()
            .map_or_else(|| list_available_presets(None), PresetCatalog::list)?;
        Ok(())
    }

    fn activate_preset(&mut self, path: PathBuf, document: PresetDocument, async_plan: bool) {
        self.preset_draft = PresetDraft::from_document(&document);
        self.preset_document = document;
        self.preset_path = Some(path);
        self.rebuild_plan(async_plan);
    }

    fn load_preset_path(&mut self, path: &Path) {
        let selector = path.to_string_lossy();
        match load_preset(&selector, None) {
            Ok((path, document)) => {
                self.activate_preset(path, document, true);
                self.set_notice(NoticeKind::Success, "Loaded and validated preset.");
            }
            Err(error) => self.set_error(format!("Preset load failed: {error}")),
        }
    }

    fn save_current_preset(&mut self) {
        match self.save_to_preset_catalog() {
            Ok(_) => self.set_notice(
                NoticeKind::Success,
                "Saved preset in the app preset folder.",
            ),
            Err(error) => self.set_error(error),
        }
    }

    fn export_current_preset(&mut self) {
        let mut dialog = rfd::FileDialog::new()
            .set_title("Export preset")
            .add_filter("TOML preset", &["toml"])
            .set_file_name(format!(
                "{}.toml",
                preset_filename_stem(&self.preset_draft.name)
            ));
        if let Some(catalog) = &self.preset_catalog {
            dialog = dialog.set_directory(catalog.directory());
        }
        let Some(export_path) = dialog.save_file() else {
            return;
        };
        let result = self.save_to_preset_catalog().and_then(|saved| {
            saved
                .catalog
                .export(&saved.path, &export_path, &saved.document)
                .map_err(|error| {
                    format!(
                        "Unclean saved the preset in the app preset folder, but export failed: {error}. Choose another export folder and retry."
                    )
                })
        });
        match result {
            Ok(true) => self.set_notice(
                NoticeKind::Success,
                "Saved preset in the app preset folder and exported a TOML copy.",
            ),
            Ok(false) => self.set_notice(
                NoticeKind::Success,
                "Saved preset in the app preset folder.",
            ),
            Err(error) => self.set_error(error),
        }
    }

    fn save_to_preset_catalog(&mut self) -> std::result::Result<SavedPreset, String> {
        let document = self
            .preset_draft
            .document(&self.preset_document)
            .map_err(|error| {
                format!("Preset save failed: {error}. Correct the rules and retry.")
            })?;
        let catalog = self.preset_catalog.clone().ok_or_else(|| {
            "Preset save failed: Unclean cannot find the app preset folder. Restart Unclean from a Windows user session and retry."
                .to_owned()
        })?;
        let managed_path = catalog
            .save(self.preset_path.as_deref(), &document)
            .map_err(|error| format!("Preset save failed: {error}"))?;

        self.activate_preset(managed_path.clone(), document.clone(), false);
        self.refresh_preset_catalog().map_err(|error| {
            format!(
                "Unclean saved the preset, but list refresh failed: {error}. Restart Unclean to reload the list."
            )
        })?;
        Ok(SavedPreset {
            catalog,
            path: managed_path,
            document,
        })
    }

    fn new_preset(&mut self) {
        let backup = PresetEditorBackup {
            path: self.preset_path.clone(),
            document: self.preset_document.clone(),
            draft: self.preset_draft.clone(),
        };
        match PresetDocument::new("New preset") {
            Ok(document) => {
                self.preset_editor_backup = Some(backup);
                self.preset_draft = PresetDraft::from_document(&document);
                self.preset_document = document;
                self.preset_path = None;
                self.rebuild_plan(false);
                self.view = View::PresetEditor;
            }
            Err(error) => self.set_error(format!("Preset creation failed: {error}")),
        }
    }

    fn show_preset_editor(&mut self) {
        self.preset_editor_backup = Some(PresetEditorBackup {
            path: self.preset_path.clone(),
            document: self.preset_document.clone(),
            draft: self.preset_draft.clone(),
        });
        self.view = View::PresetEditor;
    }

    fn cancel_preset_editor(&mut self) {
        if let Some(backup) = self.preset_editor_backup.take() {
            self.preset_path = backup.path;
            self.preset_document = backup.document;
            self.preset_draft = backup.draft;
            self.rebuild_plan(false);
        }
        self.view = View::Workspace;
    }

    fn import_preset(&mut self) {
        let mut dialog = rfd::FileDialog::new()
            .set_title("Import preset")
            .add_filter("TOML preset", &["toml"]);
        if let Some(catalog) = &self.preset_catalog {
            dialog = dialog.set_directory(catalog.directory());
        }
        if let Some(path) = dialog.pick_file() {
            self.import_preset_path(&path);
        }
    }

    fn import_preset_path(&mut self, path: &Path) {
        let Some(catalog) = self.preset_catalog.as_ref() else {
            self.set_error(
                "Preset import failed: Unclean cannot find the app preset folder. Restart Unclean from a Windows user session and retry.",
            );
            return;
        };
        let (managed_path, document) = match catalog.import(path) {
            Ok(preset) => preset,
            Err(error) => {
                self.set_error(format!("Preset import failed: {error}"));
                return;
            }
        };

        self.activate_preset(managed_path, document, true);
        if let Err(error) = self.refresh_preset_catalog() {
            self.set_error(format!(
                "Unclean imported the preset, but list refresh failed: {error}. Restart Unclean to reload the list."
            ));
            return;
        }
        self.set_notice(
            NoticeKind::Success,
            "Imported preset into the app preset folder.",
        );
    }

    fn cycle_plugin(&mut self, plugin: &str, current: DeclaredPluginState) {
        let next = match current {
            DeclaredPluginState::Enabled => DeclaredPluginState::Disabled,
            DeclaredPluginState::Disabled => DeclaredPluginState::Unspecified,
            DeclaredPluginState::Unspecified => DeclaredPluginState::Enabled,
        };
        self.preset_draft.set_plugin(plugin, next);
        self.rebuild_plan(true);
    }

    fn begin_apply(&mut self) {
        let Some(plan) = self.plan.clone() else {
            self.set_error(
                "No valid plan is available. Select an engine and correct the preset before review.",
            );
            return;
        };
        match active_engine_processes(plan.engine()) {
            Ok(processes) if processes.is_empty() => self.execute_apply(&plan),
            Ok(processes) => {
                self.pending_write = Some(PendingWrite::Apply {
                    plan: Box::new(plan),
                    processes,
                });
            }
            Err(error) => self.set_error(format!(
                "Active Unreal process check failed: {error}. Close Unreal applications and retry."
            )),
        }
    }

    fn execute_apply(&mut self, plan: &EnginePlan) {
        match execute_engine_review(plan) {
            Ok(report) => {
                let count = report.files_written;
                self.pending_write = None;
                self.view = View::Workspace;
                if let Some(index) = self.selected_engine {
                    self.select_engine(index);
                }
                self.set_notice(
                    NoticeKind::Success,
                    format!("Applied preset and wrote {count} descriptor files."),
                );
            }
            Err(error) => self.set_error(format!(
                "Preset apply failed: {error}. Review the recovery path and retry."
            )),
        }
    }

    fn begin_project_apply(&mut self) {
        let Some(plan) = self.project_plan.clone() else {
            self.set_error(
                "No valid project plan is available. Open a project and correct the preset before review.",
            );
            return;
        };
        match active_engine_processes(plan.engine()) {
            Ok(processes) if processes.is_empty() => self.execute_project_apply(&plan),
            Ok(processes) => {
                self.pending_write = Some(PendingWrite::ProjectApply {
                    plan: Box::new(plan),
                    processes,
                });
            }
            Err(error) => self.set_error(format!(
                "Active Unreal process check failed: {error}. Close Unreal applications and retry."
            )),
        }
    }

    fn execute_project_apply(&mut self, plan: &ProjectPlan) {
        match execute_project_review(plan) {
            Ok(report) => {
                self.pending_write = None;
                self.view = View::Workspace;
                let project_path = plan.project_path().to_path_buf();
                self.select_project_path(&project_path);
                self.set_notice(
                    NoticeKind::Success,
                    if report.recorded {
                        "Project plugin overrides applied."
                    } else {
                        "The project already matches the reviewed plan."
                    },
                );
            }
            Err(error) => self.set_error(format!(
                "Project apply failed: {error}. Review the recovery path and retry."
            )),
        }
    }

    fn begin_template_apply(&mut self) {
        let Some(plan) = self.template_plan.clone() else {
            self.set_error(
                "No valid template plan is available. Select templates and wait for review planning.",
            );
            return;
        };
        match active_engine_processes(plan.engine()) {
            Ok(processes) if processes.is_empty() => self.execute_template_apply(&plan),
            Ok(processes) => {
                self.pending_write = Some(PendingWrite::TemplateApply {
                    plan: Box::new(plan),
                    processes,
                });
            }
            Err(error) => self.set_error(format!(
                "Active Unreal process check failed: {error}. Close Unreal applications and retry."
            )),
        }
    }

    fn execute_template_apply(&mut self, plan: &TemplatePlan) {
        match execute_template_review(plan) {
            Ok(report) => {
                let count = report.files_written;
                self.pending_write = None;
                self.view = View::Workspace;
                if let Some(index) = self.selected_engine {
                    self.select_template_engine(index);
                }
                self.set_notice(
                    NoticeKind::Success,
                    format!("Applied template settings and wrote {count} template files."),
                );
            }
            Err(error) => self.set_error(format!(
                "Template apply failed: {error}. Review the recovery path and retry."
            )),
        }
    }

    fn review_restore(&mut self, snapshot: &str) {
        let Some(engine) = self.selected_engine().cloned() else {
            return;
        };
        match build_restore_review(&engine, snapshot) {
            Ok(plan) => {
                self.restore_requires_elevation = restore_review_requires_elevation(&plan).ok();
                self.restore_plan = Some(plan);
                self.view = View::RestoreReview;
            }
            Err(error) => self.set_error(format!(
                "Restore review build failed: {error}. Check the snapshot and journal."
            )),
        }
    }

    fn begin_restore(&mut self) {
        let Some(plan) = self.restore_plan.clone() else {
            return;
        };
        match active_engine_processes(plan.engine()) {
            Ok(processes) if processes.is_empty() => self.execute_restore(&plan),
            Ok(processes) => {
                self.pending_write = Some(PendingWrite::Restore {
                    plan: Box::new(plan),
                    processes,
                });
            }
            Err(error) => self.set_error(format!(
                "Active Unreal process check failed: {error}. Close Unreal applications and retry."
            )),
        }
    }

    fn execute_restore(&mut self, plan: &RestorePlan) {
        match execute_restore_review(plan) {
            Ok(report) => {
                let count = report.files_written;
                self.pending_write = None;
                self.view = View::Workspace;
                if let Some(index) = self.selected_engine {
                    self.select_engine(index);
                }
                self.set_notice(
                    NoticeKind::Success,
                    format!("Restored snapshot and wrote {count} descriptor files."),
                );
            }
            Err(error) => self.set_error(format!(
                "Snapshot restore failed: {error}. Review the recovery path and retry."
            )),
        }
    }

    fn review_project_restore(&mut self, snapshot: &str) {
        let Some(project_path) = self.project_path.clone() else {
            return;
        };
        match build_project_restore_review(&project_path, &self.engines, snapshot) {
            Ok(plan) => {
                self.project_restore_plan = Some(plan);
                self.view = View::ProjectRestoreReview;
            }
            Err(error) => self.set_error(format!(
                "Project restore review build failed: {error}. Check the snapshot and journal."
            )),
        }
    }

    fn begin_project_restore(&mut self) {
        let Some(plan) = self.project_restore_plan.clone() else {
            return;
        };
        match active_engine_processes(plan.engine()) {
            Ok(processes) if processes.is_empty() => self.execute_project_restore(&plan),
            Ok(processes) => {
                self.pending_write = Some(PendingWrite::ProjectRestore {
                    plan: Box::new(plan),
                    processes,
                });
            }
            Err(error) => self.set_error(format!(
                "Active Unreal process check failed: {error}. Close Unreal applications and retry."
            )),
        }
    }

    fn execute_project_restore(&mut self, plan: &ProjectRestorePlan) {
        match execute_project_restore_review(plan) {
            Ok(report) => {
                self.pending_write = None;
                self.view = View::Workspace;
                let project_path = plan.project_path().to_path_buf();
                self.select_project_path(&project_path);
                self.set_notice(
                    NoticeKind::Success,
                    if report.recorded {
                        "Project snapshot restored."
                    } else {
                        "The project already matches the selected snapshot."
                    },
                );
            }
            Err(error) => self.set_error(format!(
                "Project snapshot restore failed: {error}. Review the recovery path and retry."
            )),
        }
    }

    fn review_template_restore(&mut self, snapshot: &str) {
        let Some(engine) = self.selected_engine().cloned() else {
            return;
        };
        match build_template_restore_review(&engine, snapshot) {
            Ok(plan) => {
                self.restore_requires_elevation = template_restore_requires_elevation(&plan).ok();
                self.template_restore_plan = Some(plan);
                self.view = View::TemplateRestoreReview;
            }
            Err(error) => self.set_error(format!(
                "Template restore review build failed: {error}. Check the snapshot and journal."
            )),
        }
    }

    fn begin_template_restore(&mut self) {
        let Some(plan) = self.template_restore_plan.clone() else {
            return;
        };
        match active_engine_processes(plan.engine()) {
            Ok(processes) if processes.is_empty() => self.execute_template_restore(&plan),
            Ok(processes) => {
                self.pending_write = Some(PendingWrite::TemplateRestore {
                    plan: Box::new(plan),
                    processes,
                });
            }
            Err(error) => self.set_error(format!(
                "Active Unreal process check failed: {error}. Close Unreal applications and retry."
            )),
        }
    }

    fn execute_template_restore(&mut self, plan: &TemplateRestorePlan) {
        match execute_template_restore_review(plan) {
            Ok(report) => {
                let count = report.files_written;
                self.pending_write = None;
                self.view = View::Workspace;
                if let Some(index) = self.selected_engine {
                    self.select_template_engine(index);
                }
                self.set_notice(
                    NoticeKind::Success,
                    format!("Restored template snapshot and wrote {count} template files."),
                );
            }
            Err(error) => self.set_error(format!(
                "Template snapshot restore failed: {error}. Review the recovery path and retry."
            )),
        }
    }

    fn confirm_pending_write(&mut self) {
        let pending = self.pending_write.clone();
        match pending {
            Some(PendingWrite::Apply { plan, .. }) => self.execute_apply(&plan),
            Some(PendingWrite::Restore { plan, .. }) => self.execute_restore(&plan),
            Some(PendingWrite::ProjectApply { plan, .. }) => self.execute_project_apply(&plan),
            Some(PendingWrite::ProjectRestore { plan, .. }) => {
                self.execute_project_restore(&plan);
            }
            Some(PendingWrite::TemplateApply { plan, .. }) => {
                self.execute_template_apply(&plan);
            }
            Some(PendingWrite::TemplateRestore { plan, .. }) => {
                self.execute_template_restore(&plan);
            }
            Some(PendingWrite::MultiEngineApply { plans, .. }) => {
                self.multi_engine_plans = plans;
                self.execute_multi_engine_apply();
            }
            None => {}
        }
    }

    fn set_error(&mut self, text: impl Into<String>) {
        self.set_notice(NoticeKind::Error, text);
    }

    fn set_notice(&mut self, kind: NoticeKind, text: impl Into<String>) {
        self.notice = Some(Notice {
            kind,
            text: text.into(),
        });
    }

    fn shortcut_action(&self, context: &Context) -> Option<Action> {
        context.input(|input| {
            if self.view == View::PresetEditor {
                input
                    .key_pressed(Key::Escape)
                    .then_some(Action::CancelPresetEditor)
            } else if input.modifiers.command && input.key_pressed(Key::R) {
                Some(Action::Refresh)
            } else if self.target_mode != TargetMode::Template
                && input.modifiers.command
                && input.key_pressed(Key::O)
            {
                Some(Action::ImportPreset)
            } else if self.target_mode != TargetMode::Template
                && input.modifiers.command
                && input.key_pressed(Key::S)
            {
                Some(Action::SavePreset)
            } else if input.key_pressed(Key::Escape) && self.view != View::Workspace {
                Some(if self.view == View::MultiEngineReview {
                    Action::BackToMultiEngineSelection
                } else {
                    Action::Back
                })
            } else {
                None
            }
        })
    }

    fn render_top_bar(&mut self, root: &mut Ui) -> Option<Action> {
        let mut action = None;
        egui::Panel::top("toolbar")
            .exact_size(36.0)
            .frame(
                Frame::new()
                    .fill(theme::HEADER)
                    .stroke(Stroke::new(1.0, theme::RECESSED))
                    .inner_margin(Margin::symmetric(8, 6)),
            )
            .show(root, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new("UNCLEAN")
                            .strong()
                            .color(theme::FOREGROUND_HEADER),
                    );
                    ui.separator();
                    if self.view == View::PresetEditor {
                        ui.label(
                            RichText::new("Editing preset")
                                .strong()
                                .color(theme::ACCENT),
                        );
                    } else if self.target_mode == TargetMode::Template {
                        ui.label(RichText::new("Template field").small());
                        ui.label(
                            RichText::new("DisableEnginePluginsByDefault")
                                .monospace()
                                .color(theme::ACCENT),
                        );
                    } else {
                        action = self.render_preset_controls(ui);
                    }
                    if self.view != View::PresetEditor {
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            action = self.render_target_actions(ui).or(action.take());
                        });
                    }
                });
            });
        action
    }

    fn render_preset_controls(&self, ui: &mut Ui) -> Option<Action> {
        let mut action = None;
        ui.label(RichText::new("Preset").small());
        let selected_name = self.preset_document.preset().name.clone();
        egui::ComboBox::from_id_salt("preset_selector")
            .selected_text(selected_name)
            .width(190.0)
            .show_ui(ui, |ui| {
                for preset in &self.preset_files {
                    if ui
                        .selectable_label(
                            self.preset_path.as_ref() == Some(&preset.path),
                            &preset.name,
                        )
                        .clicked()
                    {
                        action = Some(Action::LoadPreset(preset.path.clone()));
                    }
                }
            });
        if ui.button("New").on_hover_text("Create a preset.").clicked() {
            action = Some(Action::NewPreset);
        }
        if ui
            .button("Import")
            .on_hover_text("Validate a TOML preset and add it to the app preset folder. Ctrl+O")
            .clicked()
        {
            action = Some(Action::ImportPreset);
        }
        if ui
            .button("Edit")
            .on_hover_text("Edit the current preset rules.")
            .clicked()
        {
            action = Some(Action::ShowPresetEditor);
        }
        if ui
            .button("Save")
            .on_hover_text("Validate and save the current preset in the app preset folder. Ctrl+S")
            .clicked()
        {
            action = Some(Action::SavePreset);
        }
        if ui
            .button("Export")
            .on_hover_text("Save the preset in the app preset folder and export a TOML copy.")
            .clicked()
        {
            action = Some(Action::ExportPreset);
        }
        action
    }

    fn render_target_actions(&self, ui: &mut Ui) -> Option<Action> {
        if ui
            .button("Refresh")
            .on_hover_text("Refresh discovery and selected target data. Ctrl+R")
            .clicked()
        {
            return Some(Action::Refresh);
        }
        if ui.button("History").clicked() {
            return Some(Action::ShowHistory);
        }
        if self.target_mode == TargetMode::Engine
            && ui
                .button("Apply to engines")
                .on_hover_text("Build one reviewed preset plan for each selected engine.")
                .clicked()
        {
            return Some(Action::ShowMultiEngineSelection);
        }
        if ui
            .button("Open project")
            .on_hover_text("Open a .uproject file and resolve its associated engine.")
            .clicked()
        {
            return Some(Action::OpenProject);
        }
        if self.target_mode != TargetMode::Template
            && ui
                .button("Engine templates")
                .on_hover_text("Configure defaults used by projects created from engine templates.")
                .clicked()
        {
            return Some(Action::UseTemplateTarget);
        }
        if self.target_mode != TargetMode::Engine
            && ui
                .button("Engine target")
                .on_hover_text("Return to engine-level plugin defaults.")
                .clicked()
        {
            return Some(Action::UseEngineTarget);
        }
        if self.target_mode == TargetMode::Project {
            let project_name = self
                .project_path
                .as_deref()
                .and_then(Path::file_name)
                .and_then(|name| name.to_str())
                .unwrap_or("Project");
            ui.label(RichText::new(project_name).color(theme::ACCENT).strong());
        } else if self.target_mode == TargetMode::Template {
            ui.label(
                RichText::new("New-project templates")
                    .color(theme::ACCENT)
                    .strong(),
            );
        }
        None
    }

    fn render_engine_rail(&mut self, root: &mut Ui) -> Option<Action> {
        let mut action = None;
        resizable_rail(root, "engine_rail", ENGINE_RAIL_DEFAULT_WIDTH, 8).show(root, |ui| {
            ui.set_min_width(ui.available_width());
            section_heading(ui, "ENGINE INSTALLATIONS");
            ui.add_space(2.0);
            if self.discovery_receiver.is_some() {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Discovering engine installations");
                });
                ui.add_space(4.0);
            }
            ScrollArea::vertical().show(ui, |ui| {
                action = self.render_engine_entries(ui).or_else(|| action.take());
            });
            ui.with_layout(Layout::bottom_up(Align::LEFT), |ui| {
                action = self.render_engine_footer(ui).or_else(|| action.take());
            });
        });
        action
    }

    fn render_engine_entries(&self, ui: &mut Ui) -> Option<Action> {
        let mut action = None;
        for (index, engine) in self.engines.iter().enumerate() {
            let selected = self.selected_engine == Some(index);
            let version = engine.version.as_deref().unwrap_or("Unknown version");
            let response = ui.add_sized(
                [ui.available_width(), 31.0],
                Button::new(
                    RichText::new(format!("UE {version}")).color(health_color(engine.health)),
                )
                .selected(selected),
            );
            response.clone().on_hover_ui(|ui| {
                ui.label(
                    RichText::new(engine.path.display().to_string())
                        .monospace()
                        .small(),
                );
                for issue in &engine.issues {
                    ui.colored_label(theme::WARNING, &issue.message);
                }
            });
            if response.clicked() {
                action = Some(Action::SelectEngine(index));
            }
            ui.label(
                RichText::new(format!(
                    "{} descriptors  {}",
                    engine.descriptor_count,
                    engine.health.as_str()
                ))
                .small()
                .weak(),
            );
            ui.add_space(4.0);
        }
        if !self.discovery_warnings.is_empty() {
            egui::CollapsingHeader::new(format!(
                "Discovery warnings ({})",
                self.discovery_warnings.len()
            ))
            .show(ui, |ui| {
                for warning in &self.discovery_warnings {
                    ui.colored_label(theme::WARNING, warning);
                }
            });
        }
        action
    }

    fn render_engine_footer(&self, ui: &mut Ui) -> Option<Action> {
        let action = ui
            .add_sized(
                [ui.available_width(), 24.0],
                Button::new("Add engine folder"),
            )
            .clicked()
            .then_some(Action::AddEngine);
        if let Some(workspace) = &self.workspace {
            ui.separator();
            let effective = workspace
                .plugins
                .iter()
                .filter(|plugin| plugin.effective_enabled == Some(true))
                .count();
            let changed = self.plan.as_ref().map_or(0, |plan| plan.changes().len());
            property_row(ui, "Effective", &effective.to_string());
            property_row(ui, "Planned edits", &changed.to_string());
            property_row(
                ui,
                "Drift",
                if workspace.status.drifted {
                    "Detected"
                } else {
                    "None"
                },
            );
        } else if let Some(workspace) = &self.template_workspace {
            ui.separator();
            property_row(
                ui,
                "Templates",
                &workspace.catalog.templates.len().to_string(),
            );
            property_row(ui, "Selected", &self.selected_templates.len().to_string());
            let changed = self
                .template_plan
                .as_ref()
                .map_or(0, |plan| plan.changes().len());
            property_row(ui, "Planned edits", &changed.to_string());
            property_row(
                ui,
                "Drift",
                if workspace.status.drifted {
                    "Detected"
                } else {
                    "None"
                },
            );
        }
        action
    }

    fn render_workspace(&mut self, root: &mut Ui) -> Option<Action> {
        match self.target_mode {
            TargetMode::Engine => self.render_engine_workspace(root),
            TargetMode::Project => self.render_project_workspace(root),
            TargetMode::Template => self.render_template_workspace(root),
        }
    }

    fn render_template_workspace(&mut self, root: &mut Ui) -> Option<Action> {
        let mut action = self.render_engine_rail(root);
        egui::CentralPanel::default()
            .frame(
                Frame::new()
                    .fill(theme::TITLE)
                    .inner_margin(Margin::same(10)),
            )
            .show(root, |ui| {
                sheet_title(
                    ui,
                    "New-Project Templates",
                    "Choose templates explicitly. Changes affect projects created later, and engine updates may replace template files.",
                );
                ui.add_space(8.0);
                action = self.render_template_toolbar(ui).or(action.take());
                ui.add_space(8.0);
                let Some(workspace) = &self.template_workspace else {
                    if self.template_workspace_receiver.is_some() {
                        empty_state(
                            ui,
                            "Loading engine templates",
                            "Template descriptors, history, and current drift are being read.",
                        );
                    } else {
                        empty_state(
                            ui,
                            "No engine selected",
                            "Select an Unreal Engine installation to inspect its templates.",
                        );
                    }
                    return;
                };
                action = self.render_template_catalog(ui, workspace).or(action.take());
            });
        action
    }

    fn render_template_toolbar(&self, ui: &mut Ui) -> Option<Action> {
        let mut action = None;
        ui.horizontal(|ui| {
            section_heading(ui, "SUPPRESS ENGINE PLUGINS BY DEFAULT");
            for (label, suppression) in [
                ("Enabled", ProjectSuppressionEdit::Set(true)),
                ("Disabled", ProjectSuppressionEdit::Set(false)),
                ("Clear", ProjectSuppressionEdit::Clear),
            ] {
                if ui
                    .selectable_label(self.template_suppression == suppression, label)
                    .on_hover_text(template_suppression_tooltip(suppression))
                    .clicked()
                {
                    action = Some(Action::SetTemplateSuppression(suppression));
                }
            }
            ui.separator();
            if ui.button("Select all").clicked() {
                action = Some(Action::SelectAllTemplates);
            }
            if ui.button("Clear selection").clicked() {
                action = Some(Action::ClearTemplateSelection);
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let changes = self
                    .template_plan
                    .as_ref()
                    .map_or(0, |plan| plan.changes().len());
                let label = if self.plan_receiver.is_some() {
                    "Building template plan".to_owned()
                } else {
                    format!("Review {changes} template changes")
                };
                if ui
                    .add_enabled(
                        self.template_plan.is_some(),
                        Button::new(label).fill(theme::PRIMARY),
                    )
                    .clicked()
                {
                    action = Some(Action::ReviewApply);
                }
            });
        });
        action
    }

    fn render_template_catalog(
        &self,
        ui: &mut Ui,
        workspace: &TemplateWorkspaceView,
    ) -> Option<Action> {
        let mut action = None;
        if workspace.catalog.templates.is_empty() {
            empty_state(
                ui,
                "No templates found",
                "This engine installation does not contain valid .uproject templates.",
            );
        } else {
            egui::Grid::new("template_header")
                .num_columns(5)
                .spacing([18.0, 4.0])
                .show(ui, |ui| {
                    ui.strong("Use");
                    ui.strong("Template");
                    ui.strong("Current");
                    ui.strong("Plugin refs");
                    ui.strong("Descriptor");
                    ui.end_row();
                });
            ui.separator();
            ScrollArea::vertical()
                .id_salt("template_list")
                .show(ui, |ui| {
                    egui::Grid::new("template_rows")
                        .striped(true)
                        .num_columns(5)
                        .spacing([18.0, 5.0])
                        .show(ui, |ui| {
                            for template in &workspace.catalog.templates {
                                let selected =
                                    self.selected_templates.contains(&template.relative_path);
                                if template_selection_button(ui, &template.name, selected).clicked()
                                {
                                    action = Some(Action::ToggleTemplate(
                                        template.relative_path.clone(),
                                    ));
                                }
                                ui.label(RichText::new(&template.name).strong());
                                ui.label(template.suppression.as_str());
                                ui.label(template.plugin_reference_count.to_string());
                                ui.label(
                                    RichText::new(template.relative_path.display().to_string())
                                        .monospace()
                                        .small(),
                                );
                                ui.end_row();
                            }
                        });
                });
        }
        if !workspace.catalog.warnings.is_empty() {
            egui::CollapsingHeader::new(format!(
                "Template warnings ({})",
                workspace.catalog.warnings.len()
            ))
            .show(ui, |ui| {
                for warning in &workspace.catalog.warnings {
                    ui.colored_label(theme::WARNING, &warning.message);
                }
            });
        }
        action
    }

    fn render_engine_workspace(&mut self, root: &mut Ui) -> Option<Action> {
        let mut action = self.render_engine_rail(root);
        resizable_details_panel(root, "plugin_details").show(root, |ui| {
            ScrollArea::vertical()
                .id_salt("plugin_details_scroll")
                .auto_shrink([false, false])
                .show(ui, |ui| self.render_plugin_details(ui));
        });

        egui::CentralPanel::default()
            .frame(
                Frame::new()
                    .fill(theme::TITLE)
                    .inner_margin(Margin::same(8)),
            )
            .show(root, |ui| {
                ui.horizontal(|ui| {
                    let search = ui.add_sized(
                        [280.0, 24.0],
                        TextEdit::singleline(&mut self.search).hint_text("Search plugins"),
                    );
                    search.widget_info(|| {
                        WidgetInfo::labeled(WidgetType::TextEdit, true, "Search plugins")
                    });
                    ui.checkbox(&mut self.only_changed, "Changed")
                        .on_hover_text("Show plugins with planned descriptor edits.");
                    ui.checkbox(&mut self.only_effective, "Effective")
                        .on_hover_text("Show plugins enabled after dependency resolution.");
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let changes = self.plan.as_ref().map_or(0, |plan| plan.changes().len());
                        let review_label = if self.plan_receiver.is_some() {
                            "Building plan".to_owned()
                        } else {
                            format!("Review {changes} changes")
                        };
                        if ui
                            .add_enabled(
                                self.plan.is_some(),
                                Button::new(review_label).fill(theme::PRIMARY),
                            )
                            .clicked()
                        {
                            action = Some(Action::ReviewApply);
                        }
                    });
                });
                ui.add_space(4.0);
                self.render_plugin_table(ui);
            });
        action
    }

    fn render_project_workspace(&mut self, root: &mut Ui) -> Option<Action> {
        let mut action = self.render_project_rail(root);
        resizable_details_panel(root, "project_plugin_details").show(root, |ui| {
            ScrollArea::vertical()
                .id_salt("project_plugin_details_scroll")
                .auto_shrink([false, false])
                .show(ui, |ui| self.render_project_plugin_details(ui));
        });
        egui::CentralPanel::default()
            .frame(
                Frame::new()
                    .fill(theme::TITLE)
                    .inner_margin(Margin::same(8)),
            )
            .show(root, |ui| {
                ui.horizontal(|ui| {
                    let search = ui.add_sized(
                        [280.0, 24.0],
                        TextEdit::singleline(&mut self.search).hint_text("Search project plugins"),
                    );
                    search.widget_info(|| {
                        WidgetInfo::labeled(WidgetType::TextEdit, true, "Search project plugins")
                    });
                    ui.checkbox(&mut self.only_changed, "Changed")
                        .on_hover_text("Show plugins changed by the current project plan.");
                    ui.checkbox(&mut self.only_effective, "Effective")
                        .on_hover_text("Show plugins enabled for this project.");
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let changes = self.project_plan.as_ref().map_or(0, |plan| {
                            plan.plugins()
                                .iter()
                                .filter(|plugin| {
                                    plugin.reference_before != plugin.reference_after
                                        || plugin.effective_before != plugin.effective_after
                                })
                                .count()
                        });
                        let review_label = if self.plan_receiver.is_some() {
                            "Building project plan".to_owned()
                        } else {
                            format!("Review {changes} project changes")
                        };
                        if ui
                            .add_enabled(
                                self.project_plan.is_some(),
                                Button::new(review_label).fill(theme::PRIMARY),
                            )
                            .clicked()
                        {
                            action = Some(Action::ReviewApply);
                        }
                    });
                });
                ui.add_space(4.0);
                self.render_project_plugin_table(ui);
            });
        action
    }

    fn render_project_rail(&mut self, root: &mut Ui) -> Option<Action> {
        let mut action = None;
        let mut suppression = self.project_suppression;
        let current_suppression = self
            .project_workspace
            .as_ref()
            .map(|view| view.workspace.project.suppression);
        resizable_rail(root, "project_rail", PROJECT_RAIL_DEFAULT_WIDTH, 10).show(root, |ui| {
            ui.set_min_width(ui.available_width());
            render_project_target(
                ui,
                self.project_path.as_deref(),
                self.project_workspace_receiver.is_some(),
            );
            if let Some(view) = &self.project_workspace {
                render_project_rail_state(ui, view, &mut suppression);
            }
            action = render_project_rail_footer(ui);
        });
        if suppression != self.project_suppression && current_suppression.is_some() {
            self.project_suppression = suppression;
            self.rebuild_plan(true);
        }
        action
    }

    fn render_project_plugin_table(&mut self, ui: &mut Ui) {
        let Some(view) = &self.project_workspace else {
            if self.loading_project_path.is_some() {
                empty_state(
                    ui,
                    "Loading project data",
                    "The project file, engine association, and plugin state are being read.",
                );
            } else {
                empty_state(
                    ui,
                    "No project loaded",
                    "Open a .uproject file to inspect its plugin overrides.",
                );
            }
            return;
        };
        let filtered = view
            .workspace
            .plugins
            .iter()
            .enumerate()
            .filter(|(_, status)| plugin_matches(&status.plugin, &self.search))
            .filter(|(_, status)| {
                !self.only_changed
                    || project_plan_status(self.project_plan.as_ref(), &status.plugin.name)
                        .is_some_and(|planned| {
                            planned.reference_before != planned.reference_after
                                || planned.effective_before != planned.effective_after
                        })
            })
            .filter(|(_, status)| {
                !self.only_effective
                    || project_plan_status(self.project_plan.as_ref(), &status.plugin.name)
                        .map_or(status.project_effective_enabled, |planned| {
                            planned.effective_after
                        })
            })
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        project_table_header(ui);
        let (selected_plugin, toggle) = render_project_plugin_rows(
            ui,
            view,
            &filtered,
            self.project_plan.as_ref(),
            self.selected_plugin.as_deref(),
        );
        if let Some(plugin) = selected_plugin {
            self.selected_plugin = Some(plugin);
        }
        if let Some((plugin, state)) = toggle {
            self.selected_plugin = Some(plugin.clone());
            self.cycle_plugin(&plugin, state);
        }
    }

    fn render_project_plugin_details(&mut self, ui: &mut Ui) {
        section_heading(ui, "PROJECT PLUGIN DETAILS");
        let Some(view) = &self.project_workspace else {
            ui.label("Open a project to inspect plugin details.");
            return;
        };
        let Some(status) = self.selected_plugin.as_deref().and_then(|name| {
            view.workspace
                .plugins
                .iter()
                .find(|status| status.plugin.name == name)
        }) else {
            ui.label("Select a plugin to inspect its project state.");
            return;
        };
        let planned = project_plan_status(self.project_plan.as_ref(), &status.plugin.name);
        let reference = planned.map_or(status.project_reference, |plugin| plugin.reference_after);
        let project_effective = planned.map_or(status.project_effective_enabled, |plugin| {
            plugin.effective_after
        });
        ui.columns(2, |columns| {
            let left = &mut columns[0];
            left.heading(&status.plugin.friendly_name);
            left.label(RichText::new(&status.plugin.name).monospace().weak());
            left.add_space(4.0);
            property_row(
                left,
                "Engine",
                enabled_label(status.plugin.effective_enabled == Some(true)),
            );
            property_row(left, "Override", project_reference_label(reference));
            property_row(left, "Project", enabled_label(project_effective));
            property_row(
                left,
                "Source",
                planned.map_or(status.project_origin.as_str(), |plugin| {
                    plugin.origin_after.as_str()
                }),
            );
            property_row(left, "Modules", &status.plugin.module_count.to_string());
            let right = &mut columns[1];
            section_heading(right, "PROJECT DEPENDENCY PATH");
            if status.project_effective_path.is_empty() {
                right.label(RichText::new("No active project path.").weak());
            } else {
                right.label(status.project_effective_path.join(" > "));
            }
            right.add_space(8.0);
            section_heading(right, "PROJECT DEPENDENTS");
            if status.project_reached_by.is_empty() {
                right.label(RichText::new("No effective plugin depends on this plugin.").weak());
            } else {
                right.label(status.project_reached_by.join(", "));
            }
        });
    }

    fn render_plugin_table(&mut self, ui: &mut Ui) {
        let Some(workspace) = &self.workspace else {
            if self.loading_workspace.is_some() {
                empty_state(
                    ui,
                    "Loading engine data",
                    "Plugin descriptors and dependency state are being read.",
                );
            } else {
                empty_state(
                    ui,
                    "No engine selected",
                    "Select an Unreal Engine installation or add an engine folder.",
                );
            }
            return;
        };
        let planned = planned_states(self.plan.as_ref());
        let effective = projected_effective_states(&workspace.plugins, self.plan.as_ref());
        let filtered = workspace
            .plugins
            .iter()
            .enumerate()
            .filter(|(_, plugin)| plugin_matches(plugin, &self.search))
            .filter(|(_, plugin)| !self.only_changed || planned.contains_key(&plugin.name))
            .filter(|(_, plugin)| {
                !self.only_effective || effective.get(&plugin.name).copied().unwrap_or(false)
            })
            .map(|(index, _)| index)
            .collect::<Vec<_>>();

        table_header(ui);
        let (selected_plugin, toggle) = render_plugin_rows(
            ui,
            workspace,
            &filtered,
            &planned,
            &effective,
            self.selected_plugin.as_deref(),
        );
        if let Some(plugin) = selected_plugin {
            self.selected_plugin = Some(plugin);
        }
        if let Some((plugin, state)) = toggle {
            self.selected_plugin = Some(plugin.clone());
            self.cycle_plugin(&plugin, state);
        }
    }

    fn render_plugin_details(&mut self, ui: &mut Ui) {
        section_heading(ui, "PLUGIN DETAILS");
        let Some(workspace) = &self.workspace else {
            ui.label("Select an engine to inspect plugin details.");
            return;
        };
        let Some(plugin) = self
            .selected_plugin
            .as_deref()
            .and_then(|name| workspace.plugins.iter().find(|plugin| plugin.name == name))
            .cloned()
        else {
            ui.label("Select a plugin to inspect its dependency details.");
            return;
        };
        let mut linked_plugin = None;
        ui.columns(2, |columns| {
            let left = &mut columns[0];
            left.heading(&plugin.friendly_name);
            left.label(RichText::new(&plugin.name).monospace().weak());
            left.add_space(4.0);
            property_row(left, "Declared", plugin.declared_state.as_str());
            property_row(
                left,
                "Effective",
                if plugin.effective_enabled == Some(true) {
                    "enabled"
                } else {
                    "disabled"
                },
            );
            property_row(
                left,
                "Category",
                plugin.category.as_deref().unwrap_or("Uncategorized"),
            );
            property_row(
                left,
                "Version",
                plugin.version_name.as_deref().unwrap_or("Not specified"),
            );
            property_row(left, "Modules", &plugin.module_count.to_string());
            if let Some(description) = &plugin.description {
                left.add_space(4.0);
                left.label(description);
            }

            let right = &mut columns[1];
            section_heading(right, "DEPENDS ON");
            if plugin.enabled_dependencies.is_empty() {
                right.label(RichText::new("No enabled plugin references.").weak());
            } else {
                right.horizontal_wrapped(|ui| {
                    for dependency in &plugin.enabled_dependencies {
                        if ui.link(&dependency.name).clicked() {
                            linked_plugin = Some(dependency.name.clone());
                        }
                    }
                });
            }
            right.add_space(8.0);
            section_heading(right, "REACHED BY");
            if plugin.reached_by.is_empty() {
                right.label(RichText::new("No effective plugin depends on this plugin.").weak());
            } else {
                right.horizontal_wrapped(|ui| {
                    for parent in &plugin.reached_by {
                        if ui.link(parent).clicked() {
                            linked_plugin = Some(parent.clone());
                        }
                    }
                });
            }
            if !plugin.effective_path.is_empty() {
                right.add_space(8.0);
                section_heading(right, "EFFECTIVE PATH");
                right.label(plugin.effective_path.join(" > "));
            }
        });
        if let Some(plugin) = linked_plugin {
            self.selected_plugin = Some(plugin);
        }
    }

    fn render_multi_engine_selection(&mut self, root: &mut Ui) -> Option<Action> {
        let mut action = None;
        egui::CentralPanel::default()
            .frame(
                Frame::new()
                    .fill(theme::TITLE)
                    .inner_margin(Margin::same(12)),
            )
            .show(root, |ui| {
                sheet_title(
                    ui,
                    "Apply Preset to Engines",
                    "Choose each engine that should receive the current preset. Review builds a separate transaction for every selection.",
                );
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("Select all usable").clicked() {
                        action = Some(Action::SelectAllMultiEngines);
                    }
                    if ui.button("Clear selection").clicked() {
                        action = Some(Action::ClearMultiEngineSelection);
                    }
                    ui.label(format!(
                        "{} selected",
                        self.selected_multi_engines.len()
                    ));
                });
                ui.add_space(8.0);
                section_heading(ui, "ENGINE INSTALLATIONS");
                ScrollArea::vertical()
                    .id_salt("multi_engine_selection")
                    .show(ui, |ui| {
                        for (index, engine) in self.engines.iter().enumerate() {
                            let selected = self.selected_multi_engines.contains(&index);
                            let version = engine.version.as_deref().unwrap_or("Unknown version");
                            let response = ui.add_enabled(
                                engine.health.is_selectable(),
                                Button::new(
                                    RichText::new(format!(
                                        "{}  UE {version}  {}",
                                        if selected { "Selected" } else { "Select" },
                                        engine.health.as_str()
                                    ))
                                    .color(health_color(engine.health)),
                                )
                                .selected(selected)
                                .min_size(Vec2::new(ui.available_width(), 34.0)),
                            );
                            response.clone().on_hover_text(format!(
                                "{}\n{} plugin descriptors",
                                engine.path.display(),
                                engine.descriptor_count
                            ));
                            if response.clicked() {
                                action = Some(Action::ToggleMultiEngine(index));
                            }
                            ui.add_space(4.0);
                        }
                    });
                ui.with_layout(Layout::bottom_up(Align::RIGHT), |ui| {
                    ui.horizontal(|ui| {
                        if ui.button("Back").clicked() {
                            action = Some(Action::Back);
                        }
                        let label = if self.multi_plan_receiver.is_some() {
                            "Building review".to_owned()
                        } else {
                            format!(
                                "Review {} engines",
                                self.selected_multi_engines.len()
                            )
                        };
                        if ui
                            .add_enabled(
                                !self.selected_multi_engines.is_empty()
                                    && self.multi_plan_receiver.is_none(),
                                Button::new(label).fill(theme::PRIMARY),
                            )
                            .clicked()
                        {
                            action = Some(Action::BuildMultiEngineReview);
                        }
                    });
                });
            });
        action
    }

    fn render_multi_engine_review(&mut self, root: &mut Ui) -> Option<Action> {
        let mut action = None;
        let plans = self.multi_engine_plans.clone();
        if plans.is_empty() {
            return Some(Action::BackToMultiEngineSelection);
        }
        egui::CentralPanel::default()
            .frame(
                Frame::new()
                    .fill(theme::TITLE)
                    .inner_margin(Margin::same(12)),
            )
            .show(root, |ui| {
                sheet_title(
                    ui,
                    "Review Multi-Engine Changes",
                    "Each engine keeps its own backup and transaction. Protected engines may request administrator approval one at a time.",
                );
                ui.add_space(8.0);
                let changed_engines = plans
                    .iter()
                    .filter(|reviewed| !reviewed.plan.changes().is_empty())
                    .count();
                let changed_files = plans
                    .iter()
                    .map(|reviewed| reviewed.plan.changes().len())
                    .sum::<usize>();
                property_row(ui, "Selected engines", &plans.len().to_string());
                property_row(ui, "Engines with changes", &changed_engines.to_string());
                property_row(ui, "Descriptor changes", &changed_files.to_string());
                ui.add_space(8.0);
                ScrollArea::vertical()
                    .id_salt("multi_engine_review")
                    .show(ui, |ui| {
                        for reviewed in &plans {
                            let version = reviewed
                                .plan
                                .engine()
                                .version
                                .as_deref()
                                .unwrap_or("Unknown version");
                            ui.push_id(reviewed.plan.operation_id(), |ui| {
                                egui::CollapsingHeader::new(format!(
                                    "UE {version}  {} changes",
                                    reviewed.plan.changes().len()
                                ))
                                .default_open(true)
                                .show(ui, |ui| {
                                    render_apply_summary(
                                        ui,
                                        &reviewed.plan,
                                        reviewed.requires_elevation,
                                    );
                                    review_warnings(ui, &reviewed.plan);
                                    render_apply_changes(ui, &reviewed.plan);
                                    render_plan_resolution(ui, &reviewed.plan);
                                });
                            });
                            ui.add_space(6.0);
                        }
                    });
                ui.with_layout(Layout::bottom_up(Align::RIGHT), |ui| {
                    ui.horizontal(|ui| {
                        if ui.button("Back to engine selection").clicked() {
                            action = Some(Action::BackToMultiEngineSelection);
                        }
                        if ui
                            .add_enabled(
                                changed_files > 0,
                                Button::new(format!("Apply to {} engines", plans.len()))
                                    .fill(theme::PRIMARY),
                            )
                            .clicked()
                        {
                            action = Some(Action::BeginMultiEngineApply);
                        }
                    });
                });
            });
        action
    }

    fn render_preset_editor(&mut self, root: &mut Ui) -> Option<Action> {
        let mut action = None;
        egui::CentralPanel::default()
            .frame(Frame::new().fill(theme::TITLE).inner_margin(Margin::same(12)))
            .show(root, |ui| {
                sheet_title(
                    ui,
                    "Preset Editor",
                    "Edit one plugin name or pattern per line. Validation runs before planning or saving.",
                );
                ui.add_space(8.0);
                egui::Grid::new("preset_identity")
                    .num_columns(2)
                    .spacing([10.0, 6.0])
                    .show(ui, |ui| {
                        ui.label("Name");
                        ui.add(
                            TextEdit::singleline(&mut self.preset_draft.name)
                                .desired_width(f32::INFINITY),
                        );
                        ui.end_row();
                        ui.label("Description");
                        ui.add(
                            TextEdit::singleline(&mut self.preset_draft.description)
                                .desired_width(f32::INFINITY),
                        );
                        ui.end_row();
                    });
                ui.add_space(10.0);
                ui.columns(4, |columns| {
                    rule_editor(&mut columns[0], "ENABLE", &mut self.preset_draft.enable);
                    rule_editor(&mut columns[1], "DISABLE", &mut self.preset_draft.disable);
                    rule_editor(&mut columns[2], "CLEAR", &mut self.preset_draft.clear);
                    rule_editor(
                        &mut columns[3],
                        "DISABLE MATCHING",
                        &mut self.preset_draft.disable_matching,
                    );
                });
                ui.with_layout(Layout::bottom_up(Align::RIGHT), |ui| {
                    ui.horizontal(|ui| {
                        if ui.button("Cancel").clicked() {
                            action = Some(Action::CancelPresetEditor);
                        }
                        if ui
                            .add(Button::new("Validate and use").fill(theme::PRIMARY))
                            .clicked()
                        {
                            action = Some(Action::CommitPresetEditor);
                        }
                    });
                });
            });
        action
    }

    fn render_apply_review(&mut self, root: &mut Ui) -> Option<Action> {
        let mut action = None;
        let Some(plan) = self.plan.clone() else {
            return Some(Action::Back);
        };
        egui::CentralPanel::default()
            .frame(
                Frame::new()
                    .fill(theme::TITLE)
                    .inner_margin(Margin::same(12)),
            )
            .show(root, |ui| {
                sheet_title(
                    ui,
                    "Review Engine Changes",
                    "Apply writes only the reviewed EnabledByDefault fields.",
                );
                render_apply_summary(ui, &plan, self.plan_requires_elevation);
                review_warnings(ui, &plan);
                render_apply_changes(ui, &plan);
                render_plan_resolution(ui, &plan);
                action = render_apply_footer(ui, &plan);
            });
        action
    }

    fn render_project_apply_review(&mut self, root: &mut Ui) -> Option<Action> {
        let mut action = None;
        let Some(plan) = self.project_plan.clone() else {
            return Some(Action::Back);
        };
        egui::CentralPanel::default()
            .frame(
                Frame::new()
                    .fill(theme::TITLE)
                    .inner_margin(Margin::same(12)),
            )
            .show(root, |ui| {
                sheet_title(
                    ui,
                    "Review Project Changes",
                    "This plan edits only DisableEnginePluginsByDefault and explicit Plugins entries in the selected .uproject file.",
                );
                render_project_plan_summary(ui, &plan);
                render_project_review_warnings(ui, &plan);
                render_project_plan_changes(ui, &plan);
                ui.with_layout(Layout::bottom_up(Align::RIGHT), |ui| {
                    ui.horizontal(|ui| {
                        if ui.button("Back").clicked() {
                            action = Some(Action::Back);
                        }
                        if ui
                            .add_enabled(
                                plan.change().is_some(),
                                Button::new("Apply project overrides").fill(theme::PRIMARY),
                            )
                            .clicked()
                        {
                            action = Some(Action::BeginProjectApply);
                        }
                    });
                });
            });
        action
    }

    fn render_template_apply_review(&mut self, root: &mut Ui) -> Option<Action> {
        let mut action = None;
        let Some(plan) = self.template_plan.clone() else {
            return Some(Action::Back);
        };
        egui::CentralPanel::default()
            .frame(
                Frame::new()
                    .fill(theme::TITLE)
                    .inner_margin(Margin::same(12)),
            )
            .show(root, |ui| {
                sheet_title(
                    ui,
                    "Review Template Changes",
                    "This plan edits only DisableEnginePluginsByDefault in the selected template descriptors.",
                );
                ui.add_space(8.0);
                property_row(
                    ui,
                    "Engine",
                    plan.engine().version.as_deref().unwrap_or("Unknown version"),
                );
                property_row(ui, "Selected", &plan.templates().len().to_string());
                property_row(
                    ui,
                    "Suppression",
                    suppression_edit_label(plan.suppression()),
                );
                property_row(
                    ui,
                    "New backup",
                    &plan.backup_directory().display().to_string(),
                );
                property_row(
                    ui,
                    "Writer",
                    elevation_label(self.plan_requires_elevation),
                );
                if !plan.warnings().is_empty() {
                    egui::CollapsingHeader::new(format!(
                        "Template warnings ({})",
                        plan.warnings().len()
                    ))
                    .show(ui, |ui| {
                        for warning in plan.warnings() {
                            ui.colored_label(theme::WARNING, &warning.message);
                        }
                    });
                }
                ui.add_space(8.0);
                section_heading(ui, &format!("DESCRIPTOR CHANGES ({})", plan.changes().len()));
                ScrollArea::vertical().show(ui, |ui| {
                    egui::Grid::new("template_change_grid")
                        .striped(true)
                        .num_columns(4)
                        .spacing([18.0, 5.0])
                        .show(ui, |ui| {
                            ui.strong("Template");
                            ui.strong("Before");
                            ui.strong("After");
                            ui.strong("Descriptor");
                            ui.end_row();
                            for change in plan.changes() {
                                ui.label(RichText::new(&change.template).strong());
                                ui.label(change.suppression_before.as_str());
                                ui.label(
                                    RichText::new(change.suppression_after.as_str())
                                        .color(theme::ACCENT),
                                );
                                ui.label(
                                    RichText::new(change.relative_path.display().to_string())
                                        .monospace()
                                        .small(),
                                )
                                .on_hover_text(format!(
                                    "Current SHA-256: {}\nPlanned SHA-256: {}",
                                    change.sha256_before, change.sha256_after
                                ));
                                ui.end_row();
                            }
                        });
                });
                ui.with_layout(Layout::bottom_up(Align::RIGHT), |ui| {
                    ui.horizontal(|ui| {
                        if ui.button("Back").clicked() {
                            action = Some(Action::Back);
                        }
                        if ui
                            .add_enabled(
                                !plan.changes().is_empty(),
                                Button::new("Apply template settings").fill(theme::PRIMARY),
                            )
                            .clicked()
                        {
                            action = Some(Action::BeginTemplateApply);
                        }
                    });
                });
            });
        action
    }

    fn render_history(&mut self, root: &mut Ui) -> Option<Action> {
        let mut action = None;
        egui::CentralPanel::default()
            .frame(
                Frame::new()
                    .fill(theme::TITLE)
                    .inner_margin(Margin::same(12)),
            )
            .show(root, |ui| {
                sheet_title(
                    ui,
                    "Engine History",
                    "Review recorded writes, current drift, and available recovery snapshots.",
                );
                let Some(workspace) = &self.workspace else {
                    empty_state(
                        ui,
                        "No engine selected",
                        "Select an engine to view history.",
                    );
                    return;
                };
                action = render_history_status(ui, workspace).or_else(|| action.take());
                action = render_history_operations(ui, workspace).or_else(|| action.take());
                ui.with_layout(Layout::bottom_up(Align::RIGHT), |ui| {
                    if ui.button("Back").clicked() {
                        action = Some(Action::Back);
                    }
                });
            });
        action
    }

    fn render_project_history(&mut self, root: &mut Ui) -> Option<Action> {
        let mut action = None;
        egui::CentralPanel::default()
            .frame(
                Frame::new()
                    .fill(theme::TITLE)
                    .inner_margin(Margin::same(12)),
            )
            .show(root, |ui| {
                sheet_title(
                    ui,
                    "Project History",
                    "Review recorded project writes, current drift, and available recovery snapshots.",
                );
                let Some(view) = &self.project_workspace else {
                    empty_state(ui, "No project loaded", "Open a project to view its history.");
                    return;
                };
                ui.add_space(8.0);
                section_heading(ui, "CURRENT STATUS");
                property_row(
                    ui,
                    "Recorded",
                    if view.status.recorded { "Yes" } else { "No" },
                );
                property_row(
                    ui,
                    "Drift",
                    if view.status.drifted {
                        "Detected"
                    } else {
                        "None"
                    },
                );
                for file in &view.status.files {
                    property_row(
                        ui,
                        file.relative_path.to_string_lossy().as_ref(),
                        file.state.as_str(),
                    );
                }
                ui.add_space(10.0);
                section_heading(ui, "RECORDED OPERATIONS");
                ScrollArea::vertical().show(ui, |ui| {
                    for operation in &view.history {
                        Frame::new()
                            .fill(theme::PANEL)
                            .stroke(Stroke::new(1.0, theme::RECESSED))
                            .inner_margin(Margin::same(8))
                            .show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    ui.label(
                                        RichText::new(operation.kind.as_str())
                                            .strong()
                                            .color(theme::ACCENT),
                                    );
                                    ui.label(&operation.completed);
                                    ui.label(&operation.preset);
                                    ui.with_layout(
                                        Layout::right_to_left(Align::Center),
                                        |ui| {
                                            if ui.button("Review restore").clicked() {
                                                action = Some(Action::ReviewRestore(
                                                    operation.id.clone(),
                                                ));
                                            }
                                        },
                                    );
                                });
                                ui.label(
                                    RichText::new(&operation.id).monospace().small().weak(),
                                );
                            });
                        ui.add_space(4.0);
                    }
                });
                ui.with_layout(Layout::bottom_up(Align::RIGHT), |ui| {
                    if ui.button("Back").clicked() {
                        action = Some(Action::Back);
                    }
                });
            });
        action
    }

    fn render_template_history(&mut self, root: &mut Ui) -> Option<Action> {
        let mut action = None;
        egui::CentralPanel::default()
            .frame(
                Frame::new()
                    .fill(theme::TITLE)
                    .inner_margin(Margin::same(12)),
            )
            .show(root, |ui| {
                sheet_title(
                    ui,
                    "Template History",
                    "Review recorded template writes, current drift, and available recovery snapshots.",
                );
                let Some(workspace) = &self.template_workspace else {
                    empty_state(
                        ui,
                        "No template catalog loaded",
                        "Select an engine template target to view its history.",
                    );
                    return;
                };
                ui.add_space(8.0);
                section_heading(ui, "CURRENT STATUS");
                property_row(
                    ui,
                    "Recorded",
                    if workspace.status.recorded {
                        "Yes"
                    } else {
                        "No"
                    },
                );
                property_row(
                    ui,
                    "Drift",
                    if workspace.status.drifted {
                        "Detected"
                    } else {
                        "None"
                    },
                );
                for file in &workspace.status.files {
                    property_row(
                        ui,
                        file.relative_path.to_string_lossy().as_ref(),
                        file.state.as_str(),
                    );
                }
                ui.add_space(10.0);
                section_heading(ui, "RECORDED OPERATIONS");
                ScrollArea::vertical().show(ui, |ui| {
                    for operation in &workspace.history {
                        Frame::new()
                            .fill(theme::PANEL)
                            .stroke(Stroke::new(1.0, theme::RECESSED))
                            .inner_margin(Margin::same(8))
                            .show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    ui.label(
                                        RichText::new(operation.kind.as_str())
                                            .strong()
                                            .color(theme::ACCENT),
                                    );
                                    ui.label(&operation.completed);
                                    ui.label(&operation.preset);
                                    ui.with_layout(
                                        Layout::right_to_left(Align::Center),
                                        |ui| {
                                            if ui.button("Review restore").clicked() {
                                                action = Some(Action::ReviewRestore(
                                                    operation.id.clone(),
                                                ));
                                            }
                                        },
                                    );
                                });
                                ui.label(
                                    RichText::new(&operation.id).monospace().small().weak(),
                                );
                            });
                        ui.add_space(4.0);
                    }
                });
                ui.with_layout(Layout::bottom_up(Align::RIGHT), |ui| {
                    if ui.button("Back").clicked() {
                        action = Some(Action::Back);
                    }
                });
            });
        action
    }

    fn render_restore_review(&mut self, root: &mut Ui) -> Option<Action> {
        let mut action = None;
        let Some(plan) = self.restore_plan.clone() else {
            return Some(Action::Back);
        };
        egui::CentralPanel::default()
            .frame(
                Frame::new()
                    .fill(theme::TITLE)
                    .inner_margin(Margin::same(12)),
            )
            .show(root, |ui| {
                sheet_title(
                    ui,
                    "Review Snapshot Restore",
                    "Restore uses verified snapshot bytes and records a new recovery snapshot.",
                );
                ui.add_space(8.0);
                property_row(ui, "Snapshot", plan.source_snapshot());
                property_row(ui, "Preset", plan.preset());
                property_row(
                    ui,
                    "New backup",
                    &plan.backup_directory().display().to_string(),
                );
                property_row(
                    ui,
                    "Writer",
                    elevation_label(self.restore_requires_elevation),
                );
                ui.add_space(8.0);
                section_heading(ui, &format!("RESTORE CHANGES ({})", plan.changes().len()));
                ScrollArea::vertical().show(ui, |ui| {
                    egui::Grid::new("restore_change_grid")
                        .striped(true)
                        .num_columns(3)
                        .spacing([18.0, 4.0])
                        .show(ui, |ui| {
                            ui.strong("File");
                            ui.strong("Current");
                            ui.strong("Snapshot");
                            ui.end_row();
                            for change in plan.changes() {
                                ui.label(change.relative_path.display().to_string());
                                ui.label(
                                    change
                                        .value_before
                                        .map_or("missing", DeclaredPluginState::as_str),
                                );
                                ui.label(
                                    RichText::new(change.value_after.as_str()).color(theme::ACCENT),
                                );
                                ui.end_row();
                            }
                        });
                });
                ui.with_layout(Layout::bottom_up(Align::RIGHT), |ui| {
                    ui.horizontal(|ui| {
                        if ui.button("Back").clicked() {
                            action = Some(Action::Back);
                        }
                        if ui
                            .add_enabled(
                                !plan.changes().is_empty(),
                                Button::new("Restore snapshot").fill(theme::PRIMARY),
                            )
                            .clicked()
                        {
                            action = Some(Action::BeginRestore);
                        }
                    });
                });
            });
        action
    }

    fn render_project_restore_review(&mut self, root: &mut Ui) -> Option<Action> {
        let mut action = None;
        let Some(plan) = self.project_restore_plan.clone() else {
            return Some(Action::Back);
        };
        egui::CentralPanel::default()
            .frame(
                Frame::new()
                    .fill(theme::TITLE)
                    .inner_margin(Margin::same(12)),
            )
            .show(root, |ui| {
                sheet_title(
                    ui,
                    "Review Project Snapshot",
                    "Restore uses the verified .uproject bytes from this snapshot and records a new recovery snapshot.",
                );
                ui.add_space(8.0);
                property_row(ui, "Snapshot", plan.source_snapshot());
                property_row(ui, "Project", &plan.project_path().display().to_string());
                property_row(ui, "Source", plan.preset());
                property_row(
                    ui,
                    "New backup",
                    &plan.backup_directory().display().to_string(),
                );
                property_row(ui, "Writer", "Current user");
                ui.add_space(8.0);
                section_heading(ui, "RESTORE CHANGE");
                if let Some(change) = plan.change() {
                    property_row(ui, "File", &change.relative_path.display().to_string());
                    property_row(ui, "Current SHA-256", &change.sha256_before);
                    property_row(ui, "Snapshot SHA-256", &change.sha256_after);
                    property_row(ui, "Snapshot bytes", &change.planned_byte_count.to_string());
                } else {
                    ui.label("The selected snapshot already matches the project.");
                }
                ui.with_layout(Layout::bottom_up(Align::RIGHT), |ui| {
                    ui.horizontal(|ui| {
                        if ui.button("Back").clicked() {
                            action = Some(Action::Back);
                        }
                        if ui
                            .add_enabled(
                                plan.change().is_some(),
                                Button::new("Restore project snapshot").fill(theme::PRIMARY),
                            )
                            .clicked()
                        {
                            action = Some(Action::BeginProjectRestore);
                        }
                    });
                });
            });
        action
    }

    fn render_template_restore_review(&mut self, root: &mut Ui) -> Option<Action> {
        let mut action = None;
        let Some(plan) = self.template_restore_plan.clone() else {
            return Some(Action::Back);
        };
        egui::CentralPanel::default()
            .frame(
                Frame::new()
                    .fill(theme::TITLE)
                    .inner_margin(Margin::same(12)),
            )
            .show(root, |ui| {
                sheet_title(
                    ui,
                    "Review Template Snapshot",
                    "Restore uses verified template bytes from this snapshot and records a new recovery snapshot.",
                );
                ui.add_space(8.0);
                property_row(ui, "Snapshot", plan.source_snapshot());
                property_row(ui, "Source", plan.preset());
                property_row(
                    ui,
                    "New backup",
                    &plan.backup_directory().display().to_string(),
                );
                property_row(
                    ui,
                    "Writer",
                    elevation_label(self.restore_requires_elevation),
                );
                ui.add_space(8.0);
                section_heading(ui, &format!("RESTORE CHANGES ({})", plan.changes().len()));
                ScrollArea::vertical().show(ui, |ui| {
                    egui::Grid::new("template_restore_change_grid")
                        .striped(true)
                        .num_columns(4)
                        .spacing([18.0, 5.0])
                        .show(ui, |ui| {
                            ui.strong("Descriptor");
                            ui.strong("Current SHA-256");
                            ui.strong("Snapshot SHA-256");
                            ui.strong("Bytes");
                            ui.end_row();
                            for change in plan.changes() {
                                ui.label(
                                    RichText::new(change.relative_path.display().to_string())
                                        .monospace()
                                        .small(),
                                );
                                ui.label(
                                    change.sha256_before.as_deref().unwrap_or("missing"),
                                );
                                ui.label(&change.sha256_after);
                                ui.label(change.planned_byte_count.to_string());
                                ui.end_row();
                            }
                        });
                });
                ui.with_layout(Layout::bottom_up(Align::RIGHT), |ui| {
                    ui.horizontal(|ui| {
                        if ui.button("Back").clicked() {
                            action = Some(Action::Back);
                        }
                        if ui
                            .add_enabled(
                                !plan.changes().is_empty(),
                                Button::new("Restore template snapshot").fill(theme::PRIMARY),
                            )
                            .clicked()
                        {
                            action = Some(Action::BeginTemplateRestore);
                        }
                    });
                });
            });
        action
    }

    fn render_notice(&self, root: &mut Ui) {
        let Some(notice) = &self.notice else {
            return;
        };
        egui::Panel::bottom("notice_bar")
            .exact_size(30.0)
            .frame(
                Frame::new()
                    .fill(theme::PANEL)
                    .stroke(Stroke::new(1.0, notice_color(notice.kind)))
                    .inner_margin(Margin::symmetric(8, 5)),
            )
            .show(root, |ui| {
                ui.label(RichText::new(&notice.text).color(notice_color(notice.kind)));
            });
    }

    fn render_confirmation(&mut self, context: &Context) -> Option<Action> {
        let pending = self.pending_write.as_ref()?;
        let (title, action_label, processes) = match pending {
            PendingWrite::Apply { processes, .. } => {
                ("Unreal process detected", "Apply preset", processes)
            }
            PendingWrite::Restore { processes, .. } => {
                ("Unreal process detected", "Restore snapshot", processes)
            }
            PendingWrite::ProjectApply { processes, .. } => (
                "Unreal process detected",
                "Apply project overrides",
                processes,
            ),
            PendingWrite::ProjectRestore { processes, .. } => (
                "Unreal process detected",
                "Restore project snapshot",
                processes,
            ),
            PendingWrite::TemplateApply { processes, .. } => (
                "Unreal process detected",
                "Apply template settings",
                processes,
            ),
            PendingWrite::TemplateRestore { processes, .. } => (
                "Unreal process detected",
                "Restore template snapshot",
                processes,
            ),
            PendingWrite::MultiEngineApply { processes, .. } => (
                "Unreal process detected",
                "Apply to selected engines",
                processes,
            ),
        };
        let mut action = None;
        egui::Modal::new(Id::new("active_process_confirmation"))
            .frame(
                Frame::new()
                    .fill(theme::PANEL)
                    .stroke(Stroke::new(1.0, theme::WARNING))
                    .inner_margin(Margin::same(14)),
            )
            .show(context, |ui| {
                ui.set_width(480.0);
                ui.heading(title);
                ui.label(
                    "Close these Unreal applications before writing. Continuing may conflict with files held by the engine.",
                );
                ui.add_space(8.0);
                for process in processes {
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new(&process.executable)
                                .color(theme::WARNING)
                                .strong(),
                        );
                        ui.label(format!("PID {}", process.process_id));
                    });
                    ui.label(
                        RichText::new(process.image_path.display().to_string())
                            .monospace()
                            .small()
                            .weak(),
                    );
                }
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    if ui.button("Cancel").clicked() {
                        action = Some(Action::CancelWrite);
                    }
                    if ui
                        .add(Button::new(action_label).fill(theme::WARNING))
                        .clicked()
                    {
                        action = Some(Action::ConfirmWrite);
                    }
                });
            });
        action
    }

    fn select_engine_target(&mut self, index: usize) {
        if self.target_mode == TargetMode::Template {
            self.select_template_engine(index);
        } else {
            self.select_engine(index);
        }
    }

    fn use_engine_target(&mut self) {
        if let Some(index) = self.selected_engine {
            self.select_engine(index);
        } else {
            self.target_mode = TargetMode::Engine;
            self.view = View::Workspace;
        }
    }

    fn use_template_target(&mut self) {
        if let Some(index) = self.selected_engine {
            self.select_template_engine(index);
        } else {
            self.set_notice(
                NoticeKind::Warning,
                "Select an engine before opening its templates.",
            );
        }
    }

    fn toggle_template(&mut self, path: PathBuf) {
        if !self.selected_templates.remove(&path) {
            self.selected_templates.insert(path);
        }
        self.rebuild_plan(true);
    }

    fn select_all_templates(&mut self) {
        self.selected_templates = self
            .template_workspace
            .as_ref()
            .map(|workspace| {
                workspace
                    .catalog
                    .templates
                    .iter()
                    .map(|template| template.relative_path.clone())
                    .collect()
            })
            .unwrap_or_default();
        self.rebuild_plan(true);
    }

    fn set_template_suppression(&mut self, suppression: ProjectSuppressionEdit) {
        self.template_suppression = suppression;
        self.rebuild_plan(true);
    }

    fn show_apply_review(&mut self) {
        let available = match self.target_mode {
            TargetMode::Engine => self.plan.is_some(),
            TargetMode::Project => self.project_plan.is_some(),
            TargetMode::Template => self.template_plan.is_some(),
        };
        if available {
            self.view = match self.target_mode {
                TargetMode::Engine => View::ApplyReview,
                TargetMode::Project => View::ProjectApplyReview,
                TargetMode::Template => View::TemplateApplyReview,
            };
        } else if self.plan_receiver.is_some() {
            self.set_notice(
                NoticeKind::Warning,
                "The plan is still being built. Review it when loading completes.",
            );
        } else {
            self.rebuild_plan(true);
        }
    }

    fn handle_action(&mut self, action: Action) {
        match action {
            Action::SelectEngine(index) => self.select_engine_target(index),
            Action::AddEngine => self.add_engine(),
            Action::OpenProject => self.open_project(),
            Action::UseEngineTarget => self.use_engine_target(),
            Action::UseTemplateTarget => self.use_template_target(),
            Action::ToggleTemplate(path) => self.toggle_template(path),
            Action::SelectAllTemplates => self.select_all_templates(),
            Action::ClearTemplateSelection => {
                self.selected_templates.clear();
                self.rebuild_plan(false);
            }
            Action::SetTemplateSuppression(value) => self.set_template_suppression(value),
            Action::ShowMultiEngineSelection => self.show_multi_engine_selection(),
            Action::ToggleMultiEngine(index) => {
                if self
                    .engines
                    .get(index)
                    .is_some_and(|engine| engine.health.is_selectable())
                {
                    if !self.selected_multi_engines.remove(&index) {
                        self.selected_multi_engines.insert(index);
                    }
                    self.multi_engine_plans.clear();
                }
            }
            Action::SelectAllMultiEngines => {
                self.selected_multi_engines = self
                    .engines
                    .iter()
                    .enumerate()
                    .filter(|(_, engine)| engine.health.is_selectable())
                    .map(|(index, _)| index)
                    .collect();
                self.multi_engine_plans.clear();
            }
            Action::ClearMultiEngineSelection => {
                self.selected_multi_engines.clear();
                self.multi_engine_plans.clear();
            }
            Action::BuildMultiEngineReview => self.build_multi_engine_plan(),
            Action::BackToMultiEngineSelection => self.view = View::MultiEngineSelection,
            Action::BeginMultiEngineApply => self.begin_multi_engine_apply(),
            Action::Refresh => self.refresh(),
            Action::NewPreset => self.new_preset(),
            Action::ImportPreset => self.import_preset(),
            Action::LoadPreset(path) => self.load_preset_path(&path),
            Action::SavePreset => self.save_current_preset(),
            Action::ExportPreset => self.export_current_preset(),
            Action::ShowPresetEditor => self.show_preset_editor(),
            Action::CommitPresetEditor => {
                if self.rebuild_plan(true) {
                    self.preset_editor_backup = None;
                    self.view = View::Workspace;
                    self.set_notice(NoticeKind::Success, "Preset validated.");
                }
            }
            Action::CancelPresetEditor => self.cancel_preset_editor(),
            Action::ReviewApply => self.show_apply_review(),
            Action::BeginApply => self.begin_apply(),
            Action::BeginProjectApply => self.begin_project_apply(),
            Action::BeginTemplateApply => self.begin_template_apply(),
            Action::ShowHistory => {
                self.view = match self.target_mode {
                    TargetMode::Engine => View::History,
                    TargetMode::Project => View::ProjectHistory,
                    TargetMode::Template => View::TemplateHistory,
                };
            }
            Action::ReviewRestore(snapshot) => match self.target_mode {
                TargetMode::Engine => self.review_restore(&snapshot),
                TargetMode::Project => self.review_project_restore(&snapshot),
                TargetMode::Template => self.review_template_restore(&snapshot),
            },
            Action::BeginRestore => self.begin_restore(),
            Action::BeginProjectRestore => self.begin_project_restore(),
            Action::BeginTemplateRestore => self.begin_template_restore(),
            Action::ConfirmWrite => self.confirm_pending_write(),
            Action::CancelWrite => self.pending_write = None,
            Action::Back => {
                self.pending_write = None;
                if self.view == View::PresetEditor {
                    self.cancel_preset_editor();
                } else {
                    self.view = View::Workspace;
                }
            }
        }
    }
}

impl eframe::App for UncleanApp {
    fn logic(&mut self, context: &Context, _frame: &mut eframe::Frame) {
        self.poll_background(context);
    }

    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let context = ui.ctx().clone();
        let mut action = self.shortcut_action(&context);
        action = self.render_top_bar(ui).or(action);
        self.render_notice(ui);
        let view_action = match self.view {
            View::Workspace => self.render_workspace(ui),
            View::PresetEditor => self.render_preset_editor(ui),
            View::ApplyReview => self.render_apply_review(ui),
            View::ProjectApplyReview => self.render_project_apply_review(ui),
            View::TemplateApplyReview => self.render_template_apply_review(ui),
            View::MultiEngineSelection => self.render_multi_engine_selection(ui),
            View::MultiEngineReview => self.render_multi_engine_review(ui),
            View::History => self.render_history(ui),
            View::ProjectHistory => self.render_project_history(ui),
            View::TemplateHistory => self.render_template_history(ui),
            View::RestoreReview => self.render_restore_review(ui),
            View::ProjectRestoreReview => self.render_project_restore_review(ui),
            View::TemplateRestoreReview => self.render_template_restore_review(ui),
        };
        action = view_action.or(action);
        action = self.render_confirmation(&context).or(action);
        if let Some(action) = action {
            self.handle_action(action);
        }
        if self.discovery_receiver.is_some()
            || self.workspace_receiver.is_some()
            || self.project_workspace_receiver.is_some()
            || self.template_workspace_receiver.is_some()
            || self.plan_receiver.is_some()
            || self.multi_plan_receiver.is_some()
        {
            context.request_repaint_after(Duration::from_millis(50));
        }
    }
}

fn spawn_discovery(options: DiscoveryOptions) -> Receiver<DiscoveryReport> {
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = sender.send(discover_engines(&options));
    });
    receiver
}

fn rules_from_text(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn remove_rule(rules: &mut Vec<String>, plugin: &str) {
    rules.retain(|rule| !rule.eq_ignore_ascii_case(plugin));
}

fn planned_states(plan: Option<&EnginePlan>) -> BTreeMap<String, DeclaredPluginState> {
    plan.map_or_else(BTreeMap::new, |plan| {
        plan.changes()
            .iter()
            .map(|change| (change.plugin.clone(), change.value_after))
            .collect()
    })
}

fn project_plan_status<'a>(
    plan: Option<&'a ProjectPlan>,
    plugin: &str,
) -> Option<&'a unclean_core::project_plans::ProjectPlannedPlugin> {
    plan.and_then(|plan| {
        plan.plugins()
            .iter()
            .find(|status| status.plugin.eq_ignore_ascii_case(plugin))
    })
}

fn render_project_plugin_rows(
    ui: &mut Ui,
    view: &ProjectWorkspaceView,
    filtered: &[usize],
    plan: Option<&ProjectPlan>,
    current_selection: Option<&str>,
) -> (Option<String>, Option<(String, DeclaredPluginState)>) {
    let mut selected_plugin = None;
    let mut toggle = None;
    ScrollArea::vertical()
        .id_salt("project_plugin_list")
        .auto_shrink([false, false])
        .show_rows(ui, LIST_ROW_HEIGHT, filtered.len(), |ui, range| {
            for visible_index in range {
                let status = &view.workspace.plugins[filtered[visible_index]];
                let planned = project_plan_status(plan, &status.plugin.name);
                let reference =
                    planned.map_or(status.project_reference, |plugin| plugin.reference_after);
                let project_effective = planned
                    .map_or(status.project_effective_enabled, |plugin| {
                        plugin.effective_after
                    });
                let origin = planned.map_or(status.project_origin, |plugin| plugin.origin_after);
                let state = reference.map_or(DeclaredPluginState::Unspecified, |enabled| {
                    if enabled {
                        DeclaredPluginState::Enabled
                    } else {
                        DeclaredPluginState::Disabled
                    }
                });
                let selected = current_selection == Some(&status.plugin.name);
                let row = ui.allocate_ui_with_layout(
                    Vec2::new(ui.available_width(), LIST_ROW_HEIGHT),
                    Layout::left_to_right(Align::Center),
                    |ui| {
                        ui.add_space(TABLE_BORDER_WIDTH);
                        let response = project_override_button(ui, &status.plugin.name, state);
                        if response.clicked() {
                            toggle = Some((status.plugin.name.clone(), state));
                        }
                        state_label(
                            ui,
                            status.plugin.effective_enabled == Some(true),
                            PROJECT_STATE_COLUMN_WIDTH,
                        );
                        state_label(ui, project_effective, PROJECT_STATE_COLUMN_WIDTH);
                        let name_response = table_clickable_label(
                            ui,
                            PROJECT_PLUGIN_COLUMN_WIDTH,
                            RichText::new(&status.plugin.friendly_name).color(if selected {
                                theme::ACCENT
                            } else {
                                theme::FOREGROUND
                            }),
                        );
                        if name_response.clicked() {
                            selected_plugin = Some(status.plugin.name.clone());
                        }
                        table_label(
                            ui,
                            PROJECT_SOURCE_COLUMN_WIDTH,
                            RichText::new(origin.as_str()).small(),
                        );
                        ui.label(RichText::new(status.plugin.module_count.to_string()).small());
                    },
                );
                if selected {
                    ui.painter().rect_stroke(
                        row.response.rect,
                        0.0,
                        Stroke::new(1.0, theme::PRIMARY),
                        egui::StrokeKind::Inside,
                    );
                }
            }
        });
    (selected_plugin, toggle)
}

fn project_override_button(
    ui: &mut Ui,
    plugin: &str,
    state: DeclaredPluginState,
) -> egui::Response {
    let (text, color) = match state {
        DeclaredPluginState::Enabled => ("ENABLE", theme::SUCCESS),
        DeclaredPluginState::Disabled => ("DISABLE", theme::ERROR),
        DeclaredPluginState::Unspecified => ("INHERIT", theme::FOREGROUND),
    };
    let next = match state {
        DeclaredPluginState::Enabled => "disabled",
        DeclaredPluginState::Disabled => "inherited",
        DeclaredPluginState::Unspecified => "enabled",
    };
    let label = format!(
        "{plugin} project override: {}. Activate to set {next}.",
        project_reference_label(match state {
            DeclaredPluginState::Enabled => Some(true),
            DeclaredPluginState::Disabled => Some(false),
            DeclaredPluginState::Unspecified => None,
        })
    );
    let response = ui.add_sized(
        [PROJECT_OVERRIDE_COLUMN_WIDTH, TABLE_CONTROL_HEIGHT],
        Button::new(RichText::new(text).small().color(color)).selected(true),
    );
    response.widget_info(|| WidgetInfo::selected(WidgetType::Button, true, true, label.clone()));
    response.on_hover_text(label)
}

fn state_label(ui: &mut Ui, enabled: bool, width: f32) {
    table_label(
        ui,
        width,
        RichText::new(if enabled { "ON" } else { "OFF" })
            .small()
            .color(if enabled {
                theme::SUCCESS
            } else {
                theme::FOREGROUND
            }),
    );
}

fn render_plugin_rows(
    ui: &mut Ui,
    workspace: &EngineWorkspace,
    filtered: &[usize],
    planned: &BTreeMap<String, DeclaredPluginState>,
    effective: &BTreeMap<String, bool>,
    current_selection: Option<&str>,
) -> (Option<String>, Option<(String, DeclaredPluginState)>) {
    let mut selected_plugin = None;
    let mut toggle = None;
    ScrollArea::vertical()
        .id_salt("plugin_list")
        .auto_shrink([false, false])
        .show_rows(ui, LIST_ROW_HEIGHT, filtered.len(), |ui, range| {
            for visible_index in range {
                let plugin = &workspace.plugins[filtered[visible_index]];
                let declared = planned
                    .get(&plugin.name)
                    .copied()
                    .unwrap_or(plugin.declared_state);
                let effective_on = effective.get(&plugin.name).copied().unwrap_or(false);
                let selected = current_selection == Some(&plugin.name);
                let row = ui.allocate_ui_with_layout(
                    Vec2::new(ui.available_width(), LIST_ROW_HEIGHT),
                    Layout::left_to_right(Align::Center),
                    |ui| {
                        ui.add_space(TABLE_BORDER_WIDTH);
                        let response = tri_state_button(ui, &plugin.name, declared);
                        if response.clicked() {
                            toggle = Some((plugin.name.clone(), declared));
                        }
                        table_label(
                            ui,
                            ENGINE_EFFECTIVE_COLUMN_WIDTH,
                            RichText::new(if effective_on { "ON" } else { "OFF" })
                                .small()
                                .color(if effective_on {
                                    theme::SUCCESS
                                } else {
                                    theme::FOREGROUND
                                }),
                        );
                        let name_response = table_clickable_label(
                            ui,
                            ENGINE_PLUGIN_COLUMN_WIDTH,
                            RichText::new(&plugin.friendly_name).color(if selected {
                                theme::ACCENT
                            } else {
                                theme::FOREGROUND
                            }),
                        );
                        if name_response.clicked() {
                            selected_plugin = Some(plugin.name.clone());
                        }
                        table_label(
                            ui,
                            ENGINE_CATEGORY_COLUMN_WIDTH,
                            RichText::new(plugin.category.as_deref().unwrap_or("Uncategorized"))
                                .small(),
                        );
                        ui.label(RichText::new(plugin.module_count.to_string()).small());
                    },
                );
                if selected {
                    ui.painter().rect_stroke(
                        row.response.rect,
                        0.0,
                        Stroke::new(1.0, theme::PRIMARY),
                        egui::StrokeKind::Inside,
                    );
                }
            }
        });
    (selected_plugin, toggle)
}

fn tri_state_button(ui: &mut Ui, plugin: &str, state: DeclaredPluginState) -> egui::Response {
    let (text, color) = match state {
        DeclaredPluginState::Enabled => ("TRUE", theme::SUCCESS),
        DeclaredPluginState::Disabled => ("FALSE", theme::ERROR),
        DeclaredPluginState::Unspecified => ("DEFAULT", theme::FOREGROUND),
    };
    let next = match state {
        DeclaredPluginState::Enabled => "disabled",
        DeclaredPluginState::Disabled => "unspecified",
        DeclaredPluginState::Unspecified => "enabled",
    };
    let label = format!(
        "{plugin} declared state: {}. Activate to set {next}.",
        state.as_str()
    );
    let response = ui.add_sized(
        [CONTROL_COLUMN_WIDTH, TABLE_CONTROL_HEIGHT],
        Button::new(RichText::new(text).small().color(color)).selected(true),
    );
    response.widget_info(|| WidgetInfo::selected(WidgetType::Button, true, true, label.clone()));
    response.on_hover_text(label)
}

fn template_selection_button(ui: &mut Ui, template: &str, selected: bool) -> egui::Response {
    let label = format!(
        "{template} template selection: {}. Activate to {}.",
        if selected { "selected" } else { "not selected" },
        if selected { "clear" } else { "select" }
    );
    let response = ui.selectable_label(selected, if selected { "Selected" } else { "Select" });
    response
        .widget_info(|| WidgetInfo::selected(WidgetType::Button, true, selected, label.clone()));
    response.on_hover_text(label)
}

fn table_header(ui: &mut Ui) {
    Frame::new()
        .fill(theme::HEADER)
        .stroke(Stroke::new(TABLE_BORDER_WIDTH, theme::RECESSED))
        .inner_margin(Margin::symmetric(0, 4))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                table_label(ui, CONTROL_COLUMN_WIDTH, RichText::new("DEFAULT").small());
                table_label(
                    ui,
                    ENGINE_EFFECTIVE_COLUMN_WIDTH,
                    RichText::new("EFFECTIVE").small(),
                );
                table_label(
                    ui,
                    ENGINE_PLUGIN_COLUMN_WIDTH,
                    RichText::new("PLUGIN").small(),
                );
                table_label(
                    ui,
                    ENGINE_CATEGORY_COLUMN_WIDTH,
                    RichText::new("CATEGORY").small(),
                );
                ui.label(RichText::new("MODULES").small());
            });
        });
}

fn project_table_header(ui: &mut Ui) {
    Frame::new()
        .fill(theme::HEADER)
        .stroke(Stroke::new(TABLE_BORDER_WIDTH, theme::RECESSED))
        .inner_margin(Margin::symmetric(0, 4))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                table_label(
                    ui,
                    PROJECT_OVERRIDE_COLUMN_WIDTH,
                    RichText::new("PROJECT OVERRIDE").small(),
                );
                table_label(
                    ui,
                    PROJECT_STATE_COLUMN_WIDTH,
                    RichText::new("ENGINE").small(),
                );
                table_label(
                    ui,
                    PROJECT_STATE_COLUMN_WIDTH,
                    RichText::new("PROJECT").small(),
                );
                table_label(
                    ui,
                    PROJECT_PLUGIN_COLUMN_WIDTH,
                    RichText::new("PLUGIN").small(),
                );
                table_label(
                    ui,
                    PROJECT_SOURCE_COLUMN_WIDTH,
                    RichText::new("STATE SOURCE").small(),
                );
                ui.label(RichText::new("MODULES").small());
            });
        });
}

fn table_label(ui: &mut Ui, width: f32, text: impl Into<egui::WidgetText>) -> egui::Response {
    table_cell(ui, width, |ui| ui.add(egui::Label::new(text).truncate()))
}

fn table_clickable_label(
    ui: &mut Ui,
    width: f32,
    text: impl Into<egui::WidgetText>,
) -> egui::Response {
    table_cell(ui, width, |ui| {
        ui.add(egui::Label::new(text).truncate().sense(Sense::click()))
    })
}

fn table_cell<R>(ui: &mut Ui, width: f32, add_contents: impl FnOnce(&mut Ui) -> R) -> R {
    ui.allocate_ui_with_layout(
        Vec2::new(width, TABLE_LABEL_HEIGHT),
        Layout::left_to_right(Align::Center),
        |ui| {
            ui.set_min_size(Vec2::new(width, TABLE_LABEL_HEIGHT));
            add_contents(ui)
        },
    )
    .inner
}

fn resizable_rail(ui: &Ui, id: &'static str, default_width: f32, inner_margin: i8) -> egui::Panel {
    let max_width = (ui.available_width() - WORKSPACE_MIN_WIDTH).max(RAIL_MIN_WIDTH);
    egui::Panel::left(id)
        .resizable(true)
        .default_size(default_width)
        .min_size(RAIL_MIN_WIDTH)
        .max_size(max_width)
        .frame(
            Frame::new()
                .fill(theme::PANEL)
                .stroke(Stroke::new(1.0, theme::RECESSED))
                .inner_margin(Margin::same(inner_margin)),
        )
}

fn resizable_details_panel(ui: &Ui, id: &'static str) -> egui::Panel {
    let max_height = (ui.available_height() - WORKSPACE_MIN_HEIGHT).max(DETAILS_MIN_HEIGHT);
    egui::Panel::bottom(id)
        .resizable(true)
        .default_size(DETAILS_DEFAULT_HEIGHT)
        .min_size(DETAILS_MIN_HEIGHT)
        .max_size(max_height)
        .frame(
            Frame::new()
                .fill(theme::PANEL)
                .stroke(Stroke::new(1.0, theme::RECESSED))
                .inner_margin(Margin::same(8)),
        )
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
        ProjectSuppressionEdit::Keep => "keep current field",
        ProjectSuppressionEdit::Set(true) => "suppress engine defaults",
        ProjectSuppressionEdit::Set(false) => "allow engine defaults",
        ProjectSuppressionEdit::Clear => "remove suppression field",
    }
}

const fn template_suppression_tooltip(edit: ProjectSuppressionEdit) -> &'static str {
    match edit {
        ProjectSuppressionEdit::Set(true) => {
            "New projects disable engine plugins unless their project file enables them."
        }
        ProjectSuppressionEdit::Set(false) => "New projects retain engine plugin defaults.",
        ProjectSuppressionEdit::Clear => {
            "New projects inherit Unreal behavior without this template field."
        }
        ProjectSuppressionEdit::Keep => "Template suppression remains unchanged.",
    }
}

fn section_heading(ui: &mut Ui, text: &str) {
    ui.label(
        RichText::new(text)
            .small()
            .strong()
            .color(theme::FOREGROUND_HEADER),
    );
}

fn sheet_title(ui: &mut Ui, title: &str, detail: &str) {
    ui.heading(title);
    ui.label(RichText::new(detail).weak());
    ui.separator();
}

fn property_row(ui: &mut Ui, name: &str, value: &str) {
    ui.horizontal(|ui| {
        ui.add_sized(
            [92.0, 16.0],
            egui::Label::new(RichText::new(name).small().weak()),
        );
        ui.label(value);
    });
}

fn rule_editor(ui: &mut Ui, label: &str, value: &mut String) {
    section_heading(ui, label);
    ui.add(
        TextEdit::multiline(value)
            .desired_rows(18)
            .desired_width(f32::INFINITY)
            .code_editor(),
    );
}

fn empty_state(ui: &mut Ui, title: &str, detail: &str) {
    ui.vertical_centered(|ui| {
        ui.add_space(60.0);
        ui.heading(title);
        ui.label(RichText::new(detail).weak());
    });
}

fn render_project_target(ui: &mut Ui, project_path: Option<&Path>, loading: bool) {
    section_heading(ui, "PROJECT TARGET");
    ui.add_space(4.0);
    if let Some(path) = project_path {
        ui.label(
            RichText::new(
                path.file_stem()
                    .and_then(|name| name.to_str())
                    .unwrap_or("Unreal project"),
            )
            .size(18.0)
            .strong()
            .color(theme::ACCENT),
        );
        ui.label(
            RichText::new(path.display().to_string())
                .monospace()
                .small()
                .weak(),
        );
    }
    if loading {
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label("Resolving project and engine");
        });
    }
}

fn render_project_rail_state(
    ui: &mut Ui,
    view: &ProjectWorkspaceView,
    suppression: &mut ProjectSuppressionEdit,
) {
    ui.add_space(10.0);
    section_heading(ui, "ASSOCIATED ENGINE");
    property_row(
        ui,
        "Version",
        view.workspace
            .engine
            .version
            .as_deref()
            .unwrap_or("Unknown"),
    );
    property_row(
        ui,
        "Association",
        view.workspace
            .project
            .engine_association
            .as_deref()
            .unwrap_or("Not specified"),
    );
    ui.label(
        RichText::new(view.workspace.engine.path.display().to_string())
            .monospace()
            .small()
            .weak(),
    );
    ui.add_space(10.0);
    section_heading(ui, "ENGINE DEFAULTS");
    property_row(ui, "Current", view.workspace.project.suppression.as_str());
    render_project_suppression(ui, suppression);
    ui.add_space(10.0);
    let conflicts = view
        .workspace
        .project_warnings
        .iter()
        .filter(|warning| warning.blocking)
        .count();
    property_row(
        ui,
        "Engine plugins",
        &view.workspace.plugins.len().to_string(),
    );
    property_row(
        ui,
        "Project refs",
        &view.workspace.project.plugins.len().to_string(),
    );
    property_row(ui, "Conflicts", &conflicts.to_string());
    property_row(
        ui,
        "Drift",
        if view.status.drifted {
            "Detected"
        } else {
            "None"
        },
    );
}

fn render_project_suppression(ui: &mut Ui, suppression: &mut ProjectSuppressionEdit) {
    egui::ComboBox::from_id_salt("project_suppression")
        .selected_text(suppression_edit_label(*suppression))
        .width(ui.available_width())
        .show_ui(ui, |ui| {
            ui.selectable_value(
                suppression,
                ProjectSuppressionEdit::Keep,
                "Keep current field",
            );
            ui.selectable_value(
                suppression,
                ProjectSuppressionEdit::Set(true),
                "Suppress engine defaults",
            );
            ui.selectable_value(
                suppression,
                ProjectSuppressionEdit::Set(false),
                "Allow engine defaults",
            );
            ui.selectable_value(
                suppression,
                ProjectSuppressionEdit::Clear,
                "Remove suppression field",
            );
        });
}

fn render_project_rail_footer(ui: &mut Ui) -> Option<Action> {
    let mut action = None;
    ui.with_layout(Layout::bottom_up(Align::LEFT), |ui| {
        if ui
            .add_sized([ui.available_width(), 25.0], Button::new("Engine target"))
            .clicked()
        {
            action = Some(Action::UseEngineTarget);
        }
        if ui
            .add_sized(
                [ui.available_width(), 25.0],
                Button::new("Open another project"),
            )
            .clicked()
        {
            action = Some(Action::OpenProject);
        }
    });
    action
}

fn metric(ui: &mut Ui, label: &str, before: usize, after: usize) {
    Frame::new()
        .fill(theme::PANEL)
        .stroke(Stroke::new(1.0, theme::RECESSED))
        .inner_margin(Margin::same(8))
        .show(ui, |ui| {
            section_heading(ui, label);
            ui.label(
                RichText::new(format!("{before} > {after}"))
                    .size(18.0)
                    .color(theme::ACCENT),
            );
        });
}

fn render_project_plan_summary(ui: &mut Ui, plan: &ProjectPlan) {
    ui.add_space(8.0);
    ui.columns(3, |columns| {
        let impact = plan.impact();
        metric(
            &mut columns[0],
            "PROJECT PLUGINS",
            impact.effective_plugins.before,
            impact.effective_plugins.after,
        );
        metric(
            &mut columns[1],
            "EXPLICIT REFERENCES",
            impact.explicit_references.before,
            impact.explicit_references.after,
        );
        metric(
            &mut columns[2],
            "DECLARED MODULES",
            impact.declared_modules.before,
            impact.declared_modules.after,
        );
    });
    ui.add_space(8.0);
    property_row(ui, "Project", &plan.project_path().display().to_string());
    property_row(
        ui,
        "Engine",
        &format!(
            "{}  {}",
            plan.engine().version.as_deref().unwrap_or("Unknown"),
            plan.engine().path.display()
        ),
    );
    property_row(
        ui,
        "Suppression",
        suppression_edit_label(plan.edit().suppression),
    );
    property_row(ui, "Backup", &plan.backup_directory().display().to_string());
    property_row(ui, "Writer", "Current user");
}

fn render_project_plan_changes(ui: &mut Ui, plan: &ProjectPlan) {
    ui.add_space(8.0);
    section_heading(ui, "PLUGIN RESULTS");
    ScrollArea::vertical().show(ui, |ui| {
        egui::Grid::new("project_change_grid")
            .striped(true)
            .num_columns(5)
            .spacing([16.0, 4.0])
            .show(ui, |ui| {
                ui.strong("Plugin");
                ui.strong("Engine");
                ui.strong("Override before");
                ui.strong("Override after");
                ui.strong("Project result");
                ui.end_row();
                for plugin in plan.plugins().iter().filter(|plugin| {
                    plugin.reference_before != plugin.reference_after
                        || plugin.effective_before != plugin.effective_after
                }) {
                    ui.label(&plugin.plugin);
                    ui.label(enabled_label(plugin.engine_effective_enabled));
                    ui.label(project_reference_label(plugin.reference_before));
                    ui.label(
                        RichText::new(project_reference_label(plugin.reference_after))
                            .color(theme::ACCENT),
                    );
                    ui.label(format!(
                        "{} > {}",
                        enabled_label(plugin.effective_before),
                        enabled_label(plugin.effective_after)
                    ));
                    ui.end_row();
                }
            });
    });
}

fn render_apply_summary(ui: &mut Ui, plan: &EnginePlan, requires_elevation: Option<bool>) {
    ui.add_space(8.0);
    ui.columns(3, |columns| {
        metric(
            &mut columns[0],
            "EFFECTIVE PLUGINS",
            plan.impact().effective_plugins.before,
            plan.impact().effective_plugins.after,
        );
        metric(
            &mut columns[1],
            "DECLARED MODULES",
            plan.impact().declared_modules.before,
            plan.impact().declared_modules.after,
        );
        metric(
            &mut columns[2],
            "DEFAULT ROOTS",
            plan.impact().default_roots.before,
            plan.impact().default_roots.after,
        );
    });
    ui.add_space(8.0);
    property_row(ui, "Engine", &plan.engine().path.display().to_string());
    property_row(ui, "Preset", &plan.preset().name);
    property_row(ui, "Backup", &plan.backup_directory().display().to_string());
    property_row(ui, "Writer", elevation_label(requires_elevation));
    ui.add_space(8.0);
}

fn render_apply_changes(ui: &mut Ui, plan: &EnginePlan) {
    section_heading(
        ui,
        &format!("DESCRIPTOR CHANGES ({})", plan.changes().len()),
    );
    ScrollArea::vertical()
        .id_salt("apply_review_changes")
        .max_height(250.0)
        .show(ui, |ui| {
            egui::Grid::new("apply_change_grid")
                .striped(true)
                .num_columns(4)
                .spacing([18.0, 4.0])
                .show(ui, |ui| {
                    ui.strong("Plugin");
                    ui.strong("Before");
                    ui.strong("After");
                    ui.strong("File");
                    ui.end_row();
                    for change in plan.changes() {
                        ui.label(&change.plugin);
                        ui.label(change.value_before.as_str());
                        ui.label(RichText::new(change.value_after.as_str()).color(theme::ACCENT));
                        ui.label(
                            RichText::new(change.relative_path.display().to_string())
                                .monospace()
                                .small(),
                        );
                        ui.end_row();
                    }
                });
        });
}

fn render_plan_resolution(ui: &mut Ui, plan: &EnginePlan) {
    if !plan.no_ops().is_empty() {
        ui.add_space(6.0);
        section_heading(ui, &format!("NO FILE EDIT ({})", plan.no_ops().len()));
        egui::CollapsingHeader::new("Review requests without file edits").show(ui, |ui| {
            for no_op in plan.no_ops() {
                ui.horizontal(|ui| {
                    ui.label(&no_op.plugin);
                    ui.label(RichText::new(no_op.reason.message()).weak());
                });
            }
        });
    }
    if !plan.pattern_expansions().is_empty() {
        ui.add_space(6.0);
        section_heading(
            ui,
            &format!("RESOLVED PATTERNS ({})", plan.pattern_expansions().len()),
        );
        egui::CollapsingHeader::new("Review pattern matches").show(ui, |ui| {
            for expansion in plan.pattern_expansions() {
                ui.horizontal_wrapped(|ui| {
                    ui.label(
                        RichText::new(&expansion.pattern)
                            .monospace()
                            .color(theme::ACCENT),
                    );
                    ui.label(expansion.matches.join(", "));
                });
            }
        });
    }
}

fn render_apply_footer(ui: &mut Ui, plan: &EnginePlan) -> Option<Action> {
    let mut action = None;
    ui.with_layout(Layout::bottom_up(Align::RIGHT), |ui| {
        ui.horizontal(|ui| {
            if ui.button("Back").clicked() {
                action = Some(Action::Back);
            }
            if ui
                .add_enabled(
                    !plan.changes().is_empty(),
                    Button::new("Apply preset").fill(theme::PRIMARY),
                )
                .clicked()
            {
                action = Some(Action::BeginApply);
            }
        });
    });
    action
}

fn render_history_status(ui: &mut Ui, workspace: &EngineWorkspace) -> Option<Action> {
    let mut action = None;
    ui.add_space(8.0);
    ui.horizontal(|ui| {
        status_badge(
            ui,
            if workspace.status.drifted {
                "DRIFT DETECTED"
            } else if workspace.status.recorded {
                "RECORDED STATE MATCHES"
            } else {
                "NO RECORDED STATE"
            },
            if workspace.status.drifted {
                NoticeKind::Warning
            } else {
                NoticeKind::Success
            },
        );
        if ui.button("Reapply current preset").clicked() {
            action = Some(Action::ReviewApply);
        }
    });
    if !workspace.status.files.is_empty() {
        ui.add_space(8.0);
        section_heading(ui, "LATEST RECORDED FILES");
        ScrollArea::vertical()
            .id_salt("status_files")
            .max_height(130.0)
            .show(ui, |ui| {
                for file in &workspace.status.files {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(file.state.as_str()).color(
                            if file.state.as_str() == "matching" {
                                theme::SUCCESS
                            } else {
                                theme::WARNING
                            },
                        ));
                        ui.label(file.relative_path.display().to_string());
                    });
                }
            });
    }
    action
}

fn render_history_operations(ui: &mut Ui, workspace: &EngineWorkspace) -> Option<Action> {
    let mut action = None;
    ui.add_space(8.0);
    section_heading(
        ui,
        &format!("COMPLETED OPERATIONS ({})", workspace.history.len()),
    );
    ScrollArea::vertical()
        .id_salt("operation_history")
        .show(ui, |ui| {
            for operation in &workspace.history {
                Frame::new()
                    .fill(theme::PANEL)
                    .stroke(Stroke::new(1.0, theme::RECESSED))
                    .inner_margin(Margin::same(8))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.strong(format!("{}  {}", operation.kind.as_str(), operation.preset));
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                if ui.button("Restore").clicked() {
                                    action = Some(Action::ReviewRestore(operation.id.clone()));
                                }
                                ui.label(
                                    RichText::new(&operation.completed)
                                        .monospace()
                                        .small()
                                        .weak(),
                                );
                            });
                        });
                        ui.label(
                            RichText::new(format!(
                                "{} files  {}",
                                operation.files.len(),
                                operation.id
                            ))
                            .small()
                            .weak(),
                        );
                    });
                ui.add_space(4.0);
            }
        });
    action
}

fn review_warnings(ui: &mut Ui, plan: &EnginePlan) {
    let warning_count = plan.dependency_warnings().len()
        + plan.graph_warnings().len()
        + plan.scan_warnings().len()
        + plan.unmatched_rules().len();
    if warning_count == 0 {
        status_badge(ui, "NO PLAN WARNINGS", NoticeKind::Success);
        return;
    }
    status_badge(
        ui,
        &format!("{warning_count} PLAN WARNINGS"),
        NoticeKind::Warning,
    );
    egui::CollapsingHeader::new("Review warnings")
        .default_open(true)
        .show(ui, |ui| {
            for warning in plan.dependency_warnings() {
                ui.colored_label(theme::WARNING, &warning.message);
            }
            for warning in plan.graph_warnings() {
                ui.colored_label(theme::WARNING, &warning.message);
            }
            for warning in plan.scan_warnings() {
                ui.colored_label(theme::WARNING, &warning.message);
            }
            for warning in plan.unmatched_rules() {
                ui.colored_label(
                    theme::WARNING,
                    format!(
                        "{} rule did not match: {}",
                        warning.action.as_str(),
                        warning.rule
                    ),
                );
            }
        });
}

fn render_project_review_warnings(ui: &mut Ui, plan: &ProjectPlan) {
    let warning_count = plan.dependency_warnings().len()
        + plan.project_warnings().len()
        + plan.scan_warnings().len()
        + plan.unmatched_rules().len();
    if warning_count == 0 {
        status_badge(ui, "NO PROJECT WARNINGS", NoticeKind::Success);
        return;
    }
    let blocking = plan
        .project_warnings()
        .iter()
        .filter(|warning| warning.blocking)
        .count();
    let label = if blocking == 0 {
        format!("{warning_count} PROJECT WARNINGS")
    } else {
        format!("{blocking} PROJECT CONFLICTS  {warning_count} TOTAL")
    };
    status_badge(
        ui,
        &label,
        if blocking == 0 {
            NoticeKind::Warning
        } else {
            NoticeKind::Error
        },
    );
    egui::CollapsingHeader::new("Review project warnings")
        .default_open(true)
        .show(ui, |ui| {
            for warning in plan.project_warnings() {
                ui.colored_label(
                    if warning.blocking {
                        theme::ERROR
                    } else {
                        theme::WARNING
                    },
                    &warning.message,
                );
            }
            for warning in plan.dependency_warnings() {
                ui.colored_label(theme::WARNING, &warning.message);
            }
            for warning in plan.scan_warnings() {
                ui.colored_label(theme::WARNING, &warning.message);
            }
            for warning in plan.unmatched_rules() {
                ui.colored_label(
                    theme::WARNING,
                    format!(
                        "{} rule did not match the associated engine: {}",
                        warning.action.as_str(),
                        warning.rule
                    ),
                );
            }
        });
}

fn status_badge(ui: &mut Ui, text: &str, kind: NoticeKind) {
    Frame::new()
        .fill(notice_color(kind).gamma_multiply(0.12))
        .stroke(Stroke::new(1.0, notice_color(kind)))
        .inner_margin(Margin::symmetric(7, 3))
        .show(ui, |ui| {
            ui.label(
                RichText::new(text)
                    .small()
                    .strong()
                    .color(notice_color(kind)),
            );
        });
}

const fn notice_color(kind: NoticeKind) -> Color32 {
    match kind {
        NoticeKind::Success => theme::SUCCESS,
        NoticeKind::Warning => theme::WARNING,
        NoticeKind::Error => theme::ERROR,
    }
}

const fn health_color(health: EngineHealth) -> Color32 {
    match health {
        EngineHealth::Healthy => theme::SUCCESS,
        EngineHealth::Partial => theme::WARNING,
        EngineHealth::Unavailable => theme::ERROR,
    }
}

const fn elevation_label(requires_elevation: Option<bool>) -> &'static str {
    match requires_elevation {
        Some(true) => "Administrator approval required",
        Some(false) => "Current user access",
        None => "Access check unavailable",
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use eframe::egui::containers::PanelState;
    use eframe::egui::{
        CentralPanel, Context, Event, Id, Modifiers, PointerButton, Pos2, RawInput, Rect,
        ScrollArea, Shape, vec2,
    };
    use tempfile::tempdir;
    use unclean_core::descriptors::DeclaredPluginState;
    use unclean_core::presets::{PresetDocument, save_preset};

    use super::{
        Action, PROJECT_PLUGIN_COLUMN_WIDTH, PROJECT_SOURCE_COLUMN_WIDTH,
        PROJECT_STATE_COLUMN_WIDTH, PresetDraft, TABLE_BORDER_WIDTH, TargetMode, UncleanApp, View,
        project_override_button, project_table_header, resizable_details_panel, resizable_rail,
        rules_from_text, table_label, template_selection_button, tri_state_button,
    };

    #[test]
    fn rule_editor_uses_one_trimmed_rule_per_line() {
        assert_eq!(
            rules_from_text(" First \n\nSecond\n"),
            vec!["First", "Second"]
        );
    }

    #[test]
    fn canceling_a_new_preset_restores_the_previous_preset()
    -> Result<(), Box<dyn std::error::Error>> {
        let context = Context::default();
        let mut app = UncleanApp::new(&context)?;
        let document = PresetDocument::new("Existing preset")?;
        let path = PathBuf::from("existing-preset.toml");
        app.preset_path = Some(path.clone());
        app.preset_draft = PresetDraft::from_document(&document);
        app.preset_document = document;

        app.new_preset();
        assert_eq!(app.view, View::PresetEditor);
        assert_eq!(app.preset_document.preset().name, "New preset");

        app.handle_action(Action::CancelPresetEditor);
        assert_eq!(app.view, View::Workspace);
        assert_eq!(app.preset_path, Some(path));
        assert_eq!(app.preset_document.preset().name, "Existing preset");
        Ok(())
    }

    #[test]
    fn new_presets_save_in_the_managed_catalog() -> Result<(), Box<dyn std::error::Error>> {
        let context = Context::default();
        let temp = tempdir()?;
        let directory = temp.path().join("presets");
        let mut app = UncleanApp::new_with_preset_directory(&context, Some(directory.clone()))?;
        app.preset_draft.name = "Persistent preset".to_owned();

        app.save_current_preset();

        let saved_path = app
            .preset_path
            .as_deref()
            .ok_or("the saved preset has no path")?;
        assert_eq!(
            saved_path
                .parent()
                .ok_or("saved preset path has no parent")?
                .canonicalize()?,
            directory.canonicalize()?
        );
        assert!(
            app.preset_files
                .iter()
                .any(|preset| preset.name == "Persistent-preset")
        );

        let restarted = UncleanApp::new_with_preset_directory(&context, Some(directory))?;
        assert!(
            restarted
                .preset_files
                .iter()
                .any(|preset| preset.name == "Persistent-preset")
        );
        Ok(())
    }

    #[test]
    fn imported_presets_join_the_catalog_and_survive_restart()
    -> Result<(), Box<dyn std::error::Error>> {
        let context = Context::default();
        let temp = tempdir()?;
        let directory = temp.path().join("managed");
        let baseline = PresetDocument::new("Baseline")?;
        save_preset(&directory.join("baseline.toml"), &baseline)?;
        let imported = PresetDocument::new("Imported preset")?;
        let source = save_preset(
            &temp.path().join("external").join("external-choice.toml"),
            &imported,
        )?;
        let mut app = UncleanApp::new_with_preset_directory(&context, Some(directory.clone()))?;

        app.import_preset_path(&source);

        assert_eq!(app.preset_files.len(), 2);
        assert!(
            app.preset_files
                .iter()
                .any(|preset| preset.name == "external-choice")
        );
        let restarted = UncleanApp::new_with_preset_directory(&context, Some(directory))?;
        assert_eq!(restarted.preset_files.len(), 2);
        assert!(
            restarted
                .preset_files
                .iter()
                .any(|preset| preset.name == "external-choice")
        );
        Ok(())
    }

    #[test]
    fn tri_state_control_exposes_plugin_state_to_accesskit() {
        let context = Context::default();
        context.enable_accesskit();
        let output = context.run_ui(RawInput::default(), |ui| {
            CentralPanel::default().show(ui, |ui| {
                tri_state_button(ui, "Niagara", DeclaredPluginState::Enabled);
            });
        });
        let labels = output
            .platform_output
            .accesskit_update
            .into_iter()
            .flat_map(|update| update.nodes)
            .filter_map(|(_, node)| node.label().map(ToOwned::to_owned))
            .collect::<Vec<_>>();
        assert!(
            labels
                .iter()
                .any(|label| label.contains("Niagara declared state: enabled"))
        );
    }

    #[test]
    fn project_override_control_exposes_inheritance_to_accesskit() {
        let context = Context::default();
        context.enable_accesskit();
        let output = context.run_ui(RawInput::default(), |ui| {
            CentralPanel::default().show(ui, |ui| {
                project_override_button(ui, "Niagara", DeclaredPluginState::Unspecified);
            });
        });
        let labels = output
            .platform_output
            .accesskit_update
            .into_iter()
            .flat_map(|update| update.nodes)
            .filter_map(|(_, node)| node.label().map(ToOwned::to_owned))
            .collect::<Vec<_>>();
        assert!(
            labels
                .iter()
                .any(|label| label.contains("Niagara project override: inherit"))
        );
    }

    #[test]
    fn project_table_header_and_row_share_column_origins() -> Result<(), Box<dyn std::error::Error>>
    {
        fn text_left(shape: &Shape, label: &str) -> Option<f32> {
            match shape {
                Shape::Text(text) if text.galley.job.text == label => Some(text.pos.x),
                Shape::Vec(shapes) => shapes.iter().find_map(|shape| text_left(shape, label)),
                _ => None,
            }
        }

        let context = Context::default();
        let mut row_lefts = [0.0; 6];
        let output = context.run_ui(RawInput::default(), |ui| {
            project_table_header(ui);
            ui.horizontal(|ui| {
                ui.add_space(TABLE_BORDER_WIDTH);
                row_lefts[0] = project_override_button(
                    ui,
                    "AlignmentFixture",
                    DeclaredPluginState::Unspecified,
                )
                .rect
                .left();
                row_lefts[1] = table_label(ui, PROJECT_STATE_COLUMN_WIDTH, "ENGINE VALUE")
                    .rect
                    .left();
                row_lefts[2] = table_label(ui, PROJECT_STATE_COLUMN_WIDTH, "PROJECT VALUE")
                    .rect
                    .left();
                row_lefts[3] = table_label(ui, PROJECT_PLUGIN_COLUMN_WIDTH, "PLUGIN VALUE")
                    .rect
                    .left();
                row_lefts[4] = table_label(ui, PROJECT_SOURCE_COLUMN_WIDTH, "STATE VALUE")
                    .rect
                    .left();
                row_lefts[5] = ui.label("MODULE VALUE").rect.left();
            });
        });
        let labels = [
            "PROJECT OVERRIDE",
            "ENGINE",
            "PROJECT",
            "PLUGIN",
            "STATE SOURCE",
            "MODULES",
        ];
        for (index, label) in labels.into_iter().enumerate() {
            let header_left = output
                .shapes
                .iter()
                .find_map(|clipped| text_left(&clipped.shape, label))
                .ok_or_else(|| format!("missing painted header text for {label}"))?;
            assert!(
                (header_left - row_lefts[index]).abs() <= 0.01,
                "{label} header starts at {header_left}, row starts at {}",
                row_lefts[index]
            );
        }
        Ok(())
    }

    #[test]
    fn template_selection_exposes_its_template_name_to_accesskit() {
        let context = Context::default();
        context.enable_accesskit();
        let output = context.run_ui(RawInput::default(), |ui| {
            CentralPanel::default().show(ui, |ui| {
                template_selection_button(ui, "TP_Invented", false);
            });
        });
        let labels = output
            .platform_output
            .accesskit_update
            .into_iter()
            .flat_map(|update| update.nodes)
            .filter_map(|(_, node)| node.label().map(ToOwned::to_owned))
            .collect::<Vec<_>>();
        assert!(
            labels
                .iter()
                .any(|label| label.contains("TP_Invented template selection: not selected"))
        );
    }

    #[test]
    fn template_toolbar_hides_preset_file_actions() -> Result<(), Box<dyn std::error::Error>> {
        let context = Context::default();
        context.enable_accesskit();
        let mut app = UncleanApp::new(&context)?;
        app.target_mode = TargetMode::Template;
        let output = context.run_ui(RawInput::default(), |ui| {
            app.render_top_bar(ui);
        });
        let labels = output
            .platform_output
            .accesskit_update
            .into_iter()
            .flat_map(|update| update.nodes)
            .filter_map(|(_, node)| node.label().map(ToOwned::to_owned))
            .collect::<Vec<_>>();
        assert!(!labels.iter().any(|label| matches!(
            label.as_str(),
            "New" | "Import" | "Edit" | "Save" | "Export"
        )));
        Ok(())
    }

    #[test]
    fn workspace_panel_handles_follow_pointer_drags() -> Result<(), Box<dyn std::error::Error>> {
        fn input(events: Vec<Event>) -> RawInput {
            RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(1_200.0, 700.0))),
                events,
                ..Default::default()
            }
        }

        fn pointer_button(pos: Pos2, pressed: bool) -> Event {
            Event::PointerButton {
                pos,
                button: PointerButton::Primary,
                pressed,
                modifiers: Modifiers::default(),
            }
        }

        fn render_rail(context: &Context, events: Vec<Event>) {
            let _ = context.run_ui(input(events), |ui| {
                resizable_rail(ui, "resize_test_rail", 238.0, 8).show(ui, |ui| {
                    ui.set_min_width(ui.available_width());
                });
            });
        }

        fn render_details(context: &Context, events: Vec<Event>) {
            let _ = context.run_ui(input(events), |ui| {
                resizable_details_panel(ui, "resize_test_details").show(ui, |ui| {
                    ScrollArea::vertical()
                        .id_salt("resize_test_details_scroll")
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            ui.label("Details");
                        });
                });
            });
        }

        let rail_context = Context::default();
        render_rail(&rail_context, Vec::new());
        let rail_before = PanelState::load(&rail_context, Id::new("resize_test_rail"))
            .ok_or("rail panel state was not stored")?;
        let rail_handle = Pos2::new(
            rail_before.outer_rect.right(),
            rail_before.outer_rect.center().y,
        );
        let wider_rail = rail_handle + vec2(80.0, 0.0);
        render_rail(
            &rail_context,
            vec![
                Event::PointerMoved(rail_handle),
                pointer_button(rail_handle, true),
            ],
        );
        render_rail(&rail_context, vec![Event::PointerMoved(wider_rail)]);
        render_rail(&rail_context, vec![pointer_button(wider_rail, false)]);
        let rail_after = PanelState::load(&rail_context, Id::new("resize_test_rail"))
            .ok_or("resized rail panel state was not stored")?;
        assert!(rail_after.size().x >= rail_before.size().x + 70.0);

        let details_context = Context::default();
        render_details(&details_context, Vec::new());
        let details_before = PanelState::load(&details_context, Id::new("resize_test_details"))
            .ok_or("details panel state was not stored")?;
        let details_handle = Pos2::new(
            details_before.outer_rect.center().x,
            details_before.outer_rect.top(),
        );
        let taller_details = details_handle - vec2(0.0, 80.0);
        render_details(
            &details_context,
            vec![
                Event::PointerMoved(details_handle),
                pointer_button(details_handle, true),
            ],
        );
        render_details(&details_context, vec![Event::PointerMoved(taller_details)]);
        render_details(
            &details_context,
            vec![pointer_button(taller_details, false)],
        );
        let details_after = PanelState::load(&details_context, Id::new("resize_test_details"))
            .ok_or("resized details panel state was not stored")?;
        assert!(details_after.size().y >= details_before.size().y + 70.0);
        Ok(())
    }

    #[test]
    fn compact_multi_engine_selection_keeps_its_exit_controls()
    -> Result<(), Box<dyn std::error::Error>> {
        let context = Context::default();
        context.enable_accesskit();
        let mut app = UncleanApp::new(&context)?;
        app.view = View::MultiEngineSelection;
        let output = context.run_ui(
            RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(900.0, 600.0))),
                ..Default::default()
            },
            |ui| {
                app.render_multi_engine_selection(ui);
            },
        );
        let labels = output
            .platform_output
            .accesskit_update
            .into_iter()
            .flat_map(|update| update.nodes)
            .filter_map(|(_, node)| node.label().map(ToOwned::to_owned))
            .collect::<Vec<_>>();
        assert!(labels.iter().any(|label| label == "Back"));
        assert!(labels.iter().any(|label| label == "Review 0 engines"));
        Ok(())
    }
}
