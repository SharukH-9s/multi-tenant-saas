use std::{
    fmt,
    time::{SystemTime, UNIX_EPOCH},
};

use common_errors::AppError;
use jsonwebtoken::{decode, encode, Algorithm, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::principal::AuthenticatedPrincipal;

const ACCESS_TOKEN_TYPE: &str = "access";
/// Allowed clock difference between servers, in seconds.
const LEEWAY_SECS: u64 = 5;

/// What goes inside an access token.
///
/// It identifies the user and nothing else: no tenant, no role. Those are
/// read from the database on every request, so a removed member loses access
/// immediately.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccessClaims {
    pub sub: Uuid,
    pub iss: String,
    pub aud: String,
    pub iat: u64,
    pub nbf: u64,
    pub exp: u64,
    pub jti: Uuid,
    pub typ: String,
}

impl From<&AccessClaims> for AuthenticatedPrincipal {
    fn from(claims: &AccessClaims) -> Self {
        AuthenticatedPrincipal {
            user_id: claims.sub,
            token_id: claims.jti,
        }
    }
}

#[derive(Clone)]
pub struct TokenSettings {
    pub secret: String,
    pub issuer: String,
    pub audience: String,
    pub expiry_secs: u64,
}

// Manual Debug so the signing secret can never end up in logs.
impl fmt::Debug for TokenSettings {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TokenSettings")
            .field("secret", &"<redacted>")
            .field("issuer", &self.issuer)
            .field("audience", &self.audience)
            .field("expiry_secs", &self.expiry_secs)
            .finish()
    }
}

#[derive(Debug, Clone)]
pub struct IssuedToken {
    pub token: String,
    pub expires_in_secs: u64,
}

/// Signs and verifies access tokens (HS256).
#[derive(Clone)]
pub struct TokenService {
    settings: TokenSettings,
    encoding_key: EncodingKey,
    decoding_key: DecodingKey,
    validation: Validation,
}

impl fmt::Debug for TokenService {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TokenService")
            .field("settings", &self.settings)
            .finish()
    }
}

impl TokenService {
    pub fn new(settings: TokenSettings) -> Self {
        // The algorithm is fixed here and never read from the token header,
        // which rules out "alg: none" and algorithm-swap attacks.
        let mut validation = Validation::new(Algorithm::HS256);
        validation.set_issuer(&[&settings.issuer]);
        validation.set_audience(&[&settings.audience]);
        validation.set_required_spec_claims(&["exp", "iss", "aud", "sub"]);
        validation.validate_exp = true;
        validation.validate_nbf = true;
        validation.leeway = LEEWAY_SECS;

        Self {
            encoding_key: EncodingKey::from_secret(settings.secret.as_bytes()),
            decoding_key: DecodingKey::from_secret(settings.secret.as_bytes()),
            validation,
            settings,
        }
    }

    pub fn issue_access(&self, user_id: Uuid) -> Result<IssuedToken, AppError> {
        let now = now_secs();
        let claims = AccessClaims {
            sub: user_id,
            iss: self.settings.issuer.clone(),
            aud: self.settings.audience.clone(),
            iat: now,
            nbf: now,
            exp: now + self.settings.expiry_secs,
            jti: Uuid::new_v4(),
            typ: ACCESS_TOKEN_TYPE.into(),
        };
        let token = self.sign(&claims)?;

        Ok(IssuedToken {
            token,
            expires_in_secs: self.settings.expiry_secs,
        })
    }

    /// Any problem (bad signature, expired, wrong issuer, ...) is simply
    /// `Unauthorized`. Clients never learn which check failed.
    pub fn verify_access(&self, token: &str) -> Result<AccessClaims, AppError> {
        let data = decode::<AccessClaims>(token, &self.decoding_key, &self.validation)
            .map_err(|_| AppError::Unauthorized)?;

        if data.claims.typ != ACCESS_TOKEN_TYPE {
            return Err(AppError::Unauthorized);
        }
        Ok(data.claims)
    }

    fn sign(&self, claims: &AccessClaims) -> Result<String, AppError> {
        encode(&Header::new(Algorithm::HS256), claims, &self.encoding_key)
            .map_err(|e| AppError::Internal(format!("could not sign token: {e}")))
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};

    use super::*;

    const SECRET: &str = "a-test-secret-that-is-long-enough-123";

    fn settings() -> TokenSettings {
        TokenSettings {
            secret: SECRET.into(),
            issuer: "tenant-saas".into(),
            audience: "tenant-saas-api".into(),
            expiry_secs: 900,
        }
    }

    fn service() -> TokenService {
        TokenService::new(settings())
    }

    /// Claims that would be valid, for tests to tweak one field at a time.
    fn valid_claims() -> AccessClaims {
        let now = now_secs();
        AccessClaims {
            sub: Uuid::new_v4(),
            iss: "tenant-saas".into(),
            aud: "tenant-saas-api".into(),
            iat: now,
            nbf: now,
            exp: now + 900,
            jti: Uuid::new_v4(),
            typ: "access".into(),
        }
    }

    fn sign_with(secret: &str, claims: &AccessClaims) -> String {
        encode(
            &Header::new(Algorithm::HS256),
            claims,
            &EncodingKey::from_secret(secret.as_bytes()),
        )
        .unwrap()
    }

    #[test]
    fn issued_token_verifies_and_carries_only_identity() {
        let service = service();
        let user_id = Uuid::new_v4();

        let issued = service.issue_access(user_id).unwrap();
        let claims = service.verify_access(&issued.token).unwrap();

        assert_eq!(issued.expires_in_secs, 900);
        assert_eq!(claims.sub, user_id);
        assert_eq!(claims.typ, "access");
        assert_eq!(claims.exp - claims.iat, 900);

        let principal = AuthenticatedPrincipal::from(&claims);
        assert_eq!(principal.user_id, user_id);
        assert_eq!(principal.token_id, claims.jti);
    }

    #[test]
    fn every_token_gets_a_unique_id() {
        let service = service();
        let user_id = Uuid::new_v4();
        let a = service
            .verify_access(&service.issue_access(user_id).unwrap().token)
            .unwrap();
        let b = service
            .verify_access(&service.issue_access(user_id).unwrap().token)
            .unwrap();
        assert_ne!(a.jti, b.jti);
    }

    #[test]
    fn expired_token_is_rejected() {
        let mut claims = valid_claims();
        claims.exp = now_secs() - 3600;
        let token = sign_with(SECRET, &claims);

        assert!(matches!(
            service().verify_access(&token),
            Err(AppError::Unauthorized)
        ));
    }

    #[test]
    fn token_that_is_not_valid_yet_is_rejected() {
        let mut claims = valid_claims();
        claims.nbf = now_secs() + 3600;
        let token = sign_with(SECRET, &claims);

        assert!(matches!(
            service().verify_access(&token),
            Err(AppError::Unauthorized)
        ));
    }

    #[test]
    fn wrong_issuer_is_rejected() {
        let mut claims = valid_claims();
        claims.iss = "someone-else".into();
        let token = sign_with(SECRET, &claims);

        assert!(matches!(
            service().verify_access(&token),
            Err(AppError::Unauthorized)
        ));
    }

    #[test]
    fn wrong_audience_is_rejected() {
        let mut claims = valid_claims();
        claims.aud = "another-api".into();
        let token = sign_with(SECRET, &claims);

        assert!(matches!(
            service().verify_access(&token),
            Err(AppError::Unauthorized)
        ));
    }

    #[test]
    fn wrong_secret_is_rejected() {
        let token = sign_with("a-completely-different-secret-value!!", &valid_claims());

        assert!(matches!(
            service().verify_access(&token),
            Err(AppError::Unauthorized)
        ));
    }

    #[test]
    fn refresh_typed_token_is_rejected() {
        let mut claims = valid_claims();
        claims.typ = "refresh".into();
        let token = sign_with(SECRET, &claims);

        assert!(matches!(
            service().verify_access(&token),
            Err(AppError::Unauthorized)
        ));
    }

    #[test]
    fn alg_none_token_is_rejected() {
        let header = URL_SAFE_NO_PAD.encode(r#"{"alg":"none","typ":"JWT"}"#);
        let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&valid_claims()).unwrap());
        let token = format!("{header}.{payload}.");

        assert!(matches!(
            service().verify_access(&token),
            Err(AppError::Unauthorized)
        ));
    }

    #[test]
    fn token_signed_with_a_different_algorithm_is_rejected() {
        let token = encode(
            &Header::new(Algorithm::HS384),
            &valid_claims(),
            &EncodingKey::from_secret(SECRET.as_bytes()),
        )
        .unwrap();

        assert!(matches!(
            service().verify_access(&token),
            Err(AppError::Unauthorized)
        ));
    }

    #[test]
    fn tampered_payload_is_rejected() {
        let service = service();
        let token = service.issue_access(Uuid::new_v4()).unwrap().token;

        // Swap in a payload for a different user but keep the old signature.
        let mut forged = valid_claims();
        forged.sub = Uuid::new_v4();
        let forged_payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&forged).unwrap());
        let mut parts: Vec<&str> = token.split('.').collect();
        parts[1] = &forged_payload;

        assert!(matches!(
            service.verify_access(&parts.join(".")),
            Err(AppError::Unauthorized)
        ));
    }

    #[test]
    fn garbage_is_rejected() {
        assert!(matches!(
            service().verify_access("nope"),
            Err(AppError::Unauthorized)
        ));
        assert!(matches!(
            service().verify_access(""),
            Err(AppError::Unauthorized)
        ));
    }

    #[test]
    fn debug_output_hides_the_secret() {
        let text = format!("{:?} {:?}", settings(), service());
        assert!(!text.contains(SECRET));
    }
}
