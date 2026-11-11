use std::sync::Arc;

use argon2::{
    password_hash::{PasswordHasher, PasswordVerifier},
    Algorithm, Argon2, Params, Version,
};
use common_errors::AppError;
use tokio::sync::Semaphore;

pub const MIN_PASSWORD_CHARS: usize = 12;
/// The upper bound keeps hashing cost predictable.
pub const MAX_PASSWORD_CHARS: usize = 128;

/// Argon2id cost settings plus how many hashes may run at the same time.
#[derive(Debug, Clone)]
pub struct PasswordSettings {
    pub memory_kib: u32,
    pub iterations: u32,
    pub parallelism: u32,
    pub max_concurrent_hashes: usize,
}

/// Hashes and verifies passwords with Argon2id.
///
/// Hashing is CPU heavy, so it runs on the blocking thread pool, and a
/// semaphore limits how many run at once. A flood of logins then queues up
/// instead of starving the async runtime.
#[derive(Clone)]
pub struct PasswordService {
    argon2: Arc<Argon2<'static>>,
    permits: Arc<Semaphore>,
    /// Verified when the email is unknown, so "no such user" takes about as
    /// long as "wrong password".
    dummy_hash: Arc<str>,
}

impl PasswordService {
    pub fn new(settings: &PasswordSettings) -> Result<Self, AppError> {
        let params = Params::new(
            settings.memory_kib,
            settings.iterations,
            settings.parallelism,
            None,
        )
        .map_err(|e| AppError::Internal(format!("invalid Argon2 parameters: {e}")))?;
        let argon2 = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);

        let dummy_hash = argon2
            .hash_password(b"dummy-password-for-timing")
            .map_err(|e| AppError::Internal(format!("could not build dummy hash: {e}")))?
            .to_string();

        Ok(Self {
            argon2: Arc::new(argon2),
            permits: Arc::new(Semaphore::new(settings.max_concurrent_hashes.max(1))),
            dummy_hash: dummy_hash.into(),
        })
    }

    /// Checks the length rules and returns a PHC string (`$argon2id$...`).
    pub async fn hash(&self, password: &str) -> Result<String, AppError> {
        let length = password.chars().count();
        if !(MIN_PASSWORD_CHARS..=MAX_PASSWORD_CHARS).contains(&length) {
            return Err(AppError::Validation(format!(
                "Password must be between {MIN_PASSWORD_CHARS} and {MAX_PASSWORD_CHARS} characters."
            )));
        }

        let argon2 = self.argon2.clone();
        let password = password.to_owned();
        self.run_blocking(move || {
            argon2
                .hash_password(password.as_bytes())
                .map(|hash| hash.to_string())
                .map_err(|e| AppError::Internal(format!("password hashing failed: {e}")))
        })
        .await
    }

    /// Returns `true` if the password matches `stored_hash`.
    ///
    /// Pass `None` when the user does not exist: a dummy hash is checked
    /// instead and the answer is always `false`.
    pub async fn verify(
        &self,
        password: &str,
        stored_hash: Option<&str>,
    ) -> Result<bool, AppError> {
        // Over-long input can never be a valid password, so skip the work.
        if password.chars().count() > MAX_PASSWORD_CHARS {
            return Ok(false);
        }

        let user_exists = stored_hash.is_some();
        let hash = stored_hash.unwrap_or(&self.dummy_hash).to_owned();
        let argon2 = self.argon2.clone();
        let password = password.to_owned();

        let matches = self
            .run_blocking(move || {
                Ok(argon2
                    .verify_password(password.as_bytes(), hash.as_str())
                    .is_ok())
            })
            .await?;

        Ok(matches && user_exists)
    }

    async fn run_blocking<T, F>(&self, work: F) -> Result<T, AppError>
    where
        T: Send + 'static,
        F: FnOnce() -> Result<T, AppError> + Send + 'static,
    {
        let _permit = self
            .permits
            .acquire()
            .await
            .map_err(|_| AppError::Internal("hash semaphore closed".into()))?;

        tokio::task::spawn_blocking(work)
            .await
            .map_err(|e| AppError::Internal(format!("hash task failed: {e}")))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Cheap settings so tests stay fast. Production values come from config.
    fn service() -> PasswordService {
        PasswordService::new(&PasswordSettings {
            memory_kib: 64,
            iterations: 1,
            parallelism: 1,
            max_concurrent_hashes: 2,
        })
        .unwrap()
    }

    #[tokio::test]
    async fn correct_password_verifies_and_wrong_one_does_not() {
        let service = service();
        let hash = service.hash("correct horse battery").await.unwrap();

        assert!(hash.starts_with("$argon2id$"));
        assert!(service
            .verify("correct horse battery", Some(&hash))
            .await
            .unwrap());
        assert!(!service
            .verify("wrong horse battery!", Some(&hash))
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn same_password_gets_a_different_salt_each_time() {
        let service = service();
        let a = service.hash("correct horse battery").await.unwrap();
        let b = service.hash("correct horse battery").await.unwrap();
        assert_ne!(a, b);
    }

    #[tokio::test]
    async fn unknown_user_never_verifies() {
        let service = service();
        assert!(!service.verify("anything at all here", None).await.unwrap());
    }

    #[tokio::test]
    async fn garbage_stored_hash_does_not_verify_or_panic() {
        let service = service();
        assert!(!service
            .verify("whatever password", Some("not-a-phc-string"))
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn length_limits_are_enforced() {
        let service = service();

        let too_short = "a".repeat(MIN_PASSWORD_CHARS - 1);
        assert!(matches!(
            service.hash(&too_short).await,
            Err(AppError::Validation(_))
        ));

        let too_long = "a".repeat(MAX_PASSWORD_CHARS + 1);
        assert!(matches!(
            service.hash(&too_long).await,
            Err(AppError::Validation(_))
        ));

        assert!(service.hash(&"a".repeat(MIN_PASSWORD_CHARS)).await.is_ok());
        assert!(service.hash(&"a".repeat(MAX_PASSWORD_CHARS)).await.is_ok());
    }
}
