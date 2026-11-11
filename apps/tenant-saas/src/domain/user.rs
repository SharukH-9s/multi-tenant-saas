use std::fmt;

use common_auth::{MAX_PASSWORD_CHARS, MIN_PASSWORD_CHARS};
use common_errors::AppError;

/// Same limit as the `users_email_len` constraint.
const MAX_EMAIL_CHARS: usize = 254;

/// A trimmed, lowercased email address with a basic format check.
///
/// This is not a full RFC 5322 parser. It rejects obvious mistakes; real
/// ownership of the address would be proven by email verification (out of scope).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Email(String);

impl Email {
    pub fn parse(input: &str) -> Result<Self, AppError> {
        let email = input.trim().to_lowercase();

        let valid = email.chars().count() <= MAX_EMAIL_CHARS
            && !email.chars().any(|c| c.is_whitespace() || c.is_control())
            && match email.split_once('@') {
                Some((local, domain)) => {
                    !local.is_empty()
                        && !domain.contains('@')
                        && domain.contains('.')
                        && !domain.starts_with('.')
                        && !domain.ends_with('.')
                }
                None => false,
            };

        if valid {
            Ok(Self(email))
        } else {
            Err(AppError::Validation("Email address is not valid.".into()))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A plain-text password that passed the length rules.
///
/// `Debug` is redacted so the value cannot end up in logs by accident.
#[derive(Clone)]
pub struct Password(String);

impl Password {
    pub fn parse(input: String) -> Result<Self, AppError> {
        let length = input.chars().count();

        if (MIN_PASSWORD_CHARS..=MAX_PASSWORD_CHARS).contains(&length) {
            Ok(Self(input))
        } else {
            Err(AppError::Validation(format!(
                "Password must be between {MIN_PASSWORD_CHARS} and {MAX_PASSWORD_CHARS} characters."
            )))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Password {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Password(<redacted>)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn email_is_trimmed_and_lowercased() {
        let email = Email::parse("  Alice@Example.COM ").unwrap();
        assert_eq!(email.as_str(), "alice@example.com");
    }

    #[test]
    fn email_rejects_bad_formats() {
        for bad in [
            "",
            "alice",
            "@example.com",
            "alice@",
            "alice@example",
            "alice@.com",
            "alice@example.",
            "a@b@example.com",
            "ali ce@example.com",
        ] {
            assert!(Email::parse(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn email_rejects_more_than_254_chars() {
        let too_long = format!("{}@example.com", "a".repeat(250));
        assert!(Email::parse(&too_long).is_err());
    }

    #[test]
    fn password_length_bounds() {
        assert!(Password::parse("a".repeat(MIN_PASSWORD_CHARS - 1)).is_err());
        assert!(Password::parse("a".repeat(MIN_PASSWORD_CHARS)).is_ok());
        assert!(Password::parse("a".repeat(MAX_PASSWORD_CHARS)).is_ok());
        assert!(Password::parse("a".repeat(MAX_PASSWORD_CHARS + 1)).is_err());
    }

    #[test]
    fn password_debug_is_redacted() {
        let password = Password::parse("correct-horse-battery".into()).unwrap();
        assert!(!format!("{password:?}").contains("correct-horse"));
    }
}
