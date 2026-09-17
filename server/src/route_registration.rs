//! Every routed handler must be registered — the one that was not 404'd silently.
//!
//! `character_ops::levelup` carried a full `#[post(...)]` attribute and a working
//! body, and `main.rs` never `.service()`d it (tracker #108). Its six siblings in
//! the same module were all registered, so spending a level into STAMINA or
//! MAGICKA returned 404 while every neighbouring operation worked. Nothing failed
//! at compile time: an unregistered handler is just a function nobody calls, and
//! the only symptom was a dead-code warning for its unused `LevelupRequest`,
//! sitting among warnings for features that genuinely are not implemented yet.
//!
//! So the invariant is checked here instead. At the time of writing the server has
//! 77 attributed handlers and, with the fix, all 77 are registered — the check
//! needs no allowlist, which is what makes it worth having.

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::fs;
    use std::path::{Path, PathBuf};

    fn src_dir() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
    }

    fn rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
        for e in fs::read_dir(dir).expect("read src dir").flatten() {
            let p = e.path();
            if p.is_dir() {
                rs_files(&p, out);
            } else if p.extension().is_some_and(|x| x == "rs") {
                out.push(p);
            }
        }
    }

    /// Handler names carrying an actix routing attribute, as `module::fn`.
    ///
    /// Matches `#[get("…")]` / `#[post(…)]` / `#[put]` / `#[delete]` followed by a
    /// `pub async fn`, tolerating further attributes in between.
    fn attributed_handlers() -> BTreeSet<String> {
        let mut files = Vec::new();
        rs_files(&src_dir(), &mut files);
        let mut found = BTreeSet::new();
        for f in files {
            let module = f.file_stem().unwrap().to_string_lossy().to_string();
            // Skip this file: its own prose quotes `#[post(` and `async fn` to
            // explain what it looks for, and the scanner cannot tell a doc
            // comment from code. Caught by the check itself on first run, which
            // is a small proof that the scan really is reading files.
            if module == "route_registration" {
                continue;
            }
            let src = fs::read_to_string(&f).unwrap_or_default();
            let mut rest = src.as_str();
            while let Some(i) = rest.find("#[") {
                rest = &rest[i..];
                let is_route = ["#[get(", "#[post(", "#[put(", "#[delete("]
                    .iter()
                    .any(|p| rest.starts_with(p));
                if is_route {
                    // Walk forward to the first `pub … async fn NAME`, which the
                    // attribute applies to.
                    if let Some(j) = rest.find("async fn ") {
                        let after = &rest[j + "async fn ".len()..];
                        let name: String =
                            after.chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect();
                        // Only count it when `pub` sits between the attribute and
                        // the fn — a private routed handler cannot be registered
                        // from main.rs and is not what this guards.
                        if rest[..j].contains("pub") && !name.is_empty() {
                            found.insert(format!("{module}::{name}"));
                        }
                    }
                }
                rest = &rest[2..];
            }
        }
        found
    }

    /// Names passed to `.service(...)` in `main.rs`, ignoring the module path.
    fn registered() -> BTreeSet<String> {
        let main = fs::read_to_string(src_dir().join("main.rs")).expect("read main.rs");
        let mut out = BTreeSet::new();
        let mut rest = main.as_str();
        while let Some(i) = rest.find(".service(") {
            rest = &rest[i + ".service(".len()..];
            let arg: String = rest
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == ':')
                .collect();
            if let Some(name) = arg.rsplit("::").next() {
                if !name.is_empty() {
                    out.insert(name.to_string());
                }
            }
        }
        out
    }

    #[test]
    fn every_routed_handler_is_registered_in_main() {
        let handlers = attributed_handlers();
        let registered = registered();

        // Control: the scan must actually find handlers. A regex that matches
        // nothing would make this test pass forever while checking nothing —
        // which is the failure mode of the thing it is guarding against.
        assert!(
            handlers.len() > 60,
            "only found {} attributed handlers; the scan is broken, not the server",
            handlers.len()
        );
        assert!(registered.len() > 60, "only found {} registered services", registered.len());

        let missing: Vec<&String> = handlers
            .iter()
            .filter(|h| {
                let name = h.rsplit("::").next().unwrap();
                !registered.contains(name)
            })
            .collect();
        assert!(
            missing.is_empty(),
            "these handlers carry a routing attribute but are never .service()'d, \
             so they answer 404: {missing:?}"
        );
    }

    /// The specific regression, named, so the reason survives even if the sweep
    /// above is ever loosened.
    #[test]
    fn levelup_is_registered() {
        assert!(
            registered().contains("levelup"),
            "POST /levelup 404s: character_ops::levelup is implemented but not registered"
        );
    }
}

/// Every endpoint retail answered must have a route here.
///
/// The registration check above proves we serve what we wrote. This one asks the
/// other question — whether what we wrote covers the game — by reading the
/// endpoint inventory mined from the whole capture corpus
/// (`deploy/retail-journey/endpoint_coverage.json`, 88 endpoints across 40
/// players) and requiring a route for each.
///
/// It is not hypothetical. It is what found that a player could not walk into a
/// friend's town and buy from their merchants: 1,673 captured requests from nine
/// different players, on a pair of routes that simply did not exist. None of them
/// were the player whose captures the rest of this work was built from, which is
/// the argument for measuring coverage across everyone rather than reading one
/// journey closely.
#[cfg(test)]
mod retail_coverage {
    use std::collections::BTreeSet;
    use std::fs;
    use std::path::Path;

    /// Endpoints we knowingly do not serve, each with the reason. An entry here is
    /// a decision; anything else failing this test is a gap.
    const NOT_SERVED: &[(&str, &str)] = &[
        (
            "DELETE /characters/{id}/loadouts/profiles/{n}",
            "Deleting a saved loadout profile. 3 captures from 1 player — the only \
             route in the corpus we do not answer. Saving over a slot (POST) works, \
             so nothing is unreachable; clearing one is not.",
        ),
    ];

    /// Route paths declared anywhere in `server/src`, normalised to the shape the
    /// capture inventory uses.
    fn declared_routes() -> BTreeSet<String> {
        fn walk(dir: &Path, out: &mut Vec<String>) {
            for e in fs::read_dir(dir).expect("read src").flatten() {
                let p = e.path();
                if p.is_dir() {
                    walk(&p, out);
                } else if p.extension().is_some_and(|x| x == "rs") {
                    out.push(fs::read_to_string(&p).unwrap_or_default());
                }
            }
        }
        let mut sources = Vec::new();
        walk(
            &Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
            &mut sources,
        );

        let mut out = BTreeSet::new();
        for src in sources {
            for (verb, marker) in [
                ("GET", "#[get("),
                ("POST", "#[post("),
                ("PUT", "#[put("),
                ("DELETE", "#[delete("),
            ] {
                let mut rest = src.as_str();
                while let Some(i) = rest.find(marker) {
                    rest = &rest[i + marker.len()..];
                    let Some(start) = rest.find('"') else { break };
                    let Some(len) = rest[start + 1..].find('"') else {
                        break;
                    };
                    let raw = &rest[start + 1..start + 1 + len];
                    rest = &rest[start + 1 + len..];
                    if let Some(path) = normalise(raw) {
                        out.insert(format!("{verb} {path}"));
                    }
                }
            }
        }
        out
    }

    /// `"/blades.bgs.services/api/game/v1/public/characters/{character_id}/levelup"`
    /// → `"/characters/{id}/levelup"`. Returns `None` for a route on another host
    /// or service, which the inventory does not cover.
    fn normalise(raw: &str) -> Option<String> {
        let path = raw.split("/api/game/v1/public").nth(1)?;
        let mut out = String::new();
        for seg in path.split('/').skip(1) {
            out.push('/');
            if seg.starts_with('{') {
                // `{index}` is a number on the wire; every other placeholder is a
                // uuid. The inventory spells them `{n}` and `{id}`.
                out.push_str(if seg.contains("index") { "{n}" } else { "{id}" });
            } else {
                out.push_str(seg);
            }
        }
        Some(out)
    }

    #[test]
    fn every_endpoint_retail_answered_has_a_route() {
        let p = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../deploy/retail-journey/endpoint_coverage.json");
        let raw = fs::read_to_string(&p).unwrap_or_else(|e| panic!("{p:?}: {e}"));
        let inventory: serde_json::Value = serde_json::from_str(&raw).expect("valid json");
        let rows = inventory["observations"].as_array().expect("observations");
        assert!(rows.len() >= 80, "only {} endpoints in the inventory", rows.len());

        let ours = declared_routes();
        assert!(
            ours.len() > 80,
            "only {} routes parsed out of the source — the scan is broken, and a \
             broken scan would report the whole game as missing",
            ours.len()
        );
        let excused: BTreeSet<&str> = NOT_SERVED.iter().map(|(e, _)| *e).collect();

        let mut missing = Vec::new();
        for row in rows {
            let endpoint = row["endpoint"].as_str().unwrap();
            // `{n}` and `{id}` are both identifiers; a chest slot is numeric on the
            // wire and a uuid in our path, and neither side is wrong.
            let alt = endpoint.replace("{n}", "{id}");
            if ours.contains(endpoint) || ours.contains(&alt) || excused.contains(endpoint) {
                continue;
            }
            missing.push(format!(
                "{endpoint}  ({} captures, {} players)",
                row["captures"], row["players"]
            ));
        }
        assert!(
            missing.is_empty(),
            "retail answered these and we do not — each is a step of the game a \
             player cannot take. Serve it, or add it to NOT_SERVED with the \
             reason:\n  {}",
            missing.join("\n  ")
        );
    }

    /// An excuse must name something that is actually in the corpus, or it is
    /// stale text protecting nothing.
    #[test]
    fn every_excuse_names_a_real_endpoint() {
        let p = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../deploy/retail-journey/endpoint_coverage.json");
        let raw = fs::read_to_string(&p).unwrap();
        let inventory: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let known: BTreeSet<&str> = inventory["observations"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["endpoint"].as_str().unwrap())
            .collect();
        for (endpoint, why) in NOT_SERVED {
            assert!(
                known.contains(endpoint),
                "NOT_SERVED excuses `{endpoint}`, which retail never answered"
            );
            assert!(why.len() > 40, "`{endpoint}` needs a real reason, not a label");
        }
    }
}

/// Production code must ask the LOADED scaling table, never `::default()`.
///
/// `QuestLevelScaling::default()` is an empty table, and `given_xp` falls through
/// an empty table to the last-resort `100 * enemyLevel` formula. That was harmless
/// while the default WAS the formula, and became a silent bug the moment retail's
/// real numbers moved into `quests_daily.json`: five call sites on the town-job
/// path kept handing out the old XP while quests paid retail's, and a fresh
/// character's board is entirely jobs, so it was the first thing a new player saw.
///
/// Nothing failed. Every test agreed, because every test was asking the same
/// fallback. Only driving a real character through a real server surfaced it —
/// which is the argument for this check existing at all.
#[cfg(test)]
mod scaling_comes_from_the_loaded_table {
    use std::fs;
    use std::path::{Path, PathBuf};

    fn rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
        for e in fs::read_dir(dir).expect("read src").flatten() {
            let p = e.path();
            if p.is_dir() {
                rs_files(&p, out);
            } else if p.extension().is_some_and(|x| x == "rs") {
                out.push(p);
            }
        }
    }

    /// The source with every `#[cfg(test)] mod … { … }` block removed.
    ///
    /// Cutting the file at its FIRST `#[cfg(test)]` is not good enough: `quest.rs`
    /// interleaves eight test modules with production code, so that heuristic
    /// treats 3,900 lines of handlers as tests and the check passes while the bug
    /// sits in the middle of them. This one counts braces.
    fn production_lines(src: &str) -> Vec<(usize, &str)> {
        let mut out = Vec::new();
        let mut depth: i32 = 0;
        let mut in_test = false;
        let mut pending = false;
        for (n, line) in src.lines().enumerate() {
            let t = line.trim_start();
            if !in_test && t.starts_with("#[cfg(test)]") {
                pending = true;
                continue;
            }
            if pending && (t.starts_with("mod ") || t.starts_with("pub mod ")) {
                in_test = true;
                pending = false;
                depth = 0;
            }
            if in_test {
                depth += line.matches('{').count() as i32;
                depth -= line.matches('}').count() as i32;
                if depth <= 0 && line.contains('}') {
                    in_test = false;
                }
                continue;
            }
            // `#[cfg(test)]` on a bare fn — one line of attribute, not a block.
            if pending {
                pending = false;
                continue;
            }
            out.push((n + 1, line));
        }
        out
    }

    #[test]
    fn no_production_code_asks_an_empty_scaling_table() {
        let mut files = Vec::new();
        rs_files(&Path::new(env!("CARGO_MANIFEST_DIR")).join("src"), &mut files);

        let mut offenders = Vec::new();
        let mut scanned = 0;
        for f in files {
            let name = f.file_name().unwrap().to_string_lossy().to_string();
            if name == "route_registration.rs" {
                continue; // this file quotes the pattern to explain it
            }
            let src = fs::read_to_string(&f).unwrap_or_default();
            for (n, line) in production_lines(&src) {
                scanned += 1;
                // Prose naming the pattern in order to warn about it is not it.
                if line.trim_start().starts_with("//") {
                    continue;
                }
                if line.contains("QuestLevelScaling::default()") {
                    offenders.push(format!("{name}:{n} — {}", line.trim()));
                }
            }
        }
        assert!(
            scanned > 20_000,
            "only {scanned} production lines scanned — the test-block stripper ate \
             the codebase, and an empty scan cannot find anything"
        );
        assert!(
            offenders.is_empty(),
            "these ask an EMPTY scaling table, which silently answers with the old \
             `100 * enemyLevel` formula. Thread the loaded \
             `static_data.quests_daily.level_scaling` through instead:\n  {}",
            offenders.join("\n  ")
        );
    }
}
