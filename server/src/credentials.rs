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
//! `POST /api/authentication/v1/public/auth/bnet/login`; an interactive login
//! supplies the credential, and later cold starts use the returned UUID token:
//!
//! ```json
//! {"username":"…","password":"…","deviceId":"…","platform":"gp"}
//! {"loginToken":"…","deviceId":"…","platform":"gp"}
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
    #[cfg(test)]
    {
        PBKDF2_RUNS_ON_THIS_THREAD.with(|runs| runs.set(runs.get() + 1));
        if password.len() > MAX_PASSWORD_BYTES {
            OVERSIZE_DERIVATIONS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }
    // Keyed ONCE and cloned per round. HMAC folds a key longer than its block
    // into a hash of the key first; keying inside the loop repeated that for
    // every one of the 200k rounds, so the cost of a login grew with the
    // length of the password the caller chose to send. The output is the same
    // function — the published vectors below pin it, long key included.
    let keyed = HmacSha256::new_from_slice(password).expect("hmac takes any key length");
    let mut u = {
        let mut mac = keyed.clone();
        mac.update(salt);
        // INT_32_BE(1) — the block index. dkLen is one block, so it is always 1.
        mac.update(&1u32.to_be_bytes());
        mac.finalize().into_bytes()
    };
    let mut out = u;
    for _ in 1..iterations {
        let mut mac = keyed.clone();
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

/// Derivations of a password over the limit, on ANY thread. No test derives
/// one on purpose, so anything but zero means a bound was skipped — including
/// on the blocking pool, where the per-thread counter cannot see.
#[cfg(test)]
static OVERSIZE_DERIVATIONS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

#[cfg(test)]
thread_local! {
    /// How many times this thread ran the key derivation. Lets a test say "no
    /// hash work was done" (or "it was done somewhere else") as a fact rather
    /// than a timing.
    static PBKDF2_RUNS_ON_THIS_THREAD: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
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
    format!("pbkdf2${}${}${}", DEFAULT_ITERATIONS, hex(&salt), hex(&dk))
}

/// Constant-time verify. Returns false for any malformed stored value rather
/// than erroring: a corrupt row must read as "wrong password", never as "let
/// them in".
/// A well-formed hash of a value nobody can supply, for the "no such user"
/// branch of the login and link endpoints.
///
/// IT MUST PARSE. The point is that an unknown username costs the SAME 200k
/// PBKDF2 iterations as a known one, so the response time does not reveal who
/// plays here. The previous literal padded its final field with 13 spaces —
/// 77 characters, non-hex — so `unhex` rejected it and `verify_password`
/// returned false at the parse step, BEFORE `pbkdf2_sha256`. Unknown usernames
/// answered in microseconds and known-username-wrong-password after tens of
/// milliseconds: exactly the enumeration oracle the branch was written to
/// close, on an unauthenticated endpoint.
///
/// So: four fields, non-zero iterations, salt and digest both even-length hex,
/// digest exactly DK_LEN bytes. `dummy_hash_reaches_pbkdf2` pins that.
pub const ENUMERATION_DUMMY_HASH: &str = concat!(
    "pbkdf2$200000$",
    "00000000000000000000000000000000$",
    "0000000000000000000000000000000000000000000000000000000000000000",
);

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

/// Longest password, in bytes, that is ever hashed or verified.
///
/// The login endpoints are unauthenticated and the JSON body limit is 16 MiB,
/// so without a bound the caller chooses how much this server reads and
/// hashes. Far above anything a person or a password manager produces; the
/// website has never set a maximum, so the bound is generous rather than
/// tidy — it must not turn a working login into a wrong password.
pub const MAX_PASSWORD_BYTES: usize = 1024;

/// Longest username, in bytes, that is ever looked up. A real one is at most
/// 24 ASCII characters ([`username_is_valid`]); the slack is for whitespace a
/// keyboard adds, which [`normalise_username`] trims.
pub const MAX_USERNAME_BYTES: usize = 128;

/// Could this pair be a real credential at all? Checked before anything is
/// normalised, looked up or hashed.
pub fn credential_is_within_bounds(username: &str, password: &str) -> bool {
    username.len() <= MAX_USERNAME_BYTES && password.len() <= MAX_PASSWORD_BYTES
}

#[derive(Debug, Clone, Queryable, Selectable)]
#[diesel(table_name = crate::schema::arena_credentials)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct CredentialRow {
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

// Checking a username and password lives in `crate::login_guard`: the
// failure limits, the bound on concurrent verifications and the rule that the
// pooled connection is returned before the hash runs all sit around the same
// three steps, so they are kept in one place. What it relies on from here:
//
// - [`credential_is_within_bounds`] before anything is normalised or hashed;
// - [`find_by_username`] for the row, and [`ENUMERATION_DUMMY_HASH`] when there
//   is none, so "no such user" costs the same PBKDF2 work as "wrong password";
// - [`verify_password`], run on the blocking pool.

/// [`hash_password`] on the blocking pool: tens of milliseconds of pure CPU
/// stall every other request an async worker is carrying.
pub async fn hash_password_off_the_async_workers(password: String) -> Option<String> {
    tokio::task::spawn_blocking(move || hash_password(&password))
        .await
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pbkdf2_runs_on_this_thread() -> u32 {
        PBKDF2_RUNS_ON_THIS_THREAD.with(|runs| runs.get())
    }

    /// RFC 4231 test case 6 keys HMAC-SHA256 with 131 bytes — longer than the
    /// 64-byte block, the case where the key is hashed first. One PBKDF2 round
    /// over an empty salt is HMAC(key, INT(1)), so the keyed-once derivation
    /// must agree with a freshly keyed HMAC for a long key, and with the
    /// straightforward key-every-round loop for several rounds.
    #[test]
    fn a_key_longer_than_the_hmac_block_derives_the_same_bytes() {
        for len in [1usize, 63, 64, 65, 131, 1024] {
            let key = vec![0xaau8; len];
            let mut u = {
                let mut mac = HmacSha256::new_from_slice(&key).unwrap();
                mac.update(b"salt");
                mac.update(&1u32.to_be_bytes());
                mac.finalize().into_bytes()
            };
            let mut reference = u;
            for _ in 1..5 {
                let mut mac = HmacSha256::new_from_slice(&key).unwrap();
                mac.update(&u);
                u = mac.finalize().into_bytes();
                for (o, n) in reference.iter_mut().zip(u.iter()) {
                    *o ^= *n;
                }
            }
            assert_eq!(
                pbkdf2_sha256(&key, b"salt", 5)[..],
                reference[..],
                "{len}-byte key"
            );
        }
    }

    #[test]
    fn an_over_long_credential_is_refused_before_any_hash_work() {
        let stored = hash_password("correct horse battery");
        let before = pbkdf2_runs_on_this_thread();

        let too_long = "x".repeat(MAX_PASSWORD_BYTES + 1);
        assert!(!credential_is_within_bounds("ruukoto", &too_long));
        assert!(!credential_is_within_bounds(
            &"u".repeat(MAX_USERNAME_BYTES + 1),
            "correct horse battery"
        ));
        // Bytes, not characters: the cost is in what has to be read.
        assert!(!credential_is_within_bounds(
            "ruukoto",
            &"\u{1F512}".repeat(MAX_PASSWORD_BYTES / 4 + 1)
        ));
        assert_eq!(
            pbkdf2_runs_on_this_thread(),
            before,
            "the bounds check must not derive anything"
        );

        // The longest allowed password is an ordinary one, and still verifies.
        let longest = "p".repeat(MAX_PASSWORD_BYTES);
        assert!(credential_is_within_bounds("ruukoto", &longest));
        assert!(credential_is_within_bounds(
            "  Ruukoto  ",
            "correct horse battery"
        ));
        assert!(verify_password(&longest, &hash_password(&longest)));
        assert!(verify_password("correct horse battery", &stored));
        assert!(pbkdf2_runs_on_this_thread() > before);
    }

    /// The property is "not on the thread that is serving requests". The
    /// counter is per thread, so a derivation that ran here would show. The
    /// login guard's verification is pinned the same way in `check_login_tests`.
    #[tokio::test]
    async fn hashing_runs_off_the_calling_thread() {
        let before = pbkdf2_runs_on_this_thread();
        let stored = hash_password_off_the_async_workers("pw".into())
            .await
            .expect("the hashing task joins");
        assert_eq!(
            pbkdf2_runs_on_this_thread(),
            before,
            "the derivation ran on the thread that awaited it"
        );
        assert!(verify_password("pw", &stored));
    }

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
            "pbkdf2$200000$aabb$ccdd",       // hash too short
            "pbkdf2$200000$aabb$ccdd$extra", // trailing field
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
        assert!(
            verify_password("pw", &cheap),
            "an older cost must still verify"
        );
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

#[cfg(test)]
mod check_login_tests {
    use super::*;
    use crate::login_guard::{LoginGuard, LoginGuardConfig, LoginOutcome};
    use diesel_async::AsyncConnection;

    /// The production check (limits, bounds, dummy hash, blocking pool) with
    /// the row read through this test's own connection.
    async fn check_login(
        conn: &mut AsyncPgConnection,
        username_raw: &str,
        password: &str,
    ) -> Option<Uuid> {
        // A fresh guard per call: these tests are about the credential, not
        // about the failure limits, which `login_guard` tests on their own.
        let guard = LoginGuard::new(LoginGuardConfig::from_env());
        let before = PBKDF2_RUNS_ON_THIS_THREAD.with(|runs| runs.get());
        let outcome = guard
            .check(None, username_raw, password, |name| async move {
                Ok(find_by_username(conn, &name).await.unwrap_or(None))
            })
            .await;
        assert_eq!(
            PBKDF2_RUNS_ON_THIS_THREAD.with(|runs| runs.get()),
            before,
            "the derivation ran on the thread serving the request"
        );
        match outcome {
            LoginOutcome::Accepted(id) => Some(id),
            LoginOutcome::Refused => None,
            LoginOutcome::Busy => panic!("busy with one request in flight"),
        }
    }

    /// A private `arena_credentials` holding one login, `ruukoto` / `pw`.
    async fn one_login() -> Option<(AsyncPgConnection, Uuid)> {
        let Some(url) = std::env::var("TEST_DATABASE_URL").ok() else {
            eprintln!("SKIP: TEST_DATABASE_URL unset — credential check not verified");
            return None;
        };
        let mut conn = AsyncPgConnection::establish(&url)
            .await
            .expect("TEST_DATABASE_URL is set but unreachable");
        conn.begin_test_transaction().await.unwrap();
        let schema = format!("t{}", Uuid::new_v4().simple());
        for stmt in [
            format!("CREATE SCHEMA {schema}"),
            format!("SET LOCAL search_path TO {schema}"),
            "CREATE TABLE arena_credentials (
                 username TEXT PRIMARY KEY,
                 user_id UUID NOT NULL,
                 password_hash TEXT NOT NULL,
                 created_at BIGINT NOT NULL DEFAULT 0,
                 updated_at BIGINT NOT NULL DEFAULT 0)"
                .to_string(),
        ] {
            diesel::sql_query(stmt).execute(&mut conn).await.unwrap();
        }
        let user_id = Uuid::new_v4();
        let salt = [3u8; SALT_LEN];
        let stored = format!(
            "pbkdf2$1000${}${}",
            hex(&salt),
            hex(&pbkdf2_sha256(b"pw", &salt, 1000))
        );
        diesel::sql_query(
            "INSERT INTO arena_credentials (username, user_id, password_hash) VALUES ($1, $2, $3)",
        )
        .bind::<diesel::sql_types::Text, _>("ruukoto")
        .bind::<diesel::sql_types::Uuid, _>(user_id)
        .bind::<diesel::sql_types::Text, _>(stored)
        .execute(&mut conn)
        .await
        .unwrap();
        Some((conn, user_id))
    }

    #[tokio::test]
    async fn the_right_password_names_the_account_and_nothing_else_does() {
        let Some((mut conn, user_id)) = one_login().await else {
            return;
        };
        assert_eq!(check_login(&mut conn, "ruukoto", "pw").await, Some(user_id));
        assert_eq!(
            check_login(&mut conn, "  Ruukoto ", "pw").await,
            Some(user_id)
        );
        assert_eq!(check_login(&mut conn, "ruukoto", "wrong").await, None);
        assert_eq!(check_login(&mut conn, "nobody", "pw").await, None);
    }

    /// Refused before the derivation — on whichever thread it would have run —
    /// and with the same `None` a wrong password gets, even when everything
    /// but the padding is right.
    #[tokio::test]
    async fn an_over_long_credential_reads_as_a_wrong_password() {
        let Some((mut conn, user_id)) = one_login().await else {
            return;
        };
        let huge = "x".repeat(MAX_PASSWORD_BYTES + 1);
        assert_eq!(check_login(&mut conn, "ruukoto", &huge).await, None);
        assert_eq!(check_login(&mut conn, "nobody", &huge).await, None);
        let padded = format!("ruukoto{}", " ".repeat(MAX_USERNAME_BYTES));
        assert_eq!(check_login(&mut conn, &padded, "pw").await, None);
        assert_eq!(
            // Spelled out: diesel's `RunQueryDsl::load` is in scope here.
            std::sync::atomic::AtomicU32::load(
                &OVERSIZE_DERIVATIONS,
                std::sync::atomic::Ordering::SeqCst
            ),
            0,
            "an over-long password reached the key derivation"
        );
        // The account is untouched and still opens.
        assert_eq!(check_login(&mut conn, "ruukoto", "pw").await, Some(user_id));
    }
}

#[cfg(test)]
mod dummy_hash_tests {
    use super::*;

    /// The dummy must be REJECTED as a password match but ACCEPTED by the
    /// parser, so the failing branch still pays for PBKDF2.
    ///
    /// Timing cannot be asserted reliably in a unit test, so this pins the
    /// structural property whose violation caused the leak: every field the
    /// parser requires is well-formed. A malformed control shows the assertion
    /// has teeth — it is the shape the old literal had.
    #[test]
    fn dummy_hash_reaches_pbkdf2() {
        let parts: Vec<&str> = ENUMERATION_DUMMY_HASH.split('$').collect();
        assert_eq!(parts.len(), 4, "expected pbkdf2$iters$salt$digest");
        assert_eq!(parts[0], "pbkdf2");
        assert!(parts[1].parse::<u32>().is_ok_and(|n| n > 0), "iterations");
        assert!(unhex(parts[2]).is_some(), "salt must be even-length hex");
        let digest = unhex(parts[3]).expect("digest must be even-length hex");
        assert_eq!(digest.len(), DK_LEN, "digest must be DK_LEN bytes");

        // And it must not match anything.
        assert!(!verify_password("", ENUMERATION_DUMMY_HASH));
        assert!(!verify_password("hunter2", ENUMERATION_DUMMY_HASH));

        // Control: the shape the old literal had — space-padded, odd length —
        // fails to parse, which is what made the fast path possible.
        let old_shape = "pbkdf2$200000$00000000000000000000000000000000$             0000000000000000000000000000000000000000000000000000000000000000";
        assert!(
            unhex(old_shape.split('$').nth(3).unwrap()).is_none(),
            "the control must be unparseable, or this test proves nothing"
        );
    }
}
