//!
//! Rust has no runtime reflection and `Serialize` exposes nothing about the fields a value
//! *can* carry -- a `skip_serializing_if` field simply vanishes from a serialised value -- so
//! the shapes are read out of the source instead, with `syn`.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::OnceLock;

use crate::meta::Token;

/// The document served at `/openapi.yaml`, named for the failure messages.
const SPEC_PATH: &str = "src/openapi/openapi.yaml";
const SPEC: &str = include_str!("../openapi/openapi.yaml");

/// The value a schema is pinned to: a DTO named by its Rust path, or -- for the handlers that
/// answer with a map rather than a struct -- the JSON that map serialises to.
enum Shape {
    Ty(&'static str),
    Json(serde_json::Value),
}

impl Shape {
    fn json_fields(&self) -> Vec<JsonField> {
        match self {
            Self::Ty(query) => struct_json_fields(query),
            Self::Json(value) => map_json_fields(value),
        }
    }
}

/// Pins each response schema to the Rust value the handler actually serialises.
/// A schema is only useful if it is true: the console's client is generated from
/// it, so a field the document invents does not exist at runtime, and a field it
/// omits is invisible to the UI even though the server sends it.
///
/// This compares the property set and which properties are required, not types.
/// It catches the failure that actually happens -- a field added to a DTO and
/// forgotten in the document -- without turning the document into a second copy
/// of the Rust structs.
///
/// Requiredness is not decoration. A field without `skip_serializing_if` is on
/// the wire on every response, but a schema that does not say so generates an
/// optional field, and every call site then carries a guard for a case that
/// cannot occur.
#[test]
fn spec_schemas_match_response_shapes() {
    let schemas = spec_schemas();
    let cases: Vec<(&str, Shape)> = vec![
        ("User", Shape::Ty("UserDTO")),
        ("RoleRef", Shape::Ty("RoleRefDTO")),
        ("Permission", Shape::Ty("PermissionDTO")),
        ("Role", Shape::Ty("RoleDTO")),
        (
            "Token",
            Shape::Json(
                serde_json::to_value(crate::api::auth::token_summary(Token::default()))
                    .expect("marshal token summary"),
            ),
        ),
        ("IssuedToken", Shape::Ty("IssuedTokenDTO")),
        ("HAStatus", Shape::Ty("HAStatus")),
        ("GroupMapping", Shape::Ty("GroupMappingDTO")),
        ("Repository", Shape::Ty("RepositoryDTO")),
        ("RepositoryListItem", Shape::Ty("RepositoryListItemDTO")),
        (
            "RepositoryCapabilities",
            Shape::Ty("RepositoryCapabilitiesDTO"),
        ),
        ("RepositoryName", Shape::Ty("RepositoryNameDTO")),
        ("RepoPermission", Shape::Ty("RepoPermissionDTO")),
        ("RepoToken", Shape::Ty("RepoTokenDTO")),
        ("PackageApprovalList", Shape::Ty("ApprovalListDTO")),
        ("ApprovalCount", Shape::Ty("ApprovalCountDTO")),
        ("PendingApprovalRepoList", Shape::Ty("PendingRepoListDTO")),
        ("VersionDenyList", Shape::Ty("VersionDenyListDTO")),
        ("PackageApproval", Shape::Ty("ApprovalDTO")),
        ("VersionDeny", Shape::Ty("VersionDenyDTO")),
        ("AuditLogList", Shape::Ty("AuditLogListDTO")),
        ("AuditLog", Shape::Ty("AuditLogDTO")),
        (
            "NotificationSamplePreview",
            Shape::Ty("NotificationSamplePreviewDTO"),
        ),
        (
            "NotificationSampleReceiver",
            Shape::Ty("SampleReceiverInfo"),
        ),
        (
            "NotificationSampleReport",
            Shape::Ty("NotificationSampleReportDTO"),
        ),
        (
            "NotificationSampleResult",
            Shape::Ty("NotificationSampleResultDTO"),
        ),
        ("ApprovalAlarmPayload", Shape::Ty("ApprovalPayload")),
        ("Receiver", Shape::Ty("ReceiverDTO")),
        ("Announcement", Shape::Ty("AnnouncementDTO")),
        ("SessionIdentity", Shape::Ty("SessionIdentityDTO")),
        ("ImpersonationStart", Shape::Ty("ImpersonationStartDTO")),
        ("CreatedUser", Shape::Ty("CreatedUserDTO")),
        ("StatusMessage", Shape::Ty("StatusMessageDTO")),
        ("UploadSession", Shape::Ty("UploadSessionDTO")),
        (
            "PublicationLifecycleResult",
            Shape::Ty("PublicationLifecycleResult"),
        ),
        ("ArtifactPurgeResult", Shape::Ty("ArtifactPurgeResultDTO")),
        (
            "ArtifactForceDeleteResult",
            Shape::Ty("ArtifactForceDeleteResultDTO"),
        ),
        ("BulkApproveResult", Shape::Ty("BulkApproveResultDTO")),
        ("ArtifactList", Shape::Ty("ArtifactListDTO")),
        ("Artifact", Shape::Ty("ArtifactDTO")),
        ("ArtifactPublication", Shape::Ty("PublicationDTO")),
        ("StorageStats", Shape::Ty("StorageStats")),
        ("DiskUsage", Shape::Ty("Disk")),
        ("DanglingRef", Shape::Ty("DanglingRefDTO")),
        ("StatusCount", Shape::Ty("StatusCountDTO")),
        ("VulnAdvisory", Shape::Ty("VulnAdvisory")),
        ("PendingApprovalRepo", Shape::Ty("PendingRepoDTO")),
        ("ClusterStats", Shape::Ty("ClusterStats")),
        ("MigrationList", Shape::Ty("MigrationListDTO")),
        ("MigrationSummary", Shape::Ty("MigrationSummary")),
        ("MigrationRecord", Shape::Ty("MigrationRecord")),
        ("MigrationCheck", Shape::Ty("MigrationCheck")),
        ("MigrationCheckStatus", Shape::Ty("MigrationCheckStatus")),
        ("MigrationStoredAt", Shape::Ty("MigrationStoredAt")),
        ("MigrationLocation", Shape::Ty("MigrationLocation")),
        ("MigrationSettings", Shape::Ty("MigrationSettings")),
        // Forklift coverage.
        ("CoverageProject", Shape::Ty("coverage::types::Project")),
        ("CoverageExcludedProject", Shape::Ty("ExcludedProject")),
        ("CoverageProgress", Shape::Ty("Progress")),
        ("CoverageOverview", Shape::Ty("Overview")),
        ("CoverageGroup", Shape::Ty("GroupCoverage")),
        ("CoverageSnapshot", Shape::Ty("Snapshot")),
        ("CoverageLastCommit", Shape::Ty("LastCommit")),
        ("CoveragePipeline", Shape::Ty("Pipeline")),
        ("CoveragePipelineFile", Shape::Ty("PipelineFile")),
        ("CoverageMute", Shape::Ty("CoverageMuteDTO")),
        ("CoverageHostCheck", Shape::Ty("HostCheck")),
        ("CoverageGitlabCheck", Shape::Ty("GitLabCheck")),
        ("CoverageSettings", Shape::Ty("CoverageSettingsDTO")),
        ("CoverageScanStarted", Shape::Ty("CoverageScanStartedDTO")),
        ("CoverageAlarmPayload", Shape::Ty("CoveragePayload")),
        ("CoverageAlarmPreview", Shape::Ty("CoverageAlarmPreviewDTO")),
        // Security insights.
        ("InsightsOverview", Shape::Ty("InsightsDTO")),
        ("InsightSummary", Shape::Ty("InsightSummary")),
        ("InsightReport", Shape::Ty("insights::model::Report")),
        ("InsightFinding", Shape::Ty("insights::model::Finding")),
        ("InsightAffected", Shape::Ty("insights::model::Affected")),
        ("InsightCheck", Shape::Ty("insights::model::Check")),
        ("InsightAcknowledgement", Shape::Ty("Acknowledgement")),
        ("InsightAck", Shape::Ty("AckDTO")),
        ("InsightCheckPoint", Shape::Ty("CheckPointDTO")),
        ("InsightsAlarmPreview", Shape::Ty("AlarmPreviewDTO")),
        ("InsightsAlarmPayload", Shape::Ty("InsightsPayload")),
        ("InsightSettings", Shape::Ty("InsightSettingsDTO")),
        ("InsightSeverityCounts", Shape::Ty("SeverityCounts")),
        ("RetentionPreview", Shape::Ty("RetentionPreview")),
        ("RetentionPreviewItem", Shape::Ty("RetentionPreviewItem")),
        // The policy document and each of its sections. Every section is a value
        // field, so it is always on the wire; a client that had to guard each one
        // would be guarding a case the server never produces.
        ("RepoConfig", Shape::Ty("repoconfig::Config")),
        ("CacheConfig", Shape::Ty("CacheConfig")),
        ("AgePolicyConfig", Shape::Ty("AgePolicyConfig")),
        ("ApprovalConfig", Shape::Ty("ApprovalConfig")),
        ("PolicyPipelineConfig", Shape::Ty("PolicyPipelineConfig")),
        ("GroupConfig", Shape::Ty("GroupConfig")),
        ("IPACLConfig", Shape::Ty("IPACLConfig")),
        ("NotifyConfig", Shape::Ty("repoconfig::NotifyConfig")),
        ("RetentionConfig", Shape::Ty("RetentionConfig")),
        ("VulnPolicyConfig", Shape::Ty("VulnPolicyConfig")),
        ("LicensePolicyConfig", Shape::Ty("LicensePolicyConfig")),
        ("UploadConfig", Shape::Ty("repoconfig::UploadConfig")),
        ("UpstreamAuthConfig", Shape::Ty("UpstreamAuthConfig")),
        // The Cargo Registry Web API bodies are built as maps.
        (
            "CargoSearchResult",
            Shape::Json(crate::repo::cargo_search::search_body(Vec::new(), 0)),
        ),
        (
            "CargoCrateSummary",
            Shape::Json(crate::repo::cargo_search::crate_summary_body(
                &crate::repo::cargo_search::CrateHit {
                    name: "widget".to_string(),
                    max_version: "1.0.0".to_string(),
                },
                "",
            )),
        ),
        (
            "CargoPublishResult",
            Shape::Json(crate::repo::cargo::cargo_publish_body()),
        ),
        ("CargoOk", Shape::Json(crate::repo::cargo::cargo_ok_body())),
        (
            "CargoErrors",
            Shape::Json(crate::repo::cargo::cargo_error_body("detail")),
        ),
    ];

    let mut failures = Vec::new();
    for (schema, shape) in &cases {
        let declared = schemas
            .get(*schema)
            .unwrap_or_else(|| panic!("{schema} is not defined in {SPEC_PATH}"));
        let fields = shape.json_fields();
        let serialised = names(&fields);

        for k in &serialised {
            if !declared.properties.contains(k) {
                failures.push(format!(
                            "handlers send {k:?} but {schema} does not declare it; the generated client cannot see it"
                        ));
            }
        }
        for k in &declared.properties {
            if !serialised.contains(k) {
                failures.push(format!(
                            "{schema} declares {k:?} but handlers never send it; the generated client exposes a field that is always undefined"
                        ));
            }
        }

        for f in &fields {
            let name = &f.name;
            if !f.omitempty && !declared.required.contains(name) {
                failures.push(format!(
                            "{schema}.{name} is sent on every response but the schema does not require it; the generated client makes callers guard a field that is always there"
                        ));
            } else if f.omitempty && declared.required.contains(name) {
                failures.push(format!(
                            "{schema}.{name} is omitempty, so responses can leave it out, but the schema requires it"
                        ));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// One schema flattened: everything it declares plus everything it composes in
/// through `allOf`.
pub(crate) struct SchemaShape {
    pub(crate) properties: Vec<String>,
    pub(crate) required: Vec<String>,
}

/// Pins each named request-body schema to the Rust value the handler decodes
/// into. A field the document invents is one a client can send and the server
/// will silently drop; a field it omits is one the server accepts but no
/// generated client can offer.
///
/// Unlike responses, requiredness is not read off the struct: whether a field
/// may be left out is a validation decision the handler makes, not something an
/// attribute records. Only the property set is compared.
#[test]
fn spec_schemas_match_request_shapes() {
    let schemas = spec_schemas();
    let cases = [
        ("RoleCreate", "CreateRoleReq"),
        ("PermissionCreate", "AddPermissionReq"),
        ("AnnouncementInput", "AnnouncementInput"),
        ("LoginInput", "LoginReq"),
        ("RepositoryCreate", "CreateRepositoryReq"),
        ("RepositoryInput", "UpdateRepositoryReq"),
        ("RepositorySecurityInput", "UpdateRepositorySecurityReq"),
        ("RepositoryDisabledInput", "SetDisabledReq"),
        ("UpstreamCheckInput", "CheckUpstreamReq"),
        ("UploadValidationInput", "ValidateUploadReq"),
        ("PublicationYankInput", "YankPublicationReq"),
        ("TokenCreate", "CreateTokenReq"),
        ("TokenScopesInput", "UpdateTokenReq"),
        ("UserCreate", "CreateUserReq"),
        ("UserInput", "UpdateUserReq"),
        ("ImpersonationInput", "ImpersonateReq"),
        ("RoleAssignmentCreate", "AssignRoleReq"),
        ("PackageApprovalCreate", "CreateApprovalReq"),
        ("BulkApproveInput", "ApproveAllReq"),
        ("ApprovalDecisionInput", "DecideApprovalReq"),
        ("VersionDenyCreate", "CreateVersionDenyReq"),
        ("GroupMappingCreate", "CreateGroupMappingReq"),
        ("ReceiverInput", "ReceiverReq"),
        ("WebhookTestInput", "WebhookTestReq"),
    ];

    let mut failures = Vec::new();
    for (schema, ty) in cases {
        let declared = schemas
            .get(schema)
            .unwrap_or_else(|| panic!("{schema} is not defined in {SPEC_PATH}"));
        let accepted = names(&struct_json_fields(ty));

        for k in &accepted {
            if !declared.properties.contains(k) {
                failures.push(format!(
                            "handlers accept {k:?} but {schema} does not declare it; no generated client can send it"
                        ));
            }
        }
        for k in &declared.properties {
            if !accepted.contains(k) {
                failures.push(format!(
                            "{schema} declares {k:?} but handlers ignore it; clients send a field that does nothing"
                        ));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// Pins the token-scope action enum to the validator that actually rejects
/// requests.
///
/// The property-set tests above compare names, not values, so an enum can fall
/// behind without anything failing: the action stayed grantable, the document
/// kept listing three, and the generated client had no type for the fourth.
#[test]
fn spec_token_scope_actions_match_validator() {
    use crate::auth::{
        ACTION_ADMIN, ACTION_APPROVE, ACTION_AUDIT, ACTION_DELETE, ACTION_READ, ACTION_SECURITY,
        ACTION_WRITE, Scope,
    };

    let documented = spec_enum("TokenScope", "actions");

    // Every action the authorisation model defines, so the test sees an action
    // that becomes grantable as well as one that stops being.
    for action in [
        ACTION_READ,
        ACTION_WRITE,
        ACTION_DELETE,
        ACTION_APPROVE,
        ACTION_AUDIT,
        ACTION_SECURITY,
        ACTION_ADMIN,
    ] {
        let accepted = crate::api::auth::validate_scopes(&[Scope {
            repo_pattern: "*".to_string(),
            actions: vec![action.to_string()],
        }])
        .is_none();
        let listed = documented.iter().any(|v| v == action);
        assert!(
            !accepted || listed,
            "token scopes accept {action:?} but TokenScope.actions does not list it; no generated client can request it"
        );
        assert!(
            accepted || !listed,
            "TokenScope.actions lists {action:?} but token scopes reject it; the client offers a value the server 400s on"
        );
    }
}

// ---------------------------------------------------------------------------
// The document
// ---------------------------------------------------------------------------

/// One schema as the document declares it, before `allOf` is flattened.
#[derive(Clone, Default, serde::Deserialize)]
struct SchemaNode {
    #[serde(rename = "$ref", default)]
    reference: String,
    #[serde(default)]
    properties: BTreeMap<String, serde_yaml_ng::Value>,
    #[serde(default)]
    required: Vec<String>,
    #[serde(rename = "allOf", default)]
    all_of: Vec<Self>,
}

#[derive(serde::Deserialize)]
struct SpecComponents {
    #[serde(default)]
    schemas: BTreeMap<String, SchemaNode>,
}

#[derive(serde::Deserialize)]
struct SpecDoc {
    components: SpecComponents,
}

/// The declared and required property names of every object schema in the
/// document.
pub(crate) fn spec_schemas() -> BTreeMap<String, SchemaShape> {
    let doc: SpecDoc = serde_yaml_ng::from_str(SPEC).unwrap_or_else(|e| panic!("parse spec: {e}"));
    assert!(
        !doc.components.schemas.is_empty(),
        "no schemas found in {SPEC_PATH}"
    );

    let mut out = BTreeMap::new();
    for name in doc.components.schemas.keys() {
        let mut s = resolve_shape(&doc.components.schemas, name, &[]);
        s.properties.sort();
        s.properties.dedup();
        s.required.sort();
        s.required.dedup();
        out.insert(name.clone(), s);
    }
    out
}

/// Flattens a schema's own properties together with those of every schema it
/// composes through `allOf`, which is how a list shape is expressed as "the
/// detail shape plus these extras".
fn resolve_shape(all: &BTreeMap<String, SchemaNode>, name: &str, seen: &[String]) -> SchemaShape {
    assert!(
        !seen.iter().any(|s| s == name),
        "schema {name:?} composes itself"
    );
    let mut next = seen.to_vec();
    next.push(name.to_string());
    let node = all.get(name).cloned().unwrap_or_default();
    collect(all, &node, &next)
}

fn collect(all: &BTreeMap<String, SchemaNode>, n: &SchemaNode, seen: &[String]) -> SchemaShape {
    let mut s = SchemaShape {
        properties: n.properties.keys().cloned().collect(),
        required: n.required.clone(),
    };
    for part in &n.all_of {
        let nested = match part.reference.strip_prefix("#/components/schemas/") {
            Some(reference) => resolve_shape(all, reference, seen),
            None => collect(all, part, seen),
        };
        s.properties.extend(nested.properties);
        s.required.extend(nested.required);
    }
    s
}

/// The enum values declared for one array property's items.
fn spec_enum(schema: &str, property: &str) -> Vec<String> {
    #[derive(serde::Deserialize)]
    struct Doc {
        components: Components,
    }
    #[derive(serde::Deserialize)]
    struct Components {
        schemas: BTreeMap<String, Schema>,
    }
    #[derive(serde::Deserialize)]
    struct Schema {
        #[serde(default)]
        properties: BTreeMap<String, Property>,
    }
    #[derive(serde::Deserialize)]
    struct Property {
        #[serde(default)]
        items: Items,
    }
    #[derive(Default, serde::Deserialize)]
    struct Items {
        #[serde(default)]
        r#enum: Vec<String>,
    }

    let doc: Doc = serde_yaml_ng::from_str(SPEC).unwrap_or_else(|e| panic!("parse spec: {e}"));
    let values = doc
        .components
        .schemas
        .get(schema)
        .and_then(|s| s.properties.get(property))
        .map(|p| p.items.r#enum.clone())
        .unwrap_or_default();
    assert!(
        !values.is_empty(),
        "{schema}.{property} declares no enum in {SPEC_PATH}"
    );
    values
}

// ---------------------------------------------------------------------------
// The Rust shapes
// ---------------------------------------------------------------------------

/// One key a value serialises to, and whether responses may leave it out.
#[derive(Clone, PartialEq, Eq)]
struct JsonField {
    name: String,
    omitempty: bool,
}

fn names(fs: &[JsonField]) -> Vec<String> {
    let mut out: Vec<String> = fs.iter().map(|f| f.name.clone()).collect();
    out.sort();
    out.dedup();
    out
}

/// Every top-level key the named struct can serialise to, read from its
/// declaration.
fn struct_json_fields(query: &str) -> Vec<JsonField> {
    let mut fields = collect_json_fields(query, &[]);
    fields.sort_by(|a, b| a.name.cmp(&b.name));
    fields.dedup();
    fields
}

/// The keys a handler-built map serialises to. A map has no declaration to
/// read, so it is marshalled; a key the marshalled value carries is one the
/// handler always sets, since the only maps here are built from literals.
fn map_json_fields(value: &serde_json::Value) -> Vec<JsonField> {
    let obj = value
        .as_object()
        .unwrap_or_else(|| panic!("not a JSON object: {value}"));
    let mut fields: Vec<JsonField> = obj
        .keys()
        .map(|k| JsonField {
            name: k.clone(),
            omitempty: false,
        })
        .collect();
    fields.sort_by(|a, b| a.name.cmp(&b.name));
    fields
}

fn collect_json_fields(query: &str, seen: &[String]) -> Vec<JsonField> {
    let def = find_struct(query);
    assert!(
        !seen.contains(&def.path),
        "struct {} flattens itself",
        def.path
    );
    let mut next = seen.to_vec();
    next.push(def.path.clone());

    let mut out = Vec::new();
    for f in &def.fields {
        if f.skip {
            continue;
        }
        if f.flatten {
            out.extend(collect_json_fields(&f.ty, &next));
            continue;
        }
        let name = match &f.rename {
            Some(name) => name.clone(),
            None => apply_rename_all(def.rename_all.as_deref(), &f.ident),
        };
        out.push(JsonField {
            name,
            omitempty: f.omitempty,
        });
    }
    out
}

/// One struct declaration, reduced to what decides its JSON shape.
struct StructDef {
    /// Module path plus name, e.g. `api::auth::UserDTO`.
    path: String,
    rename_all: Option<String>,
    fields: Vec<RawField>,
}

struct RawField {
    ident: String,
    /// Last segment of the declared type, for resolving a flattened field.
    ty: String,
    rename: Option<String>,
    skip: bool,
    omitempty: bool,
    flatten: bool,
}

/// Looks a struct up by name, or by as much of its module path as it takes to
/// be unambiguous (`repoconfig::Config`).
fn find_struct(query: &str) -> &'static StructDef {
    let matches: Vec<&StructDef> = structs()
        .iter()
        .filter(|s| s.path == query || s.path.ends_with(&format!("::{query}")))
        .collect();
    match matches.len() {
        1 => matches[0],
        0 => panic!("no struct named {query} in src/"),
        _ => panic!(
            "{query} is ambiguous: {}",
            matches
                .iter()
                .map(|s| s.path.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn structs() -> &'static Vec<StructDef> {
    static INDEX: OnceLock<Vec<StructDef>> = OnceLock::new();
    INDEX.get_or_init(build_index)
}

/// Parses every non-test source file and indexes the structs it declares.
fn build_index() -> Vec<StructDef> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut out = Vec::new();
    let mut stack = vec![root.clone()];
    while let Some(dir) = stack.pop() {
        let entries =
            std::fs::read_dir(&dir).unwrap_or_else(|e| panic!("read {}: {e}", dir.display()));
        for entry in entries {
            let path = entry.expect("dir entry").path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            // Test files declare fixtures whose names would collide with the
            // types under test.
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default();
            if name.ends_with("_test.rs") {
                continue;
            }
            let src = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
            let file =
                syn::parse_file(&src).unwrap_or_else(|e| panic!("parse {}: {e}", path.display()));
            collect_structs(&file.items, &module_path(&root, &path), &mut out);
        }
    }
    assert!(
        !out.is_empty(),
        "no structs found under src/; the extraction is broken, not the source"
    );
    out
}

/// The module path a file's items live in: `src/api/auth.rs` is `api::auth`.
fn module_path(root: &Path, file: &Path) -> Vec<String> {
    let relative = file.strip_prefix(root).unwrap_or(file).with_extension("");
    relative
        .components()
        .map(|c| c.as_os_str().to_string_lossy().to_string())
        .filter(|s| s != "lib")
        .collect()
}

fn collect_structs(items: &[syn::Item], module: &[String], out: &mut Vec<StructDef>) {
    for item in items {
        match item {
            syn::Item::Struct(s) => {
                if let Some(def) = struct_def(s, module) {
                    out.push(def);
                }
            }
            syn::Item::Mod(m) => {
                // An inline `#[cfg(test)] mod` holds fixtures, not the shapes
                // the handlers answer with.
                if is_cfg_test(&m.attrs) {
                    continue;
                }
                if let Some((_, inner)) = &m.content {
                    let mut nested = module.to_vec();
                    nested.push(m.ident.to_string());
                    collect_structs(inner, &nested, out);
                }
            }
            _ => {}
        }
    }
}

fn is_cfg_test(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(|a| {
        a.path().is_ident("cfg")
            && matches!(&a.meta, syn::Meta::List(l) if l.tokens.to_string() == "test")
    })
}

fn struct_def(s: &syn::ItemStruct, module: &[String]) -> Option<StructDef> {
    let syn::Fields::Named(named) = &s.fields else {
        return None;
    };
    let mut path = module.to_vec();
    path.push(s.ident.to_string());

    let mut rename_all = None;
    for option in serde_options(&s.attrs) {
        if let SerdeOption::NameValue(key, value) = option
            && key == "rename_all"
        {
            rename_all = Some(value);
        }
    }

    let fields = named
        .named
        .iter()
        .map(|f| {
            let mut raw = RawField {
                ident: ident_name(f.ident.as_ref().expect("named field")),
                ty: type_name(&f.ty),
                rename: None,
                skip: false,
                omitempty: false,
                flatten: false,
            };
            for option in serde_options(&f.attrs) {
                match option {
                    SerdeOption::Path(key) => match key.as_str() {
                        "skip" | "skip_serializing" => raw.skip = true,
                        "flatten" => raw.flatten = true,
                        _ => {}
                    },
                    SerdeOption::NameValue(key, value) => match key.as_str() {
                        "rename" => raw.rename = Some(value),
                        "skip_serializing_if" => raw.omitempty = true,
                        _ => {}
                    },
                }
            }
            raw
        })
        .collect();

    Some(StructDef {
        path: path.join("::"),
        rename_all,
        fields,
    })
}

/// One entry inside `#[serde(...)]`: a bare flag or a `key = "value"` pair.
enum SerdeOption {
    Path(String),
    NameValue(String, String),
}

fn serde_options(attrs: &[syn::Attribute]) -> Vec<SerdeOption> {
    let mut out = Vec::new();
    for attr in attrs.iter().filter(|a| a.path().is_ident("serde")) {
        let parsed = attr
            .parse_args_with(
                syn::punctuated::Punctuated::<syn::Meta, syn::Token![,]>::parse_terminated,
            )
            .unwrap_or_default();
        for meta in parsed {
            match meta {
                syn::Meta::Path(p) => {
                    if let Some(ident) = p.get_ident() {
                        out.push(SerdeOption::Path(ident.to_string()));
                    }
                }
                syn::Meta::NameValue(nv) => {
                    let Some(ident) = nv.path.get_ident() else {
                        continue;
                    };
                    let value = match &nv.value {
                        syn::Expr::Lit(syn::ExprLit {
                            lit: syn::Lit::Str(s),
                            ..
                        }) => s.value(),
                        _ => String::new(),
                    };
                    out.push(SerdeOption::NameValue(ident.to_string(), value));
                }
                // `rename(serialize = "x")` and friends: none of the DTOs use
                // the split form, so it carries no shape information here.
                syn::Meta::List(_) => {}
            }
        }
    }
    out
}

/// A raw identifier (`r#type`) serialises under the name without the escape.
fn ident_name(ident: &syn::Ident) -> String {
    let s = ident.to_string();
    s.strip_prefix("r#").unwrap_or(&s).to_string()
}

fn type_name(ty: &syn::Type) -> String {
    match ty {
        syn::Type::Path(p) => p
            .path
            .segments
            .last()
            .map(|s| s.ident.to_string())
            .unwrap_or_default(),
        _ => String::new(),
    }
}

fn apply_rename_all(rule: Option<&str>, ident: &str) -> String {
    match rule {
        None | Some("snake_case") => ident.to_string(),
        Some("camelCase") => {
            let pascal = apply_rename_all(Some("PascalCase"), ident);
            let mut chars = pascal.chars();
            match chars.next() {
                Some(first) => first.to_lowercase().collect::<String>() + chars.as_str(),
                None => pascal,
            }
        }
        Some("PascalCase") => ident
            .split('_')
            .map(|word| {
                let mut chars = word.chars();
                match chars.next() {
                    Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                    None => String::new(),
                }
            })
            .collect(),
        Some("kebab-case") => ident.replace('_', "-"),
        Some("SCREAMING_SNAKE_CASE") => ident.to_uppercase(),
        Some("lowercase") => ident.to_lowercase(),
        Some("UPPERCASE") => ident.to_uppercase(),
        Some(other) => panic!("unhandled serde rename_all rule {other:?}"),
    }
}
