//! Every span field kaibo declares is classified for export.
//!
//! The allowlist in `src/otel_filter.rs` fails closed on purpose: an attribute nobody
//! blessed never leaves the process. That is the right direction for rig's attributes,
//! which kaibo does not control. For kaibo's *own* span fields it hid a bug: nine fields
//! kaibo declared (`gen_ai.request.thinking`, `gen_ai.response.finish_reason`, `session`,
//! the model slots, the job and batch identifiers) were recorded and then dropped on
//! export, and nothing noticed, because a dropped attribute looks exactly like one that
//! was never set.
//!
//! So this guards it at the source, the way `tests/no_write_path.rs` guards writes: scan
//! the span declarations in `src/` and require each field name to appear in
//! `SAFE_ATTRIBUTES` or `CONTENT_ATTRIBUTES`. A new field then fails here until someone
//! decides which list it belongs in — or decides it should not be recorded at all.
//!
//! **The scan must prove it looked.** A scanner that matched nothing would pass. So the
//! test asserts a floor on how many fields it found, and that fields known to exist
//! (one bare, one dotted, one quoted) are among them.

use std::collections::BTreeMap;
use std::path::Path;

use kaibo::otel_filter::{CONTENT_ATTRIBUTES, SAFE_ATTRIBUTES};

/// The span-opening forms kaibo uses. Each is followed by a parenthesized argument
/// list whose `key = value` pairs are the span's fields.
const OPENERS: &[&str] = &[
    "info_span!(",
    "debug_span!(",
    "trace_span!(",
    "warn_span!(",
    "error_span!(",
    "instrument(",
];

/// Keys inside those argument lists that configure the span rather than name a field.
const META_KEYS: &[&str] = &[
    "name", "level", "target", "parent", "skip", "skip_all", "err", "ret",
];

/// Production source only: everything from the first `#[cfg(test)]` *module* on is
/// test code, whose spans never reach an exporter. A lone `#[cfg(test)]` item (a helper
/// or a test-only field) does not end production code, so the cut waits for one whose
/// next line opens a `mod`.
fn production_part(src: &str) -> &str {
    let mut from = 0;
    while let Some(i) = src[from..].find("#[cfg(test)]") {
        let at = from + i;
        let rest = src[at + "#[cfg(test)]".len()..].trim_start();
        if rest.starts_with("mod ")
            || rest.starts_with("pub mod ")
            || rest.starts_with("pub(crate) mod ")
        {
            return &src[..at];
        }
        from = at + 1;
    }
    src
}

/// The argument list after the opener at `start`, up to its matching `)`, with line
/// comments and string contents blanked so neither can fake a `key =` pair. A quoted
/// key (`"kaish.exit_code" = ...`) is kept: it is the one string that names a field.
fn argument_list(src: &str, open_paren: usize) -> String {
    let bytes = src.as_bytes();
    let mut depth = 0usize;
    let mut i = open_paren;
    let mut in_str = false;
    while i < bytes.len() {
        let c = bytes[i];
        if in_str {
            if c == b'\\' {
                i += 2;
                continue;
            }
            if c == b'"' {
                in_str = false;
            }
        } else if c == b'"' {
            in_str = true;
        } else if c == b'/' && bytes.get(i + 1) == Some(&b'/') {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        } else if c == b'(' {
            depth += 1;
        } else if c == b')' {
            depth -= 1;
            if depth == 0 {
                return strip_line_comments(&src[open_paren + 1..i]);
            }
        }
        i += 1;
    }
    panic!("unbalanced span argument list at byte {open_paren}");
}

fn strip_line_comments(s: &str) -> String {
    s.lines()
        .map(|l| match l.find("//") {
            Some(i) => &l[..i],
            None => l,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The field keys in one argument list: `key = …`, `"quoted.key" = …`, and
/// `dotted.key = …`, but not `==` and not a meta key.
fn field_keys(args: &str) -> Vec<String> {
    let mut keys = Vec::new();
    let bytes = args.as_bytes();
    for (eq, _) in args.match_indices('=') {
        if bytes.get(eq + 1) == Some(&b'=')
            || (eq > 0 && matches!(bytes[eq - 1], b'=' | b'!' | b'<' | b'>'))
        {
            continue;
        }
        let before = args[..eq].trim_end();
        let key = if let Some(stripped) = before.strip_suffix('"') {
            match stripped.rfind('"') {
                Some(q) => stripped[q + 1..].to_string(),
                None => continue,
            }
        } else {
            let start = before
                .rfind(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '.'))
                .map_or(0, |i| i + 1);
            before[start..].to_string()
        };
        if key.is_empty() || META_KEYS.contains(&key.as_str()) {
            continue;
        }
        // A string literal's body was not blanked, so `name = "run_phase"` is caught
        // by META_KEYS above; anything that is not an identifier path is not a field.
        if !key
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c == '_')
        {
            continue;
        }
        keys.push(key);
    }
    keys
}

fn collect(dir: &Path, found: &mut BTreeMap<String, Vec<String>>) {
    for entry in std::fs::read_dir(dir).expect("read src dir") {
        let path = entry.expect("dir entry").path();
        if path.is_dir() {
            collect(&path, found);
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let text = std::fs::read_to_string(&path).expect("read source file");
        let src = production_part(&text);
        for opener in OPENERS {
            for (at, _) in src.match_indices(opener) {
                let open_paren = at + opener.len() - 1;
                let line = src[..at].matches('\n').count() + 1;
                for key in field_keys(&argument_list(src, open_paren)) {
                    found
                        .entry(key)
                        .or_default()
                        .push(format!("{}:{line}", path.display()));
                }
            }
        }
    }
}

#[test]
fn every_span_field_kaibo_declares_is_classified_for_export() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut found = BTreeMap::new();
    collect(&src, &mut found);

    // The scan looked somewhere real: a bare field, a dotted field, and a quoted field
    // that kaibo is known to declare, and a floor on the total.
    for known in ["cast", "gen_ai.request.thinking", "kaish.exit_code"] {
        assert!(
            found.contains_key(known),
            "the scanner missed `{known}`, a field kaibo declares — the scan is broken, \
             not the allowlist. Found: {:?}",
            found.keys().collect::<Vec<_>>()
        );
    }
    assert!(
        found.len() >= 15,
        "only {} distinct span fields found; expected at least 15: {:?}",
        found.len(),
        found.keys().collect::<Vec<_>>()
    );
    eprintln!("checked {} distinct span fields", found.len());

    let unclassified: Vec<String> = found
        .iter()
        .filter(|(k, _)| {
            !SAFE_ATTRIBUTES.contains(&k.as_str()) && !CONTENT_ATTRIBUTES.contains(&k.as_str())
        })
        .map(|(k, sites)| format!("`{k}` at {}", sites.join(", ")))
        .collect();
    assert!(
        unclassified.is_empty(),
        "these span fields are declared but in neither SAFE_ATTRIBUTES nor \
         CONTENT_ATTRIBUTES (src/otel_filter.rs), so the exporter drops them:\n  {}\n\
         Add each to SAFE_ATTRIBUTES if it is an identifier, count, timing, or outcome; to \
         CONTENT_ATTRIBUTES if it can carry prompts, source, or paths; or stop recording it.",
        unclassified.join("\n  ")
    );
}
