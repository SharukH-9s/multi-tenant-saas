use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

pub const INGESTION_REQUESTED: &str = "ingestion.requested";
pub const EVENT_VERSION: u32 = 1;

/// The doorbell message. Unknown extra fields are accepted on purpose
/// (no `deny_unknown_fields`): only `v` decides compatibility.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IngestionEvent {
    pub v: u32,
    pub job_id: Uuid,
    pub tenant_id: Uuid,
    pub document_id: Uuid,
}

/// Why a message could not be turned into an [`IngestionEvent`].
///
/// The messages are fixed strings so that logging an error can never leak
/// payload content.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EventError {
    #[error("unsupported event version {0}")]
    UnsupportedVersion(u64),
    #[error("malformed event: {0}")]
    Malformed(&'static str),
}

impl IngestionEvent {
    pub fn new(job_id: Uuid, tenant_id: Uuid, document_id: Uuid) -> Self {
        Self {
            v: EVENT_VERSION,
            job_id,
            tenant_id,
            document_id,
        }
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("IngestionEvent always serializes")
    }

    /// Rejects unknown `v`, missing fields and non-UUID ids. Never panics.
    pub fn parse(raw: &str) -> Result<Self, EventError> {
        let value: Value =
            serde_json::from_str(raw).map_err(|_| EventError::Malformed("not valid JSON"))?;
        let object = value
            .as_object()
            .ok_or(EventError::Malformed("not a JSON object"))?;
        let version = object
            .get("v")
            .and_then(Value::as_u64)
            .ok_or(EventError::Malformed("missing or invalid version"))?;
        if version != u64::from(EVENT_VERSION) {
            return Err(EventError::UnsupportedVersion(version));
        }
        serde_json::from_value(value).map_err(|_| EventError::Malformed("missing or invalid field"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids() -> (Uuid, Uuid, Uuid) {
        (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4())
    }

    #[test]
    fn round_trips() {
        let (job, tenant, doc) = ids();
        let event = IngestionEvent::new(job, tenant, doc);
        assert_eq!(event.v, 1);
        assert_eq!(IngestionEvent::parse(&event.to_json()), Ok(event));
    }

    #[test]
    fn unknown_version_is_rejected() {
        let (job, tenant, doc) = ids();
        let raw =
            format!(r#"{{"v":2,"job_id":"{job}","tenant_id":"{tenant}","document_id":"{doc}"}}"#);
        assert_eq!(
            IngestionEvent::parse(&raw),
            Err(EventError::UnsupportedVersion(2))
        );
        // Even when the rest of a future payload looks different.
        assert_eq!(
            IngestionEvent::parse(r#"{"v":7}"#),
            Err(EventError::UnsupportedVersion(7))
        );
    }

    #[test]
    fn missing_field_is_malformed() {
        let (job, tenant, _) = ids();
        let raw = format!(r#"{{"v":1,"job_id":"{job}","tenant_id":"{tenant}"}}"#);
        assert!(matches!(
            IngestionEvent::parse(&raw),
            Err(EventError::Malformed(_))
        ));
    }

    #[test]
    fn extra_field_is_accepted() {
        let (job, tenant, doc) = ids();
        let raw = format!(
            r#"{{"v":1,"job_id":"{job}","tenant_id":"{tenant}","document_id":"{doc}","trace":"x"}}"#
        );
        assert_eq!(
            IngestionEvent::parse(&raw),
            Ok(IngestionEvent::new(job, tenant, doc))
        );
    }

    #[test]
    fn bad_uuid_is_malformed_and_error_hides_the_value() {
        let (job, tenant, _) = ids();
        let raw = format!(
            r#"{{"v":1,"job_id":"{job}","tenant_id":"{tenant}","document_id":"secret-not-a-uuid"}}"#
        );
        let err = IngestionEvent::parse(&raw).unwrap_err();
        assert!(matches!(err, EventError::Malformed(_)));
        assert!(!err.to_string().contains("secret"));
    }

    #[test]
    fn garbage_never_panics() {
        for raw in [
            "",
            "null",
            "[]",
            "{",
            "\"v\"",
            r#"{"v":"1"}"#,
            r#"{"v":-1}"#,
        ] {
            assert!(IngestionEvent::parse(raw).is_err(), "{raw}");
        }
    }
}
