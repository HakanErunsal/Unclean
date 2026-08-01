//! Reads descriptor state and produces targeted byte-preserving field edits.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use jsonc_parser::ast::{Object, Value};
use jsonc_parser::common::Range;
use jsonc_parser::tokens::{Token, TokenAndRange};
use jsonc_parser::{CollectOptions, CommentCollectionStrategy, ParseOptions, parse_to_ast};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::discovery::{EngineHealth, EngineInstallation};
use crate::{Error as ProductError, Result};

const UTF8_BOM: &[u8; 3] = b"\xEF\xBB\xBF";

/// Limits one descriptor to four mebibytes before parsing.
pub const MAX_DESCRIPTOR_BYTES: usize = 4 * 1024 * 1024;

/// Describes the declared `EnabledByDefault` value without folding an absent key into false.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeclaredPluginState {
    /// Records an explicit `true` value.
    Enabled,
    /// Records an explicit `false` value.
    Disabled,
    /// Records an absent `EnabledByDefault` key.
    Unspecified,
}

impl DeclaredPluginState {
    /// Returns the stable lowercase label used in table output.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Enabled => "enabled",
            Self::Disabled => "disabled",
            Self::Unspecified => "unspecified",
        }
    }

    const fn boolean_literal(self) -> Option<&'static [u8]> {
        match self {
            Self::Enabled => Some(b"true"),
            Self::Disabled => Some(b"false"),
            Self::Unspecified => None,
        }
    }
}

/// Identifies the accepted descriptor byte encoding.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub enum DescriptorEncoding {
    /// Records UTF-8 without a byte-order mark.
    #[serde(rename = "utf-8")]
    Utf8,
    /// Records UTF-8 with a leading byte-order mark.
    #[serde(rename = "utf-8-bom")]
    Utf8Bom,
}

/// Identifies the line endings retained from a descriptor.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DescriptorLineEnding {
    /// Records a single-line descriptor with no newline.
    None,
    /// Records line-feed endings.
    Lf,
    /// Records carriage-return and line-feed endings.
    #[serde(rename = "crlf")]
    CrLf,
    /// Records both line-ending forms in one descriptor.
    Mixed,
}

/// Identifies the indentation form used by top-level fields.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DescriptorIndentationKind {
    /// Records a single-line descriptor.
    Compact,
    /// Records space indentation.
    Spaces,
    /// Records tab indentation.
    Tabs,
    /// Records inconsistent top-level indentation.
    Mixed,
}

/// Retains the indentation form and width used for new top-level fields.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct DescriptorIndentation {
    /// Identifies the indentation form.
    pub kind: DescriptorIndentationKind,
    /// Records the repeated space or tab count when the width is consistent.
    pub width: Option<usize>,
}

/// Identifies one half-open byte range in the original descriptor.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct ByteSpan {
    /// Records the first byte in the range.
    pub start: usize,
    /// Records the byte after the range.
    pub end: usize,
}

impl ByteSpan {
    const fn from_range(range: Range, offset: usize) -> Self {
        Self {
            start: range.start + offset,
            end: range.end + offset,
        }
    }
}

/// Retains the top-level property and boolean token ranges for a present target field.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct DescriptorFieldSpan {
    /// Covers the property name, colon, and value.
    pub property: ByteSpan,
    /// Covers only the `true` or `false` token.
    pub value: ByteSpan,
}

/// Retains source details needed to make a byte-preserving edit.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct DescriptorSourceMetadata {
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
    /// Covers the target property and value when the key is present.
    pub enabled_by_default: Option<DescriptorFieldSpan>,
}

/// Identifies one enabled plugin reference declared by a descriptor.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PluginDependencyReference {
    /// Names the referenced plugin.
    pub name: String,
    /// Reports whether Unreal may continue when the referenced plugin is absent.
    pub optional: bool,
}

/// Describes one parsed plugin for frontends and machine output.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PluginDescriptor {
    /// Uses the descriptor file name as the stable plugin name.
    pub name: String,
    /// Uses `FriendlyName` when present and falls back to the stable name.
    pub friendly_name: String,
    /// Retains the optional descriptor description.
    pub description: Option<String>,
    /// Retains the optional descriptor category.
    pub category: Option<String>,
    /// Retains the optional descriptor version label.
    pub version_name: Option<String>,
    /// Preserves the declared three-valued state.
    pub declared_state: DeclaredPluginState,
    /// Lists descriptor references whose `Enabled` field is true.
    pub enabled_dependencies: Vec<PluginDependencyReference>,
    /// Leaves effective state unresolved until dependency analysis runs.
    pub effective_enabled: Option<bool>,
    /// Records one stable root-to-plugin path when dependency analysis enables the plugin.
    pub effective_path: Vec<String>,
    /// Lists effective plugins that directly depend on this plugin.
    pub reached_by: Vec<String>,
    /// Counts entries in the descriptor `Modules` array.
    pub module_count: usize,
    /// Records the descriptor path.
    pub path: PathBuf,
    /// Records the descriptor path relative to `Engine\Plugins`.
    pub relative_path: PathBuf,
    /// Retains byte-format details used by targeted edits.
    pub source: DescriptorSourceMetadata,
}

/// Identifies a stable warning category from a read-only plugin scan.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginScanWarningCode {
    /// Reports directory and entry inspection failures.
    ScanFailed,
    /// Reports an unreadable descriptor.
    ReadFailed,
    /// Reports a descriptor that did not satisfy the accepted format.
    ParseFailed,
}

impl PluginScanWarningCode {
    /// Returns the stable lowercase identifier used in table output.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ScanFailed => "scan_failed",
            Self::ReadFailed => "read_failed",
            Self::ParseFailed => "parse_failed",
        }
    }
}

/// Reports one skipped path without stopping the remaining read-only scan.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PluginScanWarning {
    /// Identifies the warning category.
    pub code: PluginScanWarningCode,
    /// Records the path that produced the warning.
    pub path: PathBuf,
    /// States the failure and recovery action.
    pub message: String,
}

/// Groups parsed plugins and nonfatal scan warnings for one engine.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PluginScanReport {
    /// Lists parsed descriptors in stable name and path order.
    pub plugins: Vec<PluginDescriptor>,
    /// Lists paths skipped during traversal, reading, or parsing.
    pub warnings: Vec<PluginScanWarning>,
}

/// Reports descriptor input that Unclean cannot parse or edit without ambiguity.
#[derive(Debug, Error)]
pub enum DescriptorError {
    /// Reports input above the parser size limit.
    #[error(
        "Descriptor exceeds the {limit} byte limit at {size} bytes. Reduce the file size before scanning."
    )]
    TooLarge {
        /// Records the input byte count.
        size: usize,
        /// Records the accepted byte limit.
        limit: usize,
    },
    /// Reports text that is not valid UTF-8.
    #[error("Descriptor encoding is invalid. Save the file as UTF-8 and retry.")]
    InvalidEncoding,
    /// Reports JSONC syntax outside the accepted Unreal descriptor shape.
    #[error("Descriptor syntax is invalid: {message}. Repair the file before scanning.")]
    InvalidSyntax {
        /// Retains the parser diagnostic.
        message: String,
    },
    /// Reports a root value other than an object.
    #[error(
        "Descriptor root is not an object. Replace the root value with a plugin descriptor object."
    )]
    RootNotObject,
    /// Reports a repeated property in one object that Unclean reads or edits.
    #[error(
        "Descriptor contains the modeled key \"{key}\" more than once in one object. Keep one value before scanning."
    )]
    DuplicateKey {
        /// Names the repeated key.
        key: String,
    },
    /// Reports a known field with an unsupported value type.
    #[error(
        "Descriptor field \"{field}\" must contain {expected}. Correct the field before scanning."
    )]
    InvalidFieldType {
        /// Names the rejected field.
        field: &'static str,
        /// Names the accepted value type.
        expected: &'static str,
    },
    /// Reports a known field with an unsupported value.
    #[error(
        "Descriptor field \"{field}\" is invalid: {message}. Correct the field before scanning."
    )]
    InvalidFieldValue {
        /// Names the rejected field.
        field: &'static str,
        /// Describes the rejected value.
        message: String,
    },
    /// Reports an edit range or generated result that failed validation.
    #[error(
        "Descriptor planning failed: {message}. Leave the file unchanged and report the descriptor shape."
    )]
    EditFailed {
        /// Describes the failed edit invariant.
        message: String,
    },
}

/// Owns one validated descriptor and plans edits against its original bytes.
pub struct DescriptorDocument {
    bytes: Vec<u8>,
    fields: DescriptorFields,
    source: DescriptorSourceMetadata,
    layout: DescriptorLayout,
}

#[derive(Clone)]
struct DescriptorFields {
    friendly_name: Option<String>,
    description: Option<String>,
    category: Option<String>,
    version_name: Option<String>,
    declared_state: DeclaredPluginState,
    enabled_dependencies: Vec<PluginDependencyReference>,
    module_count: usize,
}

struct DescriptorLayout {
    properties: Vec<PropertyLayout>,
    target_index: Option<usize>,
    closing_brace: usize,
    preferred_line_ending: &'static [u8],
    insertion_indent: Vec<u8>,
    colon_spacing: Vec<u8>,
    inline_separator: Vec<u8>,
    multiline: bool,
}

#[derive(Clone, Copy)]
struct PropertyLayout {
    property: ByteSpan,
    value: ByteSpan,
    comma_after: Option<ByteSpan>,
}

struct ByteEdit {
    span: ByteSpan,
    replacement: Vec<u8>,
}

impl DescriptorDocument {
    /// Parses UTF-8 JSONC while retaining the original bytes and target field spans.
    ///
    /// # Errors
    ///
    /// Returns an error for oversized input, invalid UTF-8, unsupported JSONC syntax, duplicate top-level keys, or invalid known field types.
    pub fn parse(bytes: &[u8]) -> std::result::Result<Self, DescriptorError> {
        if bytes.len() > MAX_DESCRIPTOR_BYTES {
            return Err(DescriptorError::TooLarge {
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
            .map_err(|_| DescriptorError::InvalidEncoding)?;
        let parsed = parse_to_ast(
            text,
            &CollectOptions {
                comments: CommentCollectionStrategy::AsTokens,
                tokens: true,
            },
            &descriptor_parse_options(),
        )
        .map_err(|error| DescriptorError::InvalidSyntax {
            message: error.to_string(),
        })?;
        let value = parsed.value.ok_or(DescriptorError::RootNotObject)?;
        let object = value.as_object().ok_or(DescriptorError::RootNotObject)?;
        let tokens = parsed.tokens.unwrap_or_default();

        reject_duplicate_modeled_keys(object)?;
        let fields = extract_fields(object)?;
        let (line_ending, preferred_line_ending) = detect_line_endings(bytes);
        let multiline = text[object.range.start..object.range.end].contains('\n');
        let (indentation, insertion_indent) = detect_indentation(text, object, multiline);
        let properties = collect_property_layout(object, &tokens, byte_offset);
        let target_index = object
            .properties
            .iter()
            .position(|property| property.name.as_str() == "EnabledByDefault");
        let target_span = target_index.map(|index| DescriptorFieldSpan {
            property: properties[index].property,
            value: properties[index].value,
        });
        let trailing_comma = properties
            .last()
            .is_some_and(|property| property.comma_after.is_some());
        let colon_spacing = detect_colon_spacing(text, object);
        let inline_separator = detect_inline_separator(text, object, &tokens, &colon_spacing);
        let top_level_object = ByteSpan::from_range(object.range, byte_offset);
        let closing_brace =
            top_level_object
                .end
                .checked_sub(1)
                .ok_or_else(|| DescriptorError::EditFailed {
                    message: "the top-level object has no closing brace range".to_owned(),
                })?;

        Ok(Self {
            bytes: bytes.to_vec(),
            fields,
            source: DescriptorSourceMetadata {
                encoding,
                line_ending,
                indentation,
                trailing_comma,
                top_level_object,
                enabled_by_default: target_span,
            },
            layout: DescriptorLayout {
                properties,
                target_index,
                closing_brace,
                preferred_line_ending,
                insertion_indent,
                colon_spacing,
                inline_separator,
                multiline,
            },
        })
    }

    /// Returns the exact bytes supplied to the parser.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Returns the declared three-valued plugin state.
    #[must_use]
    pub const fn declared_state(&self) -> DeclaredPluginState {
        self.fields.declared_state
    }

    /// Returns byte-format details and target field spans.
    #[must_use]
    pub const fn source_metadata(&self) -> &DescriptorSourceMetadata {
        &self.source
    }

    /// Builds frontend metadata for a descriptor path under one plugin root.
    #[must_use]
    pub fn plugin_descriptor(&self, path: &Path, plugin_root: &Path) -> PluginDescriptor {
        let name = path
            .file_stem()
            .map_or_else(String::new, |value| value.to_string_lossy().into_owned());
        PluginDescriptor {
            friendly_name: self
                .fields
                .friendly_name
                .clone()
                .unwrap_or_else(|| name.clone()),
            name,
            description: self.fields.description.clone(),
            category: self.fields.category.clone(),
            version_name: self.fields.version_name.clone(),
            declared_state: self.fields.declared_state,
            enabled_dependencies: self.fields.enabled_dependencies.clone(),
            effective_enabled: None,
            effective_path: Vec::new(),
            reached_by: Vec::new(),
            module_count: self.fields.module_count,
            path: path.to_path_buf(),
            relative_path: path
                .strip_prefix(plugin_root)
                .map_or_else(|_| path.to_path_buf(), Path::to_path_buf),
            source: self.source.clone(),
        }
    }

    /// Returns edited bytes for one declared state without changing the source document.
    ///
    /// # Errors
    ///
    /// Returns an error when stored ranges are invalid or the planned bytes fail a full descriptor reparse.
    pub fn edit_enabled_by_default(
        &self,
        requested: DeclaredPluginState,
    ) -> std::result::Result<Vec<u8>, DescriptorError> {
        if requested == self.fields.declared_state {
            return Ok(self.bytes.clone());
        }

        let edits = match (self.layout.target_index, requested.boolean_literal()) {
            (Some(index), Some(literal)) => vec![ByteEdit {
                span: self.layout.properties[index].value,
                replacement: literal.to_vec(),
            }],
            (Some(index), None) => self.removal_edits(index)?,
            (None, Some(literal)) => self.insertion_edits(literal),
            (None, None) => return Ok(self.bytes.clone()),
        };
        let output = apply_byte_edits(&self.bytes, edits)?;
        let verified = Self::parse(&output).map_err(|error| DescriptorError::EditFailed {
            message: format!("the planned output did not parse: {error}"),
        })?;
        if verified.declared_state() != requested {
            return Err(DescriptorError::EditFailed {
                message: "the planned output did not retain the requested state".to_owned(),
            });
        }
        Ok(output)
    }

    fn insertion_edits(&self, literal: &[u8]) -> Vec<ByteEdit> {
        let property = self.property_bytes(literal);
        let Some(last) = self.layout.properties.last() else {
            return vec![self.empty_object_insertion(property)];
        };

        if self.layout.multiline {
            let insertion = line_start(&self.bytes, self.layout.closing_brace);
            let mut replacement = self.layout.insertion_indent.clone();
            replacement.extend_from_slice(&property);
            if self.source.trailing_comma {
                replacement.push(b',');
            }
            replacement.extend_from_slice(self.layout.preferred_line_ending);
            let mut edits = vec![ByteEdit {
                span: ByteSpan {
                    start: insertion,
                    end: insertion,
                },
                replacement,
            }];
            if !self.source.trailing_comma {
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

        let insertion = trim_horizontal_end(
            &self.bytes,
            self.source.top_level_object.start + 1,
            self.layout.closing_brace,
        );
        let mut replacement = Vec::new();
        if self.source.trailing_comma {
            replacement.extend_from_slice(&self.layout.inline_separator);
            replacement.extend_from_slice(&property);
            replacement.push(b',');
        } else {
            replacement.push(b',');
            replacement.extend_from_slice(&self.layout.inline_separator);
            replacement.extend_from_slice(&property);
        }
        vec![ByteEdit {
            span: ByteSpan {
                start: insertion,
                end: insertion,
            },
            replacement,
        }]
    }

    fn empty_object_insertion(&self, property: Vec<u8>) -> ByteEdit {
        if self.layout.multiline {
            let insertion = line_start(&self.bytes, self.layout.closing_brace);
            let mut replacement = self.layout.insertion_indent.clone();
            replacement.extend_from_slice(&property);
            replacement.extend_from_slice(self.layout.preferred_line_ending);
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
                    start: self.layout.closing_brace,
                    end: self.layout.closing_brace,
                },
                replacement: property,
            }
        }
    }

    fn property_bytes(&self, literal: &[u8]) -> Vec<u8> {
        let mut property = b"\"EnabledByDefault\":".to_vec();
        property.extend_from_slice(&self.layout.colon_spacing);
        property.extend_from_slice(literal);
        property
    }

    fn removal_edits(&self, index: usize) -> std::result::Result<Vec<ByteEdit>, DescriptorError> {
        let property =
            self.layout
                .properties
                .get(index)
                .ok_or_else(|| DescriptorError::EditFailed {
                    message: "the target property index is outside the stored layout".to_owned(),
                })?;

        if let Some(comma) = property.comma_after {
            let mut end = comma.end;
            while self.bytes.get(end).is_some_and(u8::is_ascii_whitespace)
                && self.bytes.get(end) != Some(&b'\r')
                && self.bytes.get(end) != Some(&b'\n')
            {
                end += 1;
            }
            let span =
                dedicated_line_span(&self.bytes, property.property, end).unwrap_or(ByteSpan {
                    start: property.property.start,
                    end,
                });
            return Ok(vec![ByteEdit {
                span,
                replacement: Vec::new(),
            }]);
        }

        if index > 0 {
            let previous_comma =
                self.layout.properties[index - 1]
                    .comma_after
                    .ok_or_else(|| DescriptorError::EditFailed {
                        message: "the property before the target has no separating comma"
                            .to_owned(),
                    })?;
            if let Some(line_span) =
                dedicated_line_span(&self.bytes, property.property, property.property.end)
            {
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
                    end: property.property.end,
                },
                replacement: Vec::new(),
            }]);
        }

        let span = dedicated_line_span(&self.bytes, property.property, property.property.end)
            .unwrap_or(property.property);
        Ok(vec![ByteEdit {
            span,
            replacement: Vec::new(),
        }])
    }
}

/// Scans one engine plugin directory without changing descriptor files.
///
/// # Errors
///
/// Returns an error when the selected engine has no usable `Engine\Plugins` directory.
pub fn scan_engine_plugins(engine: &EngineInstallation) -> Result<PluginScanReport> {
    if engine.health == EngineHealth::Unavailable {
        return Err(ProductError::NotFound {
            item: format!(
                "usable Engine\\Plugins directory under {}",
                engine.path.display()
            ),
        });
    }

    let plugin_root = engine.path.join("Engine").join("Plugins");
    if !plugin_root.is_dir() {
        return Err(ProductError::NotFound {
            item: format!("plugin directory {}", plugin_root.display()),
        });
    }

    let mut paths = Vec::new();
    let mut warnings = Vec::new();
    collect_descriptor_paths(&plugin_root, &mut paths, &mut warnings);
    paths.sort_by(|left, right| {
        left.to_string_lossy()
            .to_ascii_lowercase()
            .cmp(&right.to_string_lossy().to_ascii_lowercase())
    });

    let mut plugins = Vec::with_capacity(paths.len());
    for path in paths {
        match fs::read(&path) {
            Ok(bytes) => match DescriptorDocument::parse(&bytes) {
                Ok(document) => {
                    plugins.push(document.plugin_descriptor(&path, &plugin_root));
                }
                Err(error) => warnings.push(PluginScanWarning {
                    code: PluginScanWarningCode::ParseFailed,
                    path,
                    message: format!("Descriptor skipped: {error}"),
                }),
            },
            Err(error) => warnings.push(PluginScanWarning {
                code: PluginScanWarningCode::ReadFailed,
                path,
                message: format!(
                    "Descriptor read failed: {error}. Check the file permissions and retry."
                ),
            }),
        }
    }
    plugins.sort_by(|left, right| {
        left.name
            .to_ascii_lowercase()
            .cmp(&right.name.to_ascii_lowercase())
            .then_with(|| left.path.cmp(&right.path))
    });
    warnings.sort_by(|left, right| {
        left.path
            .to_string_lossy()
            .to_ascii_lowercase()
            .cmp(&right.path.to_string_lossy().to_ascii_lowercase())
            .then_with(|| left.code.as_str().cmp(right.code.as_str()))
    });

    Ok(PluginScanReport { plugins, warnings })
}

fn descriptor_parse_options() -> ParseOptions {
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

fn reject_duplicate_modeled_keys(object: &Object<'_>) -> std::result::Result<(), DescriptorError> {
    const MODELED_FIELDS: [&str; 7] = [
        "EnabledByDefault",
        "FriendlyName",
        "Description",
        "Category",
        "VersionName",
        "Modules",
        "Plugins",
    ];
    let mut names = HashSet::with_capacity(MODELED_FIELDS.len());
    for property in &object.properties {
        let name = property.name.as_str();
        if !MODELED_FIELDS.contains(&name) {
            continue;
        }
        if !names.insert(name.to_owned()) {
            return Err(DescriptorError::DuplicateKey {
                key: name.to_owned(),
            });
        }
    }
    Ok(())
}

fn extract_fields(object: &Object<'_>) -> std::result::Result<DescriptorFields, DescriptorError> {
    let declared_state = match object.get("EnabledByDefault") {
        Some(property) => match &property.value {
            Value::BooleanLit(value) if value.value => DeclaredPluginState::Enabled,
            Value::BooleanLit(_) => DeclaredPluginState::Disabled,
            _ => {
                return Err(DescriptorError::InvalidFieldType {
                    field: "EnabledByDefault",
                    expected: "a boolean",
                });
            }
        },
        None => DeclaredPluginState::Unspecified,
    };
    let module_count = match object.get("Modules") {
        Some(property) => match &property.value {
            Value::Array(array) => array.elements.len(),
            _ => {
                return Err(DescriptorError::InvalidFieldType {
                    field: "Modules",
                    expected: "an array",
                });
            }
        },
        None => 0,
    };
    let enabled_dependencies = enabled_dependencies(object)?;

    Ok(DescriptorFields {
        friendly_name: optional_string(object, "FriendlyName")?,
        description: optional_string(object, "Description")?,
        category: optional_string(object, "Category")?,
        version_name: optional_string(object, "VersionName")?,
        declared_state,
        enabled_dependencies,
        module_count,
    })
}

fn enabled_dependencies(
    object: &Object<'_>,
) -> std::result::Result<Vec<PluginDependencyReference>, DescriptorError> {
    let Some(property) = object.get("Plugins") else {
        return Ok(Vec::new());
    };
    let Value::Array(array) = &property.value else {
        return Err(DescriptorError::InvalidFieldType {
            field: "Plugins",
            expected: "an array",
        });
    };
    let mut dependencies = Vec::new();
    let mut dependency_names = HashSet::new();
    for value in &array.elements {
        let Some(reference) = value.as_object() else {
            return Err(DescriptorError::InvalidFieldType {
                field: "Plugins",
                expected: "objects with string Name and boolean Enabled fields",
            });
        };
        reject_duplicate_reference_keys(reference)?;
        let Some(name_property) = reference.get("Name") else {
            return Err(DescriptorError::InvalidFieldType {
                field: "Plugins",
                expected: "objects with string Name and boolean Enabled fields",
            });
        };
        let Value::StringLit(name) = &name_property.value else {
            return Err(DescriptorError::InvalidFieldType {
                field: "Plugins",
                expected: "objects with string Name and boolean Enabled fields",
            });
        };
        if name.value.trim().is_empty() {
            return Err(DescriptorError::InvalidFieldValue {
                field: "Plugins",
                message: "plugin reference names cannot be empty".to_owned(),
            });
        }
        if !dependency_names.insert(name.value.to_ascii_lowercase()) {
            return Err(DescriptorError::InvalidFieldValue {
                field: "Plugins",
                message: format!("plugin reference \"{}\" appears more than once", name.value),
            });
        }
        let enabled = optional_boolean(reference, "Enabled")?.unwrap_or(false);
        let optional = optional_boolean(reference, "Optional")?.unwrap_or(false);
        if enabled {
            dependencies.push(PluginDependencyReference {
                name: name.value.to_string(),
                optional,
            });
        }
    }
    dependencies.sort_by(|left, right| {
        left.name
            .to_ascii_lowercase()
            .cmp(&right.name.to_ascii_lowercase())
            .then_with(|| left.name.cmp(&right.name))
    });
    Ok(dependencies)
}

fn reject_duplicate_reference_keys(
    object: &Object<'_>,
) -> std::result::Result<(), DescriptorError> {
    const MODELED_FIELDS: [&str; 3] = ["Name", "Enabled", "Optional"];
    let mut names = HashSet::with_capacity(MODELED_FIELDS.len());
    for property in &object.properties {
        let name = property.name.as_str();
        if MODELED_FIELDS.contains(&name) && !names.insert(name.to_owned()) {
            return Err(DescriptorError::DuplicateKey {
                key: format!("Plugins[].{name}"),
            });
        }
    }
    Ok(())
}

fn optional_boolean(
    object: &Object<'_>,
    name: &'static str,
) -> std::result::Result<Option<bool>, DescriptorError> {
    match object.get(name) {
        Some(property) => match &property.value {
            Value::BooleanLit(value) => Ok(Some(value.value)),
            _ => Err(DescriptorError::InvalidFieldType {
                field: "Plugins",
                expected: "objects with string Name and boolean Enabled fields",
            }),
        },
        None => Ok(None),
    }
}

fn optional_string(
    object: &Object<'_>,
    name: &'static str,
) -> std::result::Result<Option<String>, DescriptorError> {
    match object.get(name) {
        Some(property) => match &property.value {
            Value::StringLit(value) => Ok(Some(value.value.to_string())),
            _ => Err(DescriptorError::InvalidFieldType {
                field: name,
                expected: "a string",
            }),
        },
        None => Ok(None),
    }
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

fn detect_indentation(
    text: &str,
    object: &Object<'_>,
    multiline: bool,
) -> (DescriptorIndentation, Vec<u8>) {
    if !multiline {
        return (
            DescriptorIndentation {
                kind: DescriptorIndentationKind::Compact,
                width: None,
            },
            Vec::new(),
        );
    }

    let samples: Vec<&str> = object
        .properties
        .iter()
        .filter_map(|property| {
            let start = line_start(text.as_bytes(), property.range.start);
            let prefix = &text[start..property.range.start];
            prefix
                .bytes()
                .all(|byte| matches!(byte, b' ' | b'\t'))
                .then_some(prefix)
        })
        .collect();
    let insertion = samples.first().map_or_else(
        || default_insertion_indent(text, object),
        |value| value.as_bytes().to_vec(),
    );
    let Some(first) = samples.first() else {
        return (indentation_for_prefix(&insertion), insertion);
    };
    if samples.iter().any(|sample| sample != first) {
        return (
            DescriptorIndentation {
                kind: DescriptorIndentationKind::Mixed,
                width: None,
            },
            insertion,
        );
    }
    (indentation_for_prefix(first.as_bytes()), insertion)
}

fn default_insertion_indent(text: &str, object: &Object<'_>) -> Vec<u8> {
    let closing_brace = object.range.end.saturating_sub(1);
    let closing_line = line_start(text.as_bytes(), closing_brace);
    let mut indent = text.as_bytes()[closing_line..closing_brace].to_vec();
    if !indent.iter().all(|byte| matches!(byte, b' ' | b'\t')) {
        indent.clear();
    }
    indent.extend_from_slice(b"  ");
    indent
}

fn indentation_for_prefix(prefix: &[u8]) -> DescriptorIndentation {
    let kind = if prefix.iter().all(|byte| *byte == b' ') {
        DescriptorIndentationKind::Spaces
    } else if prefix.iter().all(|byte| *byte == b'\t') {
        DescriptorIndentationKind::Tabs
    } else {
        DescriptorIndentationKind::Mixed
    };
    DescriptorIndentation {
        kind,
        width: (kind != DescriptorIndentationKind::Mixed).then_some(prefix.len()),
    }
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
                .map(|token| ByteSpan::from_range(token.range, byte_offset));
            PropertyLayout {
                property: ByteSpan::from_range(property.range, byte_offset),
                value: ByteSpan::from_range(property.value_range(), byte_offset),
                comma_after,
            }
        })
        .collect()
}

trait ObjectPropertyValueRange {
    fn value_range(&self) -> Range;
}

impl ObjectPropertyValueRange for jsonc_parser::ast::ObjectProp<'_> {
    fn value_range(&self) -> Range {
        match &self.value {
            Value::StringLit(value) => value.range,
            Value::NumberLit(value) => value.range,
            Value::BooleanLit(value) => value.range,
            Value::Object(value) => value.range,
            Value::Array(value) => value.range,
            Value::NullKeyword(value) => value.range,
        }
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
        jsonc_parser::ast::ObjectPropName::String(value) => value.range.end,
        jsonc_parser::ast::ObjectPropName::Word(value) => value.range.end,
    };
    let value_start = property.value_range().start;
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

fn collect_descriptor_paths(
    root: &Path,
    paths: &mut Vec<PathBuf>,
    warnings: &mut Vec<PluginScanWarning>,
) {
    const MAX_SCAN_WORKERS: usize = 4;
    let root_scan = scan_plugin_directory(root);
    paths.extend(root_scan.files);
    warnings.extend(root_scan.warnings);
    let worker_count = root_scan.directories.len().min(MAX_SCAN_WORKERS);
    if worker_count < 2 {
        for directory in root_scan.directories {
            let scan = collect_descriptor_paths_serial(directory);
            paths.extend(scan.paths);
            warnings.extend(scan.warnings);
        }
        return;
    }
    let mut buckets = vec![Vec::new(); worker_count];
    for (index, directory) in root_scan.directories.into_iter().enumerate() {
        buckets[index % worker_count].push(directory);
    }
    let scans = std::thread::scope(|scope| {
        let handles = buckets
            .iter()
            .map(|bucket| {
                let worker_bucket = bucket.clone();
                scope.spawn(move || {
                    let mut scan = DescriptorPathScan::default();
                    for directory in worker_bucket {
                        scan.extend(collect_descriptor_paths_serial(directory));
                    }
                    scan
                })
            })
            .collect::<Vec<_>>();
        buckets
            .into_iter()
            .zip(handles)
            .map(|(bucket, handle)| {
                if let Ok(scan) = handle.join() {
                    scan
                } else {
                    let mut scan = DescriptorPathScan::default();
                    for directory in bucket {
                        scan.extend(collect_descriptor_paths_serial(directory));
                    }
                    scan
                }
            })
            .collect::<Vec<_>>()
    });
    for scan in scans {
        paths.extend(scan.paths);
        warnings.extend(scan.warnings);
    }
}

#[derive(Default)]
struct DescriptorPathScan {
    paths: Vec<PathBuf>,
    warnings: Vec<PluginScanWarning>,
}

impl DescriptorPathScan {
    fn extend(&mut self, other: Self) {
        self.paths.extend(other.paths);
        self.warnings.extend(other.warnings);
    }
}

struct PluginDirectoryScan {
    directories: Vec<PathBuf>,
    files: Vec<PathBuf>,
    warnings: Vec<PluginScanWarning>,
}

fn scan_plugin_directory(root: &Path) -> PluginDirectoryScan {
    let mut directories = Vec::new();
    let mut files = Vec::new();
    let mut warnings = Vec::new();
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) => {
            warnings.push(scan_warning(
                root.to_path_buf(),
                format!(
                    "Plugin directory scan failed: {error}. Check the directory permissions and retry."
                ),
            ));
            return PluginDirectoryScan {
                directories,
                files,
                warnings,
            };
        }
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                warnings.push(scan_warning(
                    root.to_path_buf(),
                    format!(
                        "Plugin directory entry failed: {error}. Check the directory and retry."
                    ),
                ));
                continue;
            }
        };
        let path = entry.path();
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(error) => {
                warnings.push(scan_warning(
                    path,
                    format!(
                        "Plugin path inspection failed: {error}. Check the path permissions and retry."
                    ),
                ));
                continue;
            }
        };
        if file_type.is_dir() {
            directories.push(path);
        } else if file_type.is_file() && is_plugin_descriptor(&path) {
            files.push(path);
        }
    }
    PluginDirectoryScan {
        directories,
        files,
        warnings,
    }
}

fn collect_descriptor_paths_serial(root: PathBuf) -> DescriptorPathScan {
    let mut scan = DescriptorPathScan::default();
    let mut pending = vec![root];
    while let Some(directory) = pending.pop() {
        let directory_scan = scan_plugin_directory(&directory);
        pending.extend(directory_scan.directories);
        scan.paths.extend(directory_scan.files);
        scan.warnings.extend(directory_scan.warnings);
    }
    scan
}

fn scan_warning(path: PathBuf, message: String) -> PluginScanWarning {
    PluginScanWarning {
        code: PluginScanWarningCode::ScanFailed,
        path,
        message,
    }
}

fn is_plugin_descriptor(path: &Path) -> bool {
    path.extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("uplugin"))
}

fn dedicated_line_span(bytes: &[u8], property: ByteSpan, removal_end: usize) -> Option<ByteSpan> {
    let start = line_start(bytes, property.start);
    if !bytes[start..property.start]
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
) -> std::result::Result<Vec<u8>, DescriptorError> {
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
            return Err(DescriptorError::EditFailed {
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
    use std::path::Path;

    use super::{
        DeclaredPluginState, DescriptorDocument, DescriptorEncoding, DescriptorIndentationKind,
        DescriptorLineEnding, MAX_DESCRIPTOR_BYTES, PluginScanWarningCode, scan_engine_plugins,
    };
    use crate::discovery::{DiscoverySource, EngineHealth, EngineInstallation};
    use tempfile::tempdir;

    const NORMAL_UNSET: &[u8] =
        include_bytes!("../../../tests/fixtures/descriptors/normal-unset.uplugin");
    const TRAILING_DISABLED: &[u8] =
        include_bytes!("../../../tests/fixtures/descriptors/trailing-comma-disabled.uplugin");
    const COMPACT_ENABLED: &[u8] =
        include_bytes!("../../../tests/fixtures/descriptors/compact-enabled-last.uplugin");
    const TABS_ENABLED: &[u8] =
        include_bytes!("../../../tests/fixtures/descriptors/tabs-enabled-first.uplugin");
    const TWO_SPACE_MIDDLE: &[u8] =
        include_bytes!("../../../tests/fixtures/descriptors/two-space-disabled-middle.uplugin");
    const COMMENTS_MIXED: &[u8] =
        include_bytes!("../../../tests/fixtures/descriptors/comments-mixed-unset.uplugin");
    const EMPTY_OBJECT: &[u8] =
        include_bytes!("../../../tests/fixtures/descriptors/empty-object.uplugin");
    const TRUNCATED: &[u8] =
        include_bytes!("../../../tests/fixtures/descriptors/truncated.uplugin");
    const DUPLICATE_TARGET: &[u8] =
        include_bytes!("../../../tests/fixtures/descriptors/duplicate-target.uplugin");
    const GOLDEN_NORMAL_ENABLED: &[u8] =
        include_bytes!("../../../tests/fixtures/descriptors/golden/normal-enabled.uplugin");
    const GOLDEN_TABS_UNSET: &[u8] =
        include_bytes!("../../../tests/fixtures/descriptors/golden/tabs-unset.uplugin");
    const GOLDEN_TRAILING_UNSET: &[u8] =
        include_bytes!("../../../tests/fixtures/descriptors/golden/trailing-unset.uplugin");
    const GOLDEN_COMPACT_DISABLED: &[u8] =
        include_bytes!("../../../tests/fixtures/descriptors/golden/compact-disabled.uplugin");
    const BOM_HEX: &str =
        include_str!("../../../tests/fixtures/descriptors/utf8-bom-crlf-enabled.hex");

    #[test]
    fn corpus_retains_state_and_source_metadata() -> Result<(), Box<dyn Error>> {
        let normal = DescriptorDocument::parse(NORMAL_UNSET)?;
        assert_eq!(normal.declared_state(), DeclaredPluginState::Unspecified);
        assert_eq!(
            normal.source_metadata().line_ending,
            DescriptorLineEnding::Lf
        );
        assert_eq!(
            normal.source_metadata().indentation.kind,
            DescriptorIndentationKind::Spaces
        );
        assert_eq!(normal.source_metadata().indentation.width, Some(2));

        let trailing = DescriptorDocument::parse(TRAILING_DISABLED)?;
        assert_eq!(trailing.declared_state(), DeclaredPluginState::Disabled);
        assert!(trailing.source_metadata().trailing_comma);

        let compact = DescriptorDocument::parse(COMPACT_ENABLED)?;
        assert_eq!(
            compact.source_metadata().line_ending,
            DescriptorLineEnding::Lf
        );
        assert_eq!(
            compact.source_metadata().indentation.kind,
            DescriptorIndentationKind::Compact
        );

        let tabs = DescriptorDocument::parse(TABS_ENABLED)?;
        assert_eq!(
            tabs.source_metadata().indentation.kind,
            DescriptorIndentationKind::Tabs
        );
        assert_eq!(tabs.source_metadata().indentation.width, Some(1));

        let middle = DescriptorDocument::parse(TWO_SPACE_MIDDLE)?;
        assert_eq!(middle.declared_state(), DeclaredPluginState::Disabled);

        let mixed = DescriptorDocument::parse(COMMENTS_MIXED)?;
        assert_eq!(
            mixed.source_metadata().indentation.kind,
            DescriptorIndentationKind::Mixed
        );

        let empty = DescriptorDocument::parse(EMPTY_OBJECT)?;
        assert_eq!(empty.declared_state(), DeclaredPluginState::Unspecified);

        let bom = decode_hex(BOM_HEX)?;
        let bom_document = DescriptorDocument::parse(&bom)?;
        assert_eq!(
            bom_document.source_metadata().encoding,
            DescriptorEncoding::Utf8Bom
        );
        assert_eq!(
            bom_document.source_metadata().line_ending,
            DescriptorLineEnding::CrLf
        );
        Ok(())
    }

    #[test]
    fn dependency_parsing_keeps_only_enabled_references() -> Result<(), Box<dyn Error>> {
        let source = br#"{
            "Plugins": [
                {"Name":"EnabledDependency","Enabled":true,"Optional":true},
                {"Name":"DisabledDependency","Enabled":false},
                {"Name":"UnspecifiedDependency"}
            ]
        }"#;
        let document = DescriptorDocument::parse(source)?;
        let plugin = document.plugin_descriptor(Path::new("Invented.uplugin"), Path::new(""));

        assert_eq!(plugin.enabled_dependencies.len(), 1);
        assert_eq!(plugin.enabled_dependencies[0].name, "EnabledDependency");
        assert!(plugin.enabled_dependencies[0].optional);
        Ok(())
    }

    #[test]
    fn replacement_changes_only_the_boolean_token() -> Result<(), Box<dyn Error>> {
        let document = DescriptorDocument::parse(COMPACT_ENABLED)?;
        let span = document
            .source_metadata()
            .enabled_by_default
            .ok_or("target span is missing")?
            .value;
        let edited = document.edit_enabled_by_default(DeclaredPluginState::Disabled)?;

        assert_eq!(&edited[..span.start], &COMPACT_ENABLED[..span.start]);
        assert_eq!(&edited[span.start..span.start + 5], b"false");
        assert_eq!(&edited[span.start + 5..], &COMPACT_ENABLED[span.end..]);
        assert_eq!(edited, GOLDEN_COMPACT_DISABLED);
        Ok(())
    }

    #[test]
    fn insertion_uses_existing_line_and_indent_style() -> Result<(), Box<dyn Error>> {
        let document = DescriptorDocument::parse(NORMAL_UNSET)?;
        let edited = document.edit_enabled_by_default(DeclaredPluginState::Enabled)?;
        let text = String::from_utf8(edited)?;

        assert!(text.contains(
            "  \"SyntheticUnknown\": {\n    \"Keep\": \"unchanged\",\n    \"Nested\": [1, 2, 3]\n  },\n  \"EnabledByDefault\": true\n}"
        ));
        assert_eq!(text.as_bytes(), GOLDEN_NORMAL_ENABLED);
        assert_eq!(
            DescriptorDocument::parse(text.as_bytes())?.declared_state(),
            DeclaredPluginState::Enabled
        );
        Ok(())
    }

    #[test]
    fn removal_handles_first_and_trailing_comma_fields() -> Result<(), Box<dyn Error>> {
        let first = DescriptorDocument::parse(TABS_ENABLED)?
            .edit_enabled_by_default(DeclaredPluginState::Unspecified)?;
        let first_text = String::from_utf8(first)?;
        assert!(!first_text.contains("EnabledByDefault"));
        assert!(first_text.starts_with("{\n\t\"FileVersion\""));
        assert_eq!(first_text.as_bytes(), GOLDEN_TABS_UNSET);

        let middle = DescriptorDocument::parse(TWO_SPACE_MIDDLE)?
            .edit_enabled_by_default(DeclaredPluginState::Unspecified)?;
        let middle_text = String::from_utf8(middle)?;
        assert!(!middle_text.contains("EnabledByDefault"));
        assert!(middle_text.contains("\"FriendlyName\": \"Invented Café\""));

        let trailing = DescriptorDocument::parse(TRAILING_DISABLED)?
            .edit_enabled_by_default(DeclaredPluginState::Unspecified)?;
        let trailing_text = String::from_utf8(trailing)?;
        assert!(!trailing_text.contains("EnabledByDefault"));
        assert!(
            trailing_text
                .contains("\"Description\": \"Synthetic descriptor with a trailing comma.\",\n}")
        );
        assert_eq!(trailing_text.as_bytes(), GOLDEN_TRAILING_UNSET);
        DescriptorDocument::parse(trailing_text.as_bytes())?;
        Ok(())
    }

    #[test]
    fn bom_and_crlf_survive_targeted_edits() -> Result<(), Box<dyn Error>> {
        let bytes = decode_hex(BOM_HEX)?;
        let edited = DescriptorDocument::parse(&bytes)?
            .edit_enabled_by_default(DeclaredPluginState::Disabled)?;

        assert!(edited.starts_with(b"\xEF\xBB\xBF"));
        assert_eq!(
            edited
                .windows(2)
                .filter(|window| *window == b"\r\n")
                .count(),
            bytes.windows(2).filter(|window| *window == b"\r\n").count()
        );
        assert!(!edited.windows(2).any(|window| window == b"\n\n"));
        Ok(())
    }

    #[test]
    fn ambiguous_and_bounded_inputs_are_rejected() {
        let repeated_unknown =
            br#"{"SyntheticMarker":1,"SyntheticMarker":2,"EnabledByDefault":true}"#;
        let wrong_type = br#"{"EnabledByDefault":"true"}"#;
        let duplicate_dependency =
            br#"{"Plugins":[{"Name":"Invented","Enabled":true},{"Name":"invented","Enabled":true}]}"#;
        let empty_dependency = br#"{"Plugins":[{"Name":"","Enabled":true}]}"#;
        let oversized = vec![b' '; MAX_DESCRIPTOR_BYTES + 1];
        let deeply_nested = format!(
            "{{\"SyntheticValue\":{}0{}}}",
            "[".repeat(513),
            "]".repeat(513)
        );

        assert!(DescriptorDocument::parse(DUPLICATE_TARGET).is_err());
        assert!(DescriptorDocument::parse(TRUNCATED).is_err());
        assert!(DescriptorDocument::parse(repeated_unknown).is_ok());
        assert!(DescriptorDocument::parse(wrong_type).is_err());
        assert!(DescriptorDocument::parse(duplicate_dependency).is_err());
        assert!(DescriptorDocument::parse(empty_dependency).is_err());
        assert!(DescriptorDocument::parse(&oversized).is_err());
        assert!(DescriptorDocument::parse(deeply_nested.as_bytes()).is_err());
    }

    #[test]
    fn empty_and_commented_objects_accept_targeted_insertion() -> Result<(), Box<dyn Error>> {
        for input in [EMPTY_OBJECT, COMMENTS_MIXED] {
            let edited = DescriptorDocument::parse(input)?
                .edit_enabled_by_default(DeclaredPluginState::Enabled)?;
            assert_eq!(
                DescriptorDocument::parse(&edited)?.declared_state(),
                DeclaredPluginState::Enabled
            );
        }
        Ok(())
    }

    #[test]
    fn generated_parser_and_editor_inputs_keep_invariants() -> Result<(), Box<dyn Error>> {
        let mut seed = 0xD1CE_BA5Eu32;
        for iteration in 0..2_000 {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let enabled = seed & 1 == 0;
            let trailing = seed & 2 == 0;
            let tabs = seed & 4 == 0;
            let crlf = seed & 8 == 0;
            let line = if crlf { "\r\n" } else { "\n" };
            let indent = if tabs { "\t" } else { "  " };
            let comma = if trailing { "," } else { "" };
            let input = format!(
                "{{{line}{indent}\"FileVersion\": 3,{line}{indent}\"Description\": \"Invented {iteration}\",{line}{indent}\"EnabledByDefault\": {enabled}{comma}{line}}}{line}"
            );
            let document = DescriptorDocument::parse(input.as_bytes())?;
            for requested in [
                DeclaredPluginState::Enabled,
                DeclaredPluginState::Disabled,
                DeclaredPluginState::Unspecified,
            ] {
                let edited = document.edit_enabled_by_default(requested)?;
                let verified = DescriptorDocument::parse(&edited)?;
                assert_eq!(verified.declared_state(), requested);
            }

            let mut noise = vec![0_u8; (seed as usize % 96) + 1];
            for byte in &mut noise {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                *byte = (seed & 0x7f) as u8;
            }
            let _ = DescriptorDocument::parse(&noise);
        }
        Ok(())
    }

    #[test]
    fn engine_scan_keeps_valid_plugins_when_one_descriptor_fails() -> Result<(), Box<dyn Error>> {
        let temp = tempdir()?;
        let plugin_root = temp.path().join("Engine").join("Plugins");
        fs::create_dir_all(&plugin_root)?;
        fs::write(plugin_root.join("InventedValid.uplugin"), NORMAL_UNSET)?;
        fs::write(plugin_root.join("InventedBroken.uplugin"), TRUNCATED)?;
        let engine = EngineInstallation {
            path: temp.path().to_path_buf(),
            version: Some("5.8.0-test".to_owned()),
            source: DiscoverySource::Explicit,
            health: EngineHealth::Partial,
            descriptor_count: 2,
            issues: Vec::new(),
        };

        let report = scan_engine_plugins(&engine)?;

        assert_eq!(report.plugins.len(), 1);
        assert_eq!(report.plugins[0].name, "InventedValid");
        assert_eq!(report.warnings.len(), 1);
        assert_eq!(report.warnings[0].code, PluginScanWarningCode::ParseFailed);
        Ok(())
    }

    fn decode_hex(input: &str) -> Result<Vec<u8>, Box<dyn Error>> {
        let compact = input.trim();
        if !compact.len().is_multiple_of(2) {
            return Err("hex fixture has an odd byte count".into());
        }
        compact
            .as_bytes()
            .chunks_exact(2)
            .map(|pair| {
                let text = std::str::from_utf8(pair)?;
                Ok(u8::from_str_radix(text, 16)?)
            })
            .collect()
    }
}
