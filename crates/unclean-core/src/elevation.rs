//! Defines and validates the narrow request accepted by the elevated writer.

use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::process;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::apply::{
    OperationReport, RestorePlan, TemplateRestorePlan, apply_revalidated_operation,
    apply_template_plan, build_restore_plan, build_template_restore_plan, restore_engine_plan,
    restore_template_plan,
};
use crate::descriptors::{DeclaredPluginState, DescriptorDocument};
use crate::discovery::{DiscoveryOptions, EngineHealth, EngineInstallation, discover_engines};
use crate::journal::{
    OperationTargetKind, load_journal, valid_sha256, validate_relative_plugin_path,
    validate_relative_target_path,
};
use crate::plans::{EnginePlan, PlanBuildOptions, sha256_hex};
use crate::projects::{ProjectDescriptorDocument, ProjectSuppressionEdit, ProjectSuppressionState};
use crate::templates::{TemplatePlan, build_template_plan};
use crate::{Error, Result};

/// Identifies the elevated request schema accepted by this build.
pub const ELEVATED_REQUEST_SCHEMA: u8 = 1;

/// Identifies the elevated result schema emitted by this build.
pub const ELEVATED_RESULT_SCHEMA: u8 = 1;

/// Names the internal frontend mode that hosts the elevated worker.
pub const ELEVATED_WORKER_COMMAND: &str = "__elevated-worker";

/// Names the internal option that supplies the elevated request file.
pub const ELEVATED_REQUEST_OPTION: &str = "--request";

const MAX_REQUEST_BYTES: u64 = 1_048_576;
const MAX_RESULT_BYTES: u64 = 1_048_576;
const MAX_TARGETS: usize = 4_096;
const MAX_REQUEST_LIFETIME_SECONDS: i64 = 300;
const MAX_LABEL_BYTES: usize = 512;

static ACCESS_PROBE_COUNTER: AtomicU64 = AtomicU64::new(0);

const fn engine_target_kind() -> OperationTargetKind {
    OperationTargetKind::Engine
}

/// Identifies the engine operation requested from the elevated worker.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ElevatedOperationKind {
    /// Applies reviewed plugin-state intent.
    Apply,
    /// Restores one journaled snapshot.
    Restore,
}

/// Identifies the only engine descriptor field the elevated worker may change.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum ElevatedField {
    /// Changes or removes the plugin default declaration.
    EnabledByDefault,
    /// Changes or removes the project-template suppression declaration.
    DisableEnginePluginsByDefault,
}

/// Records one reviewed state change without carrying output descriptor bytes.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ElevatedTargetIntent {
    /// Records the target path relative to the engine root.
    pub relative_path: PathBuf,
    /// Identifies the permitted descriptor field.
    pub field: ElevatedField,
    /// Records the reviewed source hash or an absent restore target.
    pub source_sha256: Option<String>,
    /// Records the requested engine-plugin state.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value_after: Option<DeclaredPluginState>,
    /// Records the requested project-template suppression state.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suppression_after: Option<ProjectSuppressionEdit>,
}

/// Carries bounded engine-state intent from a reviewed frontend to the elevated worker.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ElevatedRequest {
    /// Identifies the request schema.
    pub schema: u8,
    /// Identifies the operation and prevents replay after completion.
    pub request_id: String,
    /// Records the request creation time as Unix seconds.
    pub created_unix_seconds: i64,
    /// Records the final accepted time as Unix seconds.
    pub expires_unix_seconds: i64,
    /// Identifies apply or restore behavior.
    pub operation: ElevatedOperationKind,
    /// Identifies the descriptor boundary accepted by the worker.
    #[serde(default = "engine_target_kind")]
    pub target_kind: OperationTargetKind,
    /// Records the reviewed canonical engine root.
    pub engine_path: PathBuf,
    /// Records the reviewed engine version when discovery supplied one.
    pub engine_version: Option<String>,
    /// Names the preset associated with the reviewed operation.
    pub preset: String,
    /// Records the reviewed preset identity for recovery metadata.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preset_path: Option<PathBuf>,
    /// Identifies the reviewed source snapshot for restore.
    pub source_snapshot: Option<String>,
    /// Lists the complete reviewed target intent.
    pub targets: Vec<ElevatedTargetIntent>,
}

impl ElevatedRequest {
    /// Builds a short-lived request from one reviewed apply plan.
    ///
    /// # Errors
    ///
    /// Returns an error when the system clock cannot produce the request lifetime.
    pub fn from_engine_plan(plan: &EnginePlan) -> Result<Self> {
        let created = unix_time_now()?;
        Ok(Self {
            schema: ELEVATED_REQUEST_SCHEMA,
            request_id: plan.operation_id().to_owned(),
            created_unix_seconds: created,
            expires_unix_seconds: created + MAX_REQUEST_LIFETIME_SECONDS,
            operation: ElevatedOperationKind::Apply,
            target_kind: OperationTargetKind::Engine,
            engine_path: plan.engine().path.clone(),
            engine_version: plan.engine().version.clone(),
            preset: plan.preset().name.clone(),
            preset_path: Some(plan.preset().path.clone()),
            source_snapshot: None,
            targets: plan
                .changes()
                .iter()
                .map(|edit| ElevatedTargetIntent {
                    relative_path: edit.relative_path.clone(),
                    field: ElevatedField::EnabledByDefault,
                    source_sha256: Some(edit.sha256_before.clone()),
                    value_after: Some(edit.value_after),
                    suppression_after: None,
                })
                .collect(),
        })
    }

    /// Builds a short-lived request from one reviewed restore plan.
    ///
    /// # Errors
    ///
    /// Returns an error when the system clock cannot produce the request lifetime.
    pub fn from_restore_plan(plan: &RestorePlan) -> Result<Self> {
        let created = unix_time_now()?;
        Ok(Self {
            schema: ELEVATED_REQUEST_SCHEMA,
            request_id: plan.operation_id().to_owned(),
            created_unix_seconds: created,
            expires_unix_seconds: created + MAX_REQUEST_LIFETIME_SECONDS,
            operation: ElevatedOperationKind::Restore,
            target_kind: OperationTargetKind::Engine,
            engine_path: plan.engine().path.clone(),
            engine_version: plan.engine().version.clone(),
            preset: plan.preset().to_owned(),
            preset_path: Some(plan.preset_path().to_path_buf()),
            source_snapshot: Some(plan.source_snapshot().to_owned()),
            targets: plan
                .changes()
                .iter()
                .map(|edit| ElevatedTargetIntent {
                    relative_path: edit.relative_path.clone(),
                    field: ElevatedField::EnabledByDefault,
                    source_sha256: edit.sha256_before.clone(),
                    value_after: Some(edit.value_after),
                    suppression_after: None,
                })
                .collect(),
        })
    }

    /// Builds a short-lived request from one reviewed template apply plan.
    ///
    /// # Errors
    ///
    /// Returns an error when the system clock cannot produce the request lifetime.
    pub fn from_template_plan(plan: &TemplatePlan) -> Result<Self> {
        let created = unix_time_now()?;
        Ok(Self {
            schema: ELEVATED_REQUEST_SCHEMA,
            request_id: plan.operation_id().to_owned(),
            created_unix_seconds: created,
            expires_unix_seconds: created + MAX_REQUEST_LIFETIME_SECONDS,
            operation: ElevatedOperationKind::Apply,
            target_kind: OperationTargetKind::Template,
            engine_path: plan.engine().path.clone(),
            engine_version: plan.engine().version.clone(),
            preset: "Template suppression".to_owned(),
            preset_path: None,
            source_snapshot: None,
            targets: plan
                .changes()
                .iter()
                .map(|edit| ElevatedTargetIntent {
                    relative_path: edit.relative_path.clone(),
                    field: ElevatedField::DisableEnginePluginsByDefault,
                    source_sha256: Some(edit.sha256_before.clone()),
                    value_after: None,
                    suppression_after: Some(plan.suppression()),
                })
                .collect(),
        })
    }

    /// Builds a short-lived request from one reviewed template restore plan.
    ///
    /// # Errors
    ///
    /// Returns an error when the system clock or snapshot bytes cannot produce a request.
    pub fn from_template_restore_plan(plan: &TemplateRestorePlan) -> Result<Self> {
        let created = unix_time_now()?;
        let targets = plan
            .changes()
            .iter()
            .map(|edit| {
                Ok(ElevatedTargetIntent {
                    relative_path: edit.relative_path.clone(),
                    field: ElevatedField::DisableEnginePluginsByDefault,
                    source_sha256: edit.sha256_before.clone(),
                    value_after: None,
                    suppression_after: Some(suppression_edit_from_bytes(
                        edit.planned_bytes(),
                        &edit.relative_path,
                    )?),
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Self {
            schema: ELEVATED_REQUEST_SCHEMA,
            request_id: plan.operation_id().to_owned(),
            created_unix_seconds: created,
            expires_unix_seconds: created + MAX_REQUEST_LIFETIME_SECONDS,
            operation: ElevatedOperationKind::Restore,
            target_kind: OperationTargetKind::Template,
            engine_path: plan.engine().path.clone(),
            engine_version: plan.engine().version.clone(),
            preset: plan.preset().to_owned(),
            preset_path: None,
            source_snapshot: Some(plan.source_snapshot().to_owned()),
            targets,
        })
    }

    /// Renders the request as schema 1 JSON without output descriptor bytes.
    ///
    /// # Errors
    ///
    /// Returns an error when request serialization fails.
    pub fn render(&self) -> Result<Vec<u8>> {
        serde_json::to_vec(self).map_err(|error| Error::Internal {
            message: format!("elevated request serialization failed: {error}"),
        })
    }

    /// Parses a bounded request and rejects unknown fields or schemas.
    ///
    /// # Errors
    ///
    /// Returns an error when the input exceeds its limit or contains invalid JSON.
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > usize::try_from(MAX_REQUEST_BYTES).unwrap_or(usize::MAX) {
            return Err(Error::InvalidInput {
                message: format!("elevated request exceeds the {MAX_REQUEST_BYTES} byte limit"),
            });
        }
        serde_json::from_slice(bytes).map_err(|error| Error::InvalidInput {
            message: format!("elevated request JSON is invalid: {error}"),
        })
    }
}

/// Supplies trusted worker-owned paths and time for request execution.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ElevatedWorkerContext {
    /// Records the worker-derived backup root.
    pub backup_root: PathBuf,
    /// Records the worker-derived journal path.
    pub journal_path: PathBuf,
    /// Records the worker clock used for expiry checks.
    pub now_unix_seconds: i64,
}

impl ElevatedWorkerContext {
    /// Derives trusted paths and time from the elevated worker environment.
    ///
    /// # Errors
    ///
    /// Returns an error when application storage or the system clock is unavailable.
    pub fn for_current_process() -> Result<Self> {
        let app_data = trusted_app_data_root()?;
        let product_root = app_data.join("Unclean");
        Ok(Self {
            backup_root: product_root.join("backups"),
            journal_path: product_root.join("state.toml"),
            now_unix_seconds: unix_time_now()?,
        })
    }
}

/// Reports a structured elevated worker failure.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ElevatedFailure {
    /// Names the stable failure category.
    pub code: String,
    /// States the failure and recovery action.
    pub message: String,
    /// Records the process exit code.
    pub exit_code: u8,
}

/// Returns one machine-readable elevated worker result.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ElevatedResult {
    /// Identifies the result schema.
    pub schema: u8,
    /// Identifies the request.
    pub request_id: String,
    /// Reports whether the operation completed.
    pub ok: bool,
    /// Records a completed transaction.
    pub report: Option<OperationReport>,
    /// Records a typed worker failure.
    pub error: Option<ElevatedFailure>,
}

/// Identifies one active Unreal process tied to the selected engine.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ActiveUnrealProcess {
    /// Records the operating-system process identifier.
    pub process_id: u32,
    /// Names the recognized Unreal executable.
    pub executable: String,
    /// Records the resolved image path under the selected engine.
    pub image_path: PathBuf,
}

impl ElevatedResult {
    /// Converts one worker result into the shared operation report.
    ///
    /// # Errors
    ///
    /// Returns the typed worker failure when the elevated operation did not complete.
    pub fn into_report(self) -> Result<OperationReport> {
        if self.schema != ELEVATED_RESULT_SCHEMA {
            return Err(Error::InvalidInput {
                message: format!(
                    "this build supports elevated result schema {}; response uses schema {}",
                    ELEVATED_RESULT_SCHEMA, self.schema
                ),
            });
        }
        if self.ok {
            if self.error.is_some() {
                return Err(Error::InvalidInput {
                    message: "elevated result contains both success and failure data".to_owned(),
                });
            }
            self.report.ok_or_else(|| Error::Internal {
                message: "elevated result reports success without a transaction report".to_owned(),
            })
        } else {
            if self.report.is_some() {
                return Err(Error::InvalidInput {
                    message: "elevated result contains a report for a failed operation".to_owned(),
                });
            }
            let failure = self.error.ok_or_else(|| Error::Internal {
                message: "elevated result reports failure without an error".to_owned(),
            })?;
            Err(worker_failure(&failure))
        }
    }

    fn success(request_id: String, report: OperationReport) -> Self {
        Self {
            schema: ELEVATED_RESULT_SCHEMA,
            request_id,
            ok: true,
            report: Some(report),
            error: None,
        }
    }

    fn failure(request_id: String, error: &Error) -> Self {
        Self {
            schema: ELEVATED_RESULT_SCHEMA,
            request_id,
            ok: false,
            report: None,
            error: Some(ElevatedFailure {
                code: error.code().as_str().to_owned(),
                message: error.to_string(),
                exit_code: error.code().exit_code(),
            }),
        }
    }
}

/// Stores the worker-rederived bytes accepted by the transaction writer.
pub(crate) struct RevalidatedEngineOperation {
    pub(crate) request_id: String,
    pub(crate) engine: EngineInstallation,
    pub(crate) preset: String,
    pub(crate) preset_path: PathBuf,
    pub(crate) backup_directory: PathBuf,
    pub(crate) files: Vec<RevalidatedEngineFile>,
}

/// Stores one source-verified descriptor edit rebuilt by the worker.
pub(crate) struct RevalidatedEngineFile {
    pub(crate) target: PathBuf,
    pub(crate) relative_path: PathBuf,
    pub(crate) source_sha256: String,
    pub(crate) planned_sha256: String,
    pub(crate) value_before: DeclaredPluginState,
    pub(crate) value_after: DeclaredPluginState,
    pub(crate) planned_bytes: Vec<u8>,
}

/// Executes one parsed request using worker-owned paths and current filesystem state.
///
/// # Errors
///
/// Returns an error before backup when request identity, expiry, paths, hashes, or intent fail validation.
pub fn execute_elevated_request(
    request: &ElevatedRequest,
    context: &ElevatedWorkerContext,
) -> Result<OperationReport> {
    validate_request(request, context)?;
    let engine = revalidate_engine(request)?;
    validate_target_paths(request, &engine.path)?;
    let options = PlanBuildOptions::new(context.backup_root.clone(), request.request_id.clone())?;
    match (request.target_kind, request.operation) {
        (OperationTargetKind::Engine, ElevatedOperationKind::Apply) => {
            let operation = rederive_apply(request, engine, &options)?;
            apply_revalidated_operation(&operation, &context.journal_path)
        }
        (OperationTargetKind::Engine, ElevatedOperationKind::Restore) => {
            let snapshot = request_snapshot(request)?;
            let plan = build_restore_plan(&engine, snapshot, &context.journal_path, &options)?;
            compare_restore_intent(request, &plan)?;
            restore_engine_plan(&plan, &context.journal_path)
        }
        (OperationTargetKind::Template, ElevatedOperationKind::Apply) => {
            let selected = request
                .targets
                .iter()
                .map(|target| target.relative_path.clone())
                .collect::<Vec<_>>();
            let suppression = request
                .targets
                .first()
                .and_then(|target| target.suppression_after)
                .ok_or_else(|| Error::InvalidInput {
                    message: "template apply request has no suppression state".to_owned(),
                })?;
            let plan = build_template_plan(&engine, &selected, suppression, &options)?;
            compare_template_apply_intent(request, &plan)?;
            apply_template_plan(&plan, &context.journal_path)
        }
        (OperationTargetKind::Template, ElevatedOperationKind::Restore) => {
            let plan = build_template_restore_plan(
                &engine,
                request_snapshot(request)?,
                &context.journal_path,
                &options,
            )?;
            compare_template_restore_intent(request, &plan)?;
            restore_template_plan(&plan, &context.journal_path)
        }
        (OperationTargetKind::Project, _) => Err(Error::InvalidInput {
            message: "elevated requests do not accept project targets".to_owned(),
        }),
    }
}

fn request_snapshot(request: &ElevatedRequest) -> Result<&str> {
    request
        .source_snapshot
        .as_deref()
        .ok_or_else(|| Error::InvalidInput {
            message: "restore request has no source snapshot".to_owned(),
        })
}

/// Checks whether the current process can create replacement files beside each target.
///
/// The probe runs only after write confirmation and removes each empty probe before returning.
///
/// # Errors
///
/// Returns an error when a target path is invalid or a probe fails for a reason other than denied access.
pub fn write_access_requires_elevation(
    engine: &EngineInstallation,
    relative_paths: &[PathBuf],
) -> Result<bool> {
    let targets = relative_paths
        .iter()
        .map(|relative_path| validate_engine_target_path(&engine.path, relative_path, true))
        .collect::<Result<Vec<_>>>()?;
    targets_require_elevation(&targets)
}

/// Checks whether selected template descriptors require an elevated replacement process.
///
/// The probe runs only after write confirmation and removes each empty probe before returning.
///
/// # Errors
///
/// Returns an error when a template path is invalid or a probe cannot complete.
pub fn template_write_access_requires_elevation(
    engine: &EngineInstallation,
    relative_paths: &[PathBuf],
) -> Result<bool> {
    let targets = relative_paths
        .iter()
        .map(|relative_path| validate_template_target_path(&engine.path, relative_path, true))
        .collect::<Result<Vec<_>>>()?;
    targets_require_elevation(&targets)
}

fn targets_require_elevation(targets: &[PathBuf]) -> Result<bool> {
    for (index, target) in targets.iter().enumerate() {
        if target.is_file() {
            let metadata = fs::metadata(target).map_err(|error| Error::Internal {
                message: format!(
                    "target access metadata failed at {}: {error}",
                    target.display()
                ),
            })?;
            if !metadata.permissions().readonly() {
                match OpenOptions::new().write(true).open(target) {
                    Ok(_) => {}
                    Err(error) if error.kind() == ErrorKind::PermissionDenied => return Ok(true),
                    Err(error) => {
                        return Err(Error::Internal {
                            message: format!(
                                "target access check failed at {}: {error}",
                                target.display()
                            ),
                        });
                    }
                }
            }
        }
        let parent = target.parent().ok_or_else(|| Error::InvalidInput {
            message: format!("target has no parent: {}", target.display()),
        })?;
        let counter = ACCESS_PROBE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let probe = parent.join(format!(
            ".unclean-access-{}-{counter}-{index}.tmp",
            process::id()
        ));
        match OpenOptions::new().write(true).create_new(true).open(&probe) {
            Ok(file) => drop(file),
            Err(error) if error.kind() == ErrorKind::PermissionDenied => return Ok(true),
            Err(error) => {
                return Err(Error::WriteFailed {
                    message: format!("write-access probe failed at {}: {error}", probe.display()),
                });
            }
        }
        fs::remove_file(&probe).map_err(|error| Error::WriteFailed {
            message: format!(
                "write-access probe cleanup failed at {}: {error}",
                probe.display()
            ),
        })?;
    }
    Ok(false)
}

/// Lists active Unreal editor, build, and automation processes under one engine root.
///
/// # Errors
///
/// Returns an error when Windows process enumeration cannot start.
pub fn find_active_unreal_processes(engine_root: &Path) -> Result<Vec<ActiveUnrealProcess>> {
    #[cfg(windows)]
    {
        find_active_unreal_processes_platform(engine_root)
    }
    #[cfg(not(windows))]
    {
        let _ = engine_root;
        Ok(Vec::new())
    }
}

/// Creates request files, launches the elevated worker, and returns its structured result.
///
/// # Errors
///
/// Returns an error when request preparation, UAC launch, worker execution, or result validation fails.
pub fn run_elevated_request(request: &ElevatedRequest) -> Result<OperationReport> {
    let files = PreparedRequestFiles::create(request)?;
    let executable = std::env::current_exe().map_err(|error| Error::Internal {
        message: format!("current executable lookup failed: {error}"),
    })?;
    let exit_code = launch_elevated_worker(&executable, &files.request_path)?;
    let result = files.read_result()?;
    if result.request_id != request.request_id {
        return Err(Error::Conflict {
            message: "elevated result identifier does not match the request".to_owned(),
        });
    }
    if exit_code != result.error.as_ref().map_or(0, |failure| failure.exit_code) {
        return Err(Error::Internal {
            message: format!("elevated worker exit code {exit_code} does not match its result"),
        });
    }
    result.into_report()
}

/// Executes one request file and writes its structured sibling result.
///
/// Returns the process exit code that matches the result body.
#[must_use]
pub fn run_elevated_worker(request_path: &Path) -> u8 {
    let request_id = request_path
        .parent()
        .and_then(Path::file_name)
        .and_then(|value| value.to_str())
        .unwrap_or("invalid-request")
        .to_owned();
    let outcome = read_request_file(request_path).and_then(|request| {
        let context = ElevatedWorkerContext::for_current_process()?;
        let report = execute_elevated_request(&request, &context)?;
        Ok((request.request_id, report))
    });
    let result = match outcome {
        Ok((id, report)) => ElevatedResult::success(id, report),
        Err(error) => ElevatedResult::failure(request_id, &error),
    };
    let exit_code = result.error.as_ref().map_or(0, |error| error.exit_code);
    if write_worker_result(request_path, &result).is_err() {
        return crate::ErrorCode::Internal.exit_code();
    }
    exit_code
}

fn validate_request(request: &ElevatedRequest, context: &ElevatedWorkerContext) -> Result<()> {
    if request.schema != ELEVATED_REQUEST_SCHEMA {
        return Err(Error::InvalidInput {
            message: format!(
                "this build supports elevated request schema {}; file uses schema {}",
                ELEVATED_REQUEST_SCHEMA, request.schema
            ),
        });
    }
    if !valid_identifier(&request.request_id) {
        return Err(Error::InvalidInput {
            message: "elevated request identifier is invalid".to_owned(),
        });
    }
    validate_request_lifetime(request, context.now_unix_seconds)?;
    if request.targets.is_empty() || request.targets.len() > MAX_TARGETS {
        return Err(Error::InvalidInput {
            message: format!("elevated request must contain 1 to {MAX_TARGETS} targets"),
        });
    }
    if !request.engine_path.is_absolute()
        || request
            .preset_path
            .as_ref()
            .is_some_and(|path| !path.is_absolute())
    {
        return Err(Error::InvalidInput {
            message: "elevated request engine and preset paths must be absolute when present"
                .to_owned(),
        });
    }
    match request.target_kind {
        OperationTargetKind::Engine if request.preset_path.is_none() => {
            return Err(Error::InvalidInput {
                message: "engine request has no preset path".to_owned(),
            });
        }
        OperationTargetKind::Template if request.preset_path.is_some() => {
            return Err(Error::InvalidInput {
                message: "template request contains an engine preset path".to_owned(),
            });
        }
        OperationTargetKind::Project => {
            return Err(Error::InvalidInput {
                message: "elevated requests do not accept project targets".to_owned(),
            });
        }
        OperationTargetKind::Engine | OperationTargetKind::Template => {}
    }
    validate_label("preset", &request.preset)?;
    if request
        .engine_version
        .as_deref()
        .is_some_and(|value| value.len() > MAX_LABEL_BYTES)
    {
        return Err(Error::InvalidInput {
            message: "elevated request engine version is too long".to_owned(),
        });
    }
    validate_request_operation(request)?;
    if load_journal(&context.journal_path)?
        .operations
        .iter()
        .any(|operation| operation.id == request.request_id)
    {
        return Err(Error::Conflict {
            message: format!(
                "elevated request {} has already completed",
                request.request_id
            ),
        });
    }
    validate_request_targets(request)
}

fn validate_request_lifetime(request: &ElevatedRequest, now_unix_seconds: i64) -> Result<()> {
    if request.created_unix_seconds > now_unix_seconds
        || request.expires_unix_seconds < now_unix_seconds
        || request.expires_unix_seconds <= request.created_unix_seconds
        || request.expires_unix_seconds - request.created_unix_seconds
            > MAX_REQUEST_LIFETIME_SECONDS
    {
        return Err(Error::Conflict {
            message: "elevated request is expired or has an invalid lifetime".to_owned(),
        });
    }
    Ok(())
}

fn validate_request_operation(request: &ElevatedRequest) -> Result<()> {
    match request.operation {
        ElevatedOperationKind::Apply => {
            if request.source_snapshot.is_some()
                || request
                    .targets
                    .iter()
                    .any(|target| target.source_sha256.is_none())
            {
                return Err(Error::InvalidInput {
                    message: "apply request has restore-only fields or an absent source hash"
                        .to_owned(),
                });
            }
        }
        ElevatedOperationKind::Restore => {
            let snapshot =
                request
                    .source_snapshot
                    .as_deref()
                    .ok_or_else(|| Error::InvalidInput {
                        message: "restore request has no source snapshot".to_owned(),
                    })?;
            if !valid_identifier(snapshot) {
                return Err(Error::InvalidInput {
                    message: "restore request snapshot identifier is invalid".to_owned(),
                });
            }
        }
    }
    Ok(())
}

fn validate_request_targets(request: &ElevatedRequest) -> Result<()> {
    let mut targets = HashSet::with_capacity(request.targets.len());
    let mut template_suppression = None;
    for target in &request.targets {
        match request.target_kind {
            OperationTargetKind::Engine => {
                validate_relative_plugin_path(&target.relative_path)?;
                if target.field != ElevatedField::EnabledByDefault
                    || target.value_after.is_none()
                    || target.suppression_after.is_some()
                {
                    return Err(Error::InvalidInput {
                        message: format!(
                            "engine target has unsupported intent for {}",
                            target.relative_path.display()
                        ),
                    });
                }
            }
            OperationTargetKind::Template => {
                validate_relative_target_path(
                    OperationTargetKind::Template,
                    &target.relative_path,
                )?;
                let suppression = target
                    .suppression_after
                    .ok_or_else(|| Error::InvalidInput {
                        message: format!(
                            "template target has no suppression intent for {}",
                            target.relative_path.display()
                        ),
                    })?;
                if target.field != ElevatedField::DisableEnginePluginsByDefault
                    || target.value_after.is_some()
                    || suppression == ProjectSuppressionEdit::Keep
                {
                    return Err(Error::InvalidInput {
                        message: format!(
                            "template target has unsupported intent for {}",
                            target.relative_path.display()
                        ),
                    });
                }
                if template_suppression
                    .replace(suppression)
                    .is_some_and(|current| current != suppression)
                {
                    return Err(Error::InvalidInput {
                        message: "template targets contain different suppression states".to_owned(),
                    });
                }
            }
            OperationTargetKind::Project => {
                return Err(Error::InvalidInput {
                    message: "elevated requests do not accept project targets".to_owned(),
                });
            }
        }
        if !targets.insert(normalized_relative_path(&target.relative_path)) {
            return Err(Error::InvalidInput {
                message: format!(
                    "elevated request repeats target {}",
                    target.relative_path.display()
                ),
            });
        }
        if target
            .source_sha256
            .as_deref()
            .is_some_and(|hash| !valid_sha256(hash))
        {
            return Err(Error::InvalidInput {
                message: format!(
                    "elevated request has an invalid source hash for {}",
                    target.relative_path.display()
                ),
            });
        }
    }
    Ok(())
}

fn revalidate_engine(request: &ElevatedRequest) -> Result<EngineInstallation> {
    reject_reparse_components(&request.engine_path)?;
    let report = discover_engines(&DiscoveryOptions {
        explicit_paths: vec![request.engine_path.clone()],
        current_dir: None,
        launcher_manifest: None,
        include_registry: false,
    });
    let mut engine = report
        .engines
        .into_iter()
        .next()
        .ok_or_else(|| Error::NotFound {
            item: format!("engine installation {}", request.engine_path.display()),
        })?;
    if engine.health == EngineHealth::Unavailable {
        return Err(Error::InvalidInput {
            message: format!(
                "elevated request engine has no recognized Engine\\Plugins layout: {}",
                request.engine_path.display()
            ),
        });
    }
    let requested_root =
        resolve_final_path(&request.engine_path).map_err(|error| Error::InvalidInput {
            message: format!("elevated request engine root resolution failed: {error}"),
        })?;
    let discovered_root =
        resolve_final_path(&engine.path).map_err(|error| Error::InvalidInput {
            message: format!("discovered engine root resolution failed: {error}"),
        })?;
    if !paths_match(&requested_root, &discovered_root)
        || engine
            .version
            .as_ref()
            .is_some_and(|version| Some(version) != request.engine_version.as_ref())
    {
        return Err(Error::Conflict {
            message: "elevated request engine identity changed after review".to_owned(),
        });
    }
    if engine.version.is_none() {
        engine.version.clone_from(&request.engine_version);
    }
    Ok(engine)
}

fn validate_target_paths(request: &ElevatedRequest, engine_root: &Path) -> Result<()> {
    for target in &request.targets {
        match request.target_kind {
            OperationTargetKind::Engine => {
                validate_engine_target_path(
                    engine_root,
                    &target.relative_path,
                    target.source_sha256.is_none(),
                )?;
            }
            OperationTargetKind::Template => {
                validate_template_target_path(
                    engine_root,
                    &target.relative_path,
                    target.source_sha256.is_none(),
                )?;
            }
            OperationTargetKind::Project => {
                return Err(Error::InvalidInput {
                    message: "elevated requests do not accept project targets".to_owned(),
                });
            }
        }
    }
    Ok(())
}

pub(crate) fn validate_engine_target_path(
    engine_root: &Path,
    relative_path: &Path,
    allow_missing: bool,
) -> Result<PathBuf> {
    validate_relative_plugin_path(relative_path)?;
    let plugin_root =
        resolve_final_path(&engine_root.join("Engine").join("Plugins")).map_err(|error| {
            Error::InvalidInput {
                message: format!("engine plugin root resolution failed: {error}"),
            }
        })?;
    let path = engine_root.join(relative_path);
    reject_reparse_components(&path)?;
    let resolved = match resolve_final_path(&path) {
        Ok(path) => path,
        Err(error) if error.kind() == ErrorKind::NotFound && allow_missing => {
            let parent = path.parent().ok_or_else(|| Error::InvalidInput {
                message: format!("target has no parent: {}", path.display()),
            })?;
            resolve_final_path(parent).map_err(|parent_error| Error::InvalidInput {
                message: format!(
                    "target parent resolution failed for {}: {parent_error}",
                    path.display()
                ),
            })?
        }
        Err(error) => {
            return Err(Error::InvalidInput {
                message: format!("target resolution failed for {}: {error}", path.display()),
            });
        }
    };
    if !path_starts_with(&resolved, &plugin_root) {
        return Err(Error::InvalidInput {
            message: format!(
                "elevated target leaves the recognized plugin root: {}",
                relative_path.display()
            ),
        });
    }
    Ok(path)
}

pub(crate) fn validate_template_target_path(
    engine_root: &Path,
    relative_path: &Path,
    allow_missing: bool,
) -> Result<PathBuf> {
    validate_relative_target_path(OperationTargetKind::Template, relative_path)?;
    let template_root = resolve_final_path(&engine_root.join("Templates")).map_err(|error| {
        Error::InvalidInput {
            message: format!("engine template root resolution failed: {error}"),
        }
    })?;
    let path = engine_root.join(relative_path);
    reject_reparse_components(&path)?;
    let resolved = match resolve_final_path(&path) {
        Ok(path) => path,
        Err(error) if error.kind() == ErrorKind::NotFound && allow_missing => {
            let parent = path.parent().ok_or_else(|| Error::InvalidInput {
                message: format!("template target has no parent: {}", path.display()),
            })?;
            resolve_final_path(parent).map_err(|parent_error| Error::InvalidInput {
                message: format!(
                    "template target parent resolution failed for {}: {parent_error}",
                    path.display()
                ),
            })?
        }
        Err(error) => {
            return Err(Error::InvalidInput {
                message: format!(
                    "template target resolution failed for {}: {error}",
                    path.display()
                ),
            });
        }
    };
    if !path_starts_with(&resolved, &template_root) {
        return Err(Error::InvalidInput {
            message: format!(
                "elevated target leaves the recognized template root: {}",
                relative_path.display()
            ),
        });
    }
    Ok(path)
}

pub(crate) fn validate_project_target_path(project_path: &Path) -> Result<PathBuf> {
    if !project_path.is_absolute()
        || !project_path
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("uproject"))
    {
        return Err(Error::InvalidInput {
            message: format!(
                "project target is not one absolute .uproject path: {}",
                project_path.display()
            ),
        });
    }
    reject_reparse_components(project_path)?;
    let resolved = resolve_final_path(project_path).map_err(|error| Error::InvalidInput {
        message: format!(
            "project target resolution failed for {}: {error}",
            project_path.display()
        ),
    })?;
    Ok(resolved)
}

fn rederive_apply(
    request: &ElevatedRequest,
    engine: EngineInstallation,
    options: &PlanBuildOptions,
) -> Result<RevalidatedEngineOperation> {
    let backup_directory = options.backup_directory(&engine);
    let preset_path = request
        .preset_path
        .clone()
        .ok_or_else(|| Error::InvalidInput {
            message: "engine apply request has no preset path".to_owned(),
        })?;
    let mut files = Vec::with_capacity(request.targets.len());
    for intent in &request.targets {
        let value_after = intent.value_after.ok_or_else(|| Error::InvalidInput {
            message: format!(
                "engine target has no requested state for {}",
                intent.relative_path.display()
            ),
        })?;
        let target = engine.path.join(&intent.relative_path);
        let source = fs::read(&target).map_err(|error| match error.kind() {
            ErrorKind::NotFound => Error::NotFound {
                item: format!("elevated target {}", target.display()),
            },
            ErrorKind::PermissionDenied => Error::PermissionDenied {
                message: format!("Unclean cannot read elevated target {}", target.display()),
            },
            _ => Error::Internal {
                message: format!("elevated target read failed: {error}"),
            },
        })?;
        let source_sha256 = sha256_hex(&source);
        if intent.source_sha256.as_deref() != Some(source_sha256.as_str()) {
            return Err(Error::Conflict {
                message: format!(
                    "elevated target changed after review: {}",
                    intent.relative_path.display()
                ),
            });
        }
        let document = DescriptorDocument::parse(&source).map_err(|error| Error::InvalidInput {
            message: format!(
                "elevated target descriptor is invalid for {}: {error}",
                intent.relative_path.display()
            ),
        })?;
        let value_before = document.declared_state();
        if value_before == value_after {
            return Err(Error::Conflict {
                message: format!(
                    "elevated target no longer requires the reviewed state change: {}",
                    intent.relative_path.display()
                ),
            });
        }
        let planned_bytes = document
            .edit_enabled_by_default(value_after)
            .map_err(|error| Error::InvalidInput {
                message: format!(
                    "elevated target edit failed for {}: {error}",
                    intent.relative_path.display()
                ),
            })?;
        let verified =
            DescriptorDocument::parse(&planned_bytes).map_err(|error| Error::InvalidInput {
                message: format!(
                    "elevated target output is invalid for {}: {error}",
                    intent.relative_path.display()
                ),
            })?;
        if verified.declared_state() != value_after {
            return Err(Error::Internal {
                message: format!(
                    "elevated target output has the wrong state for {}",
                    intent.relative_path.display()
                ),
            });
        }
        files.push(RevalidatedEngineFile {
            target,
            relative_path: intent.relative_path.clone(),
            source_sha256,
            planned_sha256: sha256_hex(&planned_bytes),
            value_before,
            value_after,
            planned_bytes,
        });
    }
    Ok(RevalidatedEngineOperation {
        request_id: request.request_id.clone(),
        engine,
        preset: request.preset.clone(),
        preset_path,
        backup_directory,
        files,
    })
}

fn compare_restore_intent(request: &ElevatedRequest, plan: &RestorePlan) -> Result<()> {
    if request.targets.len() != plan.changes().len()
        || !request
            .targets
            .iter()
            .zip(plan.changes())
            .all(|(intent, edit)| {
                intent.relative_path == edit.relative_path
                    && intent.source_sha256 == edit.sha256_before
                    && intent.value_after == Some(edit.value_after)
            })
    {
        return Err(Error::Conflict {
            message: "restore state changed after review".to_owned(),
        });
    }
    Ok(())
}

fn compare_template_apply_intent(request: &ElevatedRequest, plan: &TemplatePlan) -> Result<()> {
    if request.targets.len() != plan.changes().len()
        || !request
            .targets
            .iter()
            .zip(plan.changes())
            .all(|(intent, edit)| {
                intent.relative_path == edit.relative_path
                    && intent.source_sha256.as_deref() == Some(edit.sha256_before.as_str())
                    && intent.suppression_after == Some(plan.suppression())
            })
    {
        return Err(Error::Conflict {
            message: "template apply state changed after review".to_owned(),
        });
    }
    Ok(())
}

fn compare_template_restore_intent(
    request: &ElevatedRequest,
    plan: &TemplateRestorePlan,
) -> Result<()> {
    if request.targets.len() != plan.changes().len() {
        return Err(Error::Conflict {
            message: "template restore state changed after review".to_owned(),
        });
    }
    for (intent, edit) in request.targets.iter().zip(plan.changes()) {
        let suppression = suppression_edit_from_bytes(edit.planned_bytes(), &edit.relative_path)?;
        if intent.relative_path != edit.relative_path
            || intent.source_sha256 != edit.sha256_before
            || intent.suppression_after != Some(suppression)
        {
            return Err(Error::Conflict {
                message: "template restore state changed after review".to_owned(),
            });
        }
    }
    Ok(())
}

fn suppression_edit_from_bytes(bytes: &[u8], path: &Path) -> Result<ProjectSuppressionEdit> {
    let document =
        ProjectDescriptorDocument::parse(bytes).map_err(|error| Error::InvalidInput {
            message: format!(
                "Template descriptor is invalid for {}: {error}. Repair the descriptor and retry.",
                path.display()
            ),
        })?;
    Ok(match document.project_descriptor(path).suppression {
        ProjectSuppressionState::Enabled => ProjectSuppressionEdit::Set(true),
        ProjectSuppressionState::Disabled => ProjectSuppressionEdit::Set(false),
        ProjectSuppressionState::Unspecified => ProjectSuppressionEdit::Clear,
    })
}

fn read_request_file(path: &Path) -> Result<ElevatedRequest> {
    validate_request_file_path(path)?;
    let file = OpenOptions::new()
        .read(true)
        .open(path)
        .map_err(|error| Error::InvalidInput {
            message: format!("elevated request file open failed: {error}"),
        })?;
    let length = file
        .metadata()
        .map_err(|error| Error::InvalidInput {
            message: format!("elevated request metadata read failed: {error}"),
        })?
        .len();
    if length > MAX_REQUEST_BYTES {
        return Err(Error::InvalidInput {
            message: format!("elevated request exceeds the {MAX_REQUEST_BYTES} byte limit"),
        });
    }
    let mut bytes = Vec::with_capacity(usize::try_from(length).unwrap_or(0));
    file.take(MAX_REQUEST_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| Error::InvalidInput {
            message: format!("elevated request file read failed: {error}"),
        })?;
    let request = ElevatedRequest::parse(&bytes)?;
    let directory_id = path
        .parent()
        .and_then(Path::file_name)
        .and_then(|value| value.to_str());
    if directory_id != Some(request.request_id.as_str()) {
        return Err(Error::InvalidInput {
            message: "elevated request identifier does not match its directory".to_owned(),
        });
    }
    Ok(request)
}

fn write_worker_result(request_path: &Path, result: &ElevatedResult) -> Result<()> {
    validate_request_file_path(request_path)?;
    let result_path = result_path(request_path)?;
    let bytes = serde_json::to_vec(result).map_err(|error| Error::Internal {
        message: format!("elevated result serialization failed: {error}"),
    })?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&result_path)
        .map_err(|error| Error::WriteFailed {
            message: format!(
                "elevated result file creation failed at {}: {error}",
                result_path.display()
            ),
        })?;
    file.write_all(&bytes).map_err(|error| Error::WriteFailed {
        message: format!(
            "elevated result write failed at {}: {error}",
            result_path.display()
        ),
    })?;
    file.sync_all().map_err(|error| Error::WriteFailed {
        message: format!(
            "elevated result sync failed at {}: {error}",
            result_path.display()
        ),
    })
}

struct PreparedRequestFiles {
    directory: PathBuf,
    request_path: PathBuf,
    result_path: PathBuf,
}

impl PreparedRequestFiles {
    fn create(request: &ElevatedRequest) -> Result<Self> {
        let root = default_elevation_root()?;
        fs::create_dir_all(&root).map_err(|error| Error::WriteFailed {
            message: format!(
                "elevation directory creation failed at {}: {error}",
                root.display()
            ),
        })?;
        reject_reparse_components(&root)?;
        let directory = root.join(&request.request_id);
        fs::create_dir(&directory).map_err(|error| {
            if error.kind() == ErrorKind::AlreadyExists {
                Error::Conflict {
                    message: format!(
                        "elevated request directory already exists: {}",
                        directory.display()
                    ),
                }
            } else {
                Error::WriteFailed {
                    message: format!(
                        "elevated request directory creation failed at {}: {error}",
                        directory.display()
                    ),
                }
            }
        })?;
        let request_path = directory.join("request.json");
        let result_path = directory.join("result.json");
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&request_path)
            .map_err(|error| Error::WriteFailed {
                message: format!(
                    "elevated request file creation failed at {}: {error}",
                    request_path.display()
                ),
            })?;
        file.write_all(&request.render()?)
            .map_err(|error| Error::WriteFailed {
                message: format!(
                    "elevated request write failed at {}: {error}",
                    request_path.display()
                ),
            })?;
        file.sync_all().map_err(|error| Error::WriteFailed {
            message: format!(
                "elevated request sync failed at {}: {error}",
                request_path.display()
            ),
        })?;
        Ok(Self {
            directory,
            request_path,
            result_path,
        })
    }

    fn read_result(&self) -> Result<ElevatedResult> {
        let file = OpenOptions::new()
            .read(true)
            .open(&self.result_path)
            .map_err(|error| Error::Internal {
                message: format!(
                    "elevated worker result open failed at {}: {error}",
                    self.result_path.display()
                ),
            })?;
        let length = file
            .metadata()
            .map_err(|error| Error::Internal {
                message: format!(
                    "elevated worker result metadata read failed at {}: {error}",
                    self.result_path.display()
                ),
            })?
            .len();
        if length > MAX_RESULT_BYTES {
            return Err(Error::InvalidInput {
                message: format!("elevated result exceeds the {MAX_RESULT_BYTES} byte limit"),
            });
        }
        let mut bytes = Vec::with_capacity(usize::try_from(length).unwrap_or(0));
        file.take(MAX_RESULT_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| Error::Internal {
                message: format!(
                    "elevated worker result read failed at {}: {error}",
                    self.result_path.display()
                ),
            })?;
        if bytes.len() > usize::try_from(MAX_RESULT_BYTES).unwrap_or(usize::MAX) {
            return Err(Error::InvalidInput {
                message: format!("elevated result exceeds the {MAX_RESULT_BYTES} byte limit"),
            });
        }
        serde_json::from_slice(&bytes).map_err(|error| Error::InvalidInput {
            message: format!(
                "elevated result JSON is invalid at {}: {error}",
                self.result_path.display()
            ),
        })
    }
}

impl Drop for PreparedRequestFiles {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.request_path);
        let _ = fs::remove_file(&self.result_path);
        let _ = fs::remove_dir(&self.directory);
    }
}

fn validate_request_file_path(path: &Path) -> Result<()> {
    if path.file_name().and_then(|value| value.to_str()) != Some("request.json") {
        return Err(Error::InvalidInput {
            message: "elevated worker accepts only a request.json file".to_owned(),
        });
    }
    let root =
        fs::canonicalize(default_elevation_root()?).map_err(|error| Error::InvalidInput {
            message: format!("elevation root resolution failed: {error}"),
        })?;
    reject_reparse_components(path)?;
    let parent = path.parent().ok_or_else(|| Error::InvalidInput {
        message: "elevated request path has no parent".to_owned(),
    })?;
    let resolved_parent = fs::canonicalize(parent).map_err(|error| Error::InvalidInput {
        message: format!("elevated request directory resolution failed: {error}"),
    })?;
    if resolved_parent
        .parent()
        .is_none_or(|value| !paths_match(value, &root))
    {
        return Err(Error::InvalidInput {
            message: "elevated request file leaves the trusted elevation root".to_owned(),
        });
    }
    Ok(())
}

fn result_path(request_path: &Path) -> Result<PathBuf> {
    request_path
        .parent()
        .map(|parent| parent.join("result.json"))
        .ok_or_else(|| Error::InvalidInput {
            message: "elevated request path has no parent".to_owned(),
        })
}

fn default_elevation_root() -> Result<PathBuf> {
    Ok(trusted_app_data_root()?.join("Unclean").join("elevation"))
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn trusted_app_data_root() -> Result<PathBuf> {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;
    use std::ptr;
    use std::slice;

    use windows_sys::Win32::Foundation::S_OK;
    use windows_sys::Win32::System::Com::CoTaskMemFree;
    use windows_sys::Win32::UI::Shell::{FOLDERID_RoamingAppData, SHGetKnownFolderPath};

    unsafe extern "C" {
        fn wcslen(buffer: *const u16) -> usize;
    }

    let folder_id = FOLDERID_RoamingAppData;
    let mut raw_path = ptr::null_mut();
    // SAFETY: Windows allocates the returned NUL-terminated path and accepts null optional arguments.
    let result = unsafe {
        SHGetKnownFolderPath(&raw const folder_id, 0, ptr::null_mut(), &raw mut raw_path)
    };
    if result != S_OK || raw_path.is_null() {
        // SAFETY: CoTaskMemFree accepts a null pointer and owns any failed-call allocation.
        unsafe { CoTaskMemFree(raw_path.cast()) };
        return Err(Error::Internal {
            message: format!("Windows roaming application data lookup failed with {result:#x}"),
        });
    }
    // SAFETY: A successful known-folder lookup returns an allocated NUL-terminated UTF-16 path.
    let path = unsafe {
        let length = wcslen(raw_path);
        PathBuf::from(OsString::from_wide(slice::from_raw_parts(raw_path, length)))
    };
    // SAFETY: SHGetKnownFolderPath allocated this pointer and it closes once after conversion.
    unsafe { CoTaskMemFree(raw_path.cast()) };
    Ok(path)
}

#[cfg(not(windows))]
fn trusted_app_data_root() -> Result<PathBuf> {
    std::env::var_os("APPDATA")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| Error::InvalidInput {
            message: "Unclean cannot locate application data for elevation request files"
                .to_owned(),
        })
}

fn reject_reparse_components(path: &Path) -> Result<()> {
    for current in path.ancestors() {
        match fs::symlink_metadata(current) {
            Ok(metadata) if metadata_is_reparse_point(&metadata) => {
                return Err(Error::InvalidInput {
                    message: format!(
                        "elevated path contains a reparse point: {}",
                        current.display()
                    ),
                });
            }
            Ok(_) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => {
                return Err(Error::InvalidInput {
                    message: format!(
                        "elevated path metadata read failed for {}: {error}",
                        current.display()
                    ),
                });
            }
        }
    }
    Ok(())
}

#[cfg(windows)]
fn metadata_is_reparse_point(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn metadata_is_reparse_point(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn resolve_final_path(path: &Path) -> std::io::Result<PathBuf> {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;
    use std::ptr;

    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_SHARE_DELETE, FILE_SHARE_READ,
        FILE_SHARE_WRITE, GetFinalPathNameByHandleW, OPEN_EXISTING,
    };

    let path = wide_path(path).map_err(|error| std::io::Error::other(error.to_string()))?;
    // SAFETY: This function passes a NUL-terminated path and null security data, then owns the returned handle.
    let handle = unsafe {
        CreateFileW(
            path.as_ptr(),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error());
    }
    let mut buffer = vec![0_u16; 32_768];
    // SAFETY: The open handle and buffer length describe writable UTF-16 storage.
    let length = unsafe {
        GetFinalPathNameByHandleW(
            handle,
            buffer.as_mut_ptr(),
            u32::try_from(buffer.len()).unwrap_or(u32::MAX),
            0,
        )
    };
    // SAFETY: This function owns the handle and closes it once.
    unsafe { CloseHandle(handle) };
    if length == 0 {
        return Err(std::io::Error::last_os_error());
    }
    let length = usize::try_from(length).unwrap_or(usize::MAX);
    if length >= buffer.len() {
        return Err(std::io::Error::new(
            ErrorKind::InvalidData,
            "resolved Windows path exceeds the supported length",
        ));
    }
    buffer.truncate(length);
    Ok(PathBuf::from(OsString::from_wide(&buffer)))
}

#[cfg(not(windows))]
fn resolve_final_path(path: &Path) -> std::io::Result<PathBuf> {
    fs::canonicalize(path)
}

fn path_starts_with(path: &Path, root: &Path) -> bool {
    if cfg!(windows) {
        let path = path
            .to_string_lossy()
            .replace('/', "\\")
            .to_ascii_lowercase();
        let root = root
            .to_string_lossy()
            .replace('/', "\\")
            .trim_end_matches('\\')
            .to_ascii_lowercase();
        path == root || path.starts_with(&format!("{root}\\"))
    } else {
        path.starts_with(root)
    }
}

fn paths_match(left: &Path, right: &Path) -> bool {
    if cfg!(windows) {
        left.to_string_lossy()
            .replace('/', "\\")
            .eq_ignore_ascii_case(&right.to_string_lossy().replace('/', "\\"))
    } else {
        left == right
    }
}

fn normalized_relative_path(path: &Path) -> String {
    if cfg!(windows) {
        path.to_string_lossy()
            .replace('/', "\\")
            .to_ascii_lowercase()
    } else {
        path.to_string_lossy().into_owned()
    }
}

fn validate_label(name: &str, value: &str) -> Result<()> {
    if value.is_empty() || value.len() > MAX_LABEL_BYTES {
        return Err(Error::InvalidInput {
            message: format!("elevated request {name} must contain 1 to {MAX_LABEL_BYTES} bytes"),
        });
    }
    Ok(())
}

fn valid_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value != "."
        && value != ".."
        && value.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_')
        })
}

fn unix_time_now() -> Result<i64> {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| Error::Internal {
            message: format!("the system clock cannot create an elevated request: {error}"),
        })?;
    i64::try_from(duration.as_secs()).map_err(|_| Error::Internal {
        message: "the system clock exceeds the elevated request range".to_owned(),
    })
}

fn worker_failure(failure: &ElevatedFailure) -> Error {
    let code = match failure.code.as_str() {
        "invalid_input" => crate::ErrorCode::InvalidInput,
        "not_found" => crate::ErrorCode::NotFound,
        "conflict" => crate::ErrorCode::Conflict,
        "drift" => crate::ErrorCode::Drift,
        "permission_denied" => crate::ErrorCode::PermissionDenied,
        "write_failed" => crate::ErrorCode::WriteFailed,
        "rollback_incomplete" => crate::ErrorCode::RollbackIncomplete,
        "unavailable" => crate::ErrorCode::Unavailable,
        _ => crate::ErrorCode::Internal,
    };
    Error::WorkerFailure {
        code,
        message: failure.message.clone(),
    }
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn find_active_unreal_processes_platform(engine_root: &Path) -> Result<Vec<ActiveUnrealProcess>> {
    use std::mem;

    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    };
    // SAFETY: The snapshot call has no pointer arguments and returns an owned handle.
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Err(Error::Internal {
            message: format!(
                "process enumeration failed: {}",
                std::io::Error::last_os_error()
            ),
        });
    }
    // SAFETY: Windows requires a zeroed entry with dwSize initialized before enumeration.
    let mut entry = unsafe { mem::zeroed::<PROCESSENTRY32W>() };
    entry.dwSize = u32::try_from(mem::size_of::<PROCESSENTRY32W>()).unwrap_or(u32::MAX);
    let mut matches = Vec::new();
    // SAFETY: The snapshot handle and writable entry remain valid for the enumeration loop.
    let mut has_entry = unsafe { Process32FirstW(snapshot, &raw mut entry) } != 0;
    while has_entry {
        if let Some(image_path) = process_image_path(entry.th32ProcessID) {
            let executable = image_path
                .file_name()
                .and_then(|value| value.to_str())
                .unwrap_or_default();
            if is_relevant_unreal_process(executable, &image_path, engine_root) {
                matches.push(ActiveUnrealProcess {
                    process_id: entry.th32ProcessID,
                    executable: executable.to_owned(),
                    image_path,
                });
            }
        }
        // SAFETY: The snapshot handle and writable entry stay valid until enumeration ends.
        has_entry = unsafe { Process32NextW(snapshot, &raw mut entry) } != 0;
    }
    // SAFETY: This function owns the snapshot handle and closes it once.
    unsafe { CloseHandle(snapshot) };
    matches.sort_by(|left, right| {
        left.executable
            .cmp(&right.executable)
            .then(left.process_id.cmp(&right.process_id))
    });
    Ok(matches)
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn process_image_path(process_id: u32) -> Option<PathBuf> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
    };

    // SAFETY: OpenProcess receives a process identifier from the Toolhelp snapshot.
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, process_id) };
    if process.is_null() {
        return None;
    }
    let mut buffer = vec![0_u16; 32_768];
    let mut length = u32::try_from(buffer.len()).unwrap_or(u32::MAX);
    // SAFETY: The open process handle and buffer length describe writable UTF-16 storage.
    let result =
        unsafe { QueryFullProcessImageNameW(process, 0, buffer.as_mut_ptr(), &raw mut length) };
    // SAFETY: This function owns the process handle and closes it once.
    unsafe { CloseHandle(process) };
    if result == 0 {
        None
    } else {
        buffer.truncate(usize::try_from(length).ok()?);
        Some(PathBuf::from(String::from_utf16_lossy(&buffer)))
    }
}

#[cfg(windows)]
fn is_relevant_unreal_process(executable: &str, image_path: &Path, engine_root: &Path) -> bool {
    matches!(
        executable.to_ascii_lowercase().as_str(),
        "unrealeditor.exe" | "unrealeditor-cmd.exe" | "unrealbuildtool.exe" | "automationtool.exe"
    ) && path_starts_with(image_path, engine_root)
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn launch_elevated_worker(executable: &Path, request_path: &Path) -> Result<u8> {
    use std::mem;
    use windows_sys::Win32::Foundation::{CloseHandle, ERROR_CANCELLED};
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, INFINITE, WaitForSingleObject,
    };
    use windows_sys::Win32::UI::Shell::{
        SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW,
    };

    let verb = wide_null("runas")?;
    let executable = wide_path(executable)?;
    let parameters = wide_null(&format!(
        "{ELEVATED_WORKER_COMMAND} {ELEVATED_REQUEST_OPTION} \"{}\"",
        request_path.display()
    ))?;
    // SAFETY: Windows defines zero as the valid default for optional SHELLEXECUTEINFOW fields.
    let mut info = unsafe { mem::zeroed::<SHELLEXECUTEINFOW>() };
    info.cbSize = u32::try_from(mem::size_of::<SHELLEXECUTEINFOW>()).unwrap_or(u32::MAX);
    info.fMask = SEE_MASK_NOCLOSEPROCESS;
    info.lpVerb = verb.as_ptr();
    info.lpFile = executable.as_ptr();
    info.lpParameters = parameters.as_ptr();
    info.nShow = 0;
    // SAFETY: The structure size and string pointers remain valid through the synchronous launch call.
    if unsafe { ShellExecuteExW(&raw mut info) } == 0 {
        let error = std::io::Error::last_os_error();
        return if error.raw_os_error() == Some(ERROR_CANCELLED.cast_signed()) {
            Err(Error::PermissionDenied {
                message: "The user canceled the UAC request".to_owned(),
            })
        } else {
            Err(Error::PermissionDenied {
                message: format!("elevated worker start failed: {error}"),
            })
        };
    }
    if info.hProcess.is_null() {
        return Err(Error::Internal {
            message: "the elevated worker returned no process handle".to_owned(),
        });
    }
    // SAFETY: ShellExecuteExW returned an owned process handle and the wait does not outlive it.
    unsafe { WaitForSingleObject(info.hProcess, INFINITE) };
    let mut exit_code = 0_u32;
    // SAFETY: The process handle remains open and exit_code points to writable storage.
    let exit_result = unsafe { GetExitCodeProcess(info.hProcess, &raw mut exit_code) };
    // SAFETY: This function owns the process handle and closes it once.
    unsafe { CloseHandle(info.hProcess) };
    if exit_result == 0 {
        return Err(Error::Internal {
            message: format!(
                "elevated worker exit status failed: {}",
                std::io::Error::last_os_error()
            ),
        });
    }
    u8::try_from(exit_code).map_err(|_| Error::Internal {
        message: format!("elevated worker returned unsupported exit code {exit_code}"),
    })
}

#[cfg(not(windows))]
fn launch_elevated_worker(_executable: &Path, _request_path: &Path) -> Result<u8> {
    Err(Error::Unavailable {
        command: "Windows elevation",
    })
}

#[cfg(windows)]
fn wide_path(path: &Path) -> Result<Vec<u16>> {
    use std::os::windows::ffi::OsStrExt;

    let mut value = path
        .as_os_str()
        .encode_wide()
        .map(|character| {
            if character == u16::from(b'/') {
                u16::from(b'\\')
            } else {
                character
            }
        })
        .collect::<Vec<_>>();
    if value.contains(&0) {
        return Err(Error::InvalidInput {
            message: "elevation path contains an interior NUL".to_owned(),
        });
    }
    value.push(0);
    Ok(value)
}

#[cfg(windows)]
fn wide_null(value: &str) -> Result<Vec<u16>> {
    if value.encode_utf16().any(|character| character == 0) {
        return Err(Error::InvalidInput {
            message: "elevation argument contains an interior NUL".to_owned(),
        });
    }
    Ok(value.encode_utf16().chain(std::iter::once(0)).collect())
}

#[cfg(test)]
mod tests {
    use std::error::Error as StdError;
    use std::fs;
    use std::path::PathBuf;

    use tempfile::tempdir;

    use super::{
        ELEVATED_REQUEST_SCHEMA, ElevatedField, ElevatedOperationKind, ElevatedRequest,
        ElevatedTargetIntent, ElevatedWorkerContext, execute_elevated_request,
        write_access_requires_elevation,
    };
    #[cfg(windows)]
    use super::{is_relevant_unreal_process, trusted_app_data_root};
    use crate::ErrorCode;
    use crate::apply::{build_restore_plan, build_template_restore_plan};
    use crate::descriptors::DeclaredPluginState;
    use crate::discovery::{DiscoverySource, EngineHealth, EngineInstallation, infer_engine_root};
    use crate::journal::OperationTargetKind;
    use crate::plans::{PlanBuildOptions, sha256_hex};
    use crate::projects::ProjectSuppressionEdit;
    use crate::templates::build_template_plan;

    #[test]
    fn request_json_carries_intent_without_descriptor_output() -> Result<(), Box<dyn StdError>> {
        let fixture = fixture("request-json")?;
        let request = fixture.request("request-json")?;

        let rendered = String::from_utf8(request.render()?)?;
        let parsed = ElevatedRequest::parse(rendered.as_bytes())?;

        assert_eq!(parsed, request);
        assert!(!rendered.contains("planned_bytes"));
        assert!(!rendered.contains("FileVersion"));
        Ok(())
    }

    #[test]
    fn worker_rederives_and_commits_reviewed_state() -> Result<(), Box<dyn StdError>> {
        let fixture = fixture("rederive")?;
        let request = fixture.request("rederive")?;

        let report = execute_elevated_request(&request, &fixture.context)?;

        assert!(report.recorded);
        assert_eq!(report.files_written, 1);
        let written = fs::read_to_string(&fixture.target)?;
        assert!(written.contains("\"EnabledByDefault\":false"));
        assert!(
            fixture
                .backup_directory("rederive")
                .join("manifest.toml")
                .is_file()
        );
        Ok(())
    }

    #[test]
    fn worker_rederives_template_apply_and_restore_intent() -> Result<(), Box<dyn StdError>> {
        let fixture = fixture("template-worker")?;
        let template_path = fixture
            .engine
            .path
            .join("Templates/TP_Blank/TP_Blank.uproject");
        fs::create_dir_all(fixture.engine.path.join("Templates/TP_Blank"))?;
        let source = b"{\r\n\t\"FileVersion\": 3,\r\n}\r\n";
        fs::write(&template_path, source)?;
        let apply_options =
            PlanBuildOptions::new(fixture.backup_root(), "template-worker".to_owned())?;
        let plan = build_template_plan(
            &fixture.engine,
            &[PathBuf::from("Templates/TP_Blank/TP_Blank.uproject")],
            ProjectSuppressionEdit::Set(true),
            &apply_options,
        )?;
        let mut request = ElevatedRequest::from_template_plan(&plan)?;
        request.created_unix_seconds = 90;
        request.expires_unix_seconds = 200;

        let apply_report = execute_elevated_request(&request, &fixture.context)?;

        assert_eq!(apply_report.target_kind, OperationTargetKind::Template);
        assert!(
            fs::read_to_string(&template_path)?.contains("\"DisableEnginePluginsByDefault\": true")
        );

        let restore_options =
            PlanBuildOptions::new(fixture.backup_root(), "template-worker-restore".to_owned())?;
        let restore = build_template_restore_plan(
            &fixture.engine,
            "template-worker",
            &fixture.context.journal_path,
            &restore_options,
        )?;
        let mut restore_request = ElevatedRequest::from_template_restore_plan(&restore)?;
        restore_request.created_unix_seconds = 90;
        restore_request.expires_unix_seconds = 200;

        let restore_report = execute_elevated_request(&restore_request, &fixture.context)?;

        assert_eq!(restore_report.target_kind, OperationTargetKind::Template);
        assert_eq!(fs::read(template_path)?, source);
        Ok(())
    }

    #[test]
    fn hostile_schema_fields_paths_and_duplicates_fail_before_backup()
    -> Result<(), Box<dyn StdError>> {
        let fixture = fixture("hostile")?;
        let base = fixture.request("hostile")?;

        let mut wrong_schema = base.clone();
        wrong_schema.schema = ELEVATED_REQUEST_SCHEMA + 1;
        assert_pre_backup_failure(&fixture, &wrong_schema, "hostile", ErrorCode::InvalidInput)?;

        let mut traversal = base.clone();
        traversal.request_id = "hostile-traversal".to_owned();
        traversal.targets[0].relative_path = PathBuf::from("../outside.uplugin");
        assert_pre_backup_failure(
            &fixture,
            &traversal,
            "hostile-traversal",
            ErrorCode::InvalidInput,
        )?;

        let mut outside_root = base.clone();
        outside_root.request_id = "hostile-root".to_owned();
        outside_root.targets[0].relative_path = PathBuf::from("Engine/Content/Invented.uplugin");
        assert_pre_backup_failure(
            &fixture,
            &outside_root,
            "hostile-root",
            ErrorCode::InvalidInput,
        )?;

        let mut wrong_extension = base.clone();
        wrong_extension.request_id = "hostile-extension".to_owned();
        wrong_extension.targets[0].relative_path =
            PathBuf::from("Engine/Plugins/Runtime/Invented/Invented.txt");
        assert_pre_backup_failure(
            &fixture,
            &wrong_extension,
            "hostile-extension",
            ErrorCode::InvalidInput,
        )?;

        let mut duplicate = base.clone();
        duplicate.request_id = "hostile-duplicate".to_owned();
        duplicate.targets.push(duplicate.targets[0].clone());
        assert_pre_backup_failure(
            &fixture,
            &duplicate,
            "hostile-duplicate",
            ErrorCode::InvalidInput,
        )?;

        let unknown = br#"{"schema":1,"unknown":true}"#;
        assert!(ElevatedRequest::parse(unknown).is_err());
        let mut unknown_field = serde_json::to_value(base)?;
        unknown_field["targets"][0]["field"] = serde_json::Value::String("Arbitrary".to_owned());
        assert!(serde_json::from_value::<ElevatedRequest>(unknown_field).is_err());
        Ok(())
    }

    #[test]
    fn expired_and_swapped_requests_fail_before_backup() -> Result<(), Box<dyn StdError>> {
        let fixture = fixture("stale")?;
        let mut expired = fixture.request("expired")?;
        expired.created_unix_seconds = 0;
        expired.expires_unix_seconds = 10;
        assert_pre_backup_failure(&fixture, &expired, "expired", ErrorCode::Conflict)?;

        let swapped = fixture.request("swapped")?;
        fs::write(
            &fixture.target,
            br#"{"FileVersion":3,"EnabledByDefault":false}"#,
        )?;
        assert_pre_backup_failure(&fixture, &swapped, "swapped", ErrorCode::Conflict)?;
        Ok(())
    }

    #[test]
    fn completed_request_replay_fails_before_a_second_backup() -> Result<(), Box<dyn StdError>> {
        let fixture = fixture("replay")?;
        let request = fixture.request("replay")?;
        execute_elevated_request(&request, &fixture.context)?;

        let error = execute_elevated_request(&request, &fixture.context)
            .err()
            .ok_or("replay completed")?;

        assert_eq!(error.code(), ErrorCode::Conflict);
        let engine_backups = fixture
            .backup_directory("replay")
            .parent()
            .ok_or("backup directory has no parent")?
            .to_path_buf();
        assert_eq!(
            fs::read_dir(engine_backups)?.count(),
            1,
            "replay created another operation backup directory"
        );
        Ok(())
    }

    #[test]
    fn restore_request_revalidates_the_reviewed_snapshot_intent() -> Result<(), Box<dyn StdError>> {
        let fixture = fixture("restore")?;
        let apply_request = fixture.request("restore-apply")?;
        execute_elevated_request(&apply_request, &fixture.context)?;
        let options = PlanBuildOptions::new(fixture.backup_root(), "restore-worker".to_owned())?;
        let plan = build_restore_plan(
            &fixture.engine,
            "restore-apply",
            &fixture.context.journal_path,
            &options,
        )?;
        let mut request = ElevatedRequest::from_restore_plan(&plan)?;
        request.created_unix_seconds = 90;
        request.expires_unix_seconds = 200;

        let report = execute_elevated_request(&request, &fixture.context)?;

        assert_eq!(report.kind, crate::journal::JournalOperationKind::Restore);
        assert_eq!(fs::read(&fixture.target)?, fixture.original);
        Ok(())
    }

    #[test]
    fn write_access_probe_accepts_a_writable_descriptor_directory() -> Result<(), Box<dyn StdError>>
    {
        let fixture = fixture("access-probe")?;

        assert!(!write_access_requires_elevation(
            &fixture.engine,
            std::slice::from_ref(&fixture.relative)
        )?);
        let parent = fixture.target.parent().ok_or("target has no parent")?;
        assert!(
            fs::read_dir(parent)?
                .filter_map(std::result::Result::ok)
                .all(|entry| !entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".unclean-access-"))
        );
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn active_process_filter_requires_a_known_executable_under_the_engine() {
        let engine = PathBuf::from(r"C:\Epic\UE_5.9");

        assert!(is_relevant_unreal_process(
            "UnrealEditor.exe",
            PathBuf::from(r"C:\Epic\UE_5.9\Engine\Binaries\Win64\UnrealEditor.exe").as_path(),
            &engine
        ));
        assert!(!is_relevant_unreal_process(
            "UnrealEditor.exe",
            PathBuf::from(r"C:\Epic\UE_5.8\Engine\Binaries\Win64\UnrealEditor.exe").as_path(),
            &engine
        ));
        assert!(!is_relevant_unreal_process(
            "Unrelated.exe",
            PathBuf::from(r"C:\Epic\UE_5.9\Engine\Binaries\Win64\Unrelated.exe").as_path(),
            &engine
        ));
    }

    #[cfg(windows)]
    #[test]
    fn trusted_app_data_uses_an_existing_windows_known_folder() -> Result<(), Box<dyn StdError>> {
        let path = trusted_app_data_root()?;

        assert!(path.is_absolute());
        assert!(path.is_dir());
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn reparse_target_is_rejected_before_backup() -> Result<(), Box<dyn StdError>> {
        use std::os::windows::fs::symlink_file;

        let fixture = fixture("reparse")?;
        let outside = fixture.root.join("outside.uplugin");
        fs::write(&outside, br#"{"FileVersion":3,"EnabledByDefault":true}"#)?;
        fs::remove_file(&fixture.target)?;
        symlink_file(&outside, &fixture.target)?;
        let request = fixture.request_with_hash("reparse", sha256_hex(&fs::read(&outside)?));

        assert_pre_backup_failure(&fixture, &request, "reparse", ErrorCode::InvalidInput)?;
        fs::remove_file(&fixture.target)?;
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn reparse_directory_is_rejected_before_backup() -> Result<(), Box<dyn StdError>> {
        use std::os::windows::fs::symlink_dir;

        let fixture = fixture("reparse-directory")?;
        let outside = fixture.root.join("outside-directory");
        fs::create_dir(&outside)?;
        let outside_target = outside.join("Outside.uplugin");
        fs::write(
            &outside_target,
            br#"{"FileVersion":3,"EnabledByDefault":true}"#,
        )?;
        let linked = fixture
            .engine
            .path
            .join("Engine")
            .join("Plugins")
            .join("Runtime")
            .join("Linked");
        symlink_dir(&outside, &linked)?;
        let mut request =
            fixture.request_with_hash("reparse-directory", sha256_hex(&fs::read(outside_target)?));
        request.targets[0].relative_path =
            PathBuf::from("Engine/Plugins/Runtime/Linked/Outside.uplugin");

        assert_pre_backup_failure(
            &fixture,
            &request,
            "reparse-directory",
            ErrorCode::InvalidInput,
        )?;
        fs::remove_dir(&linked)?;
        Ok(())
    }

    fn assert_pre_backup_failure(
        fixture: &Fixture,
        request: &ElevatedRequest,
        operation_id: &str,
        expected: ErrorCode,
    ) -> Result<(), Box<dyn StdError>> {
        let error = execute_elevated_request(request, &fixture.context)
            .err()
            .ok_or("hostile request completed")?;
        assert_eq!(error.code(), expected);
        assert!(!fixture.backup_directory(operation_id).exists());
        Ok(())
    }

    struct Fixture {
        _temp: tempfile::TempDir,
        #[cfg(windows)]
        root: PathBuf,
        engine: EngineInstallation,
        target: PathBuf,
        relative: PathBuf,
        preset_path: PathBuf,
        original: Vec<u8>,
        context: ElevatedWorkerContext,
    }

    impl Fixture {
        fn request(&self, request_id: &str) -> Result<ElevatedRequest, Box<dyn StdError>> {
            Ok(self.request_with_hash(request_id, sha256_hex(&fs::read(&self.target)?)))
        }

        fn request_with_hash(&self, request_id: &str, source_hash: String) -> ElevatedRequest {
            ElevatedRequest {
                schema: ELEVATED_REQUEST_SCHEMA,
                request_id: request_id.to_owned(),
                created_unix_seconds: 90,
                expires_unix_seconds: 200,
                operation: ElevatedOperationKind::Apply,
                target_kind: OperationTargetKind::Engine,
                engine_path: self.engine.path.clone(),
                engine_version: self.engine.version.clone(),
                preset: "Fixture".to_owned(),
                preset_path: Some(self.preset_path.clone()),
                source_snapshot: None,
                targets: vec![ElevatedTargetIntent {
                    relative_path: self.relative.clone(),
                    field: ElevatedField::EnabledByDefault,
                    source_sha256: Some(source_hash),
                    value_after: Some(DeclaredPluginState::Disabled),
                    suppression_after: None,
                }],
            }
        }

        fn backup_root(&self) -> PathBuf {
            self.context.backup_root.clone()
        }

        fn backup_directory(&self, operation_id: &str) -> PathBuf {
            PlanBuildOptions::new(self.backup_root(), operation_id.to_owned()).map_or_else(
                |_| self.backup_root().join("invalid"),
                |options| options.backup_directory(&self.engine),
            )
        }
    }

    fn fixture(_name: &str) -> Result<Fixture, Box<dyn StdError>> {
        let temp = tempdir()?;
        let root = temp.path().to_path_buf();
        let engine_path = root.join("UE_Invented");
        let build = engine_path.join("Engine").join("Build");
        let relative = PathBuf::from("Engine/Plugins/Runtime/Invented/Invented.uplugin");
        let target = engine_path.join(&relative);
        fs::create_dir_all(&build)?;
        fs::create_dir_all(target.parent().ok_or("target has no parent")?)?;
        fs::write(
            build.join("Build.version"),
            r#"{"MajorVersion":5,"MinorVersion":9,"PatchVersion":0}"#,
        )?;
        let original = br#"{"FileVersion":3,"EnabledByDefault":true}"#.to_vec();
        fs::write(&target, &original)?;
        let engine_path =
            infer_engine_root(&engine_path).ok_or("engine root was not discovered")?;
        let target = engine_path.join(&relative);
        let preset_path = root.join("fixture.toml");
        fs::write(&preset_path, "fixture")?;
        let engine = EngineInstallation {
            path: engine_path,
            version: Some("5.9.0".to_owned()),
            source: DiscoverySource::Explicit,
            health: EngineHealth::Partial,
            descriptor_count: 1,
            issues: Vec::new(),
        };
        Ok(Fixture {
            context: ElevatedWorkerContext {
                backup_root: root.join("backups"),
                journal_path: root.join("state.toml"),
                now_unix_seconds: 100,
            },
            _temp: temp,
            #[cfg(windows)]
            root,
            engine,
            target,
            relative,
            preset_path,
            original,
        })
    }
}
