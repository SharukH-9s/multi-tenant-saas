//! Integration tests for document lifecycle and upload endpoints (Task 8).
//!
//! Validates:
//! - Happy path uploads for all supported formats (PDF, TXT, MD)
//! - Title extraction when provided vs omitted
//! - All validation and content rejections (ensuring no orphan objects remain in storage)
//! - Deduplication on identical content upload (202 first, 200 second, 1 DB row)
//! - High-concurrency upload of identical bytes (10 concurrent: 1 202, 9 200, 1 row, 1 object)
//! - Re-upload after soft-delete creates a new document row with new ID
//! - Keyset-paginated list endpoint hiding soft-deleted documents
//! - Status endpoint contract and sensitive field redaction
//! - Delete permissions by role and soft-delete transitions

use axum::http::{Method, StatusCode};
use serde_json::json;
use sha2::{Digest, Sha256};
use test_support::TestApp;
use uuid::Uuid;

// Minimal valid PDF fixture (starts with `%PDF-`)
const VALID_PDF: &[u8] =
    b"%PDF-1.4\n1 0 obj\n<< /Type /Catalog >>\nendobj\ntrailer\n<< /Root 1 0 R >>\n%%EOF";
const VALID_TXT: &[u8] = b"This is a valid plain text document for testing.";
const VALID_MD: &[u8] = b"# Markdown Header\n\nThis is a **valid** markdown document.";

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

// ── 1. Happy path per format ──────────────────────────────────────────────────

#[tokio::test]
async fn happy_path_upload_per_format_and_metadata() {
    let Some(app) = TestApp::spawn().await else {
        return;
    };
    let session = app.create_session().await;
    let tenant_id = app
        .seed_membership(session.user_id, "admin", "active")
        .await;

    let cases = [
        ("report.pdf", VALID_PDF, "application/pdf"),
        ("notes.txt", VALID_TXT, "text/plain"),
        ("guide.md", VALID_MD, "text/markdown"),
    ];

    for (filename, content, expected_mime) in cases {
        let (status, body) = app
            .upload_file(
                tenant_id,
                &session.access_token,
                filename,
                content,
                Some("My Title"),
            )
            .await;

        assert_eq!(
            status,
            StatusCode::ACCEPTED,
            "upload failed for {filename}: {body}"
        );
        assert_eq!(body["status"], "pending");
        assert_eq!(body["mime_type"], expected_mime);
        assert_eq!(body["size_bytes"], content.len() as i64);
        assert_eq!(body["content_hash"], sha256_hex(content));
        assert_eq!(body["chunk_count"], 0);
        assert_eq!(body["title"], "My Title");
        assert_eq!(body["original_filename"], filename);

        let doc_id: Uuid = body["id"].as_str().unwrap().parse().unwrap();
        let row = app
            .document_row(tenant_id, doc_id)
            .await
            .expect("row in DB");
        assert_eq!(row.status, "pending");
        assert!(
            app.object_exists(&row.object_key).await,
            "object must exist in storage"
        );
    }
}

// ── 2. Title provided vs omitted ──────────────────────────────────────────────

#[tokio::test]
async fn title_given_and_title_omitted() {
    let Some(app) = TestApp::spawn().await else {
        return;
    };
    let session = app.create_session().await;
    let tenant_id = app
        .seed_membership(session.user_id, "admin", "active")
        .await;

    // Explicit title
    let (status, body) = app
        .upload_file(
            tenant_id,
            &session.access_token,
            "document.txt",
            b"Content with explicit title",
            Some("Custom Document Title"),
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(body["title"], "Custom Document Title");

    // Title omitted: filename without extension becomes the title
    let (status, body) = app
        .upload_file(
            tenant_id,
            &session.access_token,
            "quarterly-report-2026.pdf",
            VALID_PDF,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(body["title"], "quarterly-report-2026");
}

// ── 3. Rejections and storage cleanliness ─────────────────────────────────────

#[tokio::test]
async fn upload_rejections_and_storage_cleanliness() {
    let Some(app) = TestApp::spawn().await else {
        return;
    };
    let session = app.create_session().await;
    let tenant_id = app
        .seed_membership(session.user_id, "admin", "active")
        .await;

    // Helper: assert that no objects were left behind under this tenant
    async fn assert_clean(app: &TestApp, tenant_id: Uuid, expected_count: usize, desc: &str) {
        let prefix = format!("tenants/{tenant_id}/");
        let list = app.state.storage.list(&prefix).await.unwrap();
        assert_eq!(list.len(), expected_count, "storage leak on {desc}");
    }

    // a) Body is not multipart/form-data -> 415
    let (status, body) = app
        .request(
            Method::POST,
            &format!("/api/v1/tenants/{tenant_id}/documents"),
            Some(json!({ "some": "json" })),
            Some(&session.access_token),
        )
        .await;
    assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE, "{body}");
    assert_clean(&app, tenant_id, 0, "not multipart").await;

    // b) Unsupported extension (.exe) -> 415
    let (status, body) = app
        .upload_file(
            tenant_id,
            &session.access_token,
            "malware.exe",
            b"MZ...",
            None,
        )
        .await;
    assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE, "{body}");
    assert_clean(&app, tenant_id, 0, "bad extension").await;

    // c) Body disagrees with extension (PDF extension, text body) -> 415
    let (status, body) = app
        .upload_file(
            tenant_id,
            &session.access_token,
            "fake.pdf",
            b"NOT A REAL PDF FILE HEADER",
            None,
        )
        .await;
    assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE, "{body}");
    assert_clean(&app, tenant_id, 0, "fake pdf").await;

    // d) Invalid UTF-8 in text file -> 415
    let (status, body) = app
        .upload_file(
            tenant_id,
            &session.access_token,
            "invalid.txt",
            b"Hello \xFF World",
            None,
        )
        .await;
    assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE, "{body}");
    assert_clean(&app, tenant_id, 0, "invalid utf-8").await;

    // e) NUL byte in markdown file -> 415
    let (status, body) = app
        .upload_file(
            tenant_id,
            &session.access_token,
            "nul.md",
            b"# Header\x00with null",
            None,
        )
        .await;
    assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE, "{body}");
    assert_clean(&app, tenant_id, 0, "nul byte").await;

    // f) Zero-byte file -> 422
    let (status, body) = app
        .upload_file(tenant_id, &session.access_token, "empty.txt", b"", None)
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_clean(&app, tenant_id, 0, "zero byte").await;

    // g) Title > 200 chars -> 422
    let long_title = "a".repeat(201);
    let (status, body) = app
        .upload_file(
            tenant_id,
            &session.access_token,
            "file.txt",
            b"Valid text content",
            Some(&long_title),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_clean(&app, tenant_id, 0, "title too long").await;

    // h) No file part -> 422
    let boundary = "boundary_no_file";
    let body = format!("--{boundary}\r\nContent-Disposition: form-data; name=\"title\"\r\n\r\nOnly Title\r\n--{boundary}--\r\n");
    let (status, body) = app
        .post_multipart(
            &format!("/api/v1/tenants/{tenant_id}/documents"),
            &session.access_token,
            boundary,
            body.into_bytes(),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_clean(&app, tenant_id, 0, "no file part").await;

    // i) Two file parts -> 422
    let boundary = "boundary_two_files";
    let mut body_bytes = Vec::new();
    body_bytes.extend_from_slice(format!("--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a.txt\"\r\n\r\nFile 1\r\n").as_bytes());
    body_bytes.extend_from_slice(format!("--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"b.txt\"\r\n\r\nFile 2\r\n--{boundary}--\r\n").as_bytes());
    let (status, body) = app
        .post_multipart(
            &format!("/api/v1/tenants/{tenant_id}/documents"),
            &session.access_token,
            boundary,
            body_bytes,
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_clean(&app, tenant_id, 0, "two file parts").await;
}

// ── 4. Deduplication ──────────────────────────────────────────────────────────

#[tokio::test]
async fn duplicate_upload_returns_200_same_id_one_row() {
    let Some(app) = TestApp::spawn().await else {
        return;
    };
    let session = app.create_session().await;
    let tenant_id = app
        .seed_membership(session.user_id, "admin", "active")
        .await;

    let content = b"Identical deduplication content for test 4";

    // First upload -> 202 Accepted
    let (status1, body1) = app
        .upload_file(
            tenant_id,
            &session.access_token,
            "first.txt",
            content,
            Some("First Title"),
        )
        .await;
    assert_eq!(status1, StatusCode::ACCEPTED);
    let id1 = body1["id"].as_str().unwrap().to_string();

    // Second upload with same content -> 200 OK with the SAME document id
    let (status2, body2) = app
        .upload_file(
            tenant_id,
            &session.access_token,
            "second.txt",
            content,
            Some("Second Title"),
        )
        .await;
    assert_eq!(status2, StatusCode::OK, "{body2}");
    let id2 = body2["id"].as_str().unwrap().to_string();
    assert_eq!(
        id1, id2,
        "duplicate upload must return the existing document id"
    );

    // Check DB row count: exactly 1 live document
    let rows: Vec<(Uuid,)> =
        sqlx::query_as("SELECT id FROM documents WHERE tenant_id = $1 AND deleted_at IS NULL")
            .bind(tenant_id)
            .fetch_all(app.owner())
            .await
            .unwrap();
    assert_eq!(rows.len(), 1, "exactly one document row must exist");

    // Check storage: exactly 1 object
    let list = app
        .state
        .storage
        .list(&format!("tenants/{tenant_id}/"))
        .await
        .unwrap();
    assert_eq!(list.len(), 1, "exactly one object must survive in storage");
}

// ── 5. Concurrency: 10 simultaneous identical uploads ─────────────────────────

#[tokio::test]
async fn concurrency_ten_simultaneous_identical_uploads() {
    let Some(app) = TestApp::spawn().await else {
        return;
    };
    let session = app.create_session().await;
    let tenant_id = app
        .seed_membership(session.user_id, "admin", "active")
        .await;

    let content = b"Concurrent identical bytes across ten parallel tasks";
    let token = session.access_token.clone();

    let mut handles = Vec::new();
    for i in 0..10 {
        let app = app.clone();
        let token = token.clone();
        let filename = format!("concurrent-{i}.txt");

        handles.push(tokio::spawn(async move {
            app.upload_file(
                tenant_id,
                &token,
                &filename,
                content,
                Some("Concurrent Title"),
            )
            .await
        }));
    }

    let mut accepted_count = 0;
    let mut ok_count = 0;
    let mut returned_ids = Vec::new();

    for handle in handles {
        let (status, body) = handle.await.expect("task completed");
        if status == StatusCode::ACCEPTED {
            accepted_count += 1;
        } else if status == StatusCode::OK {
            ok_count += 1;
        } else {
            panic!("unexpected status code {status}: {body}");
        }
        returned_ids.push(body["id"].as_str().unwrap().to_string());
    }

    // Exactly one 202 Accepted, exactly nine 200 OK
    assert_eq!(accepted_count, 1, "exactly one upload must be 202");
    assert_eq!(ok_count, 9, "exactly nine duplicate uploads must be 200");

    // All tasks must have received the same document id
    let first_id = &returned_ids[0];
    for id in &returned_ids {
        assert_eq!(
            id, first_id,
            "all concurrent uploads must return the same ID"
        );
    }

    // Database must hold exactly one live document row
    let rows: Vec<(Uuid,)> =
        sqlx::query_as("SELECT id FROM documents WHERE tenant_id = $1 AND deleted_at IS NULL")
            .bind(tenant_id)
            .fetch_all(app.owner())
            .await
            .unwrap();
    assert_eq!(rows.len(), 1, "exactly one row in database");

    // Storage must hold exactly one object (9 duplicate writers cleaned up their objects)
    let list = app
        .state
        .storage
        .list(&format!("tenants/{tenant_id}/"))
        .await
        .unwrap();
    assert_eq!(list.len(), 1, "exactly one surviving object in storage");
}

// ── 6. Soft-delete then re-upload ─────────────────────────────────────────────

#[tokio::test]
async fn soft_deleted_document_can_be_uploaded_again_with_new_id() {
    let Some(app) = TestApp::spawn().await else {
        return;
    };
    let session = app.create_session().await;
    let tenant_id = app
        .seed_membership(session.user_id, "admin", "active")
        .await;

    let content = b"Content to be soft-deleted and re-uploaded";

    // 1. Initial upload
    let (status, body) = app
        .upload_file(tenant_id, &session.access_token, "reup.txt", content, None)
        .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let original_id: Uuid = body["id"].as_str().unwrap().parse().unwrap();

    // 2. Delete it
    let (status, _) = app
        .request(
            Method::DELETE,
            &format!("/api/v1/tenants/{tenant_id}/documents/{original_id}"),
            None,
            Some(&session.access_token),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // Verify row is soft-deleted
    let deleted_row = app.document_row(tenant_id, original_id).await.unwrap();
    assert_eq!(deleted_row.status, "deleted");

    // 3. Re-upload the exact same content
    let (status, body) = app
        .upload_file(tenant_id, &session.access_token, "reup2.txt", content, None)
        .await;
    assert_eq!(
        status,
        StatusCode::ACCEPTED,
        "re-upload must succeed with 202: {body}"
    );
    let new_id: Uuid = body["id"].as_str().unwrap().parse().unwrap();

    assert_ne!(
        original_id, new_id,
        "re-uploaded document must get a brand new ID"
    );
}

// ── 7. List documents: pagination and hiding deleted ──────────────────────────

#[tokio::test]
async fn list_documents_pagination_and_deleted_hidden() {
    let Some(app) = TestApp::spawn().await else {
        return;
    };
    let session = app.create_session().await;
    let tenant_id = app
        .seed_membership(session.user_id, "admin", "active")
        .await;

    // Upload 3 distinct documents
    let (s1, b1) = app
        .upload_file(
            tenant_id,
            &session.access_token,
            "doc1.txt",
            b"First doc",
            None,
        )
        .await;
    assert_eq!(s1, StatusCode::ACCEPTED);
    let id1: Uuid = b1["id"].as_str().unwrap().parse().unwrap();

    let (s2, b2) = app
        .upload_file(
            tenant_id,
            &session.access_token,
            "doc2.txt",
            b"Second doc",
            None,
        )
        .await;
    assert_eq!(s2, StatusCode::ACCEPTED);
    let id2: Uuid = b2["id"].as_str().unwrap().parse().unwrap();

    let (s3, b3) = app
        .upload_file(
            tenant_id,
            &session.access_token,
            "doc3.txt",
            b"Third doc",
            None,
        )
        .await;
    assert_eq!(s3, StatusCode::ACCEPTED);
    let id3: Uuid = b3["id"].as_str().unwrap().parse().unwrap();

    // List all: newest first -> doc3, doc2, doc1
    let (status, list) = app
        .request(
            Method::GET,
            &format!("/api/v1/tenants/{tenant_id}/documents"),
            None,
            Some(&session.access_token),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let items = list.as_array().unwrap();
    assert_eq!(items.len(), 3);
    assert_eq!(items[0]["id"], id3.to_string());
    assert_eq!(items[1]["id"], id2.to_string());
    assert_eq!(items[2]["id"], id1.to_string());

    // Keyset pagination: limit = 2
    let (status, page1) = app
        .request(
            Method::GET,
            &format!("/api/v1/tenants/{tenant_id}/documents?limit=2"),
            None,
            Some(&session.access_token),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let p1_items = page1.as_array().unwrap();
    assert_eq!(p1_items.len(), 2);
    assert_eq!(p1_items[0]["id"], id3.to_string());
    assert_eq!(p1_items[1]["id"], id2.to_string());

    let next_cursor_created_at = p1_items[1]["created_at"].as_str().unwrap();
    let next_cursor_id = p1_items[1]["id"].as_str().unwrap();

    // Page 2
    let (status, page2) = app
        .request(
            Method::GET,
            &format!(
                "/api/v1/tenants/{tenant_id}/documents?after_created_at={next_cursor_created_at}&after_id={next_cursor_id}&limit=2"
            ),
            None,
            Some(&session.access_token),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let p2_items = page2.as_array().unwrap();
    assert_eq!(p2_items.len(), 1);
    assert_eq!(p2_items[0]["id"], id1.to_string());

    // Soft-delete doc2
    let (status, _) = app
        .request(
            Method::DELETE,
            &format!("/api/v1/tenants/{tenant_id}/documents/{id2}"),
            None,
            Some(&session.access_token),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // List again: doc2 must be hidden
    let (status, list) = app
        .request(
            Method::GET,
            &format!("/api/v1/tenants/{tenant_id}/documents"),
            None,
            Some(&session.access_token),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let items = list.as_array().unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["id"], id3.to_string());
    assert_eq!(items[1]["id"], id1.to_string());
}

// ── 8. Status and Get endpoints (omits sensitive fields) ───────────────────────

#[tokio::test]
async fn status_endpoint_reflects_pending_and_omits_object_key() {
    let Some(app) = TestApp::spawn().await else {
        return;
    };
    let session = app.create_session().await;
    let tenant_id = app
        .seed_membership(session.user_id, "admin", "active")
        .await;

    let doc_id = app.create_document(tenant_id, &session.access_token).await;

    // GET /documents/{id}
    let (status, doc) = app
        .request(
            Method::GET,
            &format!("/api/v1/tenants/{tenant_id}/documents/{doc_id}"),
            None,
            Some(&session.access_token),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(doc["id"], doc_id.to_string());
    assert!(
        doc.get("object_key").is_none(),
        "object_key must not be in GET body"
    );
    assert!(
        doc.get("failure_detail").is_none(),
        "failure_detail must not be in GET body"
    );
    assert!(
        doc.get("deleted_at").is_none(),
        "deleted_at must not be in GET body"
    );

    // GET /documents/{id}/status
    let (status, stat) = app
        .request(
            Method::GET,
            &format!("/api/v1/tenants/{tenant_id}/documents/{doc_id}/status"),
            None,
            Some(&session.access_token),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(stat["id"], doc_id.to_string());
    assert_eq!(stat["status"], "pending");
    assert_eq!(stat["chunk_count"], 0);
    assert!(stat["failure_code"].is_null());
    assert!(
        stat.get("object_key").is_none(),
        "object_key must not be in status body"
    );
}

// ── 9. Delete document permissions and lifecycle ──────────────────────────────

#[tokio::test]
async fn delete_document_permissions_and_lifecycle() {
    let Some(app) = TestApp::spawn().await else {
        return;
    };

    let owner = app.create_session().await;
    let admin = app.create_session().await;
    let member_a = app.create_session().await;
    let member_b = app.create_session().await;
    let viewer = app.create_session().await;

    let tenant_id = app.create_tenant().await;
    app.add_member(tenant_id, owner.user_id, "owner", "active")
        .await;
    app.add_member(tenant_id, admin.user_id, "admin", "active")
        .await;
    app.add_member(tenant_id, member_a.user_id, "member", "active")
        .await;
    app.add_member(tenant_id, member_b.user_id, "member", "active")
        .await;
    app.add_member(tenant_id, viewer.user_id, "viewer", "active")
        .await;

    // Member A uploads a document
    let (status, body) = app
        .upload_file(
            tenant_id,
            &member_a.access_token,
            "doc_a.txt",
            b"Member A content",
            None,
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let doc_a_id: Uuid = body["id"].as_str().unwrap().parse().unwrap();
    let row_a = app.document_row(tenant_id, doc_a_id).await.unwrap();
    assert!(app.object_exists(&row_a.object_key).await);

    // a) Viewer tries to delete Member A's document -> 403 Forbidden
    let (status, _) = app
        .request(
            Method::DELETE,
            &format!("/api/v1/tenants/{tenant_id}/documents/{doc_a_id}"),
            None,
            Some(&viewer.access_token),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // b) Member B tries to delete Member A's document -> 403 Forbidden
    let (status, _) = app
        .request(
            Method::DELETE,
            &format!("/api/v1/tenants/{tenant_id}/documents/{doc_a_id}"),
            None,
            Some(&member_b.access_token),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // c) Member A deletes their own document -> 204 No Content
    let (status, _) = app
        .request(
            Method::DELETE,
            &format!("/api/v1/tenants/{tenant_id}/documents/{doc_a_id}"),
            None,
            Some(&member_a.access_token),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // Storage object is deleted, row status is 'deleted'
    assert!(
        !app.object_exists(&row_a.object_key).await,
        "storage object must be gone"
    );
    let row_a_after = app.document_row(tenant_id, doc_a_id).await.unwrap();
    assert_eq!(row_a_after.status, "deleted");

    // d) Deleting twice in a row -> second time returns 404 Not Found
    let (status, _) = app
        .request(
            Method::DELETE,
            &format!("/api/v1/tenants/{tenant_id}/documents/{doc_a_id}"),
            None,
            Some(&member_a.access_token),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // e) Admin deletes another member's document -> 204 No Content
    let (status, body) = app
        .upload_file(
            tenant_id,
            &member_b.access_token,
            "doc_b.txt",
            b"Member B content",
            None,
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let doc_b_id: Uuid = body["id"].as_str().unwrap().parse().unwrap();
    let row_b = app.document_row(tenant_id, doc_b_id).await.unwrap();

    let (status, _) = app
        .request(
            Method::DELETE,
            &format!("/api/v1/tenants/{tenant_id}/documents/{doc_b_id}"),
            None,
            Some(&admin.access_token),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(!app.object_exists(&row_b.object_key).await);

    // f) Owner deletes another member's document -> 204 No Content
    let (status, body) = app
        .upload_file(
            tenant_id,
            &member_a.access_token,
            "doc_c.txt",
            b"Member A content 2",
            None,
        )
        .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let doc_c_id: Uuid = body["id"].as_str().unwrap().parse().unwrap();
    let row_c = app.document_row(tenant_id, doc_c_id).await.unwrap();

    let (status, _) = app
        .request(
            Method::DELETE,
            &format!("/api/v1/tenants/{tenant_id}/documents/{doc_c_id}"),
            None,
            Some(&owner.access_token),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(!app.object_exists(&row_c.object_key).await);
}
