//! Account credentials, so ONE generic APK works for everybody.
//!
//! WHY THIS EXISTS
//!
//! Today a player is identified by the WireGuard IP the mitm addon stamps into
//! `X-Newblades-Device-Ip`. The client itself sends `deviceId: null`. That means
//! identity comes from the tunnel, so removing the VPN removes the player's
//! identity with it — and a VPN-free build would hand everyone a brand-new
//! anonymous character.
//!
//! The client already has the answer built in. Retail's own login is
//! `POST /api/authentication/v1/public/auth/bnet/login`, and a capture shows the
//! exact body:
//!
//! ```json
//! {"username":"…","password":"…","deviceId":"…","platform":"gp"}
//! ```
//!
//! answered with the same `SessionResponse` shape as `auth/anon`. So if a player
//! sets a username and password on their profile, they can sign in through the
//! game's own login screen and land on their own character — no VPN, no
//! certificate, no per-player APK, no device binding.
//!
//! HASHING
//!
//! PBKDF2-HMAC-SHA256, per-user random salt, 200k iterations, constant-time
//! compare. Deliberately not a bare hash: these passwords protect a player's own
//! character on a small community server, but people reuse passwords, so a
//! database read must not hand out anything crackable at speed.
//!
//! `sha2` / `hmac` / `subtle` were already in the build transitively; naming
//! them as direct dependencies added no new code, only the ability to call it.

use diesel::prelude::*;
use diesel_async::{AsyncPgConnection, RunQueryDsl};
use hmac::{Hmac, Mac};
use rand::RngExt;
use sha2::Sha256;
use subtle::ConstantTimeEq;
use uuid::Uuid;

type HmacSha256 = Hmac<Sha256>;

/// Cost. Chosen to be clearly slow on a GPU while staying imperceptible on a
/// login (a few tens of milliseconds on the box). Stored alongside the hash so
/// raising it later does not invalidate existing credentials.
pub const DEFAULT_ITERATIONS: u32 = 200_000;

const SALT_LEN: usize = 16;
const DK_LEN: usize = 32;

/// PBKDF2-HMAC-SHA256, one output block (dkLen = 32, so exactly one block —
/// which is why there is no block loop here).
///
/// Written out rather than pulled from a `pbkdf2` crate because the crate is not
/// in the tree and this is the whole of it: F(P,S,c,1) = U1 ^ U2 ^ … ^ Uc. It is
/// pinned against published vectors in the tests, so "hand-rolled" does not mean
/// "unverified".
fn pbkdf2_sha256(password: &[u8], salt: &[u8], iterations: u32) -> [u8; DK_LEN] {
    let mut u = {
        let mut mac = HmacSha256::new_from_slice(password).expect("hmac takes any key length");
        mac.update(salt);
        // INT_32_BE(1) — the block index. dkLen is one block, so it is always 1.
        mac.update(&1u32.to_be_bytes());
        mac.finalize().into_bytes()
    };
    let mut out = u;
    for _ in 1..iterations {
        let mut mac = HmacSha256::new_from_slice(password).expect("hmac takes any key length");
        mac.update(&u);
        u = mac.finalize().into_bytes();
        for (o, n) in out.iter_mut().zip(u.iter()) {
            *o ^= *n;
        }
    }
    let mut dk = [0u8; DK_LEN];
    dk.copy_from_slice(&out);
    dk
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

/// A stored credential: `pbkdf2$<iterations>$<salt hex>$<hash hex>`.
///
/// Self-describing so the cost can be raised without a migration — an old row
/// keeps verifying at its own iteration count.
pub fn hash_password(password: &str) -> String {
    // Same RNG the match registry uses for session keys — one source of
    // randomness in this binary rather than a second one just for salts.
    let salt: [u8; SALT_LEN] = rand::rng().random();
    let dk = pbkdf2_sha256(password.as_bytes(), &salt, DEFAULT_ITERATIONS);
    format!(
        "pbkdf2${}${}${}",
        DEFAULT_ITERATIONS,
        hex(&salt),
        hex(&dk)
    )
}

/// Constant-time verify. Returns false for any malformed stored value rather
/// than erroring: a corrupt row must read as "wrong password", never as "let
/// them in".
pub fn verify_password(password: &str, stored: &str) -> bool {
    let mut parts = stored.split('$');
    if parts.next() != Some("pbkdf2") {
        return false;
    }
    let Some(Ok(iterations)) = parts.next().map(str::parse::<u32>) else {
        return false;
    };
    // A zero-iteration row would make PBKDF2 degenerate; treat it as corrupt.
    if iterations == 0 {
        return false;
    }
    let (Some(salt), Some(expected)) = (parts.next().and_then(unhex), parts.next().and_then(unhex))
    else {
        return false;
    };
    if parts.next().is_some() || expected.len() != DK_LEN {
        return false;
    }
    let dk = pbkdf2_sha256(password.as_bytes(), &salt, iterations);
    dk.ct_eq(&expected[..]).into()
}

/// Usernames are compared case-insensitively, so `Ruukoto` and `ruukoto` are
/// one account. Retail's own login is a display name; letting case create two
/// accounts is a support burden nobody would thank us for.
pub fn normalise_username(raw: &str) -> String {
    raw.trim().to_lowercase()
}

/// Whether a username is acceptable. Deliberately narrow: it is typed on a
/// phone keyboard into the game's own login field.
pub fn username_is_valid(raw: &str) -> bool {
    let u = raw.trim();
    (3..=24).contains(&u.chars().count())
        && u.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.')
}

/// Minimum password length. Short enough to type on a phone, long enough that
/// 200k-iteration PBKDF2 is doing real work.
pub const MIN_PASSWORD_LEN: usize = 8;

#[derive(Debug, Clone, Queryable, Selectable)]
#[diesel(table_name = crate::schema::arena_credentials)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct CredentialRow {
    pub username: String,
    pub user_id: Uuid,
    pub password_hash: String,
}

/// Look a credential up by (already normalised) username.
pub async fn find_by_username(
    conn: &mut AsyncPgConnection,
    username: &str,
) -> QueryResult<Option<CredentialRow>> {
    use crate::schema::arena_credentials::dsl as c;
    c::arena_credentials
        .filter(c::username.eq(username))
        .select(CredentialRow::as_select())
        .first(conn)
        .await
        .optional()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Published PBKDF2-HMAC-SHA256 vectors. Without these, "hand-rolled
    /// PBKDF2" would only be self-consistent — it could be a different function
    /// entirely and every other test here would still pass.
    #[test]
    fn pbkdf2_matches_published_vectors() {
        let dk = pbkdf2_sha256(b"password", b"salt", 1);
        assert_eq!(
            hex(&dk),
            "120fb6cffcf8b32c43e7225256c4f837a86548c92ccc35480805987cb70be17b"
        );
        let dk = pbkdf2_sha256(b"password", b"salt", 2);
        assert_eq!(
            hex(&dk),
            "ae4d0c95af6b46d32d0adff928f06dd02a303f8ef3c251dfd6e2d85a95474c43"
        );
    }

    #[test]
    fn a_password_verifies_against_its_own_hash() {
        let stored = hash_password("correct horse battery");
        assert!(verify_password("correct horse battery", &stored));
        assert!(!verify_password("Correct horse battery", &stored));
        assert!(!verify_password("", &stored));
    }

    #[test]
    fn two_hashes_of_one_password_differ() {
        // Per-user salt: identical passwords must not produce identical rows,
        // or a database read tells you who shares a password.
        let a = hash_password("same");
        let b = hash_password("same");
        assert_ne!(a, b);
        assert!(verify_password("same", &a));
        assert!(verify_password("same", &b));
    }

    #[test]
    fn a_corrupt_row_never_authenticates() {
        // Every one of these must read as "wrong password", not as an error and
        // certainly not as success.
        for bad in [
            "",
            "pbkdf2",
            "pbkdf2$200000",
            "pbkdf2$200000$zz$zz",
            "pbkdf2$0$aabb$ccdd",
            "bcrypt$200000$aabb$ccdd",
            "pbkdf2$200000$aabb$ccdd",           // hash too short
            "pbkdf2$200000$aabb$ccdd$extra",     // trailing field
            "$$$",
        ] {
            assert!(!verify_password("anything", bad), "accepted {bad:?}");
        }
    }

    #[test]
    fn the_stored_format_carries_its_own_cost() {
        // So raising DEFAULT_ITERATIONS later cannot lock existing players out.
        let stored = hash_password("pw");
        assert!(stored.starts_with(&format!("pbkdf2${DEFAULT_ITERATIONS}$")));
        let cheap = {
            let salt = [7u8; SALT_LEN];
            let dk = pbkdf2_sha256(b"pw", &salt, 1000);
            format!("pbkdf2$1000${}${}", hex(&salt), hex(&dk))
        };
        assert!(verify_password("pw", &cheap), "an older cost must still verify");
    }

    #[test]
    fn usernames_are_case_insensitive_but_shape_checked() {
        assert_eq!(normalise_username("  Ruukoto "), "ruukoto");
        assert!(username_is_valid("Ruukoto"));
        assert!(username_is_valid("a_b-c.d"));
        assert!(!username_is_valid("ab"), "too short");
        assert!(!username_is_valid(&"x".repeat(25)), "too long");
        assert!(!username_is_valid("has space"));
        assert!(!username_is_valid("émoji"));
    }
}
