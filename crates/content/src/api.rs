//! Versioned operations shared by trusted application adapters and CLI.
//! Identity is supplied separately by authentication, never operation JSON.
use crate::extraction_api::ExtractionResult;
use crate::search::{SearchMode, SearchRequest, SearchResult};
use crate::{
    auth::TrustedIdentity, BinaryReadResult, Change, ContentError, FileDiff, Origin, Proposal,
    ReadResult, Revision, Service,
};
use serde::{Deserialize, Serialize};

pub const API_VERSION: u32 = 2;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub version: u32,
    #[serde(default)]
    pub idempotency_key: Option<String>,
    pub operation: Operation,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Operation {
    ImportPreview {
        base_revision: String,
        files: Vec<crate::onboarding::ImportFile>,
        #[serde(default)]
        offset: usize,
        #[serde(default)]
        next_offset: Option<usize>,
        #[serde(default)]
        conflict_policy: crate::onboarding::ConflictPolicy,
    },
    ImportCommit {
        preview: crate::onboarding::ImportPreview,
        mode: crate::onboarding::ImportMode,
        /// Recorded on the import revision, or the proposal's reason.
        #[serde(default)]
        message: Option<String>,
        #[serde(default)]
        origin: Option<Origin>,
    },
    ExportSnapshot {
        #[serde(default)]
        scope: String,
        #[serde(default)]
        revision: Option<String>,
        #[serde(default)]
        offset: usize,
        #[serde(default = "import_default_batch")]
        limit: usize,
        #[serde(default)]
        expected_policy_digest: Option<String>,
        /// `"array"` (the default) or `"base64"`.
        #[serde(default)]
        encoding: Option<String>,
    },
    Properties {
        path: String,
        #[serde(default)]
        revision: Option<String>,
    },
    Outline {
        path: String,
        #[serde(default)]
        revision: Option<String>,
    },
    /// Exactly one of `section_id` and `section` (an id or an exact heading title).
    ReadSection {
        path: String,
        #[serde(default)]
        revision: Option<String>,
        #[serde(default)]
        section_id: Option<String>,
        #[serde(default)]
        section: Option<String>,
    },
    /// Up to 64 paths at head under one byte budget.
    ReadMany {
        paths: Vec<String>,
        #[serde(default = "read_many_default_max_bytes")]
        max_bytes: usize,
    },
    ListProposals {
        #[serde(default)]
        states: Vec<crate::ProposalState>,
        #[serde(default = "search_default_limit")]
        limit: usize,
        #[serde(default)]
        offset: usize,
    },
    Head,
    PolicyGet,
    PolicySet {
        policy: crate::auth::Policy,
        expected_digest: String,
    },
    Read {
        path: String,
        #[serde(default)]
        revision: Option<String>,
    },
    ReadBinary {
        path: String,
        #[serde(default)]
        revision: Option<String>,
        #[serde(default)]
        original: bool,
        #[serde(default)]
        encoding: Option<String>,
    },
    ExtractionStatus {
        path: String,
    },
    ExtractedText {
        path: String,
    },
    RetryExtraction {
        path: String,
    },
    List {
        scope: String,
    },
    Search {
        query: String,
        #[serde(default)]
        mode: SearchMode,
        #[serde(default)]
        min_score: Option<f32>,
        #[serde(default)]
        scope: String,
        #[serde(default)]
        extensions: Vec<String>,
        #[serde(default = "search_default_limit")]
        limit: usize,
        #[serde(default)]
        offset: usize,
        #[serde(default)]
        snapshot_id: Option<String>,
        #[serde(default = "crate::search::SearchField::all")]
        fields: Vec<crate::search::SearchField>,
        /// One edit forgiven in words of 5+ characters.
        #[serde(default = "yes")]
        typos: bool,
    },
    Propose {
        base_revision: String,
        changes: Vec<Change>,
        #[serde(default)]
        reason: Option<String>,
        #[serde(default)]
        origin: Option<Origin>,
    },
    Review {
        proposal_id: String,
    },
    Approve {
        proposal_id: String,
        digest: String,
        #[serde(default)]
        reason: Option<String>,
    },
    Reject {
        proposal_id: String,
        digest: String,
        #[serde(default)]
        reason: Option<String>,
    },
    Apply {
        proposal_id: String,
        digest: String,
    },
    Withdraw {
        proposal_id: String,
        digest: String,
    },
    /// A stale proposal carried onto head; `keep` is `"fitting"`, the only mode.
    RebaseProposal {
        proposal_id: String,
        digest: String,
        keep: String,
    },
    Edit {
        base_revision: String,
        changes: Vec<Change>,
        #[serde(default)]
        message: Option<String>,
        #[serde(default)]
        origin: Option<Origin>,
    },
    History {
        path: String,
    },
    Diff {
        revision_id: String,
    },
    Me,
    Permissions {
        path: String,
    },
    Deciders {
        proposal_id: String,
    },
    Stat {
        path: String,
        #[serde(default)]
        revision: Option<String>,
    },
    Log {
        #[serde(default)]
        scope: String,
        #[serde(default = "search_default_limit")]
        limit: usize,
        #[serde(default)]
        offset: usize,
    },
    /// The link graph at head below `scope` (`""` is the whole tenant).
    Graph {
        #[serde(default)]
        scope: String,
        #[serde(default)]
        offset: usize,
        /// Pages of 1–2,000 nodes in path order; absent is the whole scope.
        #[serde(default)]
        limit: Option<usize>,
    },
    /// The visible subgraph around one file or folder.
    Neighbourhood {
        path: String,
        #[serde(default = "neighbourhood_default_depth")]
        depth: u8,
        #[serde(default = "neighbourhood_default_limit")]
        limit: usize,
    },
    Links {
        path: String,
    },
    /// Files and folders by name and path at head below `scope`.
    Find {
        query: String,
        #[serde(default)]
        scope: String,
        #[serde(default = "search_default_limit")]
        limit: usize,
        /// One edit forgiven in words of 5+ characters.
        #[serde(default = "yes")]
        typos: bool,
    },
    /// Exactly one of `revision_id` and `proposal_id`.
    Render {
        #[serde(default)]
        revision_id: Option<String>,
        #[serde(default)]
        proposal_id: Option<String>,
        #[serde(default)]
        path: Option<String>,
        #[serde(default)]
        offset: usize,
    },
}
fn yes() -> bool {
    true
}
fn import_default_batch() -> usize {
    32
}
fn search_default_limit() -> usize {
    20
}
fn read_many_default_max_bytes() -> usize {
    crate::read_many::DEFAULT_MAX_BYTES
}
fn neighbourhood_default_depth() -> u8 {
    1
}
fn neighbourhood_default_limit() -> usize {
    32
}

/// Statically typed results retain the direct v1 JSON payload shape.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ApiResult {
    // Before `Graph`: an untagged neighbourhood answer would also parse as a graph.
    Neighbourhood(crate::graph::NeighbourhoodResult),
    ReadMany(crate::read_many::ReadManyResult),
    ImportPreview(crate::onboarding::ImportPreview),
    ImportSkipped(crate::onboarding::ImportSkipped),
    ExportSnapshot(crate::onboarding::ExportSnapshot),
    ExportReport(crate::onboarding::ExportReport),
    Properties(crate::knowledge::PropertiesResult),
    Outline(crate::knowledge::OutlineResult),
    Section(crate::knowledge::SectionResult),
    ProposalList(crate::proposals::ProposalListResult),
    Policy(crate::PolicyState),
    Backup(crate::backup::BackupReport),
    Retention(crate::retention::RetentionReport),
    Search(SearchResult),
    Extraction(ExtractionResult),
    Head(String),
    Read(ReadResult),
    BinaryRead(BinaryReadResult),
    Proposal(Proposal),
    Revision(Revision),
    Paths(Vec<String>),
    History(Vec<Revision>),
    Diff(Vec<FileDiff>),
    Me(crate::ranks::MeResult),
    Permissions(crate::ranks::PermissionsResult),
    Deciders(crate::ranks::DecidersResult),
    Log(crate::log::LogResult),
    Render(crate::render::RenderResult),
    Stat(crate::stat::StatResult),
    Graph(crate::graph::GraphResult),
    Links(crate::graph::LinksResult),
    Find(crate::find::FindResult),
    Grant(crate::grant::GrantReport),
    Initialized(Initialization),
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Initialization {
    pub initialized: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Response {
    pub version: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<ApiResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ContentError>,
}
impl Response {
    pub fn failure(error: ContentError) -> Self {
        Self {
            version: API_VERSION,
            result: None,
            error: Some(error),
        }
    }
}

impl Service {
    pub fn execute(&self, identity: &TrustedIdentity, request: Request) -> Response {
        let result = (|| {
            if request.version != 1 && request.version != API_VERSION {
                return Err(ContentError::new(
                    "unsupported_version",
                    "Unsupported API version",
                ));
            }
            if request.version == 1
                && (matches!(
                    &request.operation,
                    Operation::ImportPreview { .. }
                        | Operation::ImportCommit { .. }
                        | Operation::ExportSnapshot { .. }
                        | Operation::Properties { .. }
                        | Operation::Outline { .. }
                        | Operation::ReadSection { .. }
                        | Operation::ReadMany { .. }
                        | Operation::ListProposals { .. }
                        | Operation::PolicyGet
                        | Operation::PolicySet { .. }
                        | Operation::ReadBinary { .. }
                        | Operation::Search { .. }
                        | Operation::ExtractionStatus { .. }
                        | Operation::ExtractedText { .. }
                        | Operation::RetryExtraction { .. }
                        | Operation::Me
                        | Operation::Permissions { .. }
                        | Operation::Deciders { .. }
                        | Operation::Log { .. }
                        | Operation::Render { .. }
                        | Operation::Stat { .. }
                        | Operation::Withdraw { .. }
                        | Operation::RebaseProposal { .. }
                        | Operation::Graph { .. }
                        | Operation::Neighbourhood { .. }
                        | Operation::Links { .. }
                        | Operation::Find { .. }
                ) || match &request.operation {
                    Operation::Edit {
                        changes,
                        message,
                        origin,
                        ..
                    }
                    | Operation::Propose {
                        changes,
                        reason: message,
                        origin,
                        ..
                    } => {
                        message.is_some()
                            || origin.is_some()
                            || changes.iter().any(|c| !matches!(c, Change::Put { .. }))
                    }
                    Operation::Approve { reason, .. } | Operation::Reject { reason, .. } => {
                        reason.is_some()
                    }
                    _ => false,
                })
            {
                return Err(ContentError::new(
                    "unsupported_version",
                    "This operation requires API version 2",
                ));
            }
            let key = || {
                request
                    .idempotency_key
                    .as_deref()
                    .filter(|key| !key.trim().is_empty())
                    .ok_or_else(|| {
                        ContentError::new(
                            "invalid_request",
                            "A nonempty idempotency_key is required",
                        )
                    })
            };
            let value = match request.operation {
                Operation::ImportPreview {
                    base_revision,
                    files,
                    offset,
                    next_offset,
                    conflict_policy,
                } => ApiResult::ImportPreview(self.import_preview(
                    identity,
                    &base_revision,
                    files,
                    offset,
                    next_offset,
                    conflict_policy,
                )?),
                Operation::ImportCommit {
                    preview,
                    mode,
                    message,
                    origin,
                } => {
                    self.import_commit(identity, preview, mode, key()?, message.as_deref(), origin)?
                }
                Operation::ExportSnapshot {
                    scope,
                    revision,
                    offset,
                    limit,
                    expected_policy_digest,
                    encoding,
                } => {
                    let base64 = crate::onboarding::export_base64(encoding.as_deref())?;
                    let page = self.export_snapshot(
                        identity,
                        &scope,
                        revision.as_deref(),
                        offset,
                        limit,
                        expected_policy_digest.as_deref(),
                    )?;
                    ApiResult::ExportSnapshot(if base64 { page.into_base64() } else { page })
                }
                Operation::Properties { path, revision } => {
                    ApiResult::Properties(self.properties(identity, &path, revision.as_deref())?)
                }
                Operation::Outline { path, revision } => {
                    ApiResult::Outline(self.outline(identity, &path, revision.as_deref())?)
                }
                Operation::ReadSection {
                    path,
                    revision,
                    section_id,
                    section,
                } => ApiResult::Section(self.read_section(
                    identity,
                    &path,
                    revision.as_deref(),
                    section_id.as_deref(),
                    section.as_deref(),
                )?),
                Operation::ReadMany { paths, max_bytes } => {
                    ApiResult::ReadMany(self.read_many(identity, &paths, max_bytes)?)
                }
                Operation::ListProposals {
                    states,
                    limit,
                    offset,
                } => ApiResult::ProposalList(self.list_proposals(
                    identity,
                    &crate::proposals::ProposalListRequest {
                        states,
                        limit,
                        offset,
                    },
                )?),
                Operation::PolicyGet => ApiResult::Policy(self.policy_state(identity)?),
                Operation::PolicySet {
                    policy,
                    expected_digest,
                } => {
                    self.replace_policy(identity, policy, Some(&expected_digest))?;
                    ApiResult::Initialized(Initialization { initialized: true })
                }
                Operation::Head => ApiResult::Head(self.head(identity)?),
                Operation::Read { path, revision } => {
                    ApiResult::Read(self.read(identity, &path, revision.as_deref())?)
                }
                Operation::ReadBinary {
                    path,
                    revision,
                    original,
                    encoding,
                } => ApiResult::BinaryRead(self.read_binary(
                    identity,
                    &path,
                    revision.as_deref(),
                    original,
                    encoding.as_deref(),
                )?),
                Operation::ExtractionStatus { path } => {
                    ApiResult::Extraction(self.extraction_status(identity, &path)?)
                }
                Operation::ExtractedText { path } => {
                    ApiResult::Extraction(self.extracted_text(identity, &path)?)
                }
                Operation::RetryExtraction { path } => {
                    ApiResult::Extraction(self.retry_extraction(identity, &path)?)
                }
                Operation::List { scope } => ApiResult::Paths(self.list(identity, &scope)?),
                Operation::Search {
                    query,
                    mode,
                    min_score,
                    scope,
                    extensions,
                    limit,
                    offset,
                    snapshot_id,
                    fields,
                    typos,
                } => ApiResult::Search(self.search(
                    identity,
                    &SearchRequest {
                        query,
                        mode,
                        min_score,
                        scope,
                        extensions,
                        limit,
                        offset,
                        snapshot_id,
                        fields,
                        typos,
                    },
                )?),
                Operation::Propose {
                    base_revision,
                    changes,
                    reason,
                    origin,
                } => ApiResult::Proposal(self.propose(
                    identity,
                    &base_revision,
                    changes,
                    key()?,
                    reason.as_deref(),
                    origin,
                )?),
                Operation::Review { proposal_id } => {
                    ApiResult::Proposal(self.review(identity, &proposal_id)?)
                }
                Operation::Approve {
                    proposal_id,
                    digest,
                    reason,
                } => ApiResult::Proposal(self.approve(
                    identity,
                    &proposal_id,
                    &digest,
                    key()?,
                    reason.as_deref(),
                )?),
                Operation::Reject {
                    proposal_id,
                    digest,
                    reason,
                } => ApiResult::Proposal(self.reject(
                    identity,
                    &proposal_id,
                    &digest,
                    key()?,
                    reason.as_deref(),
                )?),
                Operation::Withdraw {
                    proposal_id,
                    digest,
                } => ApiResult::Proposal(self.withdraw(identity, &proposal_id, &digest, key()?)?),
                Operation::RebaseProposal {
                    proposal_id,
                    digest,
                    keep,
                } => ApiResult::Proposal(self.rebase_proposal(
                    identity,
                    &proposal_id,
                    &digest,
                    &keep,
                    key()?,
                )?),
                Operation::Apply {
                    proposal_id,
                    digest,
                } => ApiResult::Revision(self.apply(identity, &proposal_id, &digest, key()?)?),
                Operation::Edit {
                    base_revision,
                    changes,
                    message,
                    origin,
                } => ApiResult::Revision(self.edit(
                    identity,
                    &base_revision,
                    changes,
                    key()?,
                    message.as_deref(),
                    origin,
                )?),
                Operation::History { path } => ApiResult::History(self.history(identity, &path)?),
                Operation::Diff { revision_id } => {
                    ApiResult::Diff(self.diff(identity, &revision_id)?)
                }
                Operation::Me => ApiResult::Me(self.me(identity)?),
                Operation::Permissions { path } => {
                    ApiResult::Permissions(self.permissions(identity, &path)?)
                }
                Operation::Deciders { proposal_id } => {
                    ApiResult::Deciders(self.deciders(identity, &proposal_id)?)
                }
                Operation::Stat { path, revision } => {
                    ApiResult::Stat(self.stat(identity, &path, revision.as_deref())?)
                }
                Operation::Log {
                    scope,
                    limit,
                    offset,
                } => ApiResult::Log(self.log(identity, &scope, limit, offset)?),
                Operation::Graph {
                    scope,
                    offset,
                    limit,
                } => ApiResult::Graph(self.graph(identity, &scope, offset, limit)?),
                Operation::Neighbourhood { path, depth, limit } => {
                    ApiResult::Neighbourhood(self.neighbourhood(identity, &path, depth, limit)?)
                }
                Operation::Links { path } => ApiResult::Links(self.links(identity, &path)?),
                Operation::Find {
                    query,
                    scope,
                    limit,
                    typos,
                } => ApiResult::Find(self.find(identity, &query, &scope, limit, typos)?),
                Operation::Render {
                    revision_id,
                    proposal_id,
                    path,
                    offset,
                } => ApiResult::Render(self.render(
                    identity,
                    revision_id.as_deref(),
                    proposal_id.as_deref(),
                    path.as_deref(),
                    offset,
                )?),
            };
            Ok(value)
        })();
        match result {
            Ok(value) => Response {
                version: request.version,
                result: Some(value),
                error: None,
            },
            Err(error) => Response {
                version: request.version,
                result: None,
                error: Some(error),
            },
        }
    }
}
