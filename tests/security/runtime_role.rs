use test_support::TestApp;

/// RLS is meaningless if the runtime role can bypass it.
#[tokio::test]
async fn runtime_role_is_not_superuser_and_cannot_bypass_rls() {
    let Some(app) = TestApp::spawn().await else {
        return;
    };

    let (is_super, bypass_rls): (bool, bool) =
        sqlx::query_as("SELECT rolsuper, rolbypassrls FROM pg_roles WHERE rolname = current_user")
            .fetch_one(&app.runtime_pool)
            .await
            .expect("query pg_roles");

    assert!(!is_super, "runtime role must not be a superuser");
    assert!(!bypass_rls, "runtime role must not have BYPASSRLS");
}
