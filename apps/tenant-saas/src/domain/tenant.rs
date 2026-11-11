use common_errors::AppError;

const MIN_SLUG_CHARS: usize = 3;
const MAX_SLUG_CHARS: usize = 50;
const MAX_NAME_CHARS: usize = 100;

/// A URL-safe tenant identifier such as `acme-corp`.
///
/// Same rule as the `tenants.slug` constraint: 3 to 50 characters, lowercase
/// letters, digits and hyphens, starting and ending with a letter or digit.
/// Input is not rewritten, so uppercase is rejected rather than lowercased.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Slug(String);

impl Slug {
    pub fn parse(input: &str) -> Result<Self, AppError> {
        let slug = input.trim();

        let is_alnum = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit();
        let valid = (MIN_SLUG_CHARS..=MAX_SLUG_CHARS).contains(&slug.len())
            && slug.chars().all(|c| is_alnum(c) || c == '-')
            && slug.starts_with(is_alnum)
            && slug.ends_with(is_alnum);

        if valid {
            Ok(Self(slug.to_owned()))
        } else {
            Err(AppError::Validation(format!(
                "Slug must be {MIN_SLUG_CHARS}-{MAX_SLUG_CHARS} characters: lowercase letters, \
                 digits and hyphens, starting and ending with a letter or digit."
            )))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A tenant's display name: trimmed, 1 to 100 characters, no control characters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TenantName(String);

impl TenantName {
    pub fn parse(input: &str) -> Result<Self, AppError> {
        let name = input.trim();
        let length = name.chars().count();

        if (1..=MAX_NAME_CHARS).contains(&length) && !name.chars().any(char::is_control) {
            Ok(Self(name.to_owned()))
        } else {
            Err(AppError::Validation(format!(
                "Tenant name must be 1-{MAX_NAME_CHARS} characters with no control characters."
            )))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_accepts_valid_values() {
        for good in ["abc", "acme-corp", "a1b", "x-y-z", &"a".repeat(50)] {
            assert!(Slug::parse(good).is_ok(), "{good:?} should be accepted");
        }
    }

    #[test]
    fn slug_rejects_invalid_values() {
        for bad in [
            "",
            "ab",
            "-abc",
            "abc-",
            "Acme",
            "acme corp",
            "acme_corp",
            "acme.corp",
            &"a".repeat(51),
        ] {
            assert!(Slug::parse(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn tenant_name_is_trimmed() {
        assert_eq!(
            TenantName::parse("  Acme Corp ").unwrap().as_str(),
            "Acme Corp"
        );
    }

    #[test]
    fn tenant_name_rejects_empty_long_and_control_chars() {
        assert!(TenantName::parse("   ").is_err());
        assert!(TenantName::parse(&"a".repeat(101)).is_err());
        assert!(TenantName::parse("bad\nname").is_err());
        assert!(TenantName::parse(&"a".repeat(100)).is_ok());
    }
}
