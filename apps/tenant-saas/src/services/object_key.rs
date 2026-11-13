//! Object key construction for stored documents.
//!
//! Keys follow the pattern `tenants/{tenant_id}/documents/{document_id}`,
//! so MinIO / S3 prefix listing can scope to a tenant without scanning
//! the whole bucket.  The module exists so the key shape is defined once
//! rather than scattered through the service layer.

use uuid::Uuid;

/// Builds the object storage key for a document upload.
///
/// Format: `tenants/{tenant_id}/documents/{document_id}`
pub fn for_document(tenant_id: Uuid, document_id: Uuid) -> String {
    format!("tenants/{tenant_id}/documents/{document_id}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_format_matches_expected_pattern() {
        let tenant_id = Uuid::parse_str("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa").unwrap();
        let doc_id = Uuid::parse_str("bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb").unwrap();

        let key = for_document(tenant_id, doc_id);

        assert_eq!(
            key,
            "tenants/aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa/documents/bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb"
        );
    }

    #[test]
    fn key_does_not_contain_path_traversal() {
        let key = for_document(Uuid::new_v4(), Uuid::new_v4());
        assert!(!key.contains(".."), "key must not contain '..'");
    }

    #[test]
    fn key_starts_with_tenants_prefix() {
        let key = for_document(Uuid::new_v4(), Uuid::new_v4());
        assert!(key.starts_with("tenants/"));
    }
}
