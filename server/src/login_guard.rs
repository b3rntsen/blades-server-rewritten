//! Limits on the password sign-in: how often a username or an address may get
//! it wrong, and how many password checks may run at once.
//!
//! WHY THIS EXISTS
//!
//! `auth/bnet/login`, `auth/bnet/link` and `auth/bnet/link/force` take a
//! username and password from anyone on the internet. Nothing limited how often:
//! a guesser was bounded only by how fast the box could run PBKDF2, and the box
//! is shared with every player. Three things, all here:
//!
//! 1. **Failures per username.** After `max_failures` wrong answers inside
//!    `window`, that username is refused for `lockout`. A correct login resets
//!    the count.
//! 2. **Failures per source address**, the same way, with a higher threshold
//!    because several players can share one address (carrier NAT, a household).
//! 3. **A bound on concurrent verifications.** A login used to hold its pooled
//!    database connection while the hash ran, and nothing limited how many
//!    hashes ran at once, so about ten slow bogus logins could hold all ten
//!    connections and stall every other request. The credential row is now
//!    read, the connection RETURNED, and only then the hash verified, under a
//!    semaphore sized to the CPUs. A request that cannot get a slot in time is
//!    refused as "busy" before any account data is read.
//!
//! WHAT A REFUSAL LOOKS LIKE
//!
//! A locked username or address gets exactly what a wrong password gets: the
//! same `Refused`, which the handlers turn into the same 401 envelope. The lock
//! applies to usernames that do not exist just as to ones that do, so it says
//! nothing about who plays here. It is also answered after a pause as long as a
//! typical verification, so a lock cannot be told from a wrong password by the
//! response time either.
//!
//! "Busy" is a 503, not a 401. It is decided before the credential row is read,
//! so it cannot depend on the account; and telling a player with the right
//! password "wrong password" during a flood would make them retype it and add
//! to their own failure count.
//!
//! STATE
//!
//! In memory, per process. A restart forgets every count, which costs an
//! attacker nothing they could not get by waiting out the window anyway, and a
//! table in Postgres would put a write on every failed guess — the thing being
//! limited. Each table holds at most `max_tracked` keys; when full, expired
//! entries go first, then the oldest unlocked one, so a flood of distinct
//! usernames cannot grow memory without bound. Usernames are kept only as a
//! keyed 64-bit hash.

use std::{
    collections::HashMap,
    future::Future,
    hash::{BuildHasher, Hash, RandomState},
    net::IpAddr,
    sync::{
        Arc, LazyLock, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use actix_web::HttpRequest;
use sha2::{Digest, Sha256};
use tokio::sync::Semaphore;
use uuid::Uuid;

use crate::credentials::{self, CredentialRow};

/// How many failures inside which window start a lock-out of what length.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limit {
    pub max_failures: u32,
    pub window: Duration,
    pub lockout: Duration,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoginGuardConfig {
    pub username: Limit,
    pub address: Limit,
    /// Keys each table may hold.
    pub max_tracked: usize,
    /// Password verifications allowed to run at once.
    pub verify_concurrency: usize,
    /// How long a request may wait for a verification slot before it is
    /// refused as busy.
    pub verify_wait: Duration,
}

/// Defaults, and why.
///
/// A player types this on a phone keyboard, often more than once: autocorrect,
/// a capital they did not mean, a password from the website they half remember.
/// Ten tries in fifteen minutes is far more than that; it is also four orders
/// of magnitude below what a guesser needs against even a weak password, and
/// the lock-out lasts only as long as the window, so a player who did lock
/// themselves out is back within a quarter of an hour.
///
/// The address limit is three times the username limit because an address can
/// be many players: one guessing wrong should not lock out their neighbours on
/// the same carrier NAT. It still stops one address walking a list of names.
pub const DEFAULT_USERNAME_MAX_FAILURES: u32 = 10;
pub const DEFAULT_ADDRESS_MAX_FAILURES: u32 = 30;
pub const DEFAULT_WINDOW_SECS: u64 = 15 * 60;
pub const DEFAULT_LOCKOUT_SECS: u64 = 15 * 60;
pub const DEFAULT_MAX_TRACKED: usize = 10_000;
pub const DEFAULT_VERIFY_WAIT_MS: u64 = 2_000;

/// The environment, with the default for anything unset, empty, zero or
/// unparseable.
pub const ENV_USERNAME_MAX_FAILURES: &str = "ARENA_LOGIN_USER_MAX_FAILURES";
pub const ENV_USERNAME_WINDOW_SECS: &str = "ARENA_LOGIN_USER_WINDOW_SECS";
pub const ENV_USERNAME_LOCKOUT_SECS: &str = "ARENA_LOGIN_USER_LOCKOUT_SECS";
pub const ENV_ADDRESS_MAX_FAILURES: &str = "ARENA_LOGIN_ADDR_MAX_FAILURES";
pub const ENV_ADDRESS_WINDOW_SECS: &str = "ARENA_LOGIN_ADDR_WINDOW_SECS";
pub const ENV_ADDRESS_LOCKOUT_SECS: &str = "ARENA_LOGIN_ADDR_LOCKOUT_SECS";
pub const ENV_MAX_TRACKED: &str = "ARENA_LOGIN_TRACKED_MAX";
pub const ENV_VERIFY_CONCURRENCY: &str = "ARENA_LOGIN_VERIFY_CONCURRENCY";
pub const ENV_VERIFY_WAIT_MS: &str = "ARENA_LOGIN_VERIFY_WAIT_MS";

/// One slot per CPU, at most eight. More slots than cores only makes every
/// verification slower, and the hashes share the box with the game.
fn default_verify_concurrency() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .clamp(1, 8)
}

impl LoginGuardConfig {
    pub fn from_env() -> Self {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Self {
        let num = |name: &str, default: u64| -> u64 {
            match get(name).map(|v| v.trim().to_string()) {
                None => default,
                Some(v) if v.is_empty() => default,
                Some(v) => match v.parse::<u64>() {
                    Ok(n) if n > 0 => n,
                    _ => {
                        log::warn!("{name}={v:?} is not a positive whole number; using {default}");
                        default
                    }
                },
            }
        };
        let secs = |name: &str, default: u64| Duration::from_secs(num(name, default));
        let count = |name: &str, default: u32| {
            u32::try_from(num(name, u64::from(default))).unwrap_or(default)
        };
        Self {
            username: Limit {
                max_failures: count(ENV_USERNAME_MAX_FAILURES, DEFAULT_USERNAME_MAX_FAILURES),
                window: secs(ENV_USERNAME_WINDOW_SECS, DEFAULT_WINDOW_SECS),
                lockout: secs(ENV_USERNAME_LOCKOUT_SECS, DEFAULT_LOCKOUT_SECS),
            },
            address: Limit {
                max_failures: count(ENV_ADDRESS_MAX_FAILURES, DEFAULT_ADDRESS_MAX_FAILURES),
                window: secs(ENV_ADDRESS_WINDOW_SECS, DEFAULT_WINDOW_SECS),
                lockout: secs(ENV_ADDRESS_LOCKOUT_SECS, DEFAULT_LOCKOUT_SECS),
            },
            max_tracked: num(ENV_MAX_TRACKED, DEFAULT_MAX_TRACKED as u64) as usize,
            verify_concurrency: num(ENV_VERIFY_CONCURRENCY, default_verify_concurrency() as u64)
                as usize,
            verify_wait: Duration::from_millis(num(ENV_VERIFY_WAIT_MS, DEFAULT_VERIFY_WAIT_MS)),
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct Entry {
    failures: u32,
    window_start: Instant,
    locked_until: Option<Instant>,
}

impl Entry {
    fn locked_at(&self, now: Instant) -> bool {
        self.locked_until.is_some_and(|until| now < until)
    }

    /// Nothing left to remember: not locked, and the counting window is over.
    fn expired_at(&self, now: Instant, limit: &Limit) -> bool {
        !self.locked_at(now) && now.saturating_duration_since(self.window_start) >= limit.window
    }
}

/// A bounded table of failure counts.
struct Table<K> {
    limit: Limit,
    max: usize,
    entries: HashMap<K, Entry>,
}

impl<K: Hash + Eq + Copy> Table<K> {
    fn new(limit: Limit, max: usize) -> Self {
        Self {
            limit,
            max: max.max(1),
            entries: HashMap::new(),
        }
    }

    fn is_locked(&self, key: &K, now: Instant) -> bool {
        self.entries.get(key).is_some_and(|e| e.locked_at(now))
    }

    /// Count a failure. Returns the number of failures that started a lock-out,
    /// when this one did.
    fn record_failure(&mut self, key: K, now: Instant) -> Option<u32> {
        if !self.entries.contains_key(&key) {
            self.make_room(now);
        }
        let limit = self.limit;
        let entry = self.entries.entry(key).or_insert(Entry {
            failures: 0,
            window_start: now,
            locked_until: None,
        });
        if entry.locked_at(now) {
            // Already locked: the lock is not extended, or anyone could keep a
            // player out forever by sending one guess a minute.
            return None;
        }
        if now.saturating_duration_since(entry.window_start) >= limit.window {
            entry.failures = 0;
            entry.window_start = now;
        }
        entry.failures += 1;
        if entry.failures >= limit.max_failures {
            let failures = entry.failures;
            entry.failures = 0;
            entry.window_start = now;
            entry.locked_until = Some(now + limit.lockout);
            return Some(failures);
        }
        None
    }

    fn clear(&mut self, key: &K) {
        self.entries.remove(key);
    }

    /// Make room for one more key: drop everything expired, then, if still
    /// full, the unlocked entry whose window started first, then the lock that
    /// ends soonest.
    fn make_room(&mut self, now: Instant) {
        if self.entries.len() < self.max {
            return;
        }
        let limit = self.limit;
        self.entries.retain(|_, e| !e.expired_at(now, &limit));
        while self.entries.len() >= self.max {
            let victim = self
                .entries
                .iter()
                .min_by_key(|(_, e)| match e.locked_until.filter(|_| e.locked_at(now)) {
                    None => (0u8, e.window_start),
                    Some(until) => (1u8, until),
                })
                .map(|(k, _)| *k);
            match victim {
                Some(k) => {
                    self.entries.remove(&k);
                }
                None => break,
            }
        }
    }

    fn len(&self) -> usize {
        self.entries.len()
    }
}

/// What a sign-in attempt came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginOutcome {
    /// The credential opens this account.
    Accepted(Uuid),
    /// Wrong password, no such user, out of bounds, or locked out — one answer
    /// for all of them.
    Refused,
    /// No verification slot in time. Decided before the account is read.
    Busy,
}

type Verifier = Arc<dyn Fn(&str, &str) -> bool + Send + Sync>;

pub struct LoginGuard {
    config: LoginGuardConfig,
    hasher: RandomState,
    usernames: Mutex<Table<u64>>,
    addresses: Mutex<Table<IpAddr>>,
    permits: Semaphore,
    /// Microseconds a verification typically takes (a moving average), so a
    /// locked refusal can take as long as a real one.
    typical_verify_micros: AtomicU64,
    verify: Verifier,
}

/// Before any measurement: about what 200k PBKDF2 rounds take on the box.
const INITIAL_TYPICAL_VERIFY_MICROS: u64 = 60_000;

impl LoginGuard {
    pub fn new(config: LoginGuardConfig) -> Self {
        Self::with_verifier(config, Arc::new(credentials::verify_password))
    }

    fn with_verifier(config: LoginGuardConfig, verify: Verifier) -> Self {
        Self {
            usernames: Mutex::new(Table::new(config.username, config.max_tracked)),
            addresses: Mutex::new(Table::new(config.address, config.max_tracked)),
            permits: Semaphore::new(config.verify_concurrency.max(1)),
            hasher: RandomState::new(),
            typical_verify_micros: AtomicU64::new(INITIAL_TYPICAL_VERIFY_MICROS),
            verify,
            config,
        }
    }

    fn username_key(&self, normalised: &str) -> u64 {
        self.hasher.hash_one(normalised)
    }

    /// Check a credential against the limits and, if they allow it, against
    /// the stored hash.
    ///
    /// `lookup` reads the credential row for a normalised username. It is
    /// called with no verification running yet and must not keep anything
    /// (in production: a pooled connection) past its own return — the hash is
    /// verified after it has finished.
    pub async fn check<L, Fut>(
        &self,
        source: Option<IpAddr>,
        username_raw: &str,
        password: &str,
        lookup: L,
    ) -> LoginOutcome
    where
        L: FnOnce(String) -> Fut,
        Fut: Future<Output = Result<Option<CredentialRow>, ()>>,
    {
        let source = source.map(address_key);
        // Counted under its first MAX_USERNAME_BYTES, so an over-long name
        // costs one bounded normalisation and still lands on a counter.
        let mut cut = credentials::MAX_USERNAME_BYTES.min(username_raw.len());
        while !username_raw.is_char_boundary(cut) {
            cut -= 1;
        }
        let username = credentials::normalise_username(&username_raw[..cut]);
        let user_key = self.username_key(&username);

        let locked = {
            let now = Instant::now();
            self.usernames.lock().unwrap().is_locked(&user_key, now)
                || source.is_some_and(|a| self.addresses.lock().unwrap().is_locked(&a, now))
        };
        if locked {
            tokio::time::sleep(self.typical_verify_time()).await;
            return LoginOutcome::Refused;
        }

        if !credentials::credential_is_within_bounds(username_raw, password) {
            log::warn!(
                "credential check: refused an over-long username or password without hashing (username {} bytes, password {} bytes)",
                username_raw.len(),
                password.len()
            );
            self.record_failure(user_key, &username, source);
            return LoginOutcome::Refused;
        }

        let Ok(Ok(_permit)) =
            tokio::time::timeout(self.config.verify_wait, self.permits.acquire()).await
        else {
            log::warn!(
                "credential check: no verification slot within {} ms; answered busy",
                self.config.verify_wait.as_millis()
            );
            return LoginOutcome::Busy;
        };

        let Ok(found) = lookup(username.clone()).await else {
            // The database, not the credential: nothing was learned about the
            // account, so nothing is counted against it.
            return LoginOutcome::Busy;
        };
        let (user_id, stored) = match found {
            Some(row) => (Some(row.user_id), row.password_hash),
            None => (None, credentials::ENUMERATION_DUMMY_HASH.to_string()),
        };

        let verify = self.verify.clone();
        let password = password.to_string();
        let started = Instant::now();
        let ok = tokio::task::spawn_blocking(move || verify(&password, &stored))
            .await
            .unwrap_or(false);
        self.note_verify_time(started.elapsed());

        match user_id.filter(|_| ok) {
            Some(user_id) => {
                self.usernames.lock().unwrap().clear(&user_key);
                LoginOutcome::Accepted(user_id)
            }
            None => {
                self.record_failure(user_key, &username, source);
                LoginOutcome::Refused
            }
        }
    }

    fn record_failure(&self, user_key: u64, username: &str, source: Option<IpAddr>) {
        let now = Instant::now();
        let source_text = source.map_or_else(|| "unknown".to_string(), |a| a.to_string());
        if let Some(failures) = self.usernames.lock().unwrap().record_failure(user_key, now) {
            log::info!(
                "login limit: username #{} locked for {}s after {failures} failed sign-ins (latest from {source_text})",
                username_tag(username),
                self.config.username.lockout.as_secs()
            );
        }
        if let Some(address) = source
            && let Some(failures) = self.addresses.lock().unwrap().record_failure(address, now)
        {
            log::info!(
                "login limit: address {address} locked for {}s after {failures} failed sign-ins",
                self.config.address.lockout.as_secs()
            );
        }
    }

    fn typical_verify_time(&self) -> Duration {
        Duration::from_micros(self.typical_verify_micros.load(Ordering::Relaxed))
    }

    /// Exponential moving average, weight 1/4 on the newest sample.
    fn note_verify_time(&self, took: Duration) {
        let sample = u64::try_from(took.as_micros()).unwrap_or(u64::MAX);
        let _ =
            self.typical_verify_micros
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |old| {
                    Some(old - old / 4 + sample / 4)
                });
    }

    #[cfg(test)]
    fn tracked(&self) -> (usize, usize) {
        (
            self.usernames.lock().unwrap().len(),
            self.addresses.lock().unwrap().len(),
        )
    }
}

/// The process's guard, configured from the environment on first use.
pub fn global() -> &'static LoginGuard {
    static GUARD: LazyLock<LoginGuard> = LazyLock::new(|| {
        let config = LoginGuardConfig::from_env();
        log::info!(
            "login limits: username {} failures/{}s -> {}s lock; address {} failures/{}s -> {}s lock; {} keys per table; {} concurrent verifications, {} ms wait",
            config.username.max_failures,
            config.username.window.as_secs(),
            config.username.lockout.as_secs(),
            config.address.max_failures,
            config.address.window.as_secs(),
            config.address.lockout.as_secs(),
            config.max_tracked,
            config.verify_concurrency,
            config.verify_wait.as_millis()
        );
        LoginGuard::new(config)
    });
    &GUARD
}

/// Check a credential with the process's limits, reading the row through a
/// pooled connection that is returned before the hash is verified.
pub async fn check_login(
    pool: &crate::DbPool,
    source: Option<IpAddr>,
    username_raw: &str,
    password: &str,
) -> LoginOutcome {
    check_login_with(global(), pool, source, username_raw, password).await
}

async fn check_login_with(
    guard: &LoginGuard,
    pool: &crate::DbPool,
    source: Option<IpAddr>,
    username_raw: &str,
    password: &str,
) -> LoginOutcome {
    guard
        .check(source, username_raw, password, |username| async move {
            // The connection lives in this block only: it is back in the pool
            // before the caller starts on the hash.
            let mut conn = pool.get().await.map_err(|error| {
                log::warn!("credential check: no database connection: {error}");
            })?;
            Ok(credentials::find_by_username(&mut conn, &username)
                .await
                .unwrap_or(None))
        })
        .await
}

/// A short, stable tag for a username in the log: the first 12 hex digits of
/// SHA-256 of the normalised name. The owner can match it to an account
/// (`printf %s name | shasum -a 256 | cut -c1-12`); the log never holds the
/// name, and never anything about the password.
pub fn username_tag(normalised: &str) -> String {
    Sha256::digest(normalised.as_bytes())
        .iter()
        .take(6)
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The key an address is counted under. IPv6 by /64: one subscriber is given
/// a whole /64, so counting single addresses would hand a guesser 2^64 fresh
/// counters.
fn address_key(addr: IpAddr) -> IpAddr {
    match addr.to_canonical() {
        IpAddr::V6(v6) => {
            let masked = u128::from(v6) & (u128::MAX << 64);
            IpAddr::V6(masked.into())
        }
        v4 => v4,
    }
}

// ---------------------------------------------------------------- source address

/// The same variable and default as the device-identity header's trust list:
/// the peers our own proxies connect from. The arena's HTTP port is published
/// on 127.0.0.1 only, so in practice those are the only peers there are; the
/// check is so that this never depends on that.
const TRUSTED_PEERS_ENV: &str = "ARENA_DEVICE_IP_TRUSTED_PEERS";
const DEFAULT_TRUSTED_PEERS: &str = "127.0.0.0/8,::1,172.16.0.0/12,192.168.0.0/16";

#[derive(Clone, Copy, Debug, PartialEq)]
struct PeerBlock {
    network: IpAddr,
    prefix: u8,
}

impl PeerBlock {
    fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        let (addr, prefix) = match text.split_once('/') {
            Some((addr, prefix)) => (addr, Some(prefix.parse::<u8>().ok()?)),
            None => (text, None),
        };
        let network: IpAddr = addr.parse().ok()?;
        let width = if network.is_ipv4() { 32 } else { 128 };
        let prefix = prefix.unwrap_or(width);
        (prefix <= width).then_some(Self { network, prefix })
    }

    fn contains(&self, addr: IpAddr) -> bool {
        match (self.network, addr) {
            (IpAddr::V4(net), IpAddr::V4(addr)) => {
                let mask = u32::MAX
                    .checked_shl(32 - u32::from(self.prefix))
                    .unwrap_or(0);
                u32::from(net) & mask == u32::from(addr) & mask
            }
            (IpAddr::V6(net), IpAddr::V6(addr)) => {
                let mask = u128::MAX
                    .checked_shl(128 - u32::from(self.prefix))
                    .unwrap_or(0);
                u128::from(net) & mask == u128::from(addr) & mask
            }
            _ => false,
        }
    }
}

fn parse_trusted_peers(list: &str) -> Vec<PeerBlock> {
    list.split(',')
        .filter(|entry| !entry.trim().is_empty())
        .filter_map(PeerBlock::parse)
        .collect()
}

fn trusted_peers() -> &'static [PeerBlock] {
    static PEERS: LazyLock<Vec<PeerBlock>> = LazyLock::new(|| {
        let configured = std::env::var(TRUSTED_PEERS_ENV)
            .ok()
            .filter(|list| !list.trim().is_empty());
        parse_trusted_peers(configured.as_deref().unwrap_or(DEFAULT_TRUSTED_PEERS))
    });
    &PEERS
}

/// The address a sign-in is counted against.
///
/// A forwarded address is believed only from a peer that is one of our own
/// proxies; from anyone else the socket peer is the answer, whatever headers it
/// sent. From our proxies, in order:
///
/// - `X-Newblades-Device-Ip`: the WireGuard peer address the mitm addon
///   OVERWRITES on every request it forwards (the tunnel path). The public edge
///   erases it.
/// - `X-Real-IP`: the public edge sets it to the TCP peer it saw, overwriting
///   anything the client sent.
/// - otherwise the proxy itself.
pub fn login_source(req: &HttpRequest) -> Option<IpAddr> {
    source_from(
        req.peer_addr().map(|a| a.ip()),
        |name| {
            req.headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
        },
        trusted_peers(),
    )
}

fn source_from(
    peer: Option<IpAddr>,
    header: impl Fn(&str) -> Option<String>,
    trusted: &[PeerBlock],
) -> Option<IpAddr> {
    let peer = peer.map(|p| p.to_canonical());
    let from_proxy = peer.is_some_and(|p| trusted.iter().any(|block| block.contains(p)));
    if !from_proxy {
        return peer;
    }
    ["X-Newblades-Device-Ip", "X-Real-IP"]
        .into_iter()
        .find_map(|name| header(name).and_then(|v| v.trim().parse::<IpAddr>().ok()))
        .map(|a| a.to_canonical())
        .or(peer)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    const ALICE: &str = "alice";
    fn alice_id() -> Uuid {
        Uuid::from_u128(0xA11CE)
    }

    fn config() -> LoginGuardConfig {
        LoginGuardConfig {
            username: Limit {
                max_failures: 3,
                window: Duration::from_secs(60),
                lockout: Duration::from_secs(60),
            },
            address: Limit {
                max_failures: 5,
                window: Duration::from_secs(60),
                lockout: Duration::from_secs(60),
            },
            max_tracked: 100,
            verify_concurrency: 4,
            verify_wait: Duration::from_secs(5),
        }
    }

    /// A plain-text "hash": the verifier compares strings. Keeps these tests
    /// about the limits, not about PBKDF2.
    fn plain_guard(config: LoginGuardConfig) -> LoginGuard {
        LoginGuard::with_verifier(config, Arc::new(|pw: &str, stored: &str| pw == stored))
    }

    async fn attempt(
        guard: &LoginGuard,
        source: &str,
        username: &str,
        password: &str,
    ) -> LoginOutcome {
        guard
            .check(
                Some(source.parse().unwrap()),
                username,
                password,
                |name| async move {
                    Ok((name == ALICE).then(|| CredentialRow {
                        user_id: alice_id(),
                        password_hash: "right".into(),
                    }))
                },
            )
            .await
    }

    #[tokio::test]
    async fn a_username_locks_after_n_failures_and_then_refuses_the_right_password() {
        let guard = plain_guard(config());
        for _ in 0..2 {
            assert_eq!(
                attempt(&guard, "203.0.113.1", ALICE, "wrong").await,
                LoginOutcome::Refused
            );
        }
        // Below the limit the right password still works...
        assert_eq!(
            attempt(&guard, "203.0.113.1", ALICE, "right").await,
            LoginOutcome::Accepted(alice_id())
        );
        // ...and reset the count, so three more failures are needed.
        for _ in 0..3 {
            attempt(&guard, "203.0.113.2", ALICE, "wrong").await;
        }
        // Locked: the right password, from a fresh address, gets exactly what a
        // wrong password gets.
        assert_eq!(
            attempt(&guard, "203.0.113.3", ALICE, "right").await,
            LoginOutcome::Refused
        );
        // Case and padding do not make a second username.
        assert_eq!(
            attempt(&guard, "203.0.113.4", "  ALICE ", "right").await,
            LoginOutcome::Refused
        );
    }

    #[tokio::test]
    async fn success_resets_the_username_count() {
        let guard = plain_guard(config());
        for _ in 0..2 {
            attempt(&guard, "203.0.113.1", ALICE, "wrong").await;
        }
        assert_eq!(
            attempt(&guard, "203.0.113.1", ALICE, "right").await,
            LoginOutcome::Accepted(alice_id())
        );
        for _ in 0..2 {
            attempt(&guard, "203.0.113.1", ALICE, "wrong").await;
        }
        assert_eq!(
            attempt(&guard, "203.0.113.1", ALICE, "right").await,
            LoginOutcome::Accepted(alice_id()),
            "two failures after a success must not lock: the success reset the count"
        );
    }

    /// A lock on a name nobody has is the same as a lock on a real one, so the
    /// lock is no oracle for who plays here.
    #[tokio::test]
    async fn unknown_usernames_lock_exactly_like_known_ones() {
        let guard = plain_guard(config());
        for _ in 0..3 {
            assert_eq!(
                attempt(&guard, "203.0.113.9", "nobody", "x").await,
                LoginOutcome::Refused
            );
        }
        assert!(
            guard
                .usernames
                .lock()
                .unwrap()
                .is_locked(&guard.username_key("nobody"), Instant::now())
        );
    }

    /// A locked refusal takes as long as a typical verification, so the
    /// response time does not say "locked" where it would say "wrong".
    #[tokio::test]
    async fn a_locked_refusal_takes_a_verification_time() {
        let guard = plain_guard(config());
        for _ in 0..3 {
            attempt(&guard, "203.0.113.1", ALICE, "wrong").await;
        }
        guard.typical_verify_micros.store(80_000, Ordering::Relaxed);
        let started = Instant::now();
        assert_eq!(
            attempt(&guard, "203.0.113.1", ALICE, "wrong").await,
            LoginOutcome::Refused
        );
        assert!(
            started.elapsed() >= Duration::from_millis(80),
            "locked refusal answered in {:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn an_address_locks_after_its_own_limit_across_usernames() {
        let guard = plain_guard(config());
        // Five different names, one failure each: no username is near its
        // limit, but the address reaches its own.
        for i in 0..5 {
            attempt(&guard, "198.51.100.7", &format!("user{i}"), "x").await;
        }
        assert_eq!(
            attempt(&guard, "198.51.100.7", ALICE, "right").await,
            LoginOutcome::Refused,
            "a locked address is refused even with a good credential"
        );
        // Another address is unaffected, and so is alice herself.
        assert_eq!(
            attempt(&guard, "198.51.100.8", ALICE, "right").await,
            LoginOutcome::Accepted(alice_id())
        );
    }

    #[tokio::test]
    async fn ipv6_counts_by_slash_64() {
        let guard = plain_guard(config());
        for i in 1..=5 {
            attempt(&guard, &format!("2001:db8:1:2::{i}"), &format!("u{i}"), "x").await;
        }
        assert_eq!(
            attempt(&guard, "2001:db8:1:2:ffff::1", ALICE, "right").await,
            LoginOutcome::Refused
        );
        assert_eq!(
            attempt(&guard, "2001:db8:1:3::1", ALICE, "right").await,
            LoginOutcome::Accepted(alice_id())
        );
    }

    #[tokio::test]
    async fn over_long_input_is_refused_and_counted() {
        let guard = plain_guard(config());
        let huge = "x".repeat(credentials::MAX_PASSWORD_BYTES + 1);
        for _ in 0..3 {
            assert_eq!(
                attempt(&guard, "203.0.113.5", ALICE, &huge).await,
                LoginOutcome::Refused
            );
        }
        assert_eq!(
            attempt(&guard, "203.0.113.6", ALICE, "right").await,
            LoginOutcome::Refused
        );
    }

    #[test]
    fn a_lock_expires_and_is_not_extended_by_attempts_during_it() {
        let limit = Limit {
            max_failures: 2,
            window: Duration::from_secs(10),
            lockout: Duration::from_secs(30),
        };
        let mut table: Table<u64> = Table::new(limit, 10);
        let t0 = Instant::now();
        assert_eq!(table.record_failure(1, t0), None);
        assert_eq!(table.record_failure(1, t0), Some(2));
        // Guesses during the lock do not push it out.
        table.record_failure(1, t0 + Duration::from_secs(29));
        assert!(table.is_locked(&1, t0 + Duration::from_secs(29)));
        assert!(!table.is_locked(&1, t0 + Duration::from_secs(30)));
        // Failures in different windows do not add up.
        let t1 = t0 + Duration::from_secs(40);
        assert_eq!(table.record_failure(1, t1), None);
        assert_eq!(table.record_failure(1, t1 + Duration::from_secs(11)), None);
        assert!(!table.is_locked(&1, t1 + Duration::from_secs(11)));
    }

    #[tokio::test]
    async fn a_flood_of_distinct_usernames_stays_bounded() {
        let mut cfg = config();
        cfg.max_tracked = 50;
        cfg.address.max_failures = u32::MAX;
        let guard = plain_guard(cfg);
        for i in 0..2_000 {
            attempt(&guard, "203.0.113.20", &format!("flood{i}"), "x").await;
        }
        let (users, addresses) = guard.tracked();
        assert!(users <= 50, "{users} usernames tracked");
        assert_eq!(addresses, 1);
    }

    #[test]
    fn a_full_table_evicts_unlocked_entries_before_locks() {
        let limit = Limit {
            max_failures: 2,
            window: Duration::from_secs(600),
            lockout: Duration::from_secs(600),
        };
        let mut table: Table<u64> = Table::new(limit, 3);
        let t = Instant::now();
        table.record_failure(1, t);
        table.record_failure(1, t); // locked
        table.record_failure(2, t + Duration::from_secs(1));
        table.record_failure(3, t + Duration::from_secs(2));
        table.record_failure(4, t + Duration::from_secs(3));
        assert_eq!(table.len(), 3);
        assert!(
            table.is_locked(&1, t + Duration::from_secs(3)),
            "the lock survived"
        );
        assert!(
            !table.entries.contains_key(&2),
            "oldest unlocked went first"
        );
    }

    /// At most `verify_concurrency` verifications at once; the rest wait, and
    /// past `verify_wait` they are refused as busy without touching the
    /// account.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn verifications_are_bounded_and_overflow_is_busy() {
        let running = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let (gate_tx, gate_rx) = std::sync::mpsc::channel::<()>();
        let gate_rx = Arc::new(Mutex::new(gate_rx));
        let mut cfg = config();
        cfg.verify_concurrency = 2;
        cfg.verify_wait = Duration::from_millis(300);
        cfg.username.max_failures = u32::MAX;
        cfg.address.max_failures = u32::MAX;
        let verifier: Verifier = {
            let running = running.clone();
            let peak = peak.clone();
            let gate_rx = gate_rx.clone();
            Arc::new(move |pw: &str, stored: &str| {
                let now = running.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(now, Ordering::SeqCst);
                let _ = gate_rx.lock().unwrap().recv_timeout(Duration::from_secs(5));
                running.fetch_sub(1, Ordering::SeqCst);
                pw == stored
            })
        };
        let guard = Arc::new(LoginGuard::with_verifier(cfg, verifier));
        let lookups = Arc::new(AtomicUsize::new(0));
        let mut tasks = Vec::new();
        for i in 0..5 {
            let guard = guard.clone();
            let lookups = lookups.clone();
            tasks.push(tokio::spawn(async move {
                guard
                    .check(
                        Some(format!("203.0.113.{}", 30 + i).parse().unwrap()),
                        ALICE,
                        "right",
                        |_| async move {
                            lookups.fetch_add(1, Ordering::SeqCst);
                            Ok(Some(CredentialRow {
                                user_id: alice_id(),
                                password_hash: "right".into(),
                            }))
                        },
                    )
                    .await
            }));
        }
        // Hold both slots past the wait, then let them finish.
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert_eq!(
            running.load(Ordering::SeqCst),
            2,
            "two verifications in flight"
        );
        gate_tx.send(()).unwrap();
        gate_tx.send(()).unwrap();
        let mut outcomes = Vec::new();
        for task in tasks {
            outcomes.push(task.await.unwrap());
        }
        let busy = outcomes
            .iter()
            .filter(|o| **o == LoginOutcome::Busy)
            .count();
        let accepted = outcomes
            .iter()
            .filter(|o| **o == LoginOutcome::Accepted(alice_id()))
            .count();
        assert_eq!(
            peak.load(Ordering::SeqCst),
            2,
            "never more than two at once"
        );
        assert_eq!((accepted, busy), (2, 3), "{outcomes:?}");
        assert_eq!(
            lookups.load(Ordering::SeqCst),
            2,
            "a busy refusal never read the account"
        );
    }

    #[test]
    fn config_reads_the_environment_with_safe_defaults() {
        let none = LoginGuardConfig::from_lookup(|_| None);
        assert_eq!(none.username.max_failures, DEFAULT_USERNAME_MAX_FAILURES);
        assert_eq!(none.address.max_failures, DEFAULT_ADDRESS_MAX_FAILURES);
        assert_eq!(
            none.username.window,
            Duration::from_secs(DEFAULT_WINDOW_SECS)
        );
        assert_eq!(none.max_tracked, DEFAULT_MAX_TRACKED);
        assert!((1..=8).contains(&none.verify_concurrency));

        let set = LoginGuardConfig::from_lookup(|name| match name {
            ENV_USERNAME_MAX_FAILURES => Some("5".into()),
            ENV_ADDRESS_LOCKOUT_SECS => Some(" 60 ".into()),
            ENV_VERIFY_CONCURRENCY => Some("3".into()),
            // Zero, junk and empty fall back rather than disabling a limit.
            ENV_ADDRESS_MAX_FAILURES => Some("0".into()),
            ENV_USERNAME_WINDOW_SECS => Some("soon".into()),
            ENV_MAX_TRACKED => Some("".into()),
            _ => None,
        });
        assert_eq!(set.username.max_failures, 5);
        assert_eq!(set.address.lockout, Duration::from_secs(60));
        assert_eq!(set.verify_concurrency, 3);
        assert_eq!(set.address.max_failures, DEFAULT_ADDRESS_MAX_FAILURES);
        assert_eq!(
            set.username.window,
            Duration::from_secs(DEFAULT_WINDOW_SECS)
        );
        assert_eq!(set.max_tracked, DEFAULT_MAX_TRACKED);
    }

    #[test]
    fn the_log_tag_is_stable_and_not_the_name() {
        // sha256("alice") = 2bd806c97f0e00af1a1fc3328fa763a9269723c8db8fac4f93af71db186d6e90
        assert_eq!(username_tag("alice"), "2bd806c97f0e");
        assert!(!username_tag("alice").contains("alice"));
    }

    // ------------------------------------------------------------ source address

    fn headers<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name| {
            pairs
                .iter()
                .find(|(n, _)| n.eq_ignore_ascii_case(name))
                .map(|(_, v)| v.to_string())
        }
    }

    fn ip(s: &str) -> Option<IpAddr> {
        Some(s.parse().unwrap())
    }

    #[test]
    fn forwarded_addresses_count_only_from_our_proxies() {
        let trusted = parse_trusted_peers(DEFAULT_TRUSTED_PEERS);
        let spoof = [
            ("X-Real-IP", "1.2.3.4"),
            ("X-Newblades-Device-Ip", "10.99.0.5"),
        ];
        // A client reaching the server directly is its socket peer, whatever it
        // claims.
        assert_eq!(
            source_from(ip("203.0.113.50"), headers(&spoof), &trusted),
            ip("203.0.113.50")
        );
        assert_eq!(
            source_from(ip("10.99.0.7"), headers(&spoof), &trusted),
            ip("10.99.0.7"),
            "a tunnel address is not a proxy"
        );

        // The public edge (a container on the shared network): X-Real-IP, which
        // it overwrites; it erases the device header, so that is absent.
        assert_eq!(
            source_from(
                ip("172.24.0.4"),
                headers(&[("X-Real-IP", "198.51.100.9")]),
                &trusted
            ),
            ip("198.51.100.9")
        );
        // The tunnel proxy: its device header wins over anything else present.
        assert_eq!(
            source_from(
                ip("172.26.0.1"),
                headers(&[
                    ("X-Newblades-Device-Ip", "10.99.0.5"),
                    ("X-Real-IP", "1.2.3.4")
                ]),
                &trusted
            ),
            ip("10.99.0.5")
        );
        // A proxy that forwarded nothing usable is itself.
        assert_eq!(
            source_from(
                ip("127.0.0.1"),
                headers(&[("X-Real-IP", "not an address")]),
                &trusted
            ),
            ip("127.0.0.1")
        );
        // IPv4-mapped peers are judged as IPv4.
        assert_eq!(
            source_from(
                ip("::ffff:172.24.0.4"),
                headers(&[("X-Real-IP", "198.51.100.9")]),
                &trusted
            ),
            ip("198.51.100.9")
        );
        assert_eq!(source_from(None, headers(&spoof), &trusted), None);
    }

    #[test]
    fn trusted_peer_blocks_parse_and_match() {
        let blocks = parse_trusted_peers(" 172.16.0.0/12 , ::1, junk, 10.0.0.0/33 ");
        assert_eq!(blocks.len(), 2);
        assert!(blocks[0].contains("172.31.255.255".parse().unwrap()));
        assert!(!blocks[0].contains("172.32.0.0".parse().unwrap()));
        assert!(blocks[1].contains("::1".parse().unwrap()));
    }
}

/// The connection is back in the pool before the hash runs: with a pool of ONE
/// connection, another request is served while a verification is in flight.
#[cfg(test)]
mod pool_tests {
    use super::*;
    use bb8::Pool;
    use diesel_async::{
        AsyncConnection, AsyncPgConnection, RunQueryDsl, SimpleAsyncConnection,
        pooled_connection::AsyncDieselConnectionManager,
    };

    struct Db {
        admin_url: String,
        schema: String,
        pool: crate::DbPool,
    }

    impl Db {
        async fn open() -> Option<Self> {
            let Some(admin_url) = std::env::var("TEST_DATABASE_URL").ok() else {
                eprintln!("SKIP: TEST_DATABASE_URL unset — connection release not verified");
                return None;
            };
            let schema = format!("t{}", Uuid::new_v4().simple());
            let mut admin = AsyncPgConnection::establish(&admin_url)
                .await
                .expect("TEST_DATABASE_URL is set but unreachable");
            admin
                .batch_execute(&format!(
                    "CREATE SCHEMA {schema};
                     CREATE TABLE {schema}.arena_credentials (
                         username TEXT PRIMARY KEY,
                         user_id UUID NOT NULL,
                         password_hash TEXT NOT NULL,
                         created_at BIGINT NOT NULL DEFAULT 0,
                         updated_at BIGINT NOT NULL DEFAULT 0);
                     INSERT INTO {schema}.arena_credentials (username, user_id, password_hash)
                         VALUES ('alice', '00000000-0000-0000-0000-0000000a11ce', 'right');"
                ))
                .await
                .unwrap();
            let url = format!(
                "{admin_url}{}options=-c%20search_path%3D{schema}",
                if admin_url.contains('?') { '&' } else { '?' }
            );
            let pool = Pool::builder()
                .max_size(1)
                .connection_timeout(Duration::from_millis(500))
                .build(AsyncDieselConnectionManager::<AsyncPgConnection>::new(url))
                .await
                .unwrap();
            Some(Self {
                admin_url,
                schema,
                pool,
            })
        }

        async fn close(self) {
            drop(self.pool);
            if let Ok(mut admin) = AsyncPgConnection::establish(&self.admin_url).await {
                let _ = admin
                    .batch_execute(&format!("DROP SCHEMA IF EXISTS {} CASCADE", self.schema))
                    .await;
            }
        }
    }

    async fn can_get_a_connection(pool: &crate::DbPool) -> bool {
        match pool.get().await {
            Ok(mut conn) => diesel::sql_query("SELECT 1")
                .execute(&mut *conn)
                .await
                .is_ok(),
            Err(_) => false,
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_connection_is_released_before_the_hash_is_verified() {
        let Some(db) = Db::open().await else {
            return;
        };
        let (started_tx, started_rx) = std::sync::mpsc::channel::<()>();
        let (finish_tx, finish_rx) = std::sync::mpsc::channel::<()>();
        let finish_rx = Mutex::new(finish_rx);
        let started_tx = Mutex::new(started_tx);
        let guard = Arc::new(LoginGuard::with_verifier(
            LoginGuardConfig::from_lookup(|_| None),
            Arc::new(move |pw: &str, stored: &str| {
                started_tx.lock().unwrap().send(()).unwrap();
                let _ = finish_rx
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(10));
                pw == stored
            }),
        ));
        let pool = db.pool.clone();
        let in_flight = {
            let guard = guard.clone();
            tokio::spawn(async move {
                check_login_with(&guard, &pool, ip("203.0.113.70"), "Alice", "right").await
            })
        };
        tokio::task::spawn_blocking(move || started_rx.recv_timeout(Duration::from_secs(10)))
            .await
            .unwrap()
            .expect("the verification started");

        // The one connection is free while the hash is still running.
        assert!(
            can_get_a_connection(&db.pool).await,
            "the login kept its pooled connection while verifying"
        );

        finish_tx.send(()).unwrap();
        assert_eq!(
            in_flight.await.unwrap(),
            LoginOutcome::Accepted(Uuid::from_u128(0xA11CE))
        );

        // Control: the same probe fails when the connection IS held, so the
        // assertion above can fail.
        let held = db.pool.get().await.unwrap();
        assert!(!can_get_a_connection(&db.pool).await);
        drop(held);

        db.close().await;
    }

    fn ip(s: &str) -> Option<IpAddr> {
        Some(s.parse().unwrap())
    }
}
