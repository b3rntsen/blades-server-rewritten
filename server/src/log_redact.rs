//! What may be written to the log about a secret or a device.
//!
//! The log is read by more people, and kept in more places, than the database
//! it describes. Two kinds of value must not reach it whole:
//!
//! - an account's `secret_id`, a session token, a password — never, in any
//!   form. Log the public user id instead; when the only handle on something
//!   is a value that MIGHT be a secret, log [`opaque_tag`].
//! - a device id. The anonymous login is keyed by it, so the full value is
//!   enough to sign in as that device's player. Log [`device_for_log`].

use sha2::{Digest, Sha256};

/// A short tag that says "the same value as last time" and nothing else.
///
/// Eight hex digits of a SHA-256: enough to correlate two log lines, useless
/// for recovering or guessing a 122-bit id.
pub fn opaque_tag(value: &str) -> String {
    let digest = Sha256::digest(value.as_bytes());
    format!(
        "#{:02x}{:02x}{:02x}{:02x}",
        digest[0], digest[1], digest[2], digest[3]
    )
}

/// A device id as the log may show it.
///
/// A tunnel address is kept whole: we allocated it, it is how an operator
/// finds the player, and it is not a credential — the server takes an address
/// only from its own proxy. Anything else is an install id and is cut to the
/// same eight-character prefix the website's audit trail uses, so the two can
/// be matched against each other and against `device_bindings`.
pub fn device_for_log(device_id: &str) -> String {
    let id = device_id.trim();
    if id.parse::<std::net::IpAddr>().is_ok() {
        return id.to_string();
    }
    let prefix: String = id.chars().take(8).collect();
    if id.chars().count() >= 16 {
        format!("{}…", prefix.to_lowercase())
    } else {
        // Too short for a prefix to hide anything.
        opaque_tag(id)
    }
}

/// The line `bnet/link` writes when the credential names a different account
/// from the one the client is signed in as.
///
/// It used to print the linked account's `secret_id` — the value the client
/// stores as its login token and `bnet/login` accepts on every later cold
/// start. `selectedUserId` is not printed either: current clients send the
/// public id there, older ones sent the secret, and the log cannot tell which.
pub fn link_conflict_line(linked_user_id: uuid::Uuid, selected_user_id: Option<&str>) -> String {
    format!(
        "account link: conflict — credential names user {linked_user_id}, client is signed in \
         as {}",
        match selected_user_id {
            Some(selected) => opaque_tag(selected.trim()),
            None => "nobody it named".to_string(),
        }
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    #[test]
    fn the_link_conflict_line_carries_no_secret() {
        let public = Uuid::from_u128(0x1111_2222_3333_4444_5555_6666_7777_8888);
        let secret = Uuid::from_u128(0x9999_aaaa_bbbb_cccc_dddd_eeee_ffff_0000);
        let other_secret = Uuid::from_u128(0x0f0f_0f0f_0f0f_0f0f_0f0f_0f0f_0f0f_0f0f);

        let line = link_conflict_line(public, Some(&other_secret.to_string()));
        assert!(
            line.contains(&public.to_string()),
            "the public id is the handle"
        );
        for hidden in [secret, other_secret] {
            let text = hidden.to_string();
            assert!(!line.contains(&text), "{line}");
            assert!(!line.contains(&text.replace('-', "")), "{line}");
            // Not even a recognisable piece of it.
            assert!(!line.contains(&text[..8]), "{line}");
            assert!(!line.contains(&text[24..]), "{line}");
        }
        assert!(link_conflict_line(public, None).contains("nobody"));
    }

    /// The shipped handler must build its line here and must not format the
    /// secret itself. Read from the source, because the regression is one
    /// `{secret_id}` away and no behaviour test would notice it.
    #[test]
    fn the_link_handler_does_not_format_the_secret_into_a_log() {
        let src = include_str!("authentification.rs");
        let code = src.split("\n#[cfg(test)]").next().unwrap();
        assert!(code.contains("log_redact::link_conflict_line("));
        for (n, statement) in code.split("log::").enumerate().skip(1) {
            let call = macro_call(statement);
            for banned in ["secret_id", "login_token", "password", "extra_secret"] {
                assert!(
                    !call.contains(banned),
                    "log call #{n} in authentification.rs mentions `{banned}`: {call}"
                );
            }
        }
    }

    /// A session id is the bearer token a client presents on every request; the
    /// session store's failure lines may name one only through `opaque_tag`.
    #[test]
    fn the_session_store_does_not_log_a_session_token() {
        // The whole file: session.rs has a `#[cfg(test)]` item near the top, so
        // cutting at the first one would skip the store itself.
        let code = include_str!("session.rs");
        assert!(code.contains("fn persist_session("));
        for level in ["error!(", "warn!(", "info!(", "debug!(", "trace!("] {
            for (n, statement) in code.split(level).enumerate().skip(1) {
                let call = macro_call(statement);
                let without_tags = call.replace("opaque_tag(&session_id", "");
                for banned in ["session_id", "secret_user_id", "extra_secret"] {
                    assert!(
                        !without_tags.contains(banned),
                        "{level} call #{n} in session.rs mentions `{banned}`: {call}"
                    );
                }
            }
        }
    }

    /// The text of one `level!( … )` call: up to the parenthesis that closes
    /// the one it opens, with string literals skipped so a `)` in a message
    /// does not end it early.
    fn macro_call(statement: &str) -> &str {
        let mut depth = 0usize;
        let mut in_string = false;
        let mut escaped = false;
        for (at, c) in statement.char_indices() {
            if in_string {
                match c {
                    _ if escaped => escaped = false,
                    '\\' => escaped = true,
                    '"' => in_string = false,
                    _ => {}
                }
                continue;
            }
            match c {
                '"' => in_string = true,
                '(' => depth += 1,
                ')' if depth <= 1 => return &statement[..=at],
                ')' => depth -= 1,
                _ => {}
            }
        }
        statement
    }

    #[test]
    fn the_scanner_reads_one_whole_call() {
        assert_eq!(
            macro_call(r#"info!("a) {}", f(x)); let secret_id = 1;"#),
            r#"info!("a) {}", f(x))"#
        );
        assert_eq!(
            macro_call(r#"warn!("q \" ) {}", secret_id), next"#),
            r#"warn!("q \" ) {}", secret_id)"#
        );
    }

    #[test]
    fn a_tag_correlates_without_revealing() {
        let id = "9999aaaa-bbbb-cccc-dddd-eeeeffff0000";
        assert_eq!(opaque_tag(id), opaque_tag(id));
        assert_ne!(
            opaque_tag(id),
            opaque_tag("9999aaaa-bbbb-cccc-dddd-eeeeffff0001")
        );
        assert_eq!(opaque_tag(id).len(), 9);
    }

    #[test]
    fn a_device_id_is_cut_and_an_address_is_kept() {
        assert_eq!(
            device_for_log("7D46AC1BF926F22120C2CE82FB834052"),
            "7d46ac1b…"
        );
        assert_eq!(
            device_for_log("6f9619ff-8b86-d011-b42d-00c04fc964ff"),
            "6f9619ff…"
        );
        assert_eq!(device_for_log("10.99.0.24"), "10.99.0.24");
        let short = device_for_log("shortid");
        assert!(short.starts_with('#') && !short.contains("shortid"));
    }
}
