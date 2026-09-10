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
