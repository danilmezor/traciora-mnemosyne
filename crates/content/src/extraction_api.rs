//! Current committed source extraction, authorized under the authority lock.
use crate::{
    auth::{Permission, TrustedIdentity},
    authorize_identity, err,
    extraction::{self, ExtractionRecord},
    valid_path, EntryKind, Result, Service,
};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExtractionResult {
    pub revision_id: String,
    pub path: String,
    pub file_id: String,
    pub source_blob: String,
    pub segment_count: usize,
    pub extraction: ExtractionRecord,
}
impl Service {
    pub fn extraction_status(&self, who: &TrustedIdentity, path: &str) -> Result<ExtractionResult> {
        self.extraction_operation(who, path, false, false)
    }
    pub fn extracted_text(&self, who: &TrustedIdentity, path: &str) -> Result<ExtractionResult> {
        self.extraction_operation(who, path, true, false)
    }
    pub fn retry_extraction(&self, who: &TrustedIdentity, path: &str) -> Result<ExtractionResult> {
        self.extraction_operation(who, path, true, true)
    }
    fn extraction_operation(
        &self,
        who: &TrustedIdentity,
        path: &str,
        run: bool,
        retry: bool,
    ) -> Result<ExtractionResult> {
        valid_path(path)?;
        self.transaction_for(who, false, vec![], false, None, None, |state| {
            crate::ranks::check(state, who, Permission::Read, path)?;
            if retry {
                crate::ranks::check(state, who, Permission::Edit, path)?;
            }
            let tenant = state
                .tenants
                .get(&who.tenant_id)
                .ok_or_else(|| err("not_found"))?;
            let entry = tenant
                .revisions
                .get(&tenant.head)
                .and_then(|s| s.files.get(path))
                .ok_or_else(|| err("not_found"))?;
            authorize_identity(state, who, entry, path)?;
            if entry.kind != EntryKind::Binary {
                return Err(err("not_binary"));
            }
            let blob = entry.blob.as_deref().ok_or_else(|| err("corrupt_blob"))?;
            let format = extraction::format(path).unwrap_or("unsupported");
            let mut record = if run {
                extraction::extract(&self.root, blob, format, retry)?
            } else {
                extraction::cached(&self.root, blob, format)?
                    .unwrap_or_else(|| extraction::pending(blob, format))
            };
            let segment_count = record.segments.len();
            if !run {
                record.segments.clear();
            }
            Ok(ExtractionResult {
                revision_id: tenant.head.clone(),
                path: path.into(),
                file_id: entry.file_id.clone(),
                source_blob: blob.into(),
                segment_count,
                extraction: record,
            })
        })
    }
}
