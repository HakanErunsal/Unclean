//! Reads project descriptors and produces focused edits for project plugin settings.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use jsonc_parser::ast::{Object, ObjectPropName, Value};
use jsonc_parser::common::Range;
use jsonc_parser::tokens::{Token, TokenAndRange};
use jsonc_parser::{CollectOptions, CommentCollectionStrategy, ParseOptions, parse_to_ast};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::descriptors::{
    ByteSpan, DescriptorEncoding, DescriptorIndentation, DescriptorIndentationKind,
    DescriptorLineEnding, MAX_DESCRIPTOR_BYTES,
};
use crate::discovery::EngineInstallation;

const UTF8_BOM: &[u8; 3] = b"\xEF\xBB\xBF";

/// Records whether the project suppresses plugins that engine defaults enable.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectSuppressionState {
    /// Records an explicit `true` value.
    Enabled,
    /// Records an explicit `false` value.
    Disabled,
    /// Records an absent `DisableEnginePluginsByDefault` field.
    Unspecified,
}

impl ProjectSuppressionState {
    /// Returns the stable lowercase label used in machine output.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Enabled => "enabled",
            Self::Disabled => "disabled",
            Self::Unspecified => "unspecified",
        }
    }
}

/// Records one explicit plugin reference in a project descriptor.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ProjectPluginReference {
    /// Names the referenced plugin.
    pub name: String,
    /// Reports the explicit project enablement state.
    pub enabled: bool,
}

/// Describes the project fields that Unclean reads or edits.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ProjectDescriptor {
    /// Records the selected project descriptor path.
    pub path: PathBuf,
    /// Retains the optional Unreal engine association.
    pub engine_association: Option<String>,
    /// Preserves the declared suppression state.
    pub suppression: ProjectSuppressionState,
    /// Lists explicit plugin references in source order.
    pub plugins: Vec<ProjectPluginReference>,
}

/// Retains source details used to verify focused project descriptor edits.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ProjectSourceMetadata {
    /// Records the accepted UTF-8 form.
    pub encoding: DescriptorEncoding,
    /// Records the line-ending form.
    pub line_ending: DescriptorLineEnding,
    /// Records top-level indentation.
    pub indentation: DescriptorIndentation,
    /// Reports whether the top-level object ends its last property with a comma.
    pub trailing_comma: bool,
    /// Covers the full top-level object.
    pub top_level_object: ByteSpan,
    /// Covers the suppression property and value when present.
    pub suppression: Option<ProjectFieldSpan>,
    /// Covers the plugin array property and value when present.
    pub plugins: Option<ProjectFieldSpan>,
}

/// Retains one project property span and its value span.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct ProjectFieldSpan {
    /// Covers the property name, colon, and value.
    pub property: ByteSpan,
    /// Covers only the property value.
    pub value: ByteSpan,
}

/// Selects the requested change for the project suppression field.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectSuppressionEdit {
    /// Leaves the current field unchanged.
    #[default]
    Keep,
    /// Writes an explicit boolean value.
    Set(bool),
    /// Removes the field.
    Clear,
}

/// Selects the requested change for one project plugin reference.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectPluginEditAction {
    /// Adds or updates an enabled reference.
    Enable,
    /// Adds or updates a disabled reference.
    Disable,
    /// Removes the matching explicit reference.
    Clear,
}

/// Describes one project plugin reference edit.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ProjectPluginEdit {
    /// Names the plugin without a path or extension.
    pub plugin: String,
    /// Selects the requested reference state.
    pub action: ProjectPluginEditAction,
}

/// Limits project descriptor writes to the two supported Unreal fields.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct ProjectDescriptorEdit {
    /// Selects the suppression field change.
    pub suppression: ProjectSuppressionEdit,
    /// Lists explicit plugin reference changes.
    pub plugins: Vec<ProjectPluginEdit>,
}

/// Reports unreadable project input or ambiguous focused edits.
#[derive(Debug, Error)]
pub enum ProjectDescriptorError {
    /// Reports a selected path that is not a project descriptor.
    #[error(
        "Project selection is invalid: {path} is not a .uproject file. Select one project descriptor and retry."
    )]
    InvalidPath {
        /// Records the rejected path.
        path: PathBuf,
    },
    /// Reports an unreadable project descriptor.
    #[error(
        "Project descriptor read failed for {path}: {source}. Check the path and file permissions, then retry."
    )]
    ReadFailed {
        /// Records the unreadable path.
        path: PathBuf,
        /// Retains the filesystem error.
        source: std::io::Error,
    },
    /// Reports input above the parser size limit.
    #[error(
        "Project descriptor exceeds the {limit} byte limit at {size} bytes. Reduce the file size before loading it."
    )]
    TooLarge {
        /// Records the input byte count.
        size: usize,
        /// Records the accepted byte limit.
        limit: usize,
    },
    /// Reports text that is not valid UTF-8.
    #[error("Project descriptor encoding is invalid. Save the file as UTF-8 and retry.")]
    InvalidEncoding,
    /// Reports JSONC syntax outside the accepted project descriptor shape.
    #[error("Project descriptor syntax is invalid: {message}. Repair the file before loading it.")]
    InvalidSyntax {
        /// Retains the parser diagnostic.
        message: String,
    },
    /// Reports a root value other than an object.
    #[error(
        "Project descriptor root is not an object. Replace the root value with a project descriptor object."
    )]
    RootNotObject,
    /// Reports a repeated modeled field.
    #[error(
        "Project descriptor contains the modeled key \"{key}\" more than once. Keep one value before loading it."
    )]
    DuplicateKey {
        /// Names the repeated key.
        key: String,
    },
    /// Reports a known field with an unsupported value type.
    #[error(
        "Project descriptor field \"{field}\" must contain {expected}. Correct the field before loading it."
    )]
    InvalidFieldType {
        /// Names the rejected field.
        field: String,
        /// Names the accepted value type.
        expected: &'static str,
    },
    /// Reports an invalid plugin reference.
    #[error(
        "Project plugin reference is invalid: {message}. Correct the Plugins entry before loading it."
    )]
    InvalidPluginReference {
        /// Describes the rejected reference.
        message: String,
    },
    /// Reports an engine association that cannot select one discovered engine.
    #[error(
        "Project engine association \"{association}\" did not match one discovered engine. Select the engine explicitly or register the association, then retry."
    )]
    EngineAssociationNotResolved {
        /// Records the unmatched or ambiguous association.
        association: String,
    },
    /// Reports an absent engine association.
    #[error("Project engine association is missing. Select the engine explicitly and retry.")]
    EngineAssociationMissing,
    /// Reports an invalid edit request or generated result.
    #[error(
        "Project descriptor planning failed: {message}. Leave the file unchanged and review the requested plugin names."
    )]
    EditFailed {
        /// Describes the failed edit invariant.
        message: String,
    },
}

/// Owns one validated project descriptor and plans edits against its original bytes.
pub struct ProjectDescriptorDocument {
    bytes: Vec<u8>,
    fields: ProjectFields,
    source: ProjectSourceMetadata,
    layout: ProjectLayout,
}

#[derive(Clone)]
struct ProjectFields {
    engine_association: Option<String>,
    suppression: ProjectSuppressionState,
    plugins: Vec<ProjectPluginReference>,
}

struct ProjectLayout {
    root: ObjectLayout,
    suppression_index: Option<usize>,
    plugins_index: Option<usize>,
    plugin_elements: Vec<PluginElementLayout>,
    plugins_array: Option<ArrayLayout>,
}

struct PluginElementLayout {
    name: String,
    element_index: usize,
    object: ObjectLayout,
    enabled_index: Option<usize>,
}

struct ObjectLayout {
    span: ByteSpan,
    properties: Vec<PropertyLayout>,
    closing: usize,
    preferred_line_ending: &'static [u8],
    insertion_indent: Vec<u8>,
    colon_spacing: Vec<u8>,
    inline_separator: Vec<u8>,
    multiline: bool,
    trailing_comma: bool,
}

struct ArrayLayout {
    span: ByteSpan,
    elements: Vec<ElementLayout>,
    closing: usize,
    preferred_line_ending: &'static [u8],
    insertion_indent: Vec<u8>,
    inline_separator: Vec<u8>,
    multiline: bool,
    trailing_comma: bool,
}

#[derive(Clone, Copy)]
struct PropertyLayout {
    property: ByteSpan,
    value: ByteSpan,
    comma_after: Option<ByteSpan>,
}

#[derive(Clone, Copy)]
struct ElementLayout {
    element: ByteSpan,
    comma_after: Option<ByteSpan>,
}

struct ByteEdit {
    span: ByteSpan,
    replacement: Vec<u8>,
}

impl ProjectDescriptorDocument {
    /// Loads one selected `.uproject` file without changing it.
    ///
    /// # Errors
    ///
    /// Returns an error when the path is not one readable `.uproject` file or the descriptor is invalid.
    pub fn load(path: &Path) -> Result<Self, ProjectDescriptorError> {
        if !path.is_file()
            || !path
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("uproject"))
        {
            return Err(ProjectDescriptorError::InvalidPath {
                path: path.to_path_buf(),
            });
        }
        let bytes = fs::read(path).map_err(|source| ProjectDescriptorError::ReadFailed {
            path: path.to_path_buf(),
            source,
        })?;
        Self::parse(&bytes)
    }

    /// Parses UTF-8 JSONC while retaining project fields and source spans.
    ///
    /// # Errors
    ///
    /// Returns an error for oversized input, invalid syntax, duplicate modeled fields, or invalid known field types.
    pub fn parse(bytes: &[u8]) -> Result<Self, ProjectDescriptorError> {
        if bytes.len() > MAX_DESCRIPTOR_BYTES {
            return Err(ProjectDescriptorError::TooLarge {
                size: bytes.len(),
                limit: MAX_DESCRIPTOR_BYTES,
            });
        }
        let (encoding, byte_offset) = if bytes.starts_with(UTF8_BOM) {
            (DescriptorEncoding::Utf8Bom, UTF8_BOM.len())
        } else {
            (DescriptorEncoding::Utf8, 0)
        };
        let text = std::str::from_utf8(&bytes[byte_offset..])
            .map_err(|_| ProjectDescriptorError::InvalidEncoding)?;
        let parsed = parse_to_ast(
            text,
            &CollectOptions {
                comments: CommentCollectionStrategy::AsTokens,
                tokens: true,
            },
            &parse_options(),
        )
        .map_err(|error| ProjectDescriptorError::InvalidSyntax {
            message: error.to_string(),
        })?;
        let value = parsed.value.ok_or(ProjectDescriptorError::RootNotObject)?;
        let object = value
            .as_object()
            .ok_or(ProjectDescriptorError::RootNotObject)?;
        let tokens = parsed.tokens.unwrap_or_default();

        reject_duplicate_top_level_keys(object)?;
        let fields = extract_fields(object)?;
        let (line_ending, preferred_line_ending) = detect_line_endings(bytes);
        let root = object_layout(text, object, &tokens, byte_offset, preferred_line_ending)?;
        let suppression_index = property_index(object, "DisableEnginePluginsByDefault");
        let plugins_index = property_index(object, "Plugins");
        let (plugin_elements, plugins_array) =
            project_plugin_layout(text, object, &tokens, byte_offset, preferred_line_ending)?;
        let suppression = suppression_index.map(|index| ProjectFieldSpan {
            property: root.properties[index].property,
            value: root.properties[index].value,
        });
        let plugins = plugins_index.map(|index| ProjectFieldSpan {
            property: root.properties[index].property,
            value: root.properties[index].value,
        });
        let source = ProjectSourceMetadata {
            encoding,
            line_ending,
            indentation: indentation_for_layout(&root),
            trailing_comma: root.trailing_comma,
            top_level_object: root.span,
            suppression,
            plugins,
        };

        Ok(Self {
            bytes: bytes.to_vec(),
            fields,
            source,
            layout: ProjectLayout {
                root,
                suppression_index,
                plugins_index,
                plugin_elements,
                plugins_array,
            },
        })
    }

    /// Returns the exact bytes supplied to the parser.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Returns the engine association without resolving a discovered install.
    #[must_use]
    pub fn engine_association(&self) -> Option<&str> {
        self.fields.engine_association.as_deref()
    }

    /// Returns the declared project suppression state.
    #[must_use]
    pub const fn suppression(&self) -> ProjectSuppressionState {
        self.fields.suppression
    }

    /// Returns explicit plugin references in source order.
    #[must_use]
    pub fn plugins(&self) -> &[ProjectPluginReference] {
        &self.fields.plugins
    }

    /// Returns source details and modeled field spans.
    #[must_use]
    pub const fn source_metadata(&self) -> &ProjectSourceMetadata {
        &self.source
    }

    /// Builds frontend metadata for the selected project path.
    #[must_use]
    pub fn project_descriptor(&self, path: &Path) -> ProjectDescriptor {
        ProjectDescriptor {
            path: path.to_path_buf(),
            engine_association: self.fields.engine_association.clone(),
            suppression: self.fields.suppression,
            plugins: self.fields.plugins.clone(),
        }
    }

    /// Resolves a version association to one discovered engine.
    ///
    /// # Errors
    ///
    /// Returns an error when the project has no association or the association does not identify exactly one engine.
    pub fn resolve_associated_engine<'a>(
        &self,
        engines: &'a [EngineInstallation],
    ) -> Result<&'a EngineInstallation, ProjectDescriptorError> {
        let association = self
            .engine_association()
            .ok_or(ProjectDescriptorError::EngineAssociationMissing)?;
        resolve_engine_association(
            association,
            engines,
            registered_engine_path(association).as_deref(),
        )
    }

    /// Returns edited bytes without changing the selected project file.
    ///
    /// # Errors
    ///
    /// Returns an error for duplicate edit names, invalid plugin names, invalid stored ranges, or failed output verification.
    pub fn edit(
        &self,
        requested: &ProjectDescriptorEdit,
    ) -> Result<Vec<u8>, ProjectDescriptorError> {
        validate_requested_edits(requested)?;
        let mut output = self.bytes.clone();

        if requested.suppression != ProjectSuppressionEdit::Keep {
            let current = Self::parse(&output)?;
            output = current.edit_suppression(requested.suppression)?;
        }
        for plugin_edit in &requested.plugins {
            let current = Self::parse(&output)?;
            output = current.edit_plugin(plugin_edit)?;
        }

        let verified =
            Self::parse(&output).map_err(|error| ProjectDescriptorError::EditFailed {
                message: format!("the planned output did not parse: {error}"),
            })?;
        verify_requested_edit(&verified, requested)?;
        Ok(output)
    }

    fn edit_suppression(
        &self,
        requested: ProjectSuppressionEdit,
    ) -> Result<Vec<u8>, ProjectDescriptorError> {
        let literal = match requested {
            ProjectSuppressionEdit::Keep => return Ok(self.bytes.clone()),
            ProjectSuppressionEdit::Set(true) => Some(b"true".as_slice()),
            ProjectSuppressionEdit::Set(false) => Some(b"false".as_slice()),
            ProjectSuppressionEdit::Clear => None,
        };
        let edits = object_property_edits(
            &self.bytes,
            &self.layout.root,
            self.layout.suppression_index,
            "DisableEnginePluginsByDefault",
            literal,
        )?;
        apply_byte_edits(&self.bytes, edits)
    }

    fn edit_plugin(
        &self,
        requested: &ProjectPluginEdit,
    ) -> Result<Vec<u8>, ProjectDescriptorError> {
        let normalized = normalize_plugin_name(&requested.plugin);
        let existing = self
            .layout
            .plugin_elements
            .iter()
            .find(|element| normalize_plugin_name(&element.name) == normalized);

        if requested.action == ProjectPluginEditAction::Clear {
            let Some(existing) = existing else {
                return Ok(self.bytes.clone());
            };
            let array = self.layout.plugins_array.as_ref().ok_or_else(|| {
                ProjectDescriptorError::EditFailed {
                    message: "the stored plugin entry has no plugin array layout".to_owned(),
                }
            })?;
            let edits = array_element_removal_edits(&self.bytes, array, existing.element_index)?;
            return apply_byte_edits(&self.bytes, edits);
        }

        let requested_enabled = requested.action == ProjectPluginEditAction::Enable;
        if let Some(existing) = existing {
            let current_enabled = self
                .fields
                .plugins
                .iter()
                .find(|plugin| normalize_plugin_name(&plugin.name) == normalized)
                .is_some_and(|plugin| plugin.enabled);
            if current_enabled == requested_enabled {
                return Ok(self.bytes.clone());
            }
            let literal = if requested_enabled {
                b"true".as_slice()
            } else {
                b"false".as_slice()
            };
            let edits = object_property_edits(
                &self.bytes,
                &existing.object,
                existing.enabled_index,
                "Enabled",
                Some(literal),
            )?;
            return apply_byte_edits(&self.bytes, edits);
        }

        let reference = project_plugin_reference_literal(&requested.plugin, requested_enabled)?;
        if let Some(array) = &self.layout.plugins_array {
            let edits = array_insertion_edits(&self.bytes, array, reference);
            return apply_byte_edits(&self.bytes, edits);
        }

        let array_literal = [b"[".as_slice(), reference.as_slice(), b"]".as_slice()].concat();
        let edits = object_property_edits(
            &self.bytes,
            &self.layout.root,
            self.layout.plugins_index,
            "Plugins",
            Some(&array_literal),
        )?;
        apply_byte_edits(&self.bytes, edits)
    }
}

fn parse_options() -> ParseOptions {
    ParseOptions {
        allow_comments: true,
        allow_loose_object_property_names: false,
        allow_trailing_commas: true,
        allow_missing_commas: false,
        allow_single_quoted_strings: false,
        allow_hexadecimal_numbers: false,
        allow_unary_plus_numbers: false,
    }
}

fn reject_duplicate_top_level_keys(object: &Object<'_>) -> Result<(), ProjectDescriptorError> {
    const MODELED_FIELDS: [&str; 3] = [
        "EngineAssociation",
        "DisableEnginePluginsByDefault",
        "Plugins",
    ];
    let mut names = HashSet::with_capacity(MODELED_FIELDS.len());
    for property in &object.properties {
        let name = property.name.as_str();
        if MODELED_FIELDS.contains(&name) && !names.insert(name.to_owned()) {
            return Err(ProjectDescriptorError::DuplicateKey {
                key: name.to_owned(),
            });
        }
    }
    Ok(())
}

fn extract_fields(object: &Object<'_>) -> Result<ProjectFields, ProjectDescriptorError> {
    let engine_association = optional_string(object, "EngineAssociation")?;
    let suppression = match optional_boolean(object, "DisableEnginePluginsByDefault")? {
        Some(true) => ProjectSuppressionState::Enabled,
        Some(false) => ProjectSuppressionState::Disabled,
        None => ProjectSuppressionState::Unspecified,
    };
    let plugins = extract_plugin_references(object)?;
    Ok(ProjectFields {
        engine_association,
        suppression,
        plugins,
    })
}

fn extract_plugin_references(
    object: &Object<'_>,
) -> Result<Vec<ProjectPluginReference>, ProjectDescriptorError> {
    let Some(property) = object.get("Plugins") else {
        return Ok(Vec::new());
    };
    let Value::Array(array) = &property.value else {
        return Err(ProjectDescriptorError::InvalidFieldType {
            field: "Plugins".to_owned(),
            expected: "an array",
        });
    };
    let mut plugins = Vec::with_capacity(array.elements.len());
    let mut names = HashSet::with_capacity(array.elements.len());
    for (index, value) in array.elements.iter().enumerate() {
        let reference =
            value
                .as_object()
                .ok_or_else(|| ProjectDescriptorError::InvalidFieldType {
                    field: format!("Plugins[{index}]"),
                    expected: "an object",
                })?;
        reject_duplicate_reference_keys(reference, index)?;
        let name = required_string(reference, "Name", index)?;
        if name.trim().is_empty() {
            return Err(ProjectDescriptorError::InvalidPluginReference {
                message: format!("Plugins[{index}].Name cannot be empty"),
            });
        }
        if !names.insert(normalize_plugin_name(&name)) {
            return Err(ProjectDescriptorError::InvalidPluginReference {
                message: format!("plugin \"{name}\" appears more than once"),
            });
        }
        let enabled = optional_reference_boolean(reference, "Enabled", index)?.unwrap_or(false);
        plugins.push(ProjectPluginReference { name, enabled });
    }
    Ok(plugins)
}

fn reject_duplicate_reference_keys(
    object: &Object<'_>,
    index: usize,
) -> Result<(), ProjectDescriptorError> {
    const MODELED_FIELDS: [&str; 2] = ["Name", "Enabled"];
    let mut names = HashSet::with_capacity(MODELED_FIELDS.len());
    for property in &object.properties {
        let name = property.name.as_str();
        if MODELED_FIELDS.contains(&name) && !names.insert(name.to_owned()) {
            return Err(ProjectDescriptorError::DuplicateKey {
                key: format!("Plugins[{index}].{name}"),
            });
        }
    }
    Ok(())
}

fn optional_string(
    object: &Object<'_>,
    name: &'static str,
) -> Result<Option<String>, ProjectDescriptorError> {
    match object.get(name) {
        Some(property) => match &property.value {
            Value::StringLit(value) => Ok(Some(value.value.to_string())),
            _ => Err(ProjectDescriptorError::InvalidFieldType {
                field: name.to_owned(),
                expected: "a string",
            }),
        },
        None => Ok(None),
    }
}

fn required_string(
    object: &Object<'_>,
    name: &'static str,
    index: usize,
) -> Result<String, ProjectDescriptorError> {
    match object.get(name) {
        Some(property) => match &property.value {
            Value::StringLit(value) => Ok(value.value.to_string()),
            _ => Err(ProjectDescriptorError::InvalidFieldType {
                field: format!("Plugins[{index}].{name}"),
                expected: "a string",
            }),
        },
        None => Err(ProjectDescriptorError::InvalidFieldType {
            field: format!("Plugins[{index}].{name}"),
            expected: "a string",
        }),
    }
}

fn optional_boolean(
    object: &Object<'_>,
    name: &'static str,
) -> Result<Option<bool>, ProjectDescriptorError> {
    match object.get(name) {
        Some(property) => match &property.value {
            Value::BooleanLit(value) => Ok(Some(value.value)),
            _ => Err(ProjectDescriptorError::InvalidFieldType {
                field: name.to_owned(),
                expected: "a boolean",
            }),
        },
        None => Ok(None),
    }
}

fn optional_reference_boolean(
    object: &Object<'_>,
    name: &'static str,
    index: usize,
) -> Result<Option<bool>, ProjectDescriptorError> {
    match object.get(name) {
        Some(property) => match &property.value {
            Value::BooleanLit(value) => Ok(Some(value.value)),
            _ => Err(ProjectDescriptorError::InvalidFieldType {
                field: format!("Plugins[{index}].{name}"),
                expected: "a boolean",
            }),
        },
        None => Ok(None),
    }
}

fn project_plugin_layout(
    text: &str,
    object: &Object<'_>,
    tokens: &[TokenAndRange<'_>],
    byte_offset: usize,
    preferred_line_ending: &'static [u8],
) -> Result<(Vec<PluginElementLayout>, Option<ArrayLayout>), ProjectDescriptorError> {
    let Some(property) = object.get("Plugins") else {
        return Ok((Vec::new(), None));
    };
    let Value::Array(array) = &property.value else {
        return Err(ProjectDescriptorError::InvalidFieldType {
            field: "Plugins".to_owned(),
            expected: "an array",
        });
    };
    let array_layout = array_layout(
        text,
        array.range,
        &array.elements,
        tokens,
        byte_offset,
        preferred_line_ending,
    )?;
    let mut elements = Vec::with_capacity(array.elements.len());
    for (index, value) in array.elements.iter().enumerate() {
        let reference =
            value
                .as_object()
                .ok_or_else(|| ProjectDescriptorError::InvalidFieldType {
                    field: format!("Plugins[{index}]"),
                    expected: "an object",
                })?;
        let name = required_string(reference, "Name", index)?;
        elements.push(PluginElementLayout {
            name,
            element_index: index,
            object: object_layout(text, reference, tokens, byte_offset, preferred_line_ending)?,
            enabled_index: property_index(reference, "Enabled"),
        });
    }
    Ok((elements, Some(array_layout)))
}

fn object_layout(
    text: &str,
    object: &Object<'_>,
    tokens: &[TokenAndRange<'_>],
    byte_offset: usize,
    preferred_line_ending: &'static [u8],
) -> Result<ObjectLayout, ProjectDescriptorError> {
    let span = span_from_range(object.range, byte_offset);
    let closing = span
        .end
        .checked_sub(1)
        .ok_or_else(|| ProjectDescriptorError::EditFailed {
            message: "an object has no closing brace range".to_owned(),
        })?;
    let multiline = text[object.range.start..object.range.end].contains('\n');
    let properties = collect_property_layout(object, tokens, byte_offset);
    let trailing_comma = properties
        .last()
        .is_some_and(|property| property.comma_after.is_some());
    let colon_spacing = detect_colon_spacing(text, object);
    let inline_separator = detect_inline_separator(text, object, tokens, &colon_spacing);
    let insertion_indent = detect_insertion_indent(
        text,
        object
            .properties
            .iter()
            .map(|property| property.range.start),
        object.range.end.saturating_sub(1),
        multiline,
    );
    Ok(ObjectLayout {
        span,
        properties,
        closing,
        preferred_line_ending,
        insertion_indent,
        colon_spacing,
        inline_separator,
        multiline,
        trailing_comma,
    })
}

fn array_layout(
    text: &str,
    range: Range,
    values: &[Value<'_>],
    tokens: &[TokenAndRange<'_>],
    byte_offset: usize,
    preferred_line_ending: &'static [u8],
) -> Result<ArrayLayout, ProjectDescriptorError> {
    let span = span_from_range(range, byte_offset);
    let closing = span
        .end
        .checked_sub(1)
        .ok_or_else(|| ProjectDescriptorError::EditFailed {
            message: "the Plugins array has no closing bracket range".to_owned(),
        })?;
    let multiline = text[range.start..range.end].contains('\n');
    let elements = values
        .iter()
        .enumerate()
        .map(|(index, value)| {
            let current_range = value_range(value);
            let gap_end = values
                .get(index + 1)
                .map_or(range.end.saturating_sub(1), |next| value_range(next).start);
            let comma_after = tokens
                .iter()
                .find(|token| {
                    matches!(token.token, Token::Comma)
                        && token.range.start >= current_range.end
                        && token.range.end <= gap_end
                })
                .map(|token| span_from_range(token.range, byte_offset));
            ElementLayout {
                element: span_from_range(current_range, byte_offset),
                comma_after,
            }
        })
        .collect::<Vec<_>>();
    let trailing_comma = elements
        .last()
        .is_some_and(|element| element.comma_after.is_some());
    let insertion_indent = detect_insertion_indent(
        text,
        values.iter().map(|value| value_range(value).start),
        range.end.saturating_sub(1),
        multiline,
    );
    let inline_separator = detect_array_inline_separator(text, values, tokens);
    Ok(ArrayLayout {
        span,
        elements,
        closing,
        preferred_line_ending,
        insertion_indent,
        inline_separator,
        multiline,
        trailing_comma,
    })
}

fn collect_property_layout(
    object: &Object<'_>,
    tokens: &[TokenAndRange<'_>],
    byte_offset: usize,
) -> Vec<PropertyLayout> {
    object
        .properties
        .iter()
        .enumerate()
        .map(|(index, property)| {
            let gap_end = object
                .properties
                .get(index + 1)
                .map_or(object.range.end.saturating_sub(1), |next| next.range.start);
            let comma_after = tokens
                .iter()
                .find(|token| {
                    matches!(token.token, Token::Comma)
                        && token.range.start >= property.range.end
                        && token.range.end <= gap_end
                })
                .map(|token| span_from_range(token.range, byte_offset));
            PropertyLayout {
                property: span_from_range(property.range, byte_offset),
                value: span_from_range(value_range(&property.value), byte_offset),
                comma_after,
            }
        })
        .collect()
}

fn property_index(object: &Object<'_>, name: &str) -> Option<usize> {
    object
        .properties
        .iter()
        .position(|property| property.name.as_str() == name)
}

fn value_range(value: &Value<'_>) -> Range {
    match value {
        Value::StringLit(value) => value.range,
        Value::NumberLit(value) => value.range,
        Value::BooleanLit(value) => value.range,
        Value::Object(value) => value.range,
        Value::Array(value) => value.range,
        Value::NullKeyword(value) => value.range,
    }
}

const fn span_from_range(range: Range, offset: usize) -> ByteSpan {
    ByteSpan {
        start: range.start + offset,
        end: range.end + offset,
    }
}

fn object_property_edits(
    bytes: &[u8],
    layout: &ObjectLayout,
    index: Option<usize>,
    name: &str,
    literal: Option<&[u8]>,
) -> Result<Vec<ByteEdit>, ProjectDescriptorError> {
    match (index, literal) {
        (Some(index), Some(literal)) => Ok(vec![ByteEdit {
            span: layout.properties[index].value,
            replacement: literal.to_vec(),
        }]),
        (Some(index), None) => object_property_removal_edits(bytes, layout, index),
        (None, Some(literal)) => Ok(object_property_insertion_edits(
            bytes, layout, name, literal,
        )),
        (None, None) => Ok(Vec::new()),
    }
}

fn object_property_insertion_edits(
    bytes: &[u8],
    layout: &ObjectLayout,
    name: &str,
    literal: &[u8],
) -> Vec<ByteEdit> {
    let property = property_literal(name, literal, &layout.colon_spacing);
    let Some(last) = layout.properties.last() else {
        return vec![empty_container_insertion(
            bytes,
            layout.closing,
            layout.multiline,
            &layout.insertion_indent,
            layout.preferred_line_ending,
            property,
        )];
    };
    if layout.multiline {
        let insertion = line_start(bytes, layout.closing);
        let mut replacement = layout.insertion_indent.clone();
        replacement.extend_from_slice(&property);
        if layout.trailing_comma {
            replacement.push(b',');
        }
        replacement.extend_from_slice(layout.preferred_line_ending);
        let mut edits = vec![ByteEdit {
            span: ByteSpan {
                start: insertion,
                end: insertion,
            },
            replacement,
        }];
        if !layout.trailing_comma {
            edits.push(ByteEdit {
                span: ByteSpan {
                    start: last.property.end,
                    end: last.property.end,
                },
                replacement: vec![b','],
            });
        }
        return edits;
    }
    let insertion = trim_horizontal_end(bytes, layout.span.start + 1, layout.closing);
    let replacement =
        inline_insertion_literal(&property, &layout.inline_separator, layout.trailing_comma);
    vec![ByteEdit {
        span: ByteSpan {
            start: insertion,
            end: insertion,
        },
        replacement,
    }]
}

fn object_property_removal_edits(
    bytes: &[u8],
    layout: &ObjectLayout,
    index: usize,
) -> Result<Vec<ByteEdit>, ProjectDescriptorError> {
    layout
        .properties
        .get(index)
        .ok_or_else(|| ProjectDescriptorError::EditFailed {
            message: "the property index is outside the stored object layout".to_owned(),
        })?;
    removal_edits(
        bytes,
        layout
            .properties
            .iter()
            .map(|property| (property.property, property.comma_after))
            .collect::<Vec<_>>()
            .as_slice(),
        index,
    )
}

fn array_insertion_edits(bytes: &[u8], layout: &ArrayLayout, element: Vec<u8>) -> Vec<ByteEdit> {
    let Some(last) = layout.elements.last() else {
        return vec![empty_container_insertion(
            bytes,
            layout.closing,
            layout.multiline,
            &layout.insertion_indent,
            layout.preferred_line_ending,
            element,
        )];
    };
    if layout.multiline {
        let insertion = line_start(bytes, layout.closing);
        let mut replacement = layout.insertion_indent.clone();
        replacement.extend_from_slice(&element);
        if layout.trailing_comma {
            replacement.push(b',');
        }
        replacement.extend_from_slice(layout.preferred_line_ending);
        let mut edits = vec![ByteEdit {
            span: ByteSpan {
                start: insertion,
                end: insertion,
            },
            replacement,
        }];
        if !layout.trailing_comma {
            edits.push(ByteEdit {
                span: ByteSpan {
                    start: last.element.end,
                    end: last.element.end,
                },
                replacement: vec![b','],
            });
        }
        return edits;
    }
    let insertion = trim_horizontal_end(bytes, layout.span.start + 1, layout.closing);
    let replacement =
        inline_insertion_literal(&element, &layout.inline_separator, layout.trailing_comma);
    vec![ByteEdit {
        span: ByteSpan {
            start: insertion,
            end: insertion,
        },
        replacement,
    }]
}

fn array_element_removal_edits(
    bytes: &[u8],
    layout: &ArrayLayout,
    index: usize,
) -> Result<Vec<ByteEdit>, ProjectDescriptorError> {
    removal_edits(
        bytes,
        layout
            .elements
            .iter()
            .map(|element| (element.element, element.comma_after))
            .collect::<Vec<_>>()
            .as_slice(),
        index,
    )
}

fn removal_edits(
    bytes: &[u8],
    entries: &[(ByteSpan, Option<ByteSpan>)],
    index: usize,
) -> Result<Vec<ByteEdit>, ProjectDescriptorError> {
    let (target, comma_after) =
        entries
            .get(index)
            .copied()
            .ok_or_else(|| ProjectDescriptorError::EditFailed {
                message: "the removal index is outside the stored container layout".to_owned(),
            })?;
    if let Some(comma) = comma_after {
        let mut end = comma.end;
        while bytes.get(end).is_some_and(u8::is_ascii_whitespace)
            && bytes.get(end) != Some(&b'\r')
            && bytes.get(end) != Some(&b'\n')
        {
            end += 1;
        }
        let span = dedicated_line_span(bytes, target, end).unwrap_or(ByteSpan {
            start: target.start,
            end,
        });
        return Ok(vec![ByteEdit {
            span,
            replacement: Vec::new(),
        }]);
    }
    if index > 0 {
        let previous_comma =
            entries[index - 1]
                .1
                .ok_or_else(|| ProjectDescriptorError::EditFailed {
                    message: "the entry before the target has no separating comma".to_owned(),
                })?;
        if let Some(line_span) = dedicated_line_span(bytes, target, target.end) {
            return Ok(vec![
                ByteEdit {
                    span: previous_comma,
                    replacement: Vec::new(),
                },
                ByteEdit {
                    span: line_span,
                    replacement: Vec::new(),
                },
            ]);
        }
        return Ok(vec![ByteEdit {
            span: ByteSpan {
                start: previous_comma.start,
                end: target.end,
            },
            replacement: Vec::new(),
        }]);
    }
    let span = dedicated_line_span(bytes, target, target.end).unwrap_or(target);
    Ok(vec![ByteEdit {
        span,
        replacement: Vec::new(),
    }])
}

fn empty_container_insertion(
    bytes: &[u8],
    closing: usize,
    multiline: bool,
    insertion_indent: &[u8],
    line_ending: &[u8],
    literal: Vec<u8>,
) -> ByteEdit {
    if multiline {
        let insertion = line_start(bytes, closing);
        let mut replacement = insertion_indent.to_vec();
        replacement.extend_from_slice(&literal);
        replacement.extend_from_slice(line_ending);
        ByteEdit {
            span: ByteSpan {
                start: insertion,
                end: insertion,
            },
            replacement,
        }
    } else {
        ByteEdit {
            span: ByteSpan {
                start: closing,
                end: closing,
            },
            replacement: literal,
        }
    }
}

fn inline_insertion_literal(literal: &[u8], separator: &[u8], trailing_comma: bool) -> Vec<u8> {
    let mut replacement = Vec::new();
    if trailing_comma {
        replacement.extend_from_slice(separator);
        replacement.extend_from_slice(literal);
        replacement.push(b',');
    } else {
        replacement.push(b',');
        replacement.extend_from_slice(separator);
        replacement.extend_from_slice(literal);
    }
    replacement
}

fn property_literal(name: &str, literal: &[u8], colon_spacing: &[u8]) -> Vec<u8> {
    let mut property = Vec::with_capacity(name.len() + literal.len() + colon_spacing.len() + 3);
    property.push(b'"');
    property.extend_from_slice(name.as_bytes());
    property.push(b'"');
    property.push(b':');
    property.extend_from_slice(colon_spacing);
    property.extend_from_slice(literal);
    property
}

fn project_plugin_reference_literal(
    name: &str,
    enabled: bool,
) -> Result<Vec<u8>, ProjectDescriptorError> {
    let encoded_name =
        serde_json::to_string(name).map_err(|error| ProjectDescriptorError::EditFailed {
            message: format!("plugin name encoding failed: {error}"),
        })?;
    Ok(format!("{{\"Name\": {encoded_name}, \"Enabled\": {enabled}}}").into_bytes())
}

fn validate_requested_edits(
    requested: &ProjectDescriptorEdit,
) -> Result<(), ProjectDescriptorError> {
    let mut names = HashSet::with_capacity(requested.plugins.len());
    for plugin in &requested.plugins {
        let trimmed = plugin.plugin.trim();
        if trimmed.is_empty()
            || Path::new(trimmed).components().count() != 1
            || trimmed.ends_with(".uplugin")
        {
            return Err(ProjectDescriptorError::EditFailed {
                message: format!(
                    "plugin name \"{}\" must not contain a path or .uplugin extension",
                    plugin.plugin
                ),
            });
        }
        if !names.insert(normalize_plugin_name(trimmed)) {
            return Err(ProjectDescriptorError::EditFailed {
                message: format!("plugin \"{}\" appears more than once", plugin.plugin),
            });
        }
    }
    Ok(())
}

fn verify_requested_edit(
    document: &ProjectDescriptorDocument,
    requested: &ProjectDescriptorEdit,
) -> Result<(), ProjectDescriptorError> {
    let expected_suppression = match requested.suppression {
        ProjectSuppressionEdit::Keep => None,
        ProjectSuppressionEdit::Set(true) => Some(ProjectSuppressionState::Enabled),
        ProjectSuppressionEdit::Set(false) => Some(ProjectSuppressionState::Disabled),
        ProjectSuppressionEdit::Clear => Some(ProjectSuppressionState::Unspecified),
    };
    if expected_suppression.is_some_and(|expected| document.suppression() != expected) {
        return Err(ProjectDescriptorError::EditFailed {
            message: "the planned output did not retain the requested suppression state".to_owned(),
        });
    }
    for edit in &requested.plugins {
        let actual = document
            .plugins()
            .iter()
            .find(|plugin| plugin.name.eq_ignore_ascii_case(edit.plugin.trim()));
        let matches = match edit.action {
            ProjectPluginEditAction::Enable => actual.is_some_and(|plugin| plugin.enabled),
            ProjectPluginEditAction::Disable => actual.is_some_and(|plugin| !plugin.enabled),
            ProjectPluginEditAction::Clear => actual.is_none(),
        };
        if !matches {
            return Err(ProjectDescriptorError::EditFailed {
                message: format!(
                    "the planned output did not retain the requested state for {}",
                    edit.plugin
                ),
            });
        }
    }
    Ok(())
}

fn association_matches_version(association: &str, version: &str) -> bool {
    let association = association.trim();
    let version = version.trim();
    version.eq_ignore_ascii_case(association)
        || version
            .strip_prefix(association)
            .is_some_and(|suffix| suffix.starts_with('.') || suffix.starts_with('-'))
}

fn resolve_engine_association<'a>(
    association: &str,
    engines: &'a [EngineInstallation],
    registered_path: Option<&Path>,
) -> Result<&'a EngineInstallation, ProjectDescriptorError> {
    let version_matches = engines
        .iter()
        .filter(|engine| {
            engine
                .version
                .as_deref()
                .is_some_and(|version| association_matches_version(association, version))
        })
        .collect::<Vec<_>>();
    match version_matches.as_slice() {
        [engine] => return Ok(*engine),
        [] => {}
        _ => {
            return Err(ProjectDescriptorError::EngineAssociationNotResolved {
                association: association.to_owned(),
            });
        }
    }
    let path_matches = registered_path.map_or_else(Vec::new, |registered| {
        engines
            .iter()
            .filter(|engine| paths_match(&engine.path, registered))
            .collect::<Vec<_>>()
    });
    match path_matches.as_slice() {
        [engine] => Ok(*engine),
        _ => Err(ProjectDescriptorError::EngineAssociationNotResolved {
            association: association.to_owned(),
        }),
    }
}

fn paths_match(left: &Path, right: &Path) -> bool {
    normalized_path(left) == normalized_path(right)
}

fn normalized_path(path: &Path) -> String {
    path.to_string_lossy()
        .trim_end_matches(['\\', '/'])
        .replace('/', "\\")
        .to_ascii_lowercase()
}

#[cfg(windows)]
fn registered_engine_path(association: &str) -> Option<PathBuf> {
    use winreg::RegKey;
    use winreg::enums::HKEY_CURRENT_USER;

    let current_user = RegKey::predef(HKEY_CURRENT_USER);
    let builds = current_user
        .open_subkey("SOFTWARE\\Epic Games\\Unreal Engine\\Builds")
        .ok()?;
    builds
        .get_value::<String, _>(association)
        .ok()
        .map(PathBuf::from)
}

#[cfg(not(windows))]
fn registered_engine_path(_association: &str) -> Option<PathBuf> {
    None
}

fn normalize_plugin_name(name: &str) -> String {
    name.trim().to_ascii_lowercase()
}

fn detect_line_endings(bytes: &[u8]) -> (DescriptorLineEnding, &'static [u8]) {
    let mut lf_count = 0;
    let mut crlf_count = 0;
    let mut first = None;
    for (index, byte) in bytes.iter().enumerate() {
        if *byte != b'\n' {
            continue;
        }
        if index > 0 && bytes[index - 1] == b'\r' {
            crlf_count += 1;
            first.get_or_insert(b"\r\n".as_slice());
        } else {
            lf_count += 1;
            first.get_or_insert(b"\n".as_slice());
        }
    }
    let line_ending = match (lf_count, crlf_count) {
        (0, 0) => DescriptorLineEnding::None,
        (_, 0) => DescriptorLineEnding::Lf,
        (0, _) => DescriptorLineEnding::CrLf,
        _ => DescriptorLineEnding::Mixed,
    };
    (line_ending, first.unwrap_or(b"\n"))
}

fn detect_insertion_indent(
    text: &str,
    starts: impl Iterator<Item = usize>,
    closing: usize,
    multiline: bool,
) -> Vec<u8> {
    if !multiline {
        return Vec::new();
    }
    for start in starts {
        let line = line_start(text.as_bytes(), start);
        let prefix = &text.as_bytes()[line..start];
        if prefix.iter().all(|byte| matches!(byte, b' ' | b'\t')) {
            return prefix.to_vec();
        }
    }
    let closing_line = line_start(text.as_bytes(), closing);
    let mut indent = text.as_bytes()[closing_line..closing].to_vec();
    if !indent.iter().all(|byte| matches!(byte, b' ' | b'\t')) {
        indent.clear();
    }
    indent.extend_from_slice(b"  ");
    indent
}

fn indentation_for_layout(layout: &ObjectLayout) -> DescriptorIndentation {
    if !layout.multiline {
        return DescriptorIndentation {
            kind: DescriptorIndentationKind::Compact,
            width: None,
        };
    }
    let kind = if layout.insertion_indent.iter().all(|byte| *byte == b' ') {
        DescriptorIndentationKind::Spaces
    } else if layout.insertion_indent.iter().all(|byte| *byte == b'\t') {
        DescriptorIndentationKind::Tabs
    } else {
        DescriptorIndentationKind::Mixed
    };
    DescriptorIndentation {
        kind,
        width: (kind != DescriptorIndentationKind::Mixed).then_some(layout.insertion_indent.len()),
    }
}

fn detect_colon_spacing(text: &str, object: &Object<'_>) -> Vec<u8> {
    let Some(property) = object.properties.first() else {
        return if text[object.range.start..object.range.end].contains('\n') {
            vec![b' ']
        } else {
            Vec::new()
        };
    };
    let name_end = match &property.name {
        ObjectPropName::String(value) => value.range.end,
        ObjectPropName::Word(value) => value.range.end,
    };
    let value_start = value_range(&property.value).start;
    let between = &text.as_bytes()[name_end..value_start];
    let Some(colon) = between.iter().position(|byte| *byte == b':') else {
        return vec![b' '];
    };
    let spacing = &between[colon + 1..];
    if spacing.iter().all(|byte| matches!(byte, b' ' | b'\t')) {
        spacing.to_vec()
    } else {
        vec![b' ']
    }
}

fn detect_inline_separator(
    text: &str,
    object: &Object<'_>,
    tokens: &[TokenAndRange<'_>],
    colon_spacing: &[u8],
) -> Vec<u8> {
    for pair in object.properties.windows(2) {
        let Some(comma) = tokens.iter().find(|token| {
            matches!(token.token, Token::Comma)
                && token.range.start >= pair[0].range.end
                && token.range.end <= pair[1].range.start
        }) else {
            continue;
        };
        let spacing = &text.as_bytes()[comma.range.end..pair[1].range.start];
        if spacing.iter().all(|byte| matches!(byte, b' ' | b'\t')) {
            return spacing.to_vec();
        }
    }
    if colon_spacing.is_empty() {
        Vec::new()
    } else {
        vec![b' ']
    }
}

fn detect_array_inline_separator(
    text: &str,
    values: &[Value<'_>],
    tokens: &[TokenAndRange<'_>],
) -> Vec<u8> {
    for pair in values.windows(2) {
        let left = value_range(&pair[0]);
        let right = value_range(&pair[1]);
        let Some(comma) = tokens.iter().find(|token| {
            matches!(token.token, Token::Comma)
                && token.range.start >= left.end
                && token.range.end <= right.start
        }) else {
            continue;
        };
        let spacing = &text.as_bytes()[comma.range.end..right.start];
        if spacing.iter().all(|byte| matches!(byte, b' ' | b'\t')) {
            return spacing.to_vec();
        }
    }
    vec![b' ']
}

fn dedicated_line_span(bytes: &[u8], value: ByteSpan, removal_end: usize) -> Option<ByteSpan> {
    let start = line_start(bytes, value.start);
    if !bytes[start..value.start]
        .iter()
        .all(|byte| matches!(byte, b' ' | b'\t'))
    {
        return None;
    }
    let newline = bytes
        .get(removal_end..)?
        .iter()
        .position(|byte| *byte == b'\n')
        .map(|offset| removal_end + offset)?;
    let horizontal_end = if newline > removal_end && bytes[newline - 1] == b'\r' {
        newline - 1
    } else {
        newline
    };
    bytes[removal_end..horizontal_end]
        .iter()
        .all(|byte| matches!(byte, b' ' | b'\t'))
        .then_some(ByteSpan {
            start,
            end: newline + 1,
        })
}

fn line_start(bytes: &[u8], index: usize) -> usize {
    bytes
        .get(..index)
        .and_then(|prefix| prefix.iter().rposition(|byte| *byte == b'\n'))
        .map_or(0, |position| position + 1)
}

fn trim_horizontal_end(bytes: &[u8], minimum: usize, end: usize) -> usize {
    let mut position = end;
    while position > minimum
        && bytes
            .get(position - 1)
            .is_some_and(|byte| matches!(byte, b' ' | b'\t'))
    {
        position -= 1;
    }
    position
}

fn apply_byte_edits(
    source: &[u8],
    mut edits: Vec<ByteEdit>,
) -> Result<Vec<u8>, ProjectDescriptorError> {
    edits.sort_by_key(|edit| (edit.span.start, edit.span.end));
    let added = edits
        .iter()
        .map(|edit| edit.replacement.len())
        .sum::<usize>();
    let mut output = Vec::with_capacity(source.len().saturating_add(added));
    let mut cursor = 0;
    for edit in edits {
        if edit.span.start < cursor
            || edit.span.start > edit.span.end
            || edit.span.end > source.len()
        {
            return Err(ProjectDescriptorError::EditFailed {
                message: "an edit range overlaps another range or leaves the source".to_owned(),
            });
        }
        output.extend_from_slice(&source[cursor..edit.span.start]);
        output.extend_from_slice(&edit.replacement);
        cursor = edit.span.end;
    }
    output.extend_from_slice(&source[cursor..]);
    Ok(output)
}

#[cfg(test)]
mod tests {
    use std::error::Error;
    use std::fs;
    use std::path::{Path, PathBuf};

    use tempfile::tempdir;

    use super::{
        ProjectDescriptorDocument, ProjectDescriptorEdit, ProjectDescriptorError,
        ProjectPluginEdit, ProjectPluginEditAction, ProjectSuppressionEdit,
        ProjectSuppressionState, resolve_engine_association,
    };
    use crate::discovery::{DiscoverySource, EngineHealth, EngineInstallation};

    const PROJECT: &[u8] = br#"{
  // Keep this project note.
  "FileVersion": 3,
  "EngineAssociation": "5.8",
  "Category": "Synthetic",
  "DisableEnginePluginsByDefault": false,
  "Plugins": [
    { "Name": "KeepPlugin", "Enabled": true, "SyntheticField": 7 },
    {
      "Name": "ChangePlugin",
      "Enabled": false,
      "PlatformAllowList": ["Win64"],
    },
    { "Name": "ClearPlugin", "Enabled": false },
  ],
  "SyntheticUnknown": {
    "Nested": [1, 2, 3],
  },
}
"#;

    #[test]
    fn parses_only_the_project_fields_owned_by_the_contract() -> Result<(), Box<dyn Error>> {
        let document = ProjectDescriptorDocument::parse(PROJECT)?;

        assert_eq!(document.engine_association(), Some("5.8"));
        assert_eq!(document.suppression(), ProjectSuppressionState::Disabled);
        assert_eq!(document.plugins().len(), 3);
        assert_eq!(document.plugins()[0].name, "KeepPlugin");
        assert!(document.plugins()[0].enabled);
        assert!(document.source_metadata().trailing_comma);
        assert!(document.source_metadata().suppression.is_some());
        assert!(document.source_metadata().plugins.is_some());
        Ok(())
    }

    #[test]
    fn focused_edits_preserve_comments_unknown_fields_and_formatting() -> Result<(), Box<dyn Error>>
    {
        let document = ProjectDescriptorDocument::parse(PROJECT)?;
        let edited = document.edit(&ProjectDescriptorEdit {
            suppression: ProjectSuppressionEdit::Set(true),
            plugins: vec![
                ProjectPluginEdit {
                    plugin: "ChangePlugin".to_owned(),
                    action: ProjectPluginEditAction::Enable,
                },
                ProjectPluginEdit {
                    plugin: "ClearPlugin".to_owned(),
                    action: ProjectPluginEditAction::Clear,
                },
                ProjectPluginEdit {
                    plugin: "AddedPlugin".to_owned(),
                    action: ProjectPluginEditAction::Disable,
                },
            ],
        })?;
        let text = String::from_utf8(edited.clone())?;
        let verified = ProjectDescriptorDocument::parse(&edited)?;

        assert_eq!(verified.suppression(), ProjectSuppressionState::Enabled);
        assert!(
            verified
                .plugins()
                .iter()
                .any(|plugin| plugin.name == "ChangePlugin" && plugin.enabled)
        );
        assert!(
            verified
                .plugins()
                .iter()
                .any(|plugin| plugin.name == "AddedPlugin" && !plugin.enabled)
        );
        assert!(
            !verified
                .plugins()
                .iter()
                .any(|plugin| plugin.name == "ClearPlugin")
        );
        assert!(text.contains("// Keep this project note."));
        assert!(text.contains("\"SyntheticField\": 7"));
        assert!(text.contains("\"PlatformAllowList\": [\"Win64\"],"));
        assert!(text.contains("\"SyntheticUnknown\": {\n    \"Nested\": [1, 2, 3],\n  },"));
        assert!(text.find("\"FileVersion\"") < text.find("\"EngineAssociation\""));
        assert!(text.find("\"EngineAssociation\"") < text.find("\"Category\""));
        assert!(text.ends_with("}\n"));
        Ok(())
    }

    #[test]
    fn absent_fields_accept_compact_and_multiline_insertions() -> Result<(), Box<dyn Error>> {
        let compact = ProjectDescriptorDocument::parse(br#"{"FileVersion":3}"#)?;
        let compact_edited = compact.edit(&ProjectDescriptorEdit {
            suppression: ProjectSuppressionEdit::Set(true),
            plugins: vec![ProjectPluginEdit {
                plugin: "SyntheticPlugin".to_owned(),
                action: ProjectPluginEditAction::Enable,
            }],
        })?;
        let compact_text = String::from_utf8(compact_edited.clone())?;
        assert_eq!(
            compact_text,
            r#"{"FileVersion":3,"DisableEnginePluginsByDefault":true,"Plugins":[{"Name": "SyntheticPlugin", "Enabled": true}]}"#
        );
        ProjectDescriptorDocument::parse(&compact_edited)?;

        let multiline = ProjectDescriptorDocument::parse(b"{\r\n}\r\n")?;
        let multiline_edited = multiline.edit(&ProjectDescriptorEdit {
            suppression: ProjectSuppressionEdit::Set(false),
            plugins: vec![ProjectPluginEdit {
                plugin: "SyntheticPlugin".to_owned(),
                action: ProjectPluginEditAction::Disable,
            }],
        })?;
        let multiline_text = String::from_utf8(multiline_edited)?;
        assert!(multiline_text.contains(
            "  \"DisableEnginePluginsByDefault\": false,\r\n  \"Plugins\": [{\"Name\": \"SyntheticPlugin\", \"Enabled\": false}]\r\n"
        ));
        Ok(())
    }

    #[test]
    fn clear_removes_only_modeled_project_entries() -> Result<(), Box<dyn Error>> {
        let cleared = ProjectDescriptorDocument::parse(PROJECT)?.edit(&ProjectDescriptorEdit {
            suppression: ProjectSuppressionEdit::Clear,
            plugins: vec![ProjectPluginEdit {
                plugin: "ClearPlugin".to_owned(),
                action: ProjectPluginEditAction::Clear,
            }],
        })?;
        let text = String::from_utf8(cleared.clone())?;
        let verified = ProjectDescriptorDocument::parse(&cleared)?;

        assert_eq!(verified.suppression(), ProjectSuppressionState::Unspecified);
        assert!(!text.contains("DisableEnginePluginsByDefault"));
        assert!(!text.contains("ClearPlugin"));
        assert!(text.contains("SyntheticUnknown"));
        Ok(())
    }

    #[test]
    fn load_requires_one_existing_uproject_file() -> Result<(), Box<dyn Error>> {
        let temp = tempdir()?;
        let project = temp.path().join("Synthetic.uproject");
        let wrong = temp.path().join("Synthetic.json");
        fs::write(&project, PROJECT)?;
        fs::write(&wrong, PROJECT)?;

        assert_eq!(
            ProjectDescriptorDocument::load(&project)?.engine_association(),
            Some("5.8")
        );
        assert!(matches!(
            ProjectDescriptorDocument::load(&wrong),
            Err(ProjectDescriptorError::InvalidPath { .. })
        ));
        Ok(())
    }

    #[test]
    fn version_association_resolves_exactly_one_discovered_engine() -> Result<(), Box<dyn Error>> {
        let document = ProjectDescriptorDocument::parse(PROJECT)?;
        let engines = vec![
            engine("D:\\Synthetic\\UE_5.7", "5.7.4"),
            engine("D:\\Synthetic\\UE_5.8", "5.8.1"),
        ];

        assert_eq!(
            document.resolve_associated_engine(&engines)?.path,
            PathBuf::from("D:\\Synthetic\\UE_5.8")
        );
        assert!(document.resolve_associated_engine(&engines[..1]).is_err());
        Ok(())
    }

    #[test]
    fn registered_source_association_resolves_by_engine_path() -> Result<(), Box<dyn Error>> {
        let engines = vec![
            engine("D:\\Synthetic\\UE_5.8", "5.8.1"),
            engine("D:\\Source\\UnrealEngine", "5.8.0-source"),
        ];

        assert_eq!(
            resolve_engine_association(
                "{11111111-2222-3333-4444-555555555555}",
                &engines,
                Some(Path::new("D:/Source/UnrealEngine/")),
            )?
            .path,
            PathBuf::from("D:\\Source\\UnrealEngine")
        );
        Ok(())
    }

    #[test]
    fn ambiguous_project_fields_and_edit_names_are_rejected() -> Result<(), Box<dyn Error>> {
        assert!(ProjectDescriptorDocument::parse(br#"{"Plugins":[],"Plugins":[]}"#).is_err());
        assert!(
            ProjectDescriptorDocument::parse(br#"{"Plugins":[{"Name":"Same"},{"Name":"same"}]}"#)
                .is_err()
        );
        let document = ProjectDescriptorDocument::parse(PROJECT)?;
        assert!(
            document
                .edit(&ProjectDescriptorEdit {
                    suppression: ProjectSuppressionEdit::Keep,
                    plugins: vec![
                        ProjectPluginEdit {
                            plugin: "KeepPlugin".to_owned(),
                            action: ProjectPluginEditAction::Enable,
                        },
                        ProjectPluginEdit {
                            plugin: "keepplugin".to_owned(),
                            action: ProjectPluginEditAction::Disable,
                        },
                    ],
                })
                .is_err()
        );
        Ok(())
    }

    fn engine(path: &str, version: &str) -> EngineInstallation {
        EngineInstallation {
            path: PathBuf::from(path),
            version: Some(version.to_owned()),
            source: DiscoverySource::Explicit,
            health: EngineHealth::Healthy,
            descriptor_count: 500,
            issues: Vec::new(),
        }
    }
}
