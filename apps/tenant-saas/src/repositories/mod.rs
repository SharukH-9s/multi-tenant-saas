//! Database access. Repositories run SQL on a transaction handed to them
//! (see `common_database::begin_user_tx` / `begin_tenant_tx`) and turn
//! database errors into `AppError`.
//!
//! Modules added in Task 8: `tenant_repo`. `membership_repo` has only the
//! resolver query so far; Task 8 adds member management.

pub mod db_error;
pub mod document_repo;
pub mod membership_repo;
pub mod refresh_token_repo;
pub mod tenant_repo;
pub mod user_repo;
